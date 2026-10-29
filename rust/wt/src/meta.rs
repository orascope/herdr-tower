// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The task record: what was created, where, and from what.
//!
//! One JSON file per task under `<state>/worktrees/`, written **before** the
//! worktree it describes. That order is the whole design: a crash between the
//! record and the directory leaves a record with nothing behind it, which
//! `wt list` shows; the opposite order leaves a directory nobody can name.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Error, Result};

/// One worktree task, as stored.
///
/// The field names, the omitted-when-empty rules and the RFC 3339 timestamps are
/// pinned, because every record already on disk has to keep loading. Paths are
/// `String` rather than `PathBuf`: they are JSON values that go to `git -C`
/// verbatim, and a path that is not valid UTF-8 cannot be written as a JSON
/// string in the first place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub task: String,
    /// Absolute path to the target repository root.
    pub repo: String,
    pub base: String,
    /// `agent/<task>`.
    pub branch: String,
    /// Absolute path to the worktree.
    pub path: String,
    /// `claude` | `codex` | `gemini`
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub test_cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_pass: Option<bool>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub test_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Where `wt drive` left this task, if it has been driven.
    ///
    /// Stored here rather than in a queue of its own: a merge-ready branch is
    /// a fact about the task, and a second store would be a second thing to
    /// keep in step with `wt clean`, with the record's own deletion, and with
    /// a task someone renames underneath it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<crate::drive::Drive>,
    /// The pane `wt new` was run from: the coordinator the tower draws this
    /// task's worker under. `None` for a task made outside a herdr pane, and
    /// for every record from before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Parent>,
}

/// A pane on a herdr server, as a task record names its coordinator.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    /// Empty for the default herdr socket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    pub pane_id: String,
}

impl Meta {
    /// The branch the worktree is on now — the recorded one when it cannot
    /// be read (detached, or the worktree is gone).
    ///
    /// What to *show*. `branch` stays what `wt new` cut, which is what
    /// `wt clean` deletes.
    pub fn live_branch(&self) -> String {
        crate::git::checked_out_branch(&self.path).unwrap_or_else(|| self.branch.clone())
    }
}

/// Worktree folder to the branch it is on now, for every task — the map the
/// `⎇` labels are drawn from.
///
/// An unreadable state directory is an empty map: a branch is a label, and
/// no screen should fail over one.
pub fn live_branches(state_dir: &Path) -> std::collections::HashMap<String, String> {
    list_metas(state_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| !m.path.is_empty())
        .map(|m| {
            let branch = m.live_branch();
            (m.path, branch)
        })
        .collect()
}

/// Where task records live.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("worktrees")
}

/// Encodes a task name so distinct names cannot share a filename.
///
/// The old rule replaced the separator with `_`, which made `feat/login` and
/// `feat_login` the same file. `load_meta` did not check the record it got back,
/// so `wt merge feat_login` operated on `feat/login` — and creating the second
/// task was refused as already present. Two valid names, one record, wrong task
/// merged.
///
/// Percent-encoding the three characters that matter, `%` first so the escape is
/// unambiguous. Everything else is left alone on purpose: a task called `2단계`
/// should still be findable by eye in the directory listing.
pub fn encode_task(task: &str) -> String {
    task.replace('%', "%25")
        .replace('/', "%2F")
        .replace('\\', "%5C")
}

/// The record file for a task.
pub(crate) fn record_path(state_dir: &Path, task: &str) -> PathBuf {
    dir(state_dir).join(format!("{}.json", encode_task(task)))
}

