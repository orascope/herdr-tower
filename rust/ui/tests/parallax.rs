// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Parallax Browser's handoffs in the tower: a count beside WAIT and one line
//! naming the most urgent, from the supervisor's snapshot and only while it
//! is fresh.

mod common;

use common::*;
use time::Duration;
use tower_state::{AgentState, Attention};
use tower_ui::line_text;
use tower_ui::view::screen_lines;
use tower_ui::Event;

fn screen(f: &Fixture) -> Vec<String> {
    screen_lines(&f.app, 120, 30)
        .iter()
        .map(line_text)
        .collect()
}

#[test]
fn a_waiting_handoff_is_counted_and_the_most_urgent_named() {
    let mut f = fixture();
    parallax_snapshot(
        &f.store,
        vec![
            parallax_handoff("u", "Default", "waiting_approval", Attention::Urgent, ""),
            parallax_handoff(
                "a",
                "Research",
                "needs_human",
                Attention::Action,
                "Pick a plan",
            ),
            parallax_handoff(
                "r",
                "Research",
                "completed_review",
                Attention::Review,
                "Done",
            ),
        ],
    );
    f.records(vec![agent("c1", "claude", "work", AgentState::Waiting)]);
    let s = screen(&f);
    assert!(
        s[0].contains("wait 1  parallax 2"),
        "the count sits after the tally: {}",
        s[0]
    );
    assert_eq!(
        s[1].trim_end(),
        "◆ parallax  Default · waiting for approval  +1 more",
        "a redacted handoff is named by its status"
    );
}

#[test]
fn a_titled_handoff_shows_its_title() {
    let mut f = fixture();
    parallax_snapshot(
        &f.store,
        vec![parallax_handoff(
            "a",
            "Research",
            "needs_human",
            Attention::Action,
            "Pick a plan",
        )],
    );
    f.records(vec![agent("c1", "claude", "work", AgentState::Working)]);
    let s = screen(&f);
    assert!(s[0].contains("parallax 1"), "{}", s[0]);
    assert_eq!(
        s[1].trim_end(),
        "◆ parallax  Research · needs you · Pick a plan"
    );
}

#[test]
fn nothing_shows_without_a_fresh_snapshot_that_waits() {
    let mut f = fixture();
    f.records(vec![agent("c1", "claude", "work", AgentState::Working)]);
    let plain = screen(&f);
    assert!(!plain.iter().any(|l| l.contains("parallax")));

    // Review only: finished work, not somebody waiting.
    parallax_snapshot(
        &f.store,
        vec![parallax_handoff(
            "r",
            "W",
            "completed_review",
            Attention::Review,
            "Done",
        )],
    );
    f.records(vec![agent("c1", "claude", "work", AgentState::Working)]);
    assert_eq!(screen(&f), plain);

    // Waiting, but the supervisor stopped two minutes ago.
    parallax_snapshot(
        &f.store,
        vec![parallax_handoff(
            "a",
            "W",
            "needs_human",
            Attention::Action,
            "",
        )],
    );
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
