// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `/` narrows the list to the rows that contain what is typed, as it is typed.

mod common;

use common::*;
use tower_state::AgentState;
use tower_ui::view::screen_lines;
use tower_ui::{line_text, Action, Event};

fn type_text(f: &mut Fixture, text: &str) {
    for c in text.chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
}

fn tasks(f: &Fixture) -> Vec<String> {
    f.app.agents().iter().map(|a| a.task.clone()).collect()
}

fn screen(f: &Fixture) -> Vec<String> {
    screen_lines(&f.app, 120, 40)
        .iter()
        .map(line_text)
        .collect()
}

#[test]
fn typing_narrows_the_list_and_enter_keeps_it() {
    let mut f = fixture();
    f.tap("/");
    assert!(f.app.filtering());
    type_text(&mut f, "HANDOFF");
    assert_eq!(tasks(&f), ["check the handoff doc"], "case is ignored");
    assert!(
        screen(&f)
            .iter()
            .any(|l| l.contains("filter / HANDOFF") && l.contains("1 of 2")),
        "the field says how much is left"
    );

    f.tap("enter");
    assert!(!f.app.filtering());
    assert_eq!(f.app.filter(), "HANDOFF");
    assert_eq!(
        tasks(&f),
        ["check the handoff doc"],
        "enter keeps the filter"
    );
    assert!(
        screen(&f)
            .iter()
            .any(|l| l.contains("filter / HANDOFF") && l.contains("esc clear")),
        "a kept filter stays said"
    );

    // A new reading is filtered too.
    let mut late = agent("claude-9", "claude", "handoff-lab", AgentState::Idle);
    late.task = "unrelated".into();
    let all: Vec<_> = f.app.all_agents().iter().cloned().chain([late]).collect();
    f.records(all);
    assert_eq!(f.app.agents().len(), 2, "matched by its workspace name");
    assert_eq!(f.app.all_agents().len(), 3);
}

/// `#52` finds the agent working on PR 52.
#[test]
fn an_open_pr_number_is_something_to_filter_by() {
    let mut f = fixture();
    let mut all = f.app.all_agents().to_vec();
    assert!(all.len() > 1, "the fixture has more than one agent");
    all[0].cwd = "/w/pr-52".into();
    let id = all[0].id.clone();
    f.records(all);
    f.feed(Event::Prs(std::collections::HashMap::from([(
        "/w/pr-52".to_string(),
        52,
    )])));
    assert_eq!(filter_to(&mut f, "#52"), [id]);
    assert!(filter_to(&mut f, "#53").is_empty());
}

#[test]
fn esc_clears_the_filter_in_the_field_and_on_the_list_before_quitting() {
    let mut f = fixture();
    f.tap("/");
    type_text(&mut f, "approval");
    assert_eq!(f.app.agents().len(), 1);
    assert_eq!(f.press("esc"), Action::Nothing);
    assert!(!f.app.filtering());
    assert_eq!(f.app.filter(), "");
    assert_eq!(f.app.agents().len(), 2);

    f.tap("/");
    type_text(&mut f, "approval");
    f.tap("enter");
    assert_eq!(f.press("esc"), Action::Nothing, "the first esc clears");
    assert_eq!(f.app.agents().len(), 2);
    assert_eq!(f.press("esc"), Action::Quit, "the next one leaves");
}

#[test]
fn slash_again_edits_the_kept_filter_and_korean_matches() {
    let mut f = fixture();
    let mut ko = agent("claude-5", "claude", "work", AgentState::Idle);
    ko.task = "로그인 버그 수정".into();
    let all: Vec<_> = f.app.all_agents().iter().cloned().chain([ko]).collect();
    f.records(all);

    f.tap("/");
    type_text(&mut f, "버그");
    f.tap("enter");
    assert_eq!(tasks(&f), ["로그인 버그 수정"]);

    f.tap("/");
    assert_eq!(
        f.app.input().unwrap().value(),
        "버그",
        "the field opens on it"
    );
    f.tap("backspace");
    f.tap("backspace");
    assert_eq!(f.app.agents().len(), 3, "an emptied field shows everyone");
}

#[test]
fn nothing_matching_leaves_an_empty_list_and_the_selection_follows() {
    let mut f = fixture();
    f.tap("j");
    let chosen = f.app.selected().unwrap().id.clone();
    f.tap("/");
    type_text(&mut f, "claude");
    assert_eq!(
        f.app.selected().unwrap().id,
        chosen,
        "a still-matching selection stays"
    );
    type_text(&mut f, "-no-such");
    assert!(f.app.agents().is_empty());
    assert!(f.app.selected().is_none());
    assert!(screen(&f).iter().any(|l| l.contains("0 of 2")));
}

/// A filter is a view. The header counts every agent, and a broadcast —
/// whose confirmation says "every session" — still reaches every one.
#[test]
fn the_header_and_a_broadcast_ignore_the_filter() {
    let mut f = fixture();
    f.tap("/");
    type_text(&mut f, "handoff");
    f.tap("enter");
    assert_eq!(f.app.agents().len(), 1);

    f.tap("B");
    type_text(&mut f, "점검");
    f.tap("enter");
    assert_eq!(f.press("y"), Action::Refresh);
    assert_eq!(f.tower.sent().len(), 2, "{:?}", f.tower.sent());
}

// ── generated ─────────────────────────────────────────────────────────────

