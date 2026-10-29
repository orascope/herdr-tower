// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Atomic heartbeat shared by the resident supervisor and short-lived clients.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::write_atomic;

const FILE: &str = "daemon.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub pid: u32,
    pub version: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tab_roots: Vec<String>,
    /// Backend-qualified workspace IDs in each server's sidebar order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_order: Vec<String>,
    /// Exact sidebar labels keyed by backend-qualified workspace ID.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub workspace_display_labels: std::collections::BTreeMap<String, String>,
    /// Worker pane to the coordinator pane it works under, both as
    /// `tower_mux::target_key`s: what the tower draws its session tree from.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub parents: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<DaemonBackend>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonBackend {
    pub id: String,
    pub socket: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// The pass that wrote this snapshot refreshed this backend's agent
    /// records from herdr. Answering a ping is not the same thing: the
    /// listing that refreshed the records is the one that has to have
    /// succeeded. Readers that only draw the records — the board pane —
    /// take this instead of asking herdr themselves.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refreshed: bool,
}

/// How far behind a live supervisor may fall before it is taken for hung.
///
/// A heartbeat is written once a pass, and a pass is a row of herdr calls
/// with a deadline each: on a loaded machine one pass can outlast
/// [`DaemonSnapshot::fresh`] without anything being wrong.
const BEHIND_GRACE: time::Duration = time::Duration::minutes(5);

impl DaemonSnapshot {
    pub fn fresh(&self, now: OffsetDateTime) -> bool {
        now - self.updated_at < time::Duration::seconds(30)
    }

    /// Whether the supervisor that wrote this is to be left running: fresh,
    /// or still `alive` and behind by less than [`BEHIND_GRACE`].
    ///
    /// Stale is not dead. Restarting a supervisor that is only slow cuts off
    /// the sends of the pass it is in and forgets which agents it just typed
    /// into — on exactly the machine that is already struggling, and again on
    /// every tower that opens while it is. One that has said nothing for
    /// longer is hung, and restarting it is the cure.
    pub fn running(&self, now: OffsetDateTime, alive: impl FnOnce(u32) -> bool) -> bool {
        self.fresh(now) || (now - self.updated_at < BEHIND_GRACE && alive(self.pid))
    }

    /// Whether `backend`'s records were refreshed by the supervisor's latest
    /// pass, and that pass is recent: what "observed just now" means to a
    /// reader that leaves herdr to the supervisor.
    pub fn refreshed(&self, backend: &str, now: OffsetDateTime) -> bool {
        self.fresh(now)
            && self
                .backends
                .iter()
                .any(|b| b.id == backend && b.connected && b.refreshed)
    }
}

#[derive(Debug, Clone)]
pub struct DaemonStore {
    path: PathBuf,
}