/// Writes a task record atomically.
///
/// The temp file carries the process ID rather than being a fixed
/// `<task>.json.tmp`: two processes saving the same task to one fixed name
/// interleave their writes into one temp file and the winning rename publishes
/// the mixture. The temp name is invisible to [`list_metas`], which only reads
/// `*.json`, so the on-disk contract is unaffected.
pub fn save_meta(state_dir: &Path, m: &Meta) -> Result<()> {
    let d = dir(state_dir);
    fs::create_dir_all(&d)?;
    let body = serde_json::to_vec_pretty(m)?;
    let final_path = record_path(state_dir, &m.task);
    let name = final_path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = d.join(format!("{name}.{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &final_path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Claims a task name and writes its record, or reports that the name is taken.
///
/// The whole call is under one lock, and that is what makes the claim mean
/// something. `create_new` alone is atomic about the *file*, not about the
/// record: it publishes an empty path first and fills it a moment later, and a
/// concurrent `wt new` looking in that moment finds a file it cannot parse,
/// concludes the record is corrupt, and takes it over. Both callers then believe
/// they reserved the task, both run `git worktree add`, and the loser's rollback
/// deletes the winner's record — the race this function exists to close, arriving
/// through the window the fix itself opened.
///
/// A record that exists and cannot be read is taken over rather than refused,
/// which preserves the property [`load_meta`] documents: a corrupt record must not
/// make a task name unusable and uncleanable. Under the lock, exactly one caller
/// does that and the next one finds a record it can read.
pub fn reserve_meta(state_dir: &Path, m: &Meta) -> Result<()> {
    let d = dir(state_dir);
    fs::create_dir_all(&d)?;
    // Held for the read *and* the write. `None` means the lock could not be taken
    // at all — an unwritable directory is already the larger problem, and refusing
    // to create a task over it would be a worse answer than the race.
    let _claim = claim_lock(&d);
    if load_meta(state_dir, &m.task).is_ok() {
        return Err(crate::Error::TaskExists(m.task.clone()));
    }
    // `save_meta` rather than a bare write: it publishes through a rename, so a
    // reader outside this lock — `wt list` walking the directory — never sees a
    // half-written record.
    save_meta(state_dir, m)
}

/// Serializes task reservation across processes.
///
/// One lock for the whole directory rather than one per task: `wt new` is a
/// person's command, not a loop, so two of them contending is already the rare
/// case — and a single file is one less thing to name, create and clean up.
///
/// `None` when the lock cannot be taken at all, which leaves the previous
/// behaviour rather than refusing to create a task because a lock file could not
/// be made.
pub(crate) fn claim_lock(dir: &Path) -> Option<fs::File> {
    let f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".reserve.lock"))
        .ok()?;
    f.lock().ok()?;
    Some(f)
}

/// Reads a task record.
///
/// Every failure — absent, unreadable, malformed — becomes the same
/// [`Error::NoMeta`]. That is deliberate for the message a person
/// sees, but note the consequence: [`crate::new_task`] treats "cannot load" as
/// "does not exist", so a corrupt record does not block re-creating the task and
/// gets overwritten. Preserved, because the alternative is a task name that
/// cannot be used and cannot be cleaned.
pub fn load_meta(state_dir: &Path, task: &str) -> Result<Meta> {
    let body = fs::read(record_path(state_dir, task)).map_err(|_| no_meta(task))?;
    let m: Meta = serde_json::from_slice(&body).map_err(|_| no_meta(task))?;
    // The record says which task it is, and it has to agree. The encoding above
    // makes a collision impossible, and this makes a *future* mistake in it loud
    // rather than silently merging the wrong branch.
    if m.task != task {
        return Err(no_meta(task));
    }
    Ok(m)
}

fn no_meta(task: &str) -> Error {
    Error::NoMeta(task.to_string())
}

/// Removes a task record.
pub fn delete_meta(state_dir: &Path, task: &str) -> Result<()> {
    fs::remove_file(record_path(state_dir, task))?;
    Ok(())
}

/// Every task record, oldest first.
///
/// A missing directory is an empty list, not a failure — that is what a machine
/// looks like before the first `wt new`. A file that cannot be read or parsed is
/// skipped for the same reason [`tower_state::Store::list`] skips one: a single
/// corrupt record must not blind the listing that is supposed to reveal debris.
///
/// Ties on `created_at` fall back to the task name, so records with identical
/// timestamps still list in a reproducible order.
pub fn list_metas(state_dir: &Path) -> Result<Vec<Meta>> {
    let entries = match fs::read_dir(dir(state_dir)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".json") {
            continue;
        }
        let Ok(body) = fs::read(entry.path()) else {
            continue;
        };
        let Ok(m) = serde_json::from_slice::<Meta>(&body) else {
            continue;
        };
        out.push(m);
    }
    out.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.task.cmp(&b.task))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000 + secs).unwrap()
    }

    fn meta(task: &str, created: OffsetDateTime) -> Meta {
        Meta {
            task: task.into(),
            repo: "/r".into(),
            base: "main".into(),
            branch: format!("agent/{task}"),
            path: "/w".into(),
            agent: "claude".into(),
            test_cmd: String::new(),
            test_pass: None,
            test_at: None,
            created_at: created,
            drive: None,
            parent: None,
        }
    }

    /// The coordinator `wt new` recorded survives the file; a record from
    /// before it was kept loads with none, and none is not written at all.
    #[test]
    fn the_parent_round_trips_and_an_older_record_has_none() {
        let mut m = meta("i-1", at(0));
        let plain = serde_json::to_string(&m).unwrap();
        assert!(!plain.contains("parent"), "{plain}");
        for parent in [
            Parent {
                backend: String::new(),
                pane_id: "w4F:pC".into(),
            },
            Parent {
                backend: "b0123456789abcdef".into(),
                pane_id: "w2:p한".into(),
            },
        ] {
            m.parent = Some(parent.clone());
            let back: Meta = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
            assert_eq!(back.parent, Some(parent));
        }
        let back: Meta = serde_json::from_str(&plain).unwrap();
        assert_eq!(back.parent, None);
        let default_backend = serde_json::to_string(&Parent {
            backend: String::new(),
            pane_id: "w1:p1".into(),
        })
        .unwrap();
        assert_eq!(default_backend, r#"{"pane_id":"w1:p1"}"#);
    }

    /// A worktree that is gone, or a path that was never a checkout, shows
    /// the recorded branch: the label falls back rather than disappearing.
    #[test]
    fn live_branch_falls_back_to_the_record_when_the_worktree_cannot_answer() {
        let mut m = meta("gone", at(0));
        m.path = "/nonexistent/herdr-tower/wt/gone".into();
        assert_eq!(m.live_branch(), "agent/gone");
        m.path = String::new();
        assert_eq!(m.live_branch(), "agent/gone");
    }

    /// `live_branches` has one entry per record with a path — none for a
    /// record without one, none for an empty or missing state directory —
    /// and each entry is that record's live branch.
    #[test]
    fn live_branches_has_one_entry_per_record_with_a_path() {
        let missing = tempfile::tempdir().unwrap();
        assert!(live_branches(&missing.path().join("nope")).is_empty());

        let d = tempfile::tempdir().unwrap();
        assert!(live_branches(d.path()).is_empty());

        let repo = crate::git::tests::fixture_repo();
        let repo_path = repo.path().to_str().unwrap().to_string();
        let holder = tempfile::tempdir().unwrap();
        let moved_path = holder.path().join("moved").to_str().unwrap().to_string();
        crate::git::add_worktree(&repo_path, "main", "agent/moved", &moved_path).unwrap();
        crate::git::tests::switch_new(&moved_path, "agent/explain-2969-stage2");

        let mut moved = meta("moved", at(0));
        moved.path = moved_path.clone();
        let mut gone = meta("gone", at(1));
        gone.path = "/nonexistent/herdr-tower/wt/gone".into();
        let mut pathless = meta("pathless", at(2));
        pathless.path = String::new();
        for m in [&moved, &gone, &pathless] {
            save_meta(d.path(), m).unwrap();
        }

        let got = live_branches(d.path());
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(
            got.get(&moved_path).map(String::as_str),
            Some("agent/explain-2969-stage2")
        );
        assert_eq!(
            got.get("/nonexistent/herdr-tower/wt/gone")
                .map(String::as_str),
            Some("agent/gone")
        );
        for (path, branch) in &got {
            let m = list_metas(d.path())
                .unwrap()
                .into_iter()
                .find(|m| &m.path == path)
                .unwrap();
            assert_eq!(
                branch,
                &m.live_branch(),
                "each entry is the record's live branch"
            );
        }
    }

    #[test]
    fn a_record_survives_a_round_trip_through_disk() {
        let d = tempfile::tempdir().unwrap();
        let mut m = meta("auth-api", at(0));
        m.test_cmd = "npm test".into();
        save_meta(d.path(), &m).unwrap();

        let back = load_meta(d.path(), "auth-api").unwrap();
        assert_eq!(back.branch, "agent/auth-api");
        assert_eq!(back.agent, "claude");
        assert_eq!(back.test_cmd, "npm test");
        assert_eq!(back.created_at, at(0));

        assert_eq!(list_metas(d.path()).unwrap().len(), 1);
        delete_meta(d.path(), "auth-api").unwrap();
        assert!(load_meta(d.path(), "auth-api").is_err());
    }

    #[test]
    fn a_state_directory_with_no_worktrees_lists_nothing() {
        let d = tempfile::tempdir().unwrap();
        assert!(list_metas(d.path()).unwrap().is_empty());
    }

    /// The on-disk shape is a contract with every record already written, so
    /// it is asserted as bytes and not as fields. The pinned details: key
    /// order, two-space indent, `test_*` absent until a test has run, and RFC
    /// 3339 timestamps carrying their offset.
    #[test]
    fn the_json_matches_the_recorded_bytes() {
        let d = tempfile::tempdir().unwrap();
        let created = OffsetDateTime::from_unix_timestamp(1_789_000_496)
            .unwrap()
            .to_offset(time::UtcOffset::from_hms(9, 0, 0).unwrap());
        let mut m = meta("auth-api", created);
        save_meta(d.path(), &m).unwrap();
        let raw = fs::read_to_string(record_path(d.path(), "auth-api")).unwrap();
        assert_eq!(
            raw,
            concat!(
                "{\n",
                "  \"task\": \"auth-api\",\n",
                "  \"repo\": \"/r\",\n",
                "  \"base\": \"main\",\n",
                "  \"branch\": \"agent/auth-api\",\n",
                "  \"path\": \"/w\",\n",
                "  \"agent\": \"claude\",\n",
                "  \"created_at\": \"2026-09-10T09:34:56+09:00\"\n",
                "}"
            )
        );

        m.test_cmd = "make test".into();
        m.test_pass = Some(false);
        m.test_at = Some(created);
        save_meta(d.path(), &m).unwrap();
        let raw = fs::read_to_string(record_path(d.path(), "auth-api")).unwrap();
        assert!(raw.contains("  \"test_cmd\": \"make test\",\n  \"test_pass\": false,\n  \"test_at\": \"2026-09-10T09:34:56+09:00\",\n  \"created_at\""),
            "test fields sit between agent and created_at:\n{raw}");
    }

    /// A fractional second is written with its trailing zeros trimmed, and the
    /// fraction is omitted entirely when it is zero. The `time` crate's RFC 3339
    /// does that, and this pins it — a record written as `.100000000` by one
    /// build and `.1` by another still parses, but stops being byte-comparable
    /// with the recorded shape.
    #[test]
    fn fractional_seconds_are_written_as_recorded() {
        let base = OffsetDateTime::from_unix_timestamp(1_789_000_496).unwrap();
        for (nanos, want) in [
            (0, "2026-09-10T00:34:56Z"),
            (1, "2026-09-10T00:34:56.000000001Z"),
            (100_000_000, "2026-09-10T00:34:56.1Z"),
            (120_000_000, "2026-09-10T00:34:56.12Z"),
            (123_456_789, "2026-09-10T00:34:56.123456789Z"),
        ] {
            let t = base + time::Duration::nanoseconds(nanos);
            let json = serde_json::to_string(&Wrapper { t }).unwrap();
            assert_eq!(json, format!("{{\"t\":\"{want}\"}}"), "nanos={nanos}");
        }
    }

    #[derive(Serialize)]
    struct Wrapper {
        #[serde(with = "time::serde::rfc3339")]
        t: OffsetDateTime,
    }

    /// A task named after a branch must not become a subdirectory, or the record
    /// lands somewhere that does not exist.
    ///
    /// Encoded, not substituted. Replacing the separator with `_` made
    /// `feat/login` and `feat_login` the same file — and since `load_meta` did not
    /// check the record it got back, `wt merge feat_login` operated on
    /// `feat/login` while creating the second task was refused as already present.
    #[test]
    fn a_slash_in_a_task_name_is_encoded_in_the_filename() {
        let d = tempfile::tempdir().unwrap();
        let m = meta("feat/login form", at(0));
        save_meta(d.path(), &m).unwrap();
        assert!(dir(d.path()).join("feat%2Flogin form.json").is_file());
        assert_eq!(load_meta(d.path(), "feat/login form").unwrap().task, m.task);
    }

    /// Two names that used to share a file now have one each.
    #[test]
    fn names_that_used_to_collide_keep_their_own_records() {
        let d = tempfile::tempdir().unwrap();
        save_meta(d.path(), &meta("feat/login", at(0))).unwrap();
        save_meta(d.path(), &meta("feat_login", at(1))).unwrap();

        assert_eq!(
            load_meta(d.path(), "feat/login").unwrap().task,
            "feat/login"
        );
        assert_eq!(
            load_meta(d.path(), "feat_login").unwrap().task,
            "feat_login"
        );
        assert_eq!(list_metas(d.path()).unwrap().len(), 2);

        // `%` is escaped first, so a name containing the escape cannot forge
        // another name's file.
        save_meta(d.path(), &meta("feat%2Flogin", at(2))).unwrap();
        assert_eq!(
            load_meta(d.path(), "feat/login").unwrap().task,
            "feat/login",
            "a literal %2F in a task name must not claim the encoded file"
        );
        assert_eq!(list_metas(d.path()).unwrap().len(), 3);
    }

    /// Non-ASCII stays readable: the directory listing is something people look
    /// at, and only the three characters that break a filename are encoded.
    #[test]
    fn a_korean_task_name_is_left_alone() {
        assert_eq!(encode_task("2단계"), "2단계");
        assert_eq!(encode_task("plain-task_1.2"), "plain-task_1.2");
    }

    #[test]
    fn records_list_oldest_first() {
        let d = tempfile::tempdir().unwrap();
        for (task, secs) in [("third", 30), ("first", 10), ("second", 20)] {
            save_meta(d.path(), &meta(task, at(secs))).unwrap();
        }
        let tasks: Vec<String> = list_metas(d.path())
            .unwrap()
            .into_iter()
            .map(|m| m.task)
            .collect();
        assert_eq!(tasks, ["first", "second", "third"]);
    }

    #[test]
    fn one_corrupt_record_does_not_hide_the_others() {
        let d = tempfile::tempdir().unwrap();
        save_meta(d.path(), &meta("good", at(0))).unwrap();
        fs::write(dir(d.path()).join("broken.json"), b"{ not json").unwrap();
        fs::write(dir(d.path()).join("notjson.txt"), b"ignored").unwrap();
        let got = list_metas(d.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].task, "good");
    }

    /// An unparseable record reads as absent, which is what lets `wt new` reuse
    /// the name instead of leaving it permanently unusable.
    #[test]
    fn a_malformed_record_is_indistinguishable_from_a_missing_one() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir(d.path())).unwrap();
        fs::write(dir(d.path()).join("t.json"), b"{ not json").unwrap();
        assert!(matches!(load_meta(d.path(), "t"), Err(Error::NoMeta(_))));
        assert!(matches!(
            load_meta(d.path(), "absent"),
            Err(Error::NoMeta(_))
        ));
    }

    /// Every drive state survives disk, field for field — a verdict read back
    /// wrong is a verdict about a different drive.
    #[test]
    fn a_drive_verdict_survives_a_round_trip_through_disk() {
        use crate::drive::{Drive, DriveState};
        let states = [
            DriveState::Running,
            DriveState::MergeReady,
            DriveState::Stalled,
            DriveState::Exhausted,
            DriveState::Unfinished,
            DriveState::Uncommitted,
        ];
        for state in states {
            let d = tempfile::tempdir().unwrap();
            let mut m = meta("auth-api", at(0));
            let verdict = Drive {
                state,
                attempts: 3,
                failure: "FAIL: 한글 \"quoted\"\nsecond line".into(),
                head: "0123456789abcdef0123456789abcdef01234567".into(),
                at: at(7),
            };
            m.drive = Some(verdict.clone());
            save_meta(d.path(), &m).unwrap();

            let back = load_meta(d.path(), "auth-api")
                .unwrap()
                .drive
                .expect("kept");
            assert_eq!(back.state, verdict.state, "{state:?}");
            assert_eq!(back.attempts, verdict.attempts);
            assert_eq!(back.failure, verdict.failure);
            assert_eq!(back.head, verdict.head);
            assert_eq!(back.at, verdict.at);
        }
    }

    /// A task that was never driven has no `drive` key, and still loads.
    #[test]
    fn a_record_from_before_drives_loads_with_none() {
        let d = tempfile::tempdir().unwrap();
        let m = meta("auth-api", at(0));
        save_meta(d.path(), &m).unwrap();
        let path = record_path(d.path(), "auth-api");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            !body.contains("\"drive\""),
            "an absent verdict is not written — the file keeps its pre-drive shape"
        );
        assert!(load_meta(d.path(), "auth-api").unwrap().drive.is_none());
    }
}
