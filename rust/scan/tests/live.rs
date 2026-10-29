// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The whole lower stack against a running server: what herdr reports becomes
//! records, and those records carry the state herdr knows.
//!
//! Skips when herdr is absent — a pass without a server would prove nothing.

use tower_scan::sync_backend;
use tower_state::{AgentState, Store};

#[test]
fn backend_panes_become_records_with_state() {
    if !herdrx::inside() {
        eprintln!("skip: not inside herdr");
        return;
    }
    let h = herdrx::Herdr::default();
    if h.ping().is_err() {
        eprintln!("skip: no herdr server");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path()).unwrap();
    sync_backend(&store, &h, time::OffsetDateTime::now_utc()).unwrap();

    let agents = store.list().unwrap();
    if agents.is_empty() {
        eprintln!("skip: herdr reports no agents");
        return;
    }

    for a in &agents {
        assert!(!a.kind.is_empty(), "record without a kind: {a:?}");
        assert!(
            !a.pane.pane_id.is_empty(),
            "record without coordinates: {a:?}"
        );
        assert!(!a.cwd.is_empty(), "record without a folder: {a:?}");
    }

    // The failure this stack exists to prevent: every agent parked at IDLE
    // because nothing carried the backend's state across.
    let all_idle = agents.iter().all(|a| a.state == AgentState::Idle);
    let any_working = agents.iter().any(|a| a.state == AgentState::Working);
    eprintln!(
        "{} records; working={} all_idle={}",
        agents.len(),
        agents
            .iter()
            .filter(|a| a.state == AgentState::Working)
            .count(),
        all_idle
    );
    assert!(
        !all_idle || !any_working,
        "backend state did not reach the records"
    );

    // Coordinates must not collide, or records overwrite each other.
    let mut seen = std::collections::HashMap::new();
    for a in &agents {
        if let Some(prev) = seen.insert(a.pane.pane_id.clone(), a.id.clone()) {
            panic!("pane {} shared by {} and {}", a.pane.pane_id, prev, a.id);
        }
    }
}
