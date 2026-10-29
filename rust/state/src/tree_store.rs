// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Who a pane works under, as somebody said it: `tree.json` in the state
//! directory, written by `herdr-tower tree link`.
//!
//! The tower's session tree is otherwise worked out from what the tower
//! already knows — a board item with its worker, a `wt` task with the pane
//! that made it. A link here is for the rest: a pane put under a coordinator
//! without any work handed over. It wins over both.
//!
//! A missing or unreadable file is no links: the tree falls back to what it
//! can work out, never an error line over a tree.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::write_atomic;

/// The file, in the state directory.
pub const TREE_FILE: &str = "tree.json";

/// One pane under another, on one herdr server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeLink {
    /// Empty for the default herdr socket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    pub worker: String,
    pub coordinator: String,
    /// When the link was made. herdr reuses pane ids, so a link holds only
    /// for agents that were already there: one born later in either pane is
    /// somebody else.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TreeFile {
    #[serde(default)]
    links: Vec<TreeLink>,
}

pub struct TreeStore {
    dir: PathBuf,
}

impl TreeStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    /// The links; a file that is missing or does not parse is none.
    pub fn read(&self) -> Vec<TreeLink> {
        fs::read(self.dir.join(TREE_FILE))
            .ok()
            .and_then(|body| serde_json::from_slice::<TreeFile>(&body).ok())
            .map(|f| f.links)
            .unwrap_or_default()
    }

    /// Puts `link.worker` under `link.coordinator`, replacing whatever it was
    /// under before.
    pub fn link(&self, link: TreeLink) -> std::io::Result<()> {
        let mut links = self.read();
        links.retain(|l| !(l.backend == link.backend && l.worker == link.worker));
        links.push(link);
        self.write(links)
    }

    /// Takes `worker`'s link away. Whether there was one.
    pub fn unlink(&self, backend: &str, worker: &str) -> std::io::Result<bool> {
        let mut links = self.read();
        let before = links.len();
        links.retain(|l| !(l.backend == backend && l.worker == worker));
        if links.len() == before {
            return Ok(false);
        }
        self.write(links)?;
        Ok(true)
    }

    fn write(&self, links: Vec<TreeLink>) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(&TreeFile { links })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.dir.join(TREE_FILE), &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(worker: &str, coordinator: &str) -> TreeLink {
        TreeLink {
            backend: String::new(),
            worker: worker.into(),
            coordinator: coordinator.into(),
            at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_worker_has_one_link_and_unlink_says_whether_it_had_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = TreeStore::new(dir.path());
        assert!(store.read().is_empty(), "nothing written yet");
        store.link(link("w1:p2", "w1:p1")).unwrap();
        store.link(link("w1:p3", "w1:p1")).unwrap();
        store.link(link("w1:p2", "w1:p9")).unwrap();
        let links = store.read();
        assert_eq!(links.len(), 2);
        assert!(links.contains(&link("w1:p2", "w1:p9")), "replaced");
        assert!(store.unlink("", "w1:p2").unwrap());
        assert!(!store.unlink("", "w1:p2").unwrap());
        assert_eq!(store.read(), vec![link("w1:p3", "w1:p1")]);
    }

    #[test]
    fn a_broken_file_is_no_links() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(TREE_FILE), b"{not json").unwrap();
        assert!(TreeStore::new(dir.path()).read().is_empty());
    }
}
