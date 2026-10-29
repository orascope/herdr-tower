// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The producers, wired up for real — threads, channels and all.
//!
//! Every periodic job owns its own clock, so it is worth proving that a store
//! read reaches the screen without anybody re-arming a timer, and that shutting
//! down does not wait on work that takes minutes.
//!
//! No herdr client is passed, so no watch is opened: these run on the poll
//! interval alone, which is exactly what happens when a server refuses a
//! subscription.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use tower_state::{AgentState, DaemonSnapshot, DaemonStore, Store};
use tower_ui::{Event, Feed};

/// Waits for the first event of the kind the predicate accepts.
fn wait_for<F>(feed: &Feed, what: &str, mut accept: F) -> Event
where
    F: FnMut(&Event) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(ev) = feed.try_recv() {
            if accept(&ev) {
                return ev;
            }
            continue;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no {what} within five seconds");
}

fn write_daemon(store: &Store, roots: impl IntoIterator<Item = String>) {
    write_daemon_tree(store, roots, Default::default());
}

fn write_daemon_tree(
    store: &Store,
    roots: impl IntoIterator<Item = String>,
    parents: std::collections::BTreeMap<String, String>,
) {
    let now = time::OffsetDateTime::now_utc();
    DaemonStore::new(store.dir())
        .write(&DaemonSnapshot {
            pid: 42,
            version: "v-test-daemon".into(),
            started_at: now,
            updated_at: now,
            connected: true,
            error: String::new(),
            tab_roots: roots.into_iter().collect(),
            workspace_order: Vec::new(),
            workspace_display_labels: Default::default(),
            parents,
            backends: Vec::new(),
        })
        .unwrap();
}

/// A storage failure reaches the screen, and stops being on it once the store
/// answers again.
///
/// Both halves were wrong in turn: the read failure used to be rendered as "no
/// agents" — the same picture as a fleet that has gone home — and then, once
/// reported, the error line had no way to clear itself and sat there over
/// records that were updating perfectly well.
#[test]
fn a_storage_failure_is_reported_and_then_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = tower_state::Store::new(dir.path()).unwrap();
    store
        .save(&agent("claude-1", "claude", "solo", AgentState::Waiting))
        .unwrap();
    write_daemon(&store, []);

    let tower = Arc::new(StubTower::default());
    let mut feed = Feed::spawn(store, tower, settings(home.path()));
    wait_for(&feed, "records", |e| matches!(e, Event::Records { .. }));

    // The records directory goes away under it.
    std::fs::remove_dir_all(dir.path().join("agents")).unwrap();
    let ev = wait_for(&feed, "a failure notice", |e| {
        matches!(e, Event::Health(Err(_)))
    });
    match ev {
        Event::Health(Err(msg)) => assert!(msg.contains("state store"), "{msg}"),
        other => panic!("expected a failure notice, got {other:?}"),
    }

    // And back.
    std::fs::create_dir_all(dir.path().join("agents")).unwrap();
    wait_for(&feed, "the withdrawal", |e| {
        matches!(e, Event::Health(Ok(())))
    });
    feed.stop();
}

/// The store is read once at start-up without anybody asking, and the records
/// arrive as an event like everything else.
#[test]
fn the_reader_delivers_the_store_without_being_asked() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = tower_state::Store::new(dir.path()).unwrap();
    store
        .save(&agent("claude-1", "claude", "solo", AgentState::Waiting))
        .unwrap();
    write_daemon(&store, []);

    let tower = Arc::new(StubTower::default());
    let mut feed = Feed::spawn(store, tower, settings(home.path()));

    let ev = wait_for(&feed, "records", |e| matches!(e, Event::Records { .. }));
    match ev {
        Event::Records { agents, .. } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].id, "claude-1");
        }
        other => panic!("expected records, got {other:?}"),
    }
    feed.stop();
}

/// The root Herdr measured must travel with the records; otherwise the app has
/// no alternative but to infer hierarchy from reusable pane coordinates.
#[test]
fn the_reader_delivers_tab_roots_with_the_records() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = tower_state::Store::new(dir.path()).unwrap();
    store
        .save(&agent("parent", "claude", "work", AgentState::Idle))
        .unwrap();

    let roots = HashSet::from(["w2:p5Z".to_string()]);
    write_daemon(&store, roots.clone());
    let tower = Arc::new(StubTower::default());
    let mut feed = Feed::spawn(store, tower, settings(home.path()));

    let ev = wait_for(&feed, "records with layout", |e| {
        matches!(e, Event::Records { .. })
    });
    match ev {
        Event::Records { tab_roots, .. } => assert_eq!(tab_roots, roots),
        other => panic!("expected records, got {other:?}"),
    }
    feed.stop();
}

