// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! How many of Parallax Browser's handoffs ask for the person, as the resident
//! supervisor last read it: `parallax.json` in the state directory.
//!
//! Parallax is a separate local browser with agents of its own, and a handoff
//! is one of them stopping to ask for a human. The tower's job is "one place
//! that shows who is waiting on you", so those count, and a count is all it
//! holds: what a handoff says stays in Parallax. The supervisor asks Parallax's
//! local API every ten seconds or so and writes this file; every surface that
//! shows it (`status --line` in herdr's tab bar every five seconds, the table,
//! the TUI) reads the file and never the API, so a slow or absent Parallax
//! costs them nothing.
//!
//! The file is a snapshot, not a feed: it says when it was taken, and a reader
//! ignores one older than [`PARALLAX_FRESH`] rather than showing a handoff the
//! person may have answered a minute ago because the supervisor stopped.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::write_atomic;

/// The snapshot the supervisor writes.
pub const PARALLAX_FILE: &str = "parallax.json";
/// Where earlier versions kept the handoff IDs they had announced. Only ever
/// removed now.
const LEGACY_NOTIFIED_FILE: &str = "parallax-notified.json";

/// How old a snapshot may be and still be shown.
///
/// Six polls' worth: long enough that one slow answer does not blank the tab
/// bar, short enough that a supervisor that stopped does not leave a handoff
/// on screen for good.
pub const PARALLAX_FRESH: time::Duration = time::Duration::seconds(60);

/// Parallax's `GET /handoffs/attention`: open handoffs by how much they ask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallaxCounts {
    pub urgent: u64,
    pub action: u64,
    /// Finished work to look at rather than somebody waiting, counted apart
    /// the way the tower counts DONE apart from WAIT.
    pub review: u64,
}

impl ParallaxCounts {
    /// How many wait on the person. Saturating: these are another program's
    /// numbers, and a wrong one must not stop a pass.
    pub fn waiting(&self) -> u64 {
        self.urgent.saturating_add(self.action)
    }
}

/// The supervisor's latest reading of Parallax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallaxSnapshot {
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Whether the API answered. `false` carries zero counts: an unreachable
    /// Parallax says nothing about who is waiting, and showing the last number
    /// would claim it does.
    pub reachable: bool,
    #[serde(default)]
    pub counts: ParallaxCounts,
}

impl ParallaxSnapshot {
    /// Young enough to show; see [`PARALLAX_FRESH`]. A clock that went
    /// backwards leaves the snapshot fresh rather than hiding it.
    pub fn fresh(&self, now: OffsetDateTime) -> bool {
        now - self.updated_at <= PARALLAX_FRESH
    }
}

/// How many handoffs wait on the person in `snapshot`, or zero when it is
/// missing, stale or unreachable. The one rule every surface shows by.
pub fn parallax_waiting(snapshot: Option<&ParallaxSnapshot>, now: OffsetDateTime) -> u64 {
    match snapshot {
        Some(s) if s.reachable && s.fresh(now) => s.counts.waiting(),
        _ => 0,
    }
}

/// `parallax.json` in one state directory.
#[derive(Debug, Clone)]
pub struct ParallaxStore {
    dir: PathBuf,
}

impl ParallaxStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    /// The snapshot, or `None` when there is none. A file that does not parse
    /// is `None` too: every reader of this is a display, and a display with
    /// nothing to add is better than one that fails over a half-edited file.
    pub fn read(&self) -> Option<ParallaxSnapshot> {
        let body = fs::read(self.dir.join(PARALLAX_FILE)).ok()?;
        serde_json::from_slice(&body).ok()
    }

    /// Written whole through a temp file and a rename, owner-only.
    pub fn write(&self, snapshot: &ParallaxSnapshot) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.dir.join(PARALLAX_FILE), &body)
    }

    /// Removes the snapshot: `parallax` turned off. Already gone is fine.
    pub fn remove(&self) -> std::io::Result<()> {
        remove_if_there(&self.dir.join(PARALLAX_FILE))
    }

    /// Removes what earlier versions left beside the snapshot.
    pub fn remove_legacy(&self) -> std::io::Result<()> {
        remove_if_there(&self.dir.join(LEGACY_NOTIFIED_FILE))
    }
}

fn remove_if_there(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::seconds(secs)
    }

    fn snapshot(reachable: bool, urgent: u64, action: u64, review: u64) -> ParallaxSnapshot {
        ParallaxSnapshot {
            updated_at: at(0),
            reachable,
            counts: ParallaxCounts {
                urgent,
                action,
                review,
            },
        }
    }

    #[test]
    fn waiting_is_urgent_and_action_and_never_review() {
        let s = snapshot(true, 2, 3, 7);
        assert_eq!(parallax_waiting(Some(&s), at(1)), 5);
        assert_eq!(parallax_waiting(Some(&snapshot(true, 0, 0, 7)), at(1)), 0);
        let huge = snapshot(true, u64::MAX, 1, 0);
        assert_eq!(parallax_waiting(Some(&huge), at(1)), u64::MAX);
    }

    /// One second either side of the line, unreachable and missing.
    #[test]
    fn only_a_fresh_reachable_snapshot_counts() {
        let s = snapshot(true, 1, 0, 0);
        assert_eq!(parallax_waiting(Some(&s), at(60)), 1);
        assert_eq!(parallax_waiting(Some(&s), at(61)), 0);
        assert_eq!(parallax_waiting(Some(&s), at(-3600)), 1, "clock went back");
        assert_eq!(parallax_waiting(Some(&snapshot(false, 1, 0, 0)), at(1)), 0);
        assert_eq!(parallax_waiting(None, at(1)), 0);
    }

    #[test]
    fn a_snapshot_round_trips_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        assert_eq!(store.read(), None);
        store.remove().unwrap();
        let s = snapshot(true, 1, 2, 3);
        store.write(&s).unwrap();
        assert_eq!(store.read(), Some(s));
        store.remove().unwrap();
        assert_eq!(store.read(), None);
    }

    /// A file an earlier version wrote reads as no handoffs, and what a broken
    /// file holds is no reading at all.
    #[test]
    fn an_earlier_versions_file_reads_as_zero_and_garbage_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let old = r#"{"updated_at":"2026-10-01T00:00:00Z","reachable":true,"workspaces":2,
            "handoffs":[{"id":"h","title":"secret"}]}"#;
        std::fs::write(dir.path().join(PARALLAX_FILE), old).unwrap();
        assert_eq!(store.read().unwrap().counts, ParallaxCounts::default());
        std::fs::write(dir.path().join(PARALLAX_FILE), "{").unwrap();
        assert_eq!(store.read(), None);
    }

    #[test]
    fn the_legacy_file_is_removed_and_its_absence_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        store.remove_legacy().unwrap();
        let file = dir.path().join(LEGACY_NOTIFIED_FILE);
        std::fs::write(&file, "[]").unwrap();
        store.remove_legacy().unwrap();
        assert!(!file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_snapshot_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        ParallaxStore::new(dir.path())
            .write(&snapshot(true, 0, 0, 0))
            .unwrap();
        let mode = std::fs::metadata(dir.path().join(PARALLAX_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