/// Words the generated agents and filters are made of: mixed case ASCII,
/// Korean (which has no case), an accented letter with a simple case pair,
/// and separators.
const WORDS: &[&str] = &[
    "Fix",
    "fix",
    "LOGIN",
    "login",
    "bug",
    "Bot",
    "로그인",
    "버그",
    "é",
    "É",
    "-",
    " ",
    "/",
    "a",
    "Z",
    "api",
];

fn phrase(rng: &mut tower_testrng::Rng, most: usize) -> String {
    (0..rng.below(most))
        .map(|_| *rng.pick(WORDS).unwrap())
        .collect()
}

fn generated_agents(rng: &mut tower_testrng::Rng) -> Vec<tower_state::Agent> {
    let states = [
        AgentState::Working,
        AgentState::Waiting,
        AgentState::DoneUnread,
        AgentState::Idle,
        AgentState::Error,
    ];
    (0..rng.below(9))
        .map(|i| {
            let kind = *rng.pick(&["claude", "codex", "Gemini"]).unwrap();
            let session = phrase(rng, 3);
            let state = *rng.pick(&states).unwrap();
            let mut a = agent(&format!("g-{i}"), kind, &session, state);
            a.task = phrase(rng, 4);
            a.title = phrase(rng, 2);
            a.cwd = format!("/w/{}", phrase(rng, 2));
            a.pane.tab = phrase(rng, 2);
            a
        })
        .collect()
}

/// Sets the filter the way a person does — `/`, the text, enter — and
/// returns the IDs left on the list.
fn filter_to(f: &mut Fixture, text: &str) -> Vec<String> {
    if !f.app.filter().is_empty() {
        f.tap("esc");
    }
    f.tap("/");
    type_text(f, text);
    f.tap("enter");
    assert_eq!(f.app.filter(), text);
    ids(f)
}

fn ids(f: &Fixture) -> Vec<String> {
    f.app.agents().iter().map(|a| a.id.clone()).collect()
}

/// The naive reading of the rule: an agent stays when any of the fields a
/// row can be found by holds the filter, both lowercased.
fn naive_match(a: &tower_state::Agent, filter: &str) -> bool {
    let needle = filter.to_lowercase();
    [
        &a.pane.session,
        &a.pane.tab,
        &a.title,
        &a.task,
        &a.kind,
        &a.cwd,
    ]
    .iter()
    .any(|field| field.to_lowercase().contains(&needle))
}

/// Properties over generated agent sets and filter strings (seeded):
///
/// - the filtered list is a subset of all agents, in the same order;
/// - it is exactly what the naive field scan keeps;
/// - the empty filter is everyone;
/// - filtering again with the same text changes nothing (idempotent);
/// - the upper- and lower-case spellings of a filter keep the same set.
#[test]
fn generated_filters_keep_a_subset_idempotently_and_ignore_case() {
    let mut rng = tower_testrng::Rng::new(0xF117_E125);
    for round in 0..60 {
        let mut f = fixture();
        f.records(generated_agents(&mut rng));
        let all: Vec<String> = f.app.all_agents().iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids(&f), all, "{round}: no filter is everyone");

        let text: String = (0..1 + rng.below(3))
            .map(|_| *rng.pick(WORDS).unwrap())
            .collect();
        let got = filter_to(&mut f, &text);
        let mut rest = all.iter();
        assert!(
            got.iter().all(|id| rest.any(|a| a == id)),
            "{round}: {got:?} is not an ordered subset of {all:?}"
        );
        let naive: Vec<String> = f
            .app
            .all_agents()
            .iter()
            .filter(|a| naive_match(a, &text))
            .map(|a| a.id.clone())
            .collect();
        assert_eq!(got, naive, "{round}: {text:?}");

        assert_eq!(filter_to(&mut f, &text), got, "{round}: idempotent");
        f.tap("/");
        f.tap("enter");
        assert_eq!(ids(&f), got, "{round}: reopening the field changes nothing");

        let upper = filter_to(&mut f, &text.to_uppercase());
        let lower = filter_to(&mut f, &text.to_lowercase());
        assert_eq!(upper, lower, "{round}: {text:?}");
        assert_eq!(upper, got, "{round}: {text:?}");

        f.tap("esc");
        assert_eq!(f.app.filter(), "");
        assert_eq!(ids(&f), all, "{round}: cleared is everyone again");
    }
}

/// Property over generated sets (seeded): the selection stays on its agent
/// whenever that agent still matches the filter, and the cursor is always on
/// a row of the filtered list, or on none when that list is empty.
#[test]
fn the_selection_stays_on_its_agent_while_it_matches() {
    let mut rng = tower_testrng::Rng::new(0x5E1E_C7ED);
    let mut kept = 0;
    for round in 0..60 {
        let mut f = fixture();
        f.records(generated_agents(&mut rng));
        for _ in 0..rng.below(6) {
            f.tap("j");
        }
        let before = f.app.selected().cloned();
        let text: String = (0..1 + rng.below(2))
            .map(|_| *rng.pick(WORDS).unwrap())
            .collect();
        let got = filter_to(&mut f, &text);
        if let Some(a) = &before {
            if naive_match(a, &text) {
                assert_eq!(
                    f.app.selected().map(|s| s.id.clone()),
                    Some(a.id.clone()),
                    "{round}: {text:?}"
                );
                kept += 1;
            }
        }
        match f.app.selected() {
            Some(s) => assert!(got.contains(&s.id), "{round}"),
            None => assert!(got.is_empty(), "{round}"),
        }
    }
    assert!(kept > 8, "{kept}");
}