/// The supervisor's worker → coordinator map travels with the records too,
/// or a worker in a tab of its own would be drawn as a root again.
#[test]
fn the_reader_delivers_the_supervisors_tree_with_the_records() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = tower_state::Store::new(dir.path()).unwrap();
    store
        .save(&agent("parent", "claude", "work", AgentState::Idle))
        .unwrap();
    let parents: std::collections::BTreeMap<String, String> =
        [("w4:pY".to_string(), "w4:pC".to_string())].into();
    write_daemon_tree(&store, Vec::new(), parents.clone());
    let tower = Arc::new(StubTower::default());
    let mut feed = Feed::spawn(store, tower, settings(home.path()));
    let ev = wait_for(&feed, "records with a tree", |e| {
        matches!(e, Event::Records { .. })
    });
    match ev {
        Event::Records { parents: got, .. } => assert_eq!(got, parents),
        other => panic!("expected records, got {other:?}"),
    }
    feed.stop();
}

/// `r` does not wait out the poll interval.
#[test]
fn a_refresh_is_answered_before_the_interval_would_have_elapsed() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = tower_state::Store::new(dir.path()).unwrap();
    write_daemon(&store, []);
    let tower = Arc::new(StubTower::default());
    let mut feed = Feed::spawn(store.clone(), tower, settings(home.path()));

    wait_for(&feed, "the first read", |e| {
        matches!(e, Event::Records { .. })
    });

    // A record appears; ask for a read rather than waiting two seconds for one.
    store
        .save(&agent("codex-9", "codex", "late", AgentState::Idle))
        .unwrap();
    let started = Instant::now();
    feed.refresh();
    let ev = wait_for(
        &feed,
        "the second read",
        |e| matches!(e, Event::Records { agents, .. } if !agents.is_empty()),
    );
    assert!(
        started.elapsed() < tower_ui::POLL_INTERVAL,
        "the nudge was ignored and the poll answered instead"
    );
    match ev {
        Event::Records { agents, .. } => assert_eq!(agents[0].id, "codex-9"),
        other => panic!("expected records, got {other:?}"),
    }
    feed.stop();
}

/// The preview thread captures whatever the driver last asked for, and nothing
/// when it has been asked for nothing.
#[test]
fn the_preview_captures_the_pane_it_was_last_pointed_at() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = tower_state::Store::new(dir.path()).unwrap();
    write_daemon(&store, []);
    let tower = Arc::new(StubTower {
        capture: Some("screen contents".into()),
        ..Default::default()
    });
    let mut settings = settings(home.path());
    settings.preview_interval = Duration::from_millis(200);
    let mut feed = Feed::spawn(store, tower, settings);

    feed.set_preview(Some(("p-claude-1".into(), 10)));
    let ev = wait_for(&feed, "a capture", |e| matches!(e, Event::Preview { .. }));
    match ev {
        Event::Preview { pane_id, body } => {
            assert_eq!(pane_id, "p-claude-1");
            assert_eq!(body, "screen contents");
        }
        other => panic!("expected a preview, got {other:?}"),
    }
    feed.stop();
}

/// Quitting must not wait on slow work.
///
/// A producer can be inside a slow capture or sweep, and joining it would
/// leave the user looking at a restored terminal long after they pressed `q`.
/// Only the watch reader is waited for, because only it holds a socket that
/// outlives the loop.
#[test]
fn shutting_down_does_not_wait_for_the_slow_producers() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = tower_state::Store::new(dir.path()).unwrap();
    write_daemon(&store, []);
    let tower = Arc::new(StubTower::default());
    let mut feed = Feed::spawn(store, tower, settings(home.path()));
    wait_for(&feed, "the first read", |e| {
        matches!(e, Event::Records { .. })
    });

    let started = Instant::now();
    feed.stop();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "stop() took {:?}",
        started.elapsed()
    );
    // Idempotent: `Drop` calls it again on the way out of this function.
    feed.stop();
}
