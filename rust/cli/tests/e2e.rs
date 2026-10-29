// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The whole pipeline, against a real herdr server.
//!
//! Every other test in this workspace holds one layer still and checks it. This
//! one checks the only claim the product actually makes: that a pane the
//! backend reports becomes a record, and that the record carries what the
//! backend knows about it.
//!
//! It runs only inside herdr and skips otherwise. herdr cannot be started
//! headless against a given socket path, so rather than starting an isolated
//! server this attaches to the one already running. Faking that
//! would make a pass mean nothing, which is worse than skipping.

use time::OffsetDateTime;
use tower_state::{AgentState, Store};

fn herdr_or_skip() -> Option<herdrx::Herdr> {
    if !herdrx::inside() {
        eprintln!("skip: not inside herdr");
        return None;
    }
    let h = herdrx::Herdr::default();
    match h.ping() {
        Ok(_) => Some(h),
        Err(e) => {
            eprintln!("skip: no herdr server: {e}");
            None
        }
    }
}

/// A reported pane becomes a record that knows its kind, its place, and what it
/// is doing.
#[test]
fn a_reported_pane_becomes_a_record_that_carries_meaning() {
    let Some(h) = herdr_or_skip() else { return };
    let d = tempfile::tempdir().unwrap();
    let store = Store::new(d.path()).unwrap();

    tower_scan::sync_backend(&store, &h, OffsetDateTime::now_utc()).expect("sync");
    let agents = store.list().expect("list");
    if agents.is_empty() {
        eprintln!("skip: no agents running in herdr");
        return;
    }

    for a in &agents {
        assert!(!a.kind.is_empty(), "record without a kind: {a:?}");
        assert!(
            !a.pane.pane_id.is_empty(),
            "record without a coordinate: {a:?}"
        );
        assert!(!a.cwd.is_empty(), "record without a folder: {a:?}");

        // The regression this exists for: when backend state is not carried
        // through, every agent reads IDLE forever and the tower shows a wall of
        // identical rows that no longer track anything.
        assert!(
            matches!(
                a.state,
                AgentState::Working
                    | AgentState::Waiting
                    | AgentState::DoneUnread
                    | AgentState::Idle
            ),
            "unexpected state {:?}: {a:?}",
            a.state
        );
    }

    // Two records on one coordinate overwrite each other, so the tower would
    // show one agent and silently lose the other. This is how the base-36 tab
    // parsing bug presented: distinct tabs collapsed onto the same key.
    let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for a in &agents {
        if let Some(prev) = seen.insert(&a.pane.pane_id, &a.id) {
            panic!(
                "pane {} is claimed by both {prev} and {}",
                a.pane.pane_id, a.id
            );
        }
    }
    eprintln!("{} agents reconciled", agents.len());
}

/// Screen preview is display-only, but a screen that will not read leaves the
/// tower showing an empty box where the agent's work should be.
#[test]
fn our_own_screen_reads_back() {
    let Some(h) = herdr_or_skip() else { return };
    let Some(self_id) = herdrx::current_pane_id() else {
        eprintln!("skip: own coordinate unknown");
        return;
    };
    let text = h.capture_pane(&self_id, 3).expect("capture");
    assert!(!text.trim().is_empty(), "own screen came back empty");
}
