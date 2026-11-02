// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! A workspace's rule folds its rows under it, as a machine's band does.

mod common;

use common::*;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tower_state::AgentState;
use tower_ui::view::{rows, screen_lines};
use tower_ui::{line_text, Action, Event, RemoteAgent, RemoteMachine};

const WIDTH: u16 = 120;

fn screen(f: &Fixture) -> Vec<String> {
    let (w, h) = f.app.size();
    screen_lines(&f.app, w.into(), h.into())
        .iter()
        .map(|l| line_text(l))
        .collect()
}

fn line_of(f: &mut Fixture, text: &str) -> u16 {
    let at = screen(f)
        .iter()
        .position(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{}", screen(f).join("\n")));
    f.app.remember_drawn();
    at as u16
}

fn click_row(f: &mut Fixture, row: u16) {
    let event = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 2,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(f.app.on_event(event), Action::Nothing);
}

fn click(f: &mut Fixture, text: &str) {
    let row = line_of(f, text);
    click_row(f, row);
}

fn on_screen(f: &Fixture, text: &str) -> bool {
    screen(f).iter().any(|l| l.contains(text))
}

fn selected(f: &Fixture) -> Option<String> {
    f.app.selected().map(|a| a.id.clone())
}

fn three_workspaces(f: &mut Fixture) {
    f.records(vec![
        agent("a-1", "claude", "alpha", AgentState::Idle),
        agent("a-2", "codex", "alpha", AgentState::Idle),
        agent("b-1", "claude", "beta", AgentState::Idle),
        agent("b-2", "codex", "beta", AgentState::Idle),
        agent("c-1", "claude", "gamma", AgentState::Idle),
    ]);
}

fn remote_agent(workspace: &str, id: &str) -> RemoteAgent {
    RemoteAgent {
        workspace: workspace.into(),
        pane_id: format!("w2:{id}"),
        agent: "claude".into(),
        status: "idle".into(),
        id: id.into(),
        loop_state: None,
        schedules: Vec::new(),
        title: String::new(),
        tab: String::new(),
        session_id: String::new(),
        cwd: String::new(),
    }
}

fn machine(label: &str, agents: Vec<RemoteAgent>) -> RemoteMachine {
    RemoteMachine {
        label: label.into(),
        target: format!("me@{label}"),
        agents,
        ..Default::default()
    }
}

/// Two machines, each with a workspace called `shared`.
fn two_machines() -> Vec<RemoteMachine> {
    vec![
        machine(
            "desk",
            vec![
                remote_agent("shared", "d1"),
                remote_agent("shared", "d2"),
                remote_agent("tools", "d3"),
            ],
        ),
        machine("gpu", vec![remote_agent("shared", "g1")]),
    ]
}

/// What the list draws must agree with what the cursor moves over: the line
/// of the cursor draws the agent selected, and every rule can be clicked.
fn assert_consistent(f: &Fixture, context: &str) {
    let drawn = rows(&f.app, WIDTH.into());
    let agents = drawn.agent_of.iter().flatten().count();
    assert_eq!(agents, f.app.agents().len(), "{context}: every local row");
    let remote = drawn.remote_of.iter().flatten().count();
    assert_eq!(
        remote,
        f.app.remote_agents().len(),
        "{context}: every remote row"
    );
    assert_eq!(drawn.lines.len(), drawn.band_of.len(), "{context}");
    if f.app.remote_mode() {
        if remote > 0 {
            assert_eq!(
                drawn.remote_of[drawn.cursor_row],
                Some(f.app.remote_cursor()),
                "{context}: the remote bar is on the selected row"
            );
        }
    } else if agents > 0 {
        assert_eq!(
            drawn.agent_of[drawn.cursor_row],
            Some(f.app.cursor()),
            "{context}: the bar is on the selected row"
        );
    }
    for (line, band) in drawn.lines.iter().zip(&drawn.band_of) {
        let text = line_text(line);
        if text.contains('▼') || text.contains('▶') {
            assert!(band.is_some(), "{context}: {text:?} can be clicked");
        }
    }
}

#[test]
fn a_click_on_a_rule_folds_its_rows_and_another_opens_them() {
    let mut f = fixture();
    three_workspaces(&mut f);
    assert!(on_screen(&f, "── ▼ alpha (2) "));
    assert!(on_screen(&f, "── ▼ gamma (1) "));

    click(&mut f, "▼ beta");
    assert!(on_screen(&f, "── ▶ beta (2) "), "the rule keeps counting");
    assert_eq!(
        f.app
            .agents()
            .iter()
            .map(|a| a.id.as_str())
            .collect::<Vec<_>>(),
        ["a-1", "a-2", "c-1"]
    );
    assert_consistent(&f, "folded");

    click(&mut f, "▶ beta");
    assert!(on_screen(&f, "── ▼ beta (2) "));
    assert_eq!(f.app.agents().len(), 5);
    assert_consistent(&f, "opened");
}

/// There is no machine band above the rules here, and folding every one of
/// them must still leave something to click.
#[test]
fn every_workspace_can_be_folded_and_opened_without_a_machine_on_screen() {
    let mut f = fixture();
    three_workspaces(&mut f);
    for name in ["alpha", "beta", "gamma"] {
        click(&mut f, &format!("▼ {name}"));
        assert_consistent(&f, name);
    }
    assert!(f.app.agents().is_empty());
    assert_eq!(f.app.selected().map(|a| a.id.clone()), None);
    assert!(on_screen(&f, "▶ alpha (2)") && on_screen(&f, "▶ gamma (1)"));
    f.tap("j");
    f.tap("k");
    click(&mut f, "▶ beta");
    assert_eq!(f.app.agents().len(), 2);
    assert_consistent(&f, "one open again");
}

#[test]
fn a_selection_stays_unless_it_is_folded_away() {
    let mut f = fixture();
    three_workspaces(&mut f);
    f.tap("j");
    f.tap("j");
    f.tap("j");
    assert_eq!(selected(&f).as_deref(), Some("b-2"));
    for _ in 0..2 {
        click(&mut f, " alpha");
        assert_eq!(
            selected(&f).as_deref(),
            Some("b-2"),
            "another workspace's rule leaves it"
        );
    }
    click(&mut f, " beta");
    assert_eq!(
        selected(&f).as_deref(),
        Some("c-1"),
        "the second row of the group folded away: the row after the rule"
    );
    click(&mut f, " beta");
    assert_eq!(selected(&f).as_deref(), Some("c-1"), "and it stays there");
    click(&mut f, " gamma");
    assert_eq!(
        selected(&f).as_deref(),
        Some("b-2"),
        "nothing after the rule: the row before it"
    );
}

#[test]
fn folded_rows_are_skipped_by_the_keys() {
    let mut f = fixture();
    three_workspaces(&mut f);
    click(&mut f, " beta");
    let mut seen = vec![selected(&f).unwrap()];
    for _ in 0..4 {
        f.tap("j");
        seen.push(selected(&f).unwrap());
    }
    assert_eq!(seen, ["a-1", "a-2", "c-1", "c-1", "c-1"]);
    f.tap("k");
    assert_eq!(selected(&f).as_deref(), Some("a-2"));
}

/// New records come every second; a fold outlives them, and a row added to a
/// folded workspace is counted rather than drawn.
#[test]
fn a_fold_outlives_a_refresh() {
    let mut f = fixture();
    three_workspaces(&mut f);
    click(&mut f, " beta");
    f.records(vec![
        agent("a-1", "claude", "alpha", AgentState::Idle),
        agent("b-1", "claude", "beta", AgentState::Idle),
        agent("b-2", "codex", "beta", AgentState::Idle),
        agent("b-3", "gemini", "beta", AgentState::Idle),
    ]);
    assert!(on_screen(&f, "▶ beta (3)"));
    assert!(f.app.agents().iter().all(|a| a.pane.session != "beta"));
    assert_consistent(&f, "refreshed");
}

/// By ID, so a rename keeps a workspace folded and two of one name fold apart.
#[test]
fn a_workspace_is_folded_by_its_id() {
    let with = |id: &str, workspace: &str, session: &str| {
        let mut a = agent(id, "claude", session, AgentState::Idle);
        a.pane.workspace_id = workspace.into();
        a
    };
    let mut f = fixture();
    f.records(vec![with("x", "w1", "same"), with("y", "w2", "same")]);
    assert_eq!(f.app.agents().len(), 2);
    let first_rule = rows(&f.app, WIDTH.into())
        .band_of
        .into_iter()
        .flatten()
        .next();
    f.app.toggle_fold(&first_rule.unwrap());
    assert_eq!(f.app.agents().len(), 1, "only the one with that ID");
    let kept = f.app.agents()[0].id.clone();
    let renamed = f
        .app
        .all_agents()
        .iter()
        .map(|a| {
            let (workspace, session) = if a.id == kept {
                (a.pane.workspace_id.clone(), a.pane.session.clone())
            } else {
                (a.pane.workspace_id.clone(), "renamed".to_string())
            };
            with(&a.id, &workspace, &session)
        })
        .collect();
    f.records(renamed);
    assert_eq!(f.app.agents().len(), 1, "still folded under its new name");
    assert_eq!(f.app.agents()[0].id, kept);
}

/// Folding a row away is not filtering it out.
#[test]
fn a_filter_counts_what_is_folded_and_leaves_it_folded() {
    let mut f = fixture();
    three_workspaces(&mut f);
    f.feed(Event::Remote(two_machines()));
    click(&mut f, "▼ beta");
    click(&mut f, "▼ tools");
    f.tap("/");
    for c in "e".chars() {
        f.tap(&c.to_string());
    }
    let v = screen(&f).join("\n");
    assert!(v.contains("▶ beta (2)"), "folded still:\n{v}");
    assert!(f.app.agents().iter().all(|a| a.pane.session != "beta"));
    assert_consistent(&f, "filtered");
    f.tap("esc");
    assert_consistent(&f, "unfiltered");
}

#[test]
fn a_saved_machines_workspace_folds_too_and_apart_from_anothers_of_one_name() {
    let mut f = fixture();
    three_workspaces(&mut f);
    f.feed(Event::Remote(two_machines()));
    assert_eq!(f.app.remote_agents().len(), 4);
    let row = line_of(&mut f, "▼ shared (2)");
    click_row(&mut f, row);
    assert!(on_screen(&f, "▶ shared (2)"));
    assert!(on_screen(&f, "▼ shared (1)"), "gpu's has the same name");
    assert_eq!(
        f.app
            .remote_agents()
            .iter()
            .map(|(_, a)| a.id.as_str())
            .collect::<Vec<_>>(),
        ["d3", "g1"]
    );
    assert_consistent(&f, "remote folded");
    click(&mut f, "▶ shared (2)");
    assert_eq!(f.app.remote_agents().len(), 4);
}

#[test]
fn a_remote_selection_folded_away_goes_to_the_row_after_the_rule() {
    let mut f = fixture();
    three_workspaces(&mut f);
    f.feed(Event::Remote(two_machines()));
    let at = |f: &Fixture| f.app.selected_remote().map(|(_, a)| a.id.clone());
    click(&mut f, "w2:d2");
    assert_eq!(at(&f).as_deref(), Some("d2"));
    click(&mut f, "▼ tools");
    assert_eq!(at(&f).as_deref(), Some("d2"), "another workspace's rule");
    click(&mut f, "▶ tools");
    click(&mut f, "▼ shared (2)");
    assert_eq!(at(&f).as_deref(), Some("d3"), "the row after the rule");
    assert_consistent(&f, "moved");
    click(&mut f, "▼ shared (1)");
    assert_eq!(at(&f).as_deref(), Some("d3"), "another machine's rule");
    click(&mut f, "▼ tools");
    assert_eq!(at(&f), None, "every remote row folded");
    assert!(!f.app.remote_mode(), "back on the local rows");
    assert_consistent(&f, "none left");
}

/// `k` from the first remote row has nowhere to go with no local row drawn,
/// and the local band stays to bring them back.
#[test]
fn every_local_workspace_folded_leaves_the_cursor_on_the_machines() {
    let mut f = fixture();
    three_workspaces(&mut f);
    f.feed(Event::Remote(two_machines()));
    for name in ["alpha", "beta", "gamma"] {
        click(&mut f, &format!("▼ {name}"));
        assert_consistent(&f, name);
    }
    assert!(f.app.remote_mode(), "the selection went to the machines");
    assert_eq!(f.app.remote_cursor(), 0);
    assert!(on_screen(&f, "@local (5)"));
    f.tap("k");
    assert!(f.app.remote_mode(), "no local row to go up into");
    click(&mut f, "▶ beta");
    f.tap("k");
    assert!(!f.app.remote_mode());
    assert_eq!(selected(&f).as_deref(), Some("b-2"));
}

/// A folded machine hides its folded workspaces; opened, they are still folded.
#[test]
fn a_machines_fold_and_its_workspaces_are_kept_apart() {
    let mut f = fixture();
    three_workspaces(&mut f);
    f.feed(Event::Remote(two_machines()));
    click(&mut f, "▼ beta");
    click(&mut f, " @local");
    assert!(!on_screen(&f, " beta"));
    assert!(f.app.agents().is_empty());
    click(&mut f, " @local");
    assert!(on_screen(&f, "▶ beta (2)"));
    assert_eq!(f.app.agents().len(), 3);
    click(&mut f, "▼ tools");
    click(&mut f, " @desk");
    click(&mut f, " @desk");
    assert!(on_screen(&f, "▶ tools (1)"));
    assert_consistent(&f, "both");
}

fn six_workspaces(f: &mut Fixture) {
    let mut records = Vec::new();
    for w in 0..6 {
        for a in 0..4 {
            records.push(agent(
                &format!("w{w}-{a}"),
                "claude",
                &format!("ws{w}"),
                AgentState::Idle,
            ));
        }
    }
    f.records(records);
}

/// The rule clicked stays on the row it was clicked on, though the list drawn
/// around the cursor would have moved it, and the selection stays too.
#[test]
fn a_rule_clicked_mid_list_stays_where_it_was() {
    let mut f = fixture();
    f.app.set_size(WIDTH, 16);
    six_workspaces(&mut f);
    for _ in 0..13 {
        f.tap("j");
    }
    let row = line_of(&mut f, "▼ ws2");
    let before = selected(&f);
    click_row(&mut f, row);
    assert_eq!(line_of(&mut f, "▶ ws2"), row, "the rule did not move");
    assert_eq!(selected(&f), before, "nor the selection");
    click_row(&mut f, row);
    assert_eq!(line_of(&mut f, "▼ ws2"), row);
    assert_eq!(selected(&f), before);
}

/// Near the list's ends it moves only as far as they make it, and every rule
/// is still on the screen to click again.
#[test]
fn a_rule_clicked_anywhere_is_still_on_the_screen() {
    for steps in [0usize, 7, 13, 19, 23] {
        for name in 0..6 {
            let mut f = fixture();
            f.app.set_size(WIDTH, 16);
            six_workspaces(&mut f);
            for _ in 0..steps {
                f.tap("j");
            }
            let open = format!("▼ ws{name}");
            if !on_screen(&f, &open) {
                continue;
            }
            click(&mut f, &open);
            assert!(on_screen(&f, &format!("▶ ws{name}")), "{steps} {name}");
            click(&mut f, &format!("▶ ws{name}"));
            assert!(on_screen(&f, &open), "{steps} {name}");
        }
    }
}

/// A rule held at the top while its rows scroll past folds them too, as a
/// band does.
#[test]
fn a_repeated_rule_folds_its_workspace() {
    let mut f = fixture();
    f.app.set_size(WIDTH, 12);
    six_workspaces(&mut f);
    for _ in 0..11 {
        f.tap("j");
    }
    let lines = screen(&f);
    let up = lines.iter().position(|l| l.contains("↑"));
    let row = up.expect("the list scrolled") + 1;
    let rule = lines[row].clone();
    assert!(rule.contains("▼ ws"), "a rule repeated there: {rule:?}");
    f.app.remember_drawn();
    click_row(&mut f, row as u16);
    assert!(on_screen(&f, "▶ ws"), "{}", screen(&f).join("\n"));
    assert_consistent(&f, "sticky");
}

/// Whatever is folded, in whatever order, with the records and machines
/// changing under it: the list and the cursor agree.
#[test]
fn generated_folds_keep_the_list_and_the_cursor_in_step() {
    let mut rng = tower_testrng::Rng::new(0x5f01d);
    for round in 0..120 {
        let mut f = fixture();
        f.app.set_size(WIDTH, 24);
        let workspaces = 1 + rng.below(4);
        let local: Vec<_> = (0..1 + rng.below(9))
            .map(|i| {
                let w = rng.below(workspaces);
                agent(
                    &format!("l{i}"),
                    "claude",
                    &format!("ws{w}"),
                    AgentState::Idle,
                )
            })
            .collect();
        f.records(local);
        if rng.one_in(2) {
            f.feed(Event::Remote(two_machines()));
        }
        for step in 0..30 {
            let context = format!("round {round} step {step}");
            let bands: Vec<String> = rows(&f.app, WIDTH.into())
                .band_of
                .into_iter()
                .flatten()
                .collect();
            match rng.below(6) {
                0..=2 => {
                    if let Some(band) = rng.pick(&bands) {
                        let band = band.clone();
                        f.app.toggle_fold(&band);
                    }
                }
                3 => f.tap("j"),
                4 => f.tap("k"),
                _ => f.feed(Event::Remote(if rng.one_in(2) {
                    two_machines()
                } else {
                    Vec::new()
                })),
            }
            assert_consistent(&f, &context);
        }
    }
}
