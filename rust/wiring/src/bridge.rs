// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The codex/gemini Discord bridge: a directory, not a registration.
//!
//! A bridge has no entry in `bots.json`. It claims an agent by pointing at that
//! agent's folder from a `*WORKDIR=` line in one of its `.env` files, so the
//! only way to find it is to read the candidate roots and look.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::{read_text, sorted_dir_names};

/// A codex/gemini Discord bridge wired to this folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bridge {
    /// The bridge root. Also serves as a needle when matching launch units —
    /// the unit that starts a bridge names the bridge, not the agent's folder.
    pub dir: PathBuf,
    /// Whether `daemon.pid` names a process that is still there.
    pub alive: bool,
    /// Channel IDs gathered from every `*CHANNEL*` key in the `.env`.
    pub channels: Vec<String>,
}

/// Finds the bridge whose `.env` points at `folder`, if there is one.
///
/// Scanning does not stop at the first hit, so with two profiles pointing at
/// one folder the last one in sort order wins. Kept as it was: a folder served
/// by two bridges is already misconfigured, and quietly preferring the first
/// would be no more correct than preferring the last — only different.
pub(crate) fn detect(roots: &[PathBuf], folder: &str) -> Option<Bridge> {
    let mut found = None;
    for root in roots {
        for name in sorted_dir_names(root) {
            if name != ".env" && !name.starts_with(".env.") {
                continue;
            }
            let Some(env) = read_text(&root.join(&name)) else {
                continue;
            };
            if !points_to(&env, folder) {
                continue;
            }
            found = Some(Bridge {
                dir: root.clone(),
                alive: pid_alive(&root.join(data_dir(&name)).join("daemon.pid")),
                channels: channels(&env),
            });
        }
    }
    found
}

/// Where a profile keeps its state: `.env` → `data`, `.env.staging` →
/// `data-staging`.
///
/// The pid we probe lives in there, so getting this wrong would report a
/// stopped profile as alive on the strength of a sibling profile's daemon.
fn data_dir(env_name: &str) -> String {
    match env_name.strip_prefix(".env").unwrap_or("") {
        "" => "data".to_string(),
        suffix => format!("data{}", suffix.replacen('.', "-", 1)),
    }
}

/// Whether a `*WORKDIR=` line in this `.env` names exactly this folder.
///
/// The key is matched by suffix (`CODEX_WORKDIR`, `GEMINI_WORKDIR`, …) because
/// each bridge spells its own prefix, and the value must be equal, not a
/// prefix — a bridge serving `/a/b` does not serve `/a/b/child`.
fn points_to(env: &str, folder: &str) -> bool {
    env.lines().any(|line| match assignment(line) {
        Some((key, value)) => key.ends_with("WORKDIR") && value == folder,
        None => false,
    })
}

/// One active `KEY=value` line, or `None` for a comment or anything else.
///
/// Searching the raw line for `WORKDIR=` matched a *retired* entry —
/// `# CODEX_WORKDIR=/project` — and made the profile claim the folder anyway. The
/// bridge then read as present and `drives_session` marked every session there as
/// the Discord-driven bot, using a daemon and channels the user had commented out.
///
/// The key has to *end* with the suffix rather than merely contain it, so
/// `OLD_WORKDIR_BACKUP` does not claim the folder. `OLD_WORKDIR` still does, and
/// has to: the prefix belongs to the bridge — `CODEX_`, `GEMINI_`, whatever the
/// next one calls itself — and no rule separates a prefix someone retired from one
/// this code has not met. Retiring a line means commenting it out, and that is
/// what is now respected.
fn assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    // `export KEY=value` is an assignment; the word is not part of the key.
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((key, value.trim()))
}

