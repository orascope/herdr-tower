// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Keeping what the tower leaves on disk from growing without end.
//!
//! Most of the state directory already has a bound — the records go a day
//! after their agent dies, `supervisor.log` rolls over at a mebibyte — but a
//! few things only ever grew: a lock file for every popup ever opened, the
//! crash output launchd appends to `daemon.log`, and the loop instruction
//! remembered for every session that ever had one. The resident supervisor sweeps them at start and hourly.
//!
//! Nothing here fails the supervisor: a file that cannot be read, locked or
//! removed is left for the next sweep.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tower_state::{LoopStore, Store};

/// A tower lock file nobody holds is removed once it is this old.
///
/// A week, not an hour: a tower that restarts in the terminal it had — the
/// dock does — opens the same file again, and a lock removed in the instant
/// between its open and its lock would let a second tower take that terminal.
/// A terminal whose last tower went quiet a week ago is not coming back.
pub const LOCK_MIN_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// `daemon.log` past this is cut down to its tail.
pub const DAEMON_LOG_MAX: u64 = 1024 * 1024;
/// How much of `daemon.log`'s tail is kept.
pub const DAEMON_LOG_KEEP: u64 = 64 * 1024;

/// What one sweep removed, for the supervisor's log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sweep {
    pub locks: usize,
    pub messages: usize,
}

impl Sweep {
    /// One line for the log, or `None` when nothing was removed.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.locks > 0 {
            parts.push(format!("{} stale tower locks", self.locks));
        }
        if self.messages > 0 {
            parts.push(format!("{} remembered loop messages", self.messages));
        }
        (!parts.is_empty()).then(|| format!("housekeeping: removed {}", parts.join(", ")))
    }
}

/// One sweep of the state directory `store` lives in, and of the tower locks
/// under `lock_roots` (the state directory's `towers` and the runtime
/// coordination directory).
pub fn sweep(store: &Store, loops: &LoopStore, lock_roots: &[PathBuf], now: SystemTime) -> Sweep {
    let locks = lock_roots
        .iter()
        .map(|root| prune_tower_locks(root, now))
        .sum();
    let messages = store
        .list()
        .ok()
        .map(|agents| {
            let keep: Vec<(String, String)> = agents
                .into_iter()
                .filter(|a| !a.session_id.trim().is_empty())
                .map(|a| (a.pane.backend, a.session_id))
                .collect();
            loops.prune_messages(&keep).unwrap_or(0)
        })
        .unwrap_or(0);
    Sweep { locks, messages }
}

/// Cuts `daemon.log` down to its last [`DAEMON_LOG_KEEP`] bytes once it is past
/// [`DAEMON_LOG_MAX`], and says whether it did.
///
/// In place, never replaced: launchd holds the file open for the supervisor's
/// stdout and stderr, and a file renamed over it would leave launchd writing
/// into one nobody can see.
pub fn trim_daemon_log(path: &Path) -> std::io::Result<bool> {
    let mut f = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    if len <= DAEMON_LOG_MAX {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(len - DAEMON_LOG_KEEP))?;
    let mut tail = Vec::with_capacity(DAEMON_LOG_KEEP as usize);
    f.read_to_end(&mut tail)?;
    f.set_len(0)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&tail)?;
    Ok(true)
}

