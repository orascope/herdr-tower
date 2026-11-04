// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Parallax Browser's handoffs in the tower: a count beside WAIT, from the
//! supervisor's snapshot and only while it is fresh.

mod common;

use common::*;
use time::Duration;
use tower_state::AgentState;
use tower_ui::line_text;
use tower_ui::view::screen_lines;
use tower_ui::Event;

fn screen(f: &Fixture) -> Vec<String> {
    screen_lines(&f.app, 120, 30)
        .iter()
        .map(line_text)
        .collect()
}

/// Urgent and action count, review does not, and the count adds no line: the
/// tower has only a number, so what each one asks is left to Parallax.
#[test]
fn waiting_handoffs_are_counted_beside_wait() {
    let mut f = fixture();
    f.records(vec![agent("c1", "claude", "work", AgentState::Waiting)]);
    let before = screen(&f);
    parallax_snapshot(&f.store, 1, 1, 5);
    f.records(vec![agent("c1", "claude", "work", AgentState::Waiting)]);
    let s = screen(&f);
    assert!(
        s[0].contains("wait 1  parallax 2"),
        "the count sits after the tally: {}",
        s[0]
    );
    assert_eq!(s[1..], before[1..], "nothing but the count changes");
}

#[test]
fn nothing_shows_without_a_fresh_snapshot_that_waits() {
    let mut f = fixture();
    f.records(vec![agent("c1", "claude", "work", AgentState::Working)]);
    let plain = screen(&f);
    assert!(!plain.iter().any(|l| l.contains("parallax")));

    // Review only: finished work, not somebody waiting.
    parallax_snapshot(&f.store, 0, 0, 3);
    f.records(vec![agent("c1", "claude", "work", AgentState::Working)]);
    assert_eq!(screen(&f), plain);

    // Waiting, but the supervisor stopped two minutes ago.
    parallax_snapshot(&f.store, 0, 1, 0);
    let _ = f.app.on_event(Event::Records {
        agents: vec![agent("c1", "claude", "work", AgentState::Working)],
        branches: Default::default(),
        tab_roots: Default::default(),
        parents: Default::default(),
        daemon: None,
        now: t0() + Duration::minutes(2),
    });
    assert!(!screen(&f).iter().any(|l| l.contains("parallax")));
}
