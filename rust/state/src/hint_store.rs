// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What the tower's `T` and `/` fields offer as a dim hint when they open:
//! `tui-hints.json` in the state directory.
//!
//! The hint is the last value the person used, fish-shell style — one `→`
//! takes it back. `L` needs no file of its own for this: its last message is
//! already remembered per conversation by [`crate::LoopStore`]. The other two
//! have nothing that remembers them, and a tower's memory is not enough: the
//! dock tower is restarted on every upgrade, which would forget them several
//! times a week.
//!
//! Everything here is a convenience. A missing, unreadable or half-edited file
//! reads as "nothing remembered", and the field falls back to its example —
//! never an error line over a hint.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::write_atomic;

/// The file, in the state directory.
pub const HINTS_FILE: &str = "tui-hints.json";

/// The last values, as typed. Empty means none yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldHints {
    /// The last line `T` armed a schedule from: the time and the message,
    /// together, as the field takes them.
    #[serde(default)]
    pub schedule: String,
    /// The last non-empty filter `/` was used with.
    #[serde(default)]
    pub filter: String,
}

pub struct HintStore {
    dir: PathBuf,
}

impl HintStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    /// The remembered values; a file that is missing or does not parse is
    /// none remembered.
    pub fn read(&self) -> FieldHints {
        fs::read(self.dir.join(HINTS_FILE))
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok())
            .unwrap_or_default()
    }

    /// Written whole through a temp file and a rename, owner-only: a schedule
    /// line is a prompt meant for one of the person's agents, as private as a
    /// record.
    pub fn write(&self, hints: &FieldHints) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(hints)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.dir.join(HINTS_FILE), &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    #[test]
    fn hints_round_trip_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = HintStore::new(dir.path());
        assert_eq!(store.read(), FieldHints::default(), "nothing written yet");
        let hints = FieldHints {
            schedule: "+30m 빌드 확인".into(),
            filter: "codex".into(),
        };
        store.write(&hints).unwrap();
        assert_eq!(store.read(), hints);
        let mode = fs::metadata(dir.path().join(HINTS_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// A broken file, or one from a build that knew only one of the fields,
    /// is read as far as it goes rather than failing.
    #[test]
    fn a_broken_or_partial_file_reads_as_far_as_it_goes() {
        let dir = tempfile::tempdir().unwrap();
        let store = HintStore::new(dir.path());
        fs::write(dir.path().join(HINTS_FILE), b"{not json").unwrap();
        assert_eq!(store.read(), FieldHints::default());
        fs::write(dir.path().join(HINTS_FILE), br#"{"filter":"z-ai"}"#).unwrap();
        assert_eq!(
            store.read(),
            FieldHints {
                schedule: String::new(),
                filter: "z-ai".into(),
            }
        );
    }
}