impl DaemonStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(FILE),
        }
    }

    pub fn read(&self) -> std::io::Result<Option<DaemonSnapshot>> {
        match fs::read(&self.path) {
            Ok(body) => serde_json::from_slice(&body)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn write(&self, snapshot: &DaemonSnapshot) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.path, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with(backends: Vec<DaemonBackend>, at: OffsetDateTime) -> DaemonSnapshot {
        DaemonSnapshot {
            pid: 1,
            version: "v".into(),
            started_at: at,
            updated_at: at,
            connected: true,
            error: String::new(),
            tab_roots: Vec::new(),
            workspace_order: Vec::new(),
            workspace_display_labels: Default::default(),
            parents: Default::default(),
            backends,
        }
    }

    fn named(id: &str, connected: bool, refreshed: bool) -> DaemonBackend {
        DaemonBackend {
            id: id.into(),
            socket: format!("/{id}/herdr.sock"),
            connected,
            error: String::new(),
            refreshed,
        }
    }

    /// `refreshed` holds only for that backend, connected, refreshed by the
    /// pass, in a snapshot still fresh — each condition flipped alone fails.
    #[test]
    fn refreshed_needs_every_condition() {
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let ok = snapshot_with(vec![named("", true, true), named("lab", true, false)], at);
        assert!(ok.refreshed("", at));
        assert!(
            !ok.refreshed("lab", at),
            "a backend the pass did not refresh"
        );
        assert!(
            !ok.refreshed("other", at),
            "a backend the snapshot does not list"
        );
        assert!(
            !ok.refreshed("", at + time::Duration::seconds(30)),
            "a snapshot past its freshness"
        );
        let down = snapshot_with(vec![named("", false, true)], at);
        assert!(!down.refreshed("", at), "a disconnected backend");
        assert!(!snapshot_with(Vec::new(), at).refreshed("", at));
    }

    /// A fresh heartbeat is running whatever the process table says; a stale
    /// one is running only while its process lives and the grace lasts.
    #[test]
    fn running_leaves_a_slow_supervisor_alone() {
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snap = snapshot_with(Vec::new(), at);
        let dead = |_| false;
        let alive = |pid| pid == 1;
        assert!(snap.running(at, dead), "fresh");
        let behind = at + time::Duration::seconds(45);
        assert!(snap.running(behind, alive), "past fresh, but alive");
        assert!(!snap.running(behind, dead), "past fresh, and gone");
        assert!(
            snap.running(at + BEHIND_GRACE - time::Duration::seconds(1), alive),
            "just inside the grace"
        );
        assert!(
            !snap.running(at + BEHIND_GRACE, alive),
            "silent past the grace is hung"
        );
        let mut other = snap.clone();
        other.pid = 2;
        assert!(
            !other.running(behind, alive),
            "the pid it is asked about is the snapshot's"
        );
    }

    /// Over a sweep of times: with the process gone `running` is exactly
    /// `fresh`; with it alive, `running` holds from the write on and, once it
    /// stops holding, never holds again as time goes on — a supervisor is not
    /// taken back for running by waiting longer. A heartbeat from the future
    /// (a clock that went back) is fresh, and so running.
    #[test]
    fn running_is_fresh_for_the_dead_and_ends_once_for_the_living() {
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snap = snapshot_with(Vec::new(), at);
        let mut stopped = false;
        for secs in (-120..=900).step_by(5) {
            let now = at + time::Duration::seconds(secs);
            assert_eq!(snap.running(now, |_| false), snap.fresh(now), "{secs}s");
            let alive = snap.running(now, |_| true);
            assert!(!(stopped && alive), "running again at {secs}s");
            stopped |= !alive;
            assert_eq!(alive, now - at < BEHIND_GRACE, "{secs}s");
        }
        assert!(stopped, "the sweep reached past the grace");
        assert!(snap.running(at - time::Duration::hours(1), |_| false));
    }

    /// `alive` is asked only when the heartbeat alone does not answer: a
    /// fresh one needs no process check, and one past the grace gets none.
    #[test]
    fn running_asks_about_the_process_only_when_it_matters() {
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snap = snapshot_with(Vec::new(), at);
        let asked = std::cell::Cell::new(0);
        let alive = |_| {
            asked.set(asked.get() + 1);
            true
        };
        assert!(snap.running(at, alive));
        assert!(!snap.running(at + BEHIND_GRACE, alive));
        assert_eq!(asked.get(), 0);
        assert!(snap.running(at + time::Duration::seconds(31), alive));
        assert_eq!(asked.get(), 1);
    }

    /// The flag survives the file, and a snapshot written before it existed
    /// reads as not refreshed rather than failing to read.
    #[test]
    fn refreshed_round_trips_and_defaults_to_false() {
        let dir = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(dir.path());
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snap = snapshot_with(vec![named("", true, true), named("lab", true, false)], at);
        store.write(&snap).unwrap();
        let back = store.read().unwrap().unwrap();
        assert_eq!(back.backends, snap.backends);
        let body = std::fs::read_to_string(dir.path().join("daemon.json")).unwrap();
        assert_eq!(
            body.matches("refreshed").count(),
            1,
            "false is left out: {body}"
        );

        let mut old: serde_json::Value = serde_json::from_str(&body).unwrap();
        for b in old["backends"].as_array_mut().unwrap() {
            b.as_object_mut().unwrap().remove("refreshed");
        }
        let old = serde_json::to_string(&old).unwrap();
        assert!(!old.contains("refreshed"), "{old}");
        std::fs::write(dir.path().join("daemon.json"), old).unwrap();
        let back = store.read().unwrap().unwrap();
        assert!(back.backends.iter().all(|b| !b.refreshed));
    }

    #[test]
    fn a_snapshot_round_trips_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(dir.path());
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = DaemonSnapshot {
            pid: 42,
            version: "v0.2.0".into(),
            started_at: now,
            updated_at: now,
            connected: true,
            error: String::new(),
            tab_roots: vec!["p1".into()],
            workspace_order: vec!["wZ".into(), "w2".into(), "b0123456789abcdef:w2".into()],
            workspace_display_labels: [
                ("w2".into(), "[w2]".into()),
                ("b0123456789abcdef:w2".into(), "[w2]w2".into()),
            ]
            .into(),
            parents: Default::default(),
            backends: vec![DaemonBackend {
                id: String::new(),
                socket: "/tmp/herdr.sock".into(),
                connected: true,
                error: String::new(),
                refreshed: false,
            }],
        };
        store.write(&snapshot).unwrap();
        assert_eq!(store.read().unwrap(), Some(snapshot.clone()));
        assert!(snapshot.fresh(now + time::Duration::seconds(29)));
        assert!(!snapshot.fresh(now + time::Duration::seconds(30)));
    }

    /// The worker → coordinator map survives the file, and a heartbeat from
    /// a supervisor that never wrote one reads as an empty map, not an error.
    #[test]
    fn parents_round_trip_and_an_older_file_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(dir.path());
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut snapshot = DaemonSnapshot {
            pid: 1,
            version: "v".into(),
            started_at: now,
            updated_at: now,
            connected: true,
            error: String::new(),
            tab_roots: Vec::new(),
            workspace_order: Vec::new(),
            workspace_display_labels: Default::default(),
            parents: Default::default(),
            backends: Vec::new(),
        };
        store.write(&snapshot).unwrap();
        let body = fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(
            !body.contains("parents"),
            "an empty map is left out: {body}"
        );
        snapshot.parents = [
            ("w4:pY".to_string(), "w4:pC".to_string()),
            (
                "bdeadbeefdeadbeef:w1:p2".to_string(),
                "bdeadbeefdeadbeef:w1:p1".to_string(),
            ),
        ]
        .into();
        store.write(&snapshot).unwrap();
        assert_eq!(store.read().unwrap(), Some(snapshot));
        let older = r#"{"pid":1,"version":"v","started_at":"2024-10-02T00:00:00Z","updated_at":"2024-10-02T00:00:00Z","connected":true}"#;
        fs::write(dir.path().join(FILE), older).unwrap();
        assert!(store.read().unwrap().unwrap().parents.is_empty());
    }
}