/// Removes the tower lock files under `root` (one folder per herdr server)
/// that nobody holds and that are older than [`LOCK_MIN_AGE`]. `dock.lock` is
/// never touched. Answers how many went.
///
/// Each is removed while this sweep holds it, so no tower can be holding it at
/// the time.
pub fn prune_tower_locks(root: &Path, now: SystemTime) -> usize {
    let Ok(servers) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for server in servers.flatten() {
        let Ok(locks) = fs::read_dir(server.path()) else {
            continue;
        };
        for lock in locks.flatten() {
            let name = lock.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".lock") || name == "dock.lock" {
                continue;
            }
            let old = lock
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > LOCK_MIN_AGE);
            if !old {
                continue;
            }
            let Ok(f) = fs::OpenOptions::new().write(true).open(lock.path()) else {
                continue;
            };
            if f.try_lock().is_ok() && fs::remove_file(lock.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// The folders tower locks live under: the state directory's own, and the
/// runtime coordination directory when there is one.
pub fn lock_roots(state_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![state_dir.join("towers")];
    if let Ok(runtime) = tower_state::coordination_dir() {
        roots.push(runtime);
    }
    let mut seen = HashSet::new();
    roots.retain(|r| seen.insert(r.clone()));
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// The coarsest modification time a filesystem keeps (FAT and exFAT: two
    /// seconds). A file set to exactly a bound's age can come back older than
    /// it, rounded down, so "still within" is tested this far inside.
    const MTIME_GRAIN: Duration = Duration::from_secs(2);

    fn file(path: &Path, len: usize, age: Duration, now: SystemTime) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; len]).unwrap();
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(now - age))
            .unwrap();
    }

    #[test]
    fn only_old_unheld_tower_locks_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let server = root.path().join("0123456789abcdef");
        file(&server.join("term_old.lock"), 0, 8 * DAY, now);
        file(&server.join("term_fresh.lock"), 0, DAY, now);
        file(&server.join("term_held.lock"), 0, 8 * DAY, now);
        file(&server.join("dock.lock"), 0, 30 * DAY, now);
        file(&server.join("notes.txt"), 0, 30 * DAY, now);
        let held = fs::File::options()
            .write(true)
            .open(server.join("term_held.lock"))
            .unwrap();
        held.lock().unwrap();

        assert_eq!(prune_tower_locks(root.path(), now), 1);
        assert!(!server.join("term_old.lock").exists());
        for kept in [
            "term_fresh.lock",
            "term_held.lock",
            "dock.lock",
            "notes.txt",
        ] {
            assert!(server.join(kept).exists(), "{kept} is kept");
        }
        drop(held);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_log_is_cut_to_its_tail_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        assert!(!trim_daemon_log(&path).unwrap(), "no file, nothing to do");
        fs::write(&path, b"short\n").unwrap();
        assert!(!trim_daemon_log(&path).unwrap());

        let mut body = vec![b'a'; DAEMON_LOG_MAX as usize];
        body.extend_from_slice(b"the end\n");
        fs::write(&path, &body).unwrap();
        // A writer holding the file open keeps writing into the same file.
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap());
        assert!(trim_daemon_log(&path).unwrap());
        let kept = fs::read(&path).unwrap();
        assert_eq!(kept.len() as u64, DAEMON_LOG_KEEP);
        assert!(kept.ends_with(b"the end\n"));
        writer.write_all(b"after\n").unwrap();
        assert!(fs::read(&path).unwrap().ends_with(b"the end\nafter\n"));
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap()),
            inode,
            "the same file, not a replacement"
        );
    }

    /// A loop instruction stays remembered while a record or a lease still
    /// names its conversation, and goes once neither does.
    #[test]
    fn a_sweep_forgets_loop_messages_nobody_can_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path()).unwrap();
        let loops = LoopStore::new(dir.path()).unwrap();
        let now = time::OffsetDateTime::now_utc();
        let mut kept = tower_state::Agent::new("claude-w2:p1", "claude", now);
        kept.session_id = "sid-kept".into();
        kept.pane.pane_id = "w2:p1".into();
        store.save(&kept).unwrap();
        fs::write(
            dir.path().join("loop-messages.json"),
            r#"{"sid-kept": "keep going", "sid-gone": "long forgotten"}"#,
        )
        .unwrap();

        let swept = sweep(&store, &loops, &[], SystemTime::now());
        assert_eq!(swept.messages, 1);
        assert_eq!(
            loops.remembered_message("", "sid-kept").unwrap().as_deref(),
            Some("keep going")
        );
        assert_eq!(loops.remembered_message("", "sid-gone").unwrap(), None);
        assert_eq!(sweep(&store, &loops, &[], SystemTime::now()).messages, 0);
    }

    /// A small deterministic generator (an LCG), so the property tests walk the
    /// same inputs on every run and a failure can be replayed.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// The week is "more than", not "at least", so a lock at the week (to
    /// within [`MTIME_GRAIN`]) is kept.
    /// A missing root is no error, a `.lock` file sitting directly in the root
    /// is not in a server folder and is left alone, a name that only contains
    /// `.lock` is not a lock, and every server folder is counted.
    #[test]
    fn stale_locks_are_counted_across_servers_and_nothing_else_is_touched() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        assert_eq!(prune_tower_locks(&root.path().join("none"), now), 0);

        let (one, two) = (
            root.path().join("server-one"),
            root.path().join("server-two"),
        );
        file(&one.join("term_a.lock"), 0, 8 * DAY, now);
        file(&two.join("term_b.lock"), 0, 8 * DAY, now);
        file(
            &two.join("term_week.lock"),
            0,
            LOCK_MIN_AGE - MTIME_GRAIN,
            now,
        );
        file(&two.join("term_c.lock.bak"), 0, 30 * DAY, now);
        file(&root.path().join("loose.lock"), 0, 30 * DAY, now);

        assert_eq!(prune_tower_locks(root.path(), now), 2);
        assert!(!one.join("term_a.lock").exists() && !two.join("term_b.lock").exists());
        for kept in [
            two.join("term_week.lock"),
            two.join("term_c.lock.bak"),
            root.path().join("loose.lock"),
        ] {
            assert!(kept.exists(), "{} is kept", kept.display());
        }
        assert_eq!(prune_tower_locks(root.path(), now), 0, "a second sweep");
    }

    /// Exactly a mebibyte is still within the bound; one byte more is cut to
    /// the tail. An empty file is nothing to do, and a path that is a folder is
    /// an error to report rather than a quiet `false`.
    #[test]
    fn the_daemon_log_bound_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        fs::write(&path, b"").unwrap();
        assert!(!trim_daemon_log(&path).unwrap(), "empty");

        let at = vec![b'a'; DAEMON_LOG_MAX as usize];
        fs::write(&path, &at).unwrap();
        assert!(!trim_daemon_log(&path).unwrap(), "at the bound");
        assert_eq!(fs::read(&path).unwrap(), at);

        let mut over = at.clone();
        over.push(b'z');
        fs::write(&path, &over).unwrap();
        assert!(trim_daemon_log(&path).unwrap(), "a byte over");
        assert_eq!(
            fs::read(&path).unwrap(),
            &over[over.len() - DAEMON_LOG_KEEP as usize..]
        );

        assert!(trim_daemon_log(dir.path()).is_err(), "a folder");
    }

    /// Property, against the naive rule written out: a log over the bound
    /// becomes exactly its last 64 KiB — a suffix, byte for byte, of what was
    /// there — and one within it is untouched. Either way the result is within
    /// the bound, and trimming again changes nothing.
    #[test]
    fn a_trimmed_log_is_the_tail_of_the_original_and_within_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut rng = Lcg(64 * 1024);
        let max = DAEMON_LOG_MAX as usize;
        for round in 0..16 {
            // Half of them within a few bytes of the bound, where it matters.
            let len = if round % 2 == 0 {
                max - 3 + rng.below(7) as usize
            } else {
                rng.below(3 * DAEMON_LOG_MAX) as usize
            };
            let body: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
            fs::write(&path, &body).unwrap();

            let want: &[u8] = if len > max {
                &body[len - DAEMON_LOG_KEEP as usize..]
            } else {
                &body
            };
            assert_eq!(trim_daemon_log(&path).unwrap(), len > max, "round {round}");
            let got = fs::read(&path).unwrap();
            assert!(got.len() <= max, "round {round}");
            assert!(body.ends_with(&got), "round {round}: a suffix");
            assert_eq!(got, want, "round {round}");
            assert!(!trim_daemon_log(&path).unwrap(), "round {round}: again");
        }
    }

    /// One sweep does all of it: the locks under every root it is handed,
    /// added up. The second sweep finds nothing, so it logs nothing.
    #[test]
    fn a_sweep_adds_up_every_root_and_a_second_one_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let store = Store::new(dir.path()).unwrap();
        let loops = LoopStore::new(dir.path()).unwrap();
        let roots = [dir.path().join("towers"), dir.path().join("runtime")];
        for root in &roots {
            file(&root.join("server/term.lock"), 0, 8 * DAY, now);
        }

        let first = sweep(&store, &loops, &roots, now);
        assert_eq!(
            first,
            Sweep {
                locks: 2,
                messages: 0,
            }
        );
        let second = sweep(&store, &loops, &roots, now);
        assert_eq!(second, Sweep::default());
        assert_eq!(second.summary(), None);
    }

    /// A record keeps its conversation's instruction under its own herdr
    /// server's name: the same session ID remembered for a different server,
    /// or for none, is a different conversation and goes.
    #[test]
    fn a_record_keeps_the_loop_message_of_its_own_server_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path()).unwrap();
        let loops = LoopStore::new(dir.path()).unwrap();
        let mut agent =
            tower_state::Agent::new("claude-w1:p1", "claude", time::OffsetDateTime::now_utc());
        agent.session_id = "sid".into();
        agent.pane.pane_id = "w1:p1".into();
        agent.pane.backend = "srv2".into();
        store.save(&agent).unwrap();
        fs::write(
            dir.path().join("loop-messages.json"),
            r#"{"srv2:sid": "mine", "sid": "default server", "srv3:sid": "another"}"#,
        )
        .unwrap();

        assert_eq!(sweep(&store, &loops, &[], SystemTime::now()).messages, 2);
        assert_eq!(
            loops.remembered_message("srv2", "sid").unwrap().as_deref(),
            Some("mine")
        );
        assert_eq!(loops.remembered_message("", "sid").unwrap(), None);
        assert_eq!(loops.remembered_message("srv3", "sid").unwrap(), None);
    }

    /// The state folder's `towers` comes first; after it, at most one more
    /// root, the runtime `coordination` folder, absolute and outside the state
    /// folder; and no folder is listed twice. Stated as what the roots must
    /// look like rather than rebuilt the way `lock_roots` builds them, which
    /// would agree with whatever it did.
    #[test]
    fn lock_roots_are_the_state_towers_folder_then_the_runtime_one() {
        let dir = tempfile::tempdir().unwrap();
        let roots = lock_roots(dir.path());
        assert_eq!(roots.first(), Some(&dir.path().join("towers")));
        assert!(roots.len() <= 2, "{roots:?}");
        if let Some(runtime) = roots.get(1) {
            assert!(runtime.is_absolute(), "{}", runtime.display());
            assert_eq!(
                runtime.file_name().and_then(|n| n.to_str()),
                Some("coordination")
            );
            assert!(!runtime.starts_with(dir.path()), "{}", runtime.display());
        }
        assert_eq!(roots.iter().collect::<HashSet<_>>().len(), roots.len());
    }

    /// The summary names each kind in a fixed order, only when its count is not
    /// zero.
    #[test]
    fn the_summary_names_each_nonzero_kind_in_order() {
        let cases = [
            (
                Sweep {
                    messages: 2,
                    ..Sweep::default()
                },
                Some("housekeeping: removed 2 remembered loop messages"),
            ),
            (
                Sweep {
                    locks: 1,
                    messages: 4,
                },
                Some("housekeeping: removed 1 stale tower locks, 4 remembered loop messages"),
            ),
            (
                Sweep {
                    locks: 2,
                    ..Sweep::default()
                },
                Some("housekeeping: removed 2 stale tower locks"),
            ),
        ];
        for (sweep, want) in cases {
            assert_eq!(sweep.summary().as_deref(), want, "{sweep:?}");
        }
    }

    /// Property: there is a line exactly when something was removed, and it
    /// carries every count that is not zero.
    #[test]
    fn the_summary_exists_exactly_when_something_was_removed() {
        let mut rng = Lcg(7);
        for _ in 0..500 {
            let s = Sweep {
                locks: rng.below(3) as usize,
                messages: rng.below(3) as usize,
            };
            let line = s.summary();
            assert_eq!(line.is_some(), s.locks + s.messages > 0, "{s:?}");
            let line = line.unwrap_or_default();
            for (count, what) in [
                (s.locks, "stale tower locks"),
                (s.messages, "remembered loop messages"),
            ] {
                assert_eq!(
                    line.contains(&format!("{count} {what}")),
                    count > 0,
                    "{s:?}: {line}"
                );
            }
        }
    }

    #[test]
    fn the_summary_says_only_what_was_removed() {
        assert_eq!(Sweep::default().summary(), None);
        let s = Sweep {
            locks: 12,
            messages: 0,
        };
        assert_eq!(
            s.summary().as_deref(),
            Some("housekeeping: removed 12 stale tower locks")
        );
    }
}