/// The channel IDs a bridge `.env` mentions.
///
/// Any key containing `CHANNEL` counts, since bridges name theirs differently,
/// and every comma-separated value has to parse as a number — that filter is
/// what keeps a placeholder like `CHANNEL_ID=your-id-here` out of the card.
/// Sorted and de-duplicated so two keys naming one channel show it once.
///
/// Through [`assignment`], like [`points_to`]: a raw `split_once` read
/// `# DISCORD_CHANNEL_ID=123` as an active key, so a channel the user had
/// deliberately commented out went on being listed in `status` and on the card.
fn channels(env: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for line in env.lines() {
        let Some((key, val)) = assignment(line) else {
            continue;
        };
        if !key.contains("CHANNEL") {
            continue;
        }
        for tok in val.split(',') {
            let tok = tok.trim();
            if !tok.is_empty() && tok.parse::<u64>().is_ok() {
                out.insert(tok.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Whether the pid file names a live process.
///
/// A pid file left behind by a crash is the normal case, which is why the card
/// says "daemon dead" rather than trusting the file's existence.
fn pid_alive(pid_file: &Path) -> bool {
    let Some(raw) = read_text(pid_file) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return false;
    };
    // 0 and negatives address a process *group* in kill(2), so a truncated or
    // garbage pid file would probe every process in the group and report a dead
    // daemon as alive. Calling kill(2) directly carries that hazard, and this
    // check pays for it.
    if pid <= 0 {
        return false;
    }
    process_exists(pid)
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 inspects the process table and touches no
    // memory of ours; every outcome is in the return value.
    //
    // A process owned by another user answers EPERM and reads here as dead.
    // That is the safer verdict anyway: a bridge daemon runs as the user who
    // wrote the pid file, so EPERM means the pid was recycled by someone else.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Windows has no signal 0; the process table answers instead.
#[cfg(not(unix))]
fn process_exists(pid: i32) -> bool {
    tower_os::pid_alive(pid as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bridge serves one folder exactly. Prefix matching here would attach
    /// every sub-folder agent to a bridge that never heard of it.
    #[test]
    fn workdir_must_match_the_whole_folder() {
        let env = "TOKEN=x\nCODEX_WORKDIR=/Users/x/work\n";
        assert!(points_to(env, "/Users/x/work"));
        assert!(!points_to(env, "/Users/x/work/child"));
        assert!(!points_to(env, "/Users/x/wor"));
        // Trailing whitespace and CRLF come from hand-edited .env files.
        assert!(points_to(
            "GEMINI_WORKDIR=/Users/x/work  \r\n",
            "/Users/x/work"
        ));
    }

    /// The numeric filter is the only thing separating a real channel from a
    /// placeholder, and a bridge names its keys however it likes.
    #[test]
    fn channels_take_numeric_values_from_any_channel_key() {
        let env = "DISCORD_CHANNEL_ID=1533823223442182294\n\
                   EXTRA_CHANNELS=1529498215651741706, 1533823223442182294\n\
                   CHANNEL_HINT=your-channel-here\n\
                   TOKEN=99\n";
        assert_eq!(
            channels(env),
            vec![
                "1529498215651741706".to_string(),
                "1533823223442182294".to_string()
            ],
            "de-duplicated, sorted, and no placeholder or non-CHANNEL key"
        );
    }

    /// A second profile has its own state directory; sharing `data` would make
    /// one daemon vouch for the other.
    #[test]
    fn each_env_profile_has_its_own_state_dir() {
        assert_eq!(data_dir(".env"), "data");
        assert_eq!(data_dir(".env.staging"), "data-staging");
        assert_eq!(data_dir(".env.a.b"), "data-a.b");
    }

    /// A garbage pid file must not be able to address a process group.
    #[test]
    fn non_positive_pids_are_never_alive() {
        let dir = tempfile::tempdir().unwrap();
        for junk in ["0", "-1", "", "not-a-pid"] {
            let f = dir.path().join("daemon.pid");
            std::fs::write(&f, junk).unwrap();
            assert!(!pid_alive(&f), "{junk:?} must not read as a live daemon");
        }
        assert!(!pid_alive(&dir.path().join("missing.pid")));
        // Our own pid is the one process we can prove is there.
        let f = dir.path().join("self.pid");
        std::fs::write(&f, format!("{}\n", std::process::id())).unwrap();
        assert!(pid_alive(&f));
    }

    /// The bytes on disk, parents and all — the fixture shape
    /// `tests/wiring.rs` already uses for these trees.
    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A pid that certainly was a process and certainly is not one now: a child
    /// of ours, reaped. `/bin/sh` rather than `/bin/true`, which macOS keeps in
    /// `/usr/bin`.
    #[cfg(unix)]
    fn reaped_child_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("/bin/sh exists on every Unix this ships to");
        let pid = child.id();
        child.wait().expect("reaped, so the pid is free again");
        pid
    }

    /// A pid that *was* a process is the case the check exists for, and the
    /// only dead pid we can prove: a reaped child, so the number is a real
    /// former pid rather than an impossible one like 999999999. A probe that
    /// asked the kernel nothing — or asked it about a process group — would
    /// report this crashed daemon as running.
    ///
    /// Nothing is signalled that we did not start, and signal 0 delivers
    /// nothing in any case.
    #[cfg(unix)]
    #[test]
    fn a_reaped_childs_pid_is_not_alive() {
        let dir = tempfile::tempdir().unwrap();
        let pid = reaped_child_pid();
        let f = dir.path().join("daemon.pid");
        write(&f, &format!("{pid}\n"));
        assert!(
            !pid_alive(&f),
            "pid {pid} was reaped, so its pid file is a crash leftover"
        );
    }

    /// Round trip through the file a daemon actually writes: whatever padding
    /// it used — a trailing newline, CRLF from a hand-edited file, spaces, a
    /// signed number — the pid has to come back as the same process. A running
    /// bridge reported dead is the same card error as a dead one reported
    /// running, only quieter.
    #[cfg(unix)]
    #[test]
    fn a_pid_reads_back_through_whatever_padding_it_was_written_with() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("daemon.pid");
        let live = std::process::id();
        // A former pid of our own, for the same paddings on the dead side.
        let dead = reaped_child_pid();
        for pad in [
            "{}", "{}\n", "{}\r\n", "  {}  ", "\t{}\n", "+{}\n", "{}\n\n",
        ] {
            write(&f, &pad.replace("{}", &live.to_string()));
            assert!(pid_alive(&f), "{pad:?} names this very process");
            write(&f, &pad.replace("{}", &dead.to_string()));
            assert!(!pid_alive(&f), "{pad:?} names a reaped child");
        }
    }

    /// A profile's daemon lives in that profile's own state directory, and the
    /// pid is read from there: `.env.staging` probes `data-staging/daemon.pid`
    /// and never the `data/daemon.pid` beside it. Getting this wrong reports a
    /// stopped staging bridge as running on the strength of the default
    /// profile's daemon — with the default profile's channels on the card.
    #[test]
    fn a_profile_reads_its_pid_from_its_own_state_dir() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("codex-discord");
        let folder = "/Users/x/codex-workspace";
        write(
            &dir.join(".env.staging"),
            &format!("CODEX_WORKDIR={folder}\nSTAGE_CHANNEL_ID=7\n"),
        );
        // The one live daemon belongs to the *default* profile, which nothing
        // here points at.
        write(
            &dir.join("data").join("daemon.pid"),
            &format!("{}\n", std::process::id()),
        );
        let roots = vec![dir.clone()];
        let bridge = detect(&roots, folder).expect("the staging .env points here");
        assert_eq!(bridge.dir, dir);
        assert_eq!(bridge.channels, vec!["7".to_string()]);
        assert!(
            !bridge.alive,
            "data/daemon.pid belongs to .env, not to .env.staging"
        );

        // The staging profile's own pid file, and only now is it alive.
        write(
            &dir.join("data-staging").join("daemon.pid"),
            &format!("{}\n", std::process::id()),
        );
        assert!(detect(&roots, folder).unwrap().alive);

        // A root that is not there, and no roots at all, are ordinary answers.
        assert!(detect(&[root.path().join("nope")], folder).is_none());
        assert!(detect(&[], folder).is_none());
    }

    /// Two profiles pointing at one folder is already a misconfiguration, and
    /// `detect` commits to a rule for it rather than stopping at the first hit:
    /// the last matching name in sort order wins, and a later root wins over an
    /// earlier one. `fs::read_dir` promises no order at all, so a lost sort
    /// would surface as a bridge reporting another profile's channels and
    /// another profile's daemon — intermittently, per machine.
    ///
    /// Property, over generated profile sets: the winner is read independently
    /// as the greatest of the names written, by plain string comparison.
    #[test]
    fn the_last_matching_profile_in_sort_order_is_the_one_reported() {
        let folder = "/Users/x/work";
        let mut seed: u64 = 20_260_922;
        let mut next = |bound: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % bound
        };
        for pass in 0..40 {
            let root = tempfile::tempdir().unwrap();
            let count = 2 + next(4);
            let mut names: Vec<String> = Vec::new();
            while names.len() < count {
                let len = 1 + next(3);
                let suffix: String = (0..len)
                    .map(|_| "abcxyz029".as_bytes()[next(9)] as char)
                    .collect();
                let name = if next(5) == 0 {
                    ".env".to_string()
                } else {
                    format!(".env.{suffix}")
                };
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            for (i, name) in names.iter().enumerate() {
                write(
                    &root.path().join(name),
                    &format!("CODEX_WORKDIR={folder}\nCHANNEL_ID={}\n", 1000 + i),
                );
            }
            // Decoys, all of them sorting last: a profile pointing elsewhere,
            // and two files that are not profiles at all.
            write(
                &root.path().join(".env.zzz-elsewhere"),
                &format!("CODEX_WORKDIR={folder}-other\nCHANNEL_ID=999\n"),
            );
            write(
                &root.path().join(".envrc"),
                &format!("CODEX_WORKDIR={folder}\nCHANNEL_ID=998\n"),
            );
            write(
                &root.path().join("zzz-notes.txt"),
                &format!("CODEX_WORKDIR={folder}\nCHANNEL_ID=997\n"),
            );

            let mut by_name = names.clone();
            by_name.sort();
            let winner = by_name.last().unwrap();
            let want = 1000 + names.iter().position(|n| n == winner).unwrap();

            let bridge = detect(&[root.path().to_path_buf()], folder)
                .expect("these profiles all point here");
            assert_eq!(bridge.dir, root.path());
            assert_eq!(
                bridge.channels,
                vec![want.to_string()],
                "pass {pass}: of {names:?} the winner is {winner:?}"
            );
        }

        // Two roots, same rule: the later one is the one reported.
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(
            &a.path().join(".env"),
            &format!("CODEX_WORKDIR={folder}\nCHANNEL_ID=11\n"),
        );
        write(
            &b.path().join(".env"),
            &format!("CODEX_WORKDIR={folder}\nCHANNEL_ID=22\n"),
        );
        let roots = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        let found = detect(&roots, folder).expect("both roots point here");
        assert_eq!(found.dir, b.path());
        assert_eq!(found.channels, vec!["22".to_string()]);
    }
}

#[cfg(test)]
mod assignment_tests {
    use super::*;

    /// A commented channel is not a channel. The same raw `split_once` that let a
    /// retired `WORKDIR` claim a folder let a retired channel ID onto the card.
    #[test]
    fn only_active_assignments_contribute_channels() {
        let env = "DISCORD_CHANNEL_ID=111\n\
                   # DISCORD_CHANNEL_ID=222\n\
                   #ALERT_CHANNEL=333\n\
                   export NOTIFY_CHANNEL_IDS=444,555\n\
                   CHANNEL_ID=your-id-here\n";
        assert_eq!(channels(env), vec!["111", "444", "555"]);
    }

    /// A retired entry must not claim the folder.
    ///
    /// Searching the raw line for `WORKDIR=` matched `# CODEX_WORKDIR=/project`,
    /// so the bridge read as present and `drives_session` marked every session in
    /// that folder as the Discord-driven bot — using a daemon and channels the user
    /// had deliberately commented out.
    #[test]
    fn only_an_active_assignment_claims_a_folder() {
        assert!(points_to("CODEX_WORKDIR=/project\n", "/project"));
        assert!(points_to("export CLAUDE_WORKDIR=/project\n", "/project"));
        assert!(points_to("  WORKDIR = /project  \n", "/project"));

        for dead in [
            "# CODEX_WORKDIR=/project\n",
            "  #CODEX_WORKDIR=/project\n",
            "# export CODEX_WORKDIR=/project\n",
        ] {
            assert!(!points_to(dead, "/project"), "{dead:?} is commented out");
        }
        // The key has to *end* with the suffix, not merely contain it.
        assert!(
            !points_to("OLD_WORKDIR_BACKUP=/project\n", "/project"),
            "a different key that happens to contain the word"
        );
        // And a line that is not an assignment at all.
        assert!(!points_to("see WORKDIR=/project in the docs\n", "/project"));
    }
}
