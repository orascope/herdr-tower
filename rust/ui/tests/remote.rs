// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Remote mode: the keys that act on agents on saved machines.

mod common;

use common::*;
use tower_ui::view::screen_lines;
use tower_ui::{line_text, Event, RemoteAction, RemoteAgent, RemoteMachine};

/// The whole screen as one string, for substring checks.
fn screen(f: &Fixture, width: u16, height: u16) -> String {
    screen_lines(&f.app, width as usize, height as usize)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn machines() -> Vec<RemoteMachine> {
    let agent = |pane: &str, loop_state: Option<&str>| RemoteAgent {
        workspace: "diskann".into(),
        pane_id: pane.into(),
        agent: "claude".into(),
        status: "idle".into(),
        id: format!("claude-{pane}"),
        loop_state: loop_state.map(str::to_string),
        schedules: Vec::new(),
        title: String::new(),
        tab: String::new(),
        session_id: String::new(),
        cwd: String::new(),
    };
    vec![RemoteMachine {
        label: "gpu".into(),
        target: "alice@gpu".into(),
        agents: vec![
            agent("w2:p6", None),
            agent("w2:p7", Some("(desc)  idle=30m  sent=3  expires=71h")),
        ],
        error: None,
        stale_since: None,
        ..Default::default()
    }]
}

fn with_remote(tower: StubTower) -> Fixture {
    let mut f = fixture_with(tower);
    f.feed(Event::Remote(machines()));
    f
}

fn acts(f: &Fixture) -> Vec<(String, String, RemoteAction)> {
    f.tower.remote_acts.lock().unwrap().clone()
}

#[test]
fn keys_on_the_local_rows_never_reach_a_remote_agent() {
    let mut f = fixture();
    f.feed(Event::Remote(machines()));
    f.tap("x");
    f.tap("m");
    f.tap("esc");
    assert!(acts(&f).is_empty(), "the local rows' keys stay local");
    assert!(!f.app.remote_mode());
}

#[test]
fn j_runs_from_the_last_local_row_into_the_machines_and_k_comes_back() {
    let mut f = fixture();
    let local = f.app.agents().len();
    assert!(local > 0, "the fixture has local agents");
    f.feed(Event::Remote(machines()));
    for _ in 0..local {
        f.tap("j");
    }
    assert!(f.app.remote_mode(), "past the last local row");
    assert_eq!(f.app.remote_cursor(), 0);
    f.tap("j");
    assert_eq!(f.app.remote_cursor(), 1);
    f.tap("k");
    f.tap("k");
    assert!(
        !f.app.remote_mode(),
        "k from the first remote row goes back"
    );
    assert_eq!(f.app.cursor(), local - 1, "onto the last local row");

    // Nothing remote: j stays put on the last local row.
    let mut f = fixture();
    for _ in 0..local + 2 {
        f.tap("j");
    }
    assert!(!f.app.remote_mode());
}

#[test]
fn a_local_only_key_on_a_remote_row_only_says_so() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    for key in ["enter", "i", "g", "T"] {
        f.tap(key);
        assert!(f.app.remote_mode(), "{key} keeps the cursor where it is");
        assert!(acts(&f).is_empty(), "{key} reaches no machine");
    }
    f.tap("o");
    assert!(
        f.app.notice().contains("on another machine"),
        "{}",
        f.app.notice()
    );
    f.tap("esc");
    assert!(!f.app.remote_mode(), "esc goes back to the local rows");
}

#[test]
fn a_remote_row_previews_its_pane_through_herdr_there() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    let (key, _) = tower_ui::view::preview_request(&f.app).expect("a preview is asked for");
    assert_eq!(key, "@gpu\tw2:p6");
    f.feed(Event::Preview {
        pane_id: key,
        body: "remote pane text".into(),
    });
    let v = screen(&f, 200, 60);
    assert!(v.contains("remote pane text"), "{v}");
    assert!(
        v.contains("@gpu · diskann"),
        "the heading names the machine:\n{v}"
    );

    // A machine that stopped answering is not read.
    f.feed(Event::Remote(stale_machine()));
    assert!(tower_ui::view::preview_request(&f.app).is_none());
}

#[test]
fn l_starts_a_loop_and_stops_the_one_that_runs() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    assert!(f.app.remote_mode());
    f.tap("L");
    f.tap("j");
    f.tap("L");
    assert_eq!(
        acts(&f),
        vec![
            ("gpu".into(), "claude-w2:p6".into(), RemoteAction::LoopStart),
            ("gpu".into(), "claude-w2:p7".into(), RemoteAction::LoopStop),
        ]
    );
    let v = screen(&f, 120, 80);
    assert!(v.contains("↻ starting…"), "{v}");
    assert!(!v.contains("↻ 30m"), "the stopped loop is not shown:\n{v}");
}

#[test]
fn m_sends_a_message_and_a_refusal_is_shown() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    f.tap("m");
    for c in "go on".chars() {
        f.tap(&c.to_string());
    }
    f.tap("enter");
    assert_eq!(
        acts(&f),
        vec![(
            "gpu".into(),
            "claude-w2:p6".into(),
            RemoteAction::Send("go on".into())
        )]
    );

    let mut f = with_remote(StubTower {
        remote_fails: Some("ssh timed out".into()),
        ..StubTower::default()
    });
    f.tap("j");
    f.tap("L");
    let v = screen(&f, 120, 40);
    assert!(v.contains("gpu · claude-w2:p6: ssh timed out"), "{v}");
}

#[test]
fn x_asks_before_it_interrupts() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    f.tap("x");
    assert!(screen(&f, 120, 40).contains("[y/N] press Escape in claude-w2:p6 on gpu"));
    f.tap("n");
    assert!(acts(&f).is_empty());
    f.tap("x");
    f.tap("y");
    assert_eq!(acts(&f)[0].2, RemoteAction::Interrupt);
}

#[test]
fn esc_leaves_remote_mode_and_a_vanished_machine_does_too() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    f.tap("esc");
    assert!(!f.app.remote_mode());
    f.tap("j");
    f.feed(Event::Remote(Vec::new()));
    assert!(!f.app.remote_mode());
}

#[test]
fn a_running_remote_loop_wears_the_local_badge() {
    let f = with_remote(StubTower::default());
    let v = screen(&f, 200, 80);
    assert!(v.contains("↻ 30m · 3 · 71h"), "{v}");
}

fn stale_machine() -> Vec<RemoteMachine> {
    let mut m = machines();
    m[0].agents.clear();
    m[0].error = Some("not reachable".into());
    m
}

#[test]
fn a_machine_that_stops_answering_keeps_its_rows_dimmed_and_refuses_keys() {
    let mut f = with_remote(StubTower::default());
    f.feed(Event::Remote(stale_machine()));
    let v = screen(&f, 200, 80);
    assert!(v.contains("@gpu (2) · not reachable · last read"), "{v}");
    assert!(v.contains("w2:p6"), "the last read stays on screen:\n{v}");
    f.tap("j");
    f.tap("L");
    assert!(
        acts(&f).is_empty(),
        "nothing is sent to a machine that is not there"
    );
    assert!(f.app.err().unwrap_or_default().contains("not answering"));

    f.feed(Event::Remote(machines()));
    let v = screen(&f, 200, 80);
    assert!(v.contains("\n ▼ @gpu "), "live again:\n{v}");
}

#[test]
fn the_filter_reaches_remote_rows_and_a_machine_name_keeps_them_all() {
    let mut f = with_remote(StubTower::default());
    f.tap("/");
    for c in "w2:p7".chars() {
        f.tap(&c.to_string());
    }
    let v = screen(&f, 200, 80);
    assert!(v.contains("w2:p7"), "{v}");
    assert!(!v.contains("w2:p6"), "filtered out:\n{v}");

    f.tap("esc");
    f.tap("/");
    for c in "gpu".chars() {
        f.tap(&c.to_string());
    }
    let v = screen(&f, 200, 80);
    assert!(v.contains("w2:p6") && v.contains("w2:p7"), "{v}");
}

/// The filter's count includes the saved machines' rows: a filter that left
/// only a remote row read `0 of N` over the row it was showing.
#[test]
fn the_filter_count_includes_remote_rows() {
    let mut f = with_remote(StubTower::default());
    let local = f.app.all_agents().len();
    f.tap("/");
    for c in "w2:p7".chars() {
        f.tap(&c.to_string());
    }
    let v = screen(&f, 200, 80);
    let want = format!("1 of {}", local + 2);
    assert!(v.contains(&want), "want {want:?}:\n{v}");
}

#[test]
fn a_remote_title_reads_tab_then_name_like_a_local_one() {
    let mut m = machines();
    m[0].agents[0].tab = "build".into();
    m[0].agents[0].title = "v400-build-gap".into();
    let mut f = fixture_with(StubTower::default());
    f.feed(Event::Remote(m));
    assert!(screen(&f, 200, 80).contains("build›v400-build-gap"));
}

#[test]
fn a_tab_named_after_its_pane_says_it_once() {
    let mut m = machines();
    m[0].agents[0].tab = "pin-diskann-v4".into();
    m[0].agents[0].title = "pin-diskann-v4".into();
    let mut f = fixture_with(StubTower::default());
    f.feed(Event::Remote(m));
    let v = screen(&f, 200, 80);
    assert!(v.contains("pin-diskann-v4 "), "{v}");
    assert!(!v.contains("pin-diskann-v4›"), "{v}");
}

fn mouse(kind: crossterm::event::MouseEventKind, row: u16) -> Event {
    Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column: 10,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    })
}

/// The screen row `text` is drawn on, after drawing the screen as the driver
/// does before any click can land.
fn line_of(f: &mut Fixture, text: &str) -> u16 {
    let (w, h) = f.app.size();
    let line = screen_lines(&f.app, w.into(), h.into())
        .iter()
        .position(|l| line_text(l).contains(text))
        .unwrap_or_else(|| panic!("{text:?} is not on screen")) as u16;
    f.app.remember_drawn();
    line
}

#[test]
fn a_click_on_a_remote_row_selects_it_and_a_local_click_comes_back() {
    let mut f = fixture();
    f.feed(Event::Remote(machines()));
    let row = line_of(&mut f, "w2:p7");
    assert_eq!(
        f.app.on_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row,
        )),
        tower_ui::Action::Nothing
    );
    assert!(f.app.remote_mode());
    assert_eq!(f.app.remote_cursor(), 1, "the second remote agent");
    assert!(f.tower.focused().is_empty(), "nothing is jumped to");

    let row = line_of(&mut f, "check the handoff doc");
    assert_eq!(
        f.app.on_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row,
        )),
        tower_ui::Action::Nothing
    );
    assert!(!f.app.remote_mode(), "a local row takes the cursor back");
    assert_eq!(f.app.selected().unwrap().id, "claude-3");
}

#[test]
fn the_wheel_runs_into_the_machines_like_j() {
    let mut f = fixture();
    f.feed(Event::Remote(machines()));
    let local = f.app.agents().len();
    for _ in 0..local {
        assert_eq!(
            f.app
                .on_event(mouse(crossterm::event::MouseEventKind::ScrollDown, 5)),
            tower_ui::Action::Nothing
        );
    }
    assert!(f.app.remote_mode());
    assert_eq!(
        f.app
            .on_event(mouse(crossterm::event::MouseEventKind::ScrollUp, 5)),
        tower_ui::Action::Nothing
    );
    assert!(!f.app.remote_mode());
}

fn with_session(f: &mut Fixture, schedules: Vec<tower_ui::RemoteSchedule>) {
    let mut m = machines();
    m[0].agents[0].session_id = "b8120ca6-e8d0-4000-8000-000000000001".into();
    m[0].agents[0].schedules = schedules;
    f.feed(Event::Remote(m));
}

#[test]
fn t_on_a_remote_row_schedules_there_and_the_row_wears_the_badge() {
    let mut f = fixture_with(StubTower::default());
    with_session(&mut f, Vec::new());
    f.tap("j");
    f.tap("T");
    for c in "19:00 check the build".chars() {
        f.tap(&c.to_string());
    }
    f.tap("tab");
    f.tap("enter");
    assert_eq!(
        acts(&f),
        vec![(
            "gpu".into(),
            "claude-w2:p6".into(),
            RemoteAction::Schedule {
                time: "19:00".into(),
                message: "check the build".into(),
                repeat: tower_state::Repeat::Daily,
            }
        )]
    );
    assert!(screen(&f, 200, 80).contains("⏰ 19:00"));
}

#[test]
fn t_on_a_remote_row_with_schedules_cancels_them() {
    let mut f = fixture_with(StubTower::default());
    with_session(
        &mut f,
        vec![tower_ui::RemoteSchedule {
            id: "t3f2a91".into(),
            next: "19:00".into(),
        }],
    );
    assert!(screen(&f, 200, 80).contains("⏰ 19:00"));
    f.tap("j");
    f.tap("T");
    assert_eq!(
        acts(&f)[0].2,
        RemoteAction::Unschedule(vec!["t3f2a91".into()])
    );
    assert!(!screen(&f, 200, 80).contains("⏰"));
}

#[test]
fn a_remote_schedule_needs_a_session_id_and_a_time_it_can_read() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    f.tap("T");
    assert!(f.app.err().unwrap_or_default().contains("session id"));
    assert!(acts(&f).is_empty());

    let mut f = fixture_with(StubTower::default());
    with_session(&mut f, Vec::new());
    f.tap("j");
    f.tap("T");
    for c in "soon check".chars() {
        f.tap(&c.to_string());
    }
    f.tap("enter");
    assert!(
        f.app.err().unwrap_or_default().starts_with("⏰"),
        "{:?}",
        f.app.err()
    );
    assert!(
        acts(&f).is_empty(),
        "nothing is sent for a time it cannot read"
    );
}

/// The ssh round trip is the driver's, on a thread: the key press only hands
/// it over, and the screen keeps answering until the machine does.
#[test]
fn a_remote_action_is_handed_off_and_one_waits_for_the_other() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    let tower_ui::Action::Remote(first) = f.press("L") else {
        panic!("L hands the loop to the driver");
    };
    assert!(
        acts(&f).is_empty(),
        "nothing went over ssh on the key press"
    );
    assert!(f.app.notice().contains("asking gpu"), "{}", f.app.notice());

    // A second one while the first is out is refused, not raced.
    f.tap("j");
    assert_eq!(f.press("L"), tower_ui::Action::Nothing);
    assert!(
        f.app
            .err()
            .unwrap_or_default()
            .contains("still waiting for gpu"),
        "{:?}",
        f.app.err()
    );

    let done = first.run(f.tower.as_ref());
    f.feed(done);
    assert!(screen(&f, 120, 80).contains("↻ starting…"));
    assert!(
        matches!(f.press("L"), tower_ui::Action::Remote(_)),
        "the answer frees the next one"
    );
}

#[test]
fn a_refusal_also_frees_the_next_remote_action() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    let tower_ui::Action::Remote(first) = f.press("L") else {
        panic!("L hands the loop to the driver");
    };
    f.feed(first.done(Err("ssh timed out".into())));
    assert!(screen(&f, 120, 40).contains("gpu · claude-w2:p6: ssh timed out"));
    assert!(
        !screen(&f, 120, 80).contains("↻ starting…"),
        "a refusal shows nothing started"
    );
    assert!(matches!(f.press("L"), tower_ui::Action::Remote(_)));
}

#[test]
fn a_message_typed_while_another_action_is_out_is_kept() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    let tower_ui::Action::Remote(first) = f.press("L") else {
        panic!("L hands the loop to the driver");
    };
    f.tap("m");
    for c in "go on".chars() {
        f.tap(&c.to_string());
    }
    assert_eq!(f.press("enter"), tower_ui::Action::Nothing);
    assert!(f
        .app
        .err()
        .unwrap_or_default()
        .contains("still waiting for gpu"));
    assert!(
        screen(&f, 120, 40).contains("go on"),
        "the message is still in the field"
    );

    let done = first.run(f.tower.as_ref());
    f.feed(done);
    f.tap("enter");
    assert_eq!(
        acts(&f)[1],
        (
            "gpu".into(),
            "claude-w2:p6".into(),
            RemoteAction::Send("go on".into())
        )
    );
}

/// Hands out one remote action and keeps it unanswered: the state every
/// busy-guard case starts from.
fn with_one_out(f: &mut Fixture) -> tower_ui::RemoteRequest {
    f.tap("j");
    match f.press("L") {
        tower_ui::Action::Remote(request) => *request,
        other => panic!("L hands the loop to the driver, got {other:?}"),
    }
}

fn is_waiting(f: &Fixture) -> bool {
    f.app
        .err()
        .unwrap_or_default()
        .contains("still waiting for gpu · claude-w2:p6")
}

#[test]
fn a_schedule_line_typed_while_another_action_is_out_is_kept() {
    let mut f = fixture_with(StubTower::default());
    with_session(&mut f, Vec::new());
    let first = with_one_out(&mut f);
    f.tap("T");
    for c in "19:00 check the build".chars() {
        f.tap(&c.to_string());
    }
    assert_eq!(f.press("enter"), tower_ui::Action::Nothing);
    assert!(is_waiting(&f), "{:?}", f.app.err());
    assert!(screen(&f, 200, 60).contains("19:00 check the build"));

    f.feed(first.run(f.tower.as_ref()));
    f.tap("enter");
    assert!(
        matches!(acts(&f)[1].2, RemoteAction::Schedule { .. }),
        "{:?}",
        acts(&f)
    );
}

/// Every key that reaches a machine is refused while one action is out, and
/// none of them is sent — `x` after its confirmation, `T` that cancels.
#[test]
fn every_remote_key_waits_for_the_one_out() {
    let mut f = fixture_with(StubTower::default());
    with_session(
        &mut f,
        vec![tower_ui::RemoteSchedule {
            id: "t1".into(),
            next: "19:00".into(),
        }],
    );
    let first = with_one_out(&mut f);
    for keys in [&["L"][..], &["T"], &["x", "y"]] {
        let mut last = tower_ui::Action::Nothing;
        for k in keys {
            last = f.press(k);
        }
        assert_eq!(last, tower_ui::Action::Nothing, "{keys:?}");
        assert!(is_waiting(&f), "{keys:?}: {:?}", f.app.err());
    }
    assert!(acts(&f).is_empty(), "nothing went out: {:?}", acts(&f));
    f.feed(first.run(f.tower.as_ref()));
    assert_eq!(acts(&f).len(), 1);
}

/// A refusal before anything is handed out leaves nothing to wait for.
#[test]
fn a_refused_action_does_not_hold_the_next_one() {
    let mut f = with_remote(StubTower::default());
    f.tap("j");
    f.feed(Event::Remote(stale_machine()));
    f.tap("L");
    assert!(f.app.err().unwrap_or_default().contains("not answering"));
    f.feed(Event::Remote(machines()));
    assert!(matches!(f.press("L"), tower_ui::Action::Remote(_)));
}

/// The answer may arrive after its machine is gone from the list; it still
/// ends the wait, and touches no row.
#[test]
fn an_answer_for_a_machine_that_left_still_ends_the_wait() {
    let mut f = with_remote(StubTower::default());
    let first = with_one_out(&mut f);
    f.feed(Event::Remote(Vec::new()));
    f.feed(first.run(f.tower.as_ref()));
    assert!(f.app.err().is_none(), "{:?}", f.app.err());
    f.feed(Event::Remote(machines()));
    f.tap("j");
    assert!(
        !screen(&f, 120, 80).contains("↻ starting…"),
        "the fresh read is what is shown"
    );
    assert!(matches!(f.press("L"), tower_ui::Action::Remote(_)));
}

/// Round trip through the driver's contract: whatever request goes out, the
/// event that comes back names the same machine, agent and action.
#[test]
fn a_request_reports_back_what_was_asked() {
    let mut f = fixture_with(StubTower::default());
    with_session(&mut f, Vec::new());
    f.tap("j");
    let requests = [
        RemoteAction::LoopStart,
        RemoteAction::LoopStop,
        RemoteAction::Interrupt,
        RemoteAction::Send("hi".into()),
        RemoteAction::Unschedule(vec!["a".into(), "b".into()]),
    ];
    let m = machines().remove(0);
    for action in requests {
        let request = tower_ui::RemoteRequest {
            machine: m.clone(),
            agent: m.agents[0].clone(),
            action: action.clone(),
        };
        for result in [Ok("done".to_string()), Err("no".to_string())] {
            let ev = request.clone().done(result.clone());
            let Event::RemoteDone {
                label,
                id,
                action: got,
                result: r,
            } = ev
            else {
                panic!("done gives RemoteDone");
            };
            assert_eq!((label.as_str(), id.as_str()), ("gpu", "claude-w2:p6"));
            assert_eq!(got, action);
            assert_eq!(r, result);
        }
    }
}

fn click(f: &mut Fixture, text: &str) {
    let row = line_of(f, text);
    assert_eq!(
        f.app.on_event(mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row,
        )),
        tower_ui::Action::Nothing
    );
}

#[test]
fn a_click_on_a_machines_band_folds_its_rows_and_another_opens_them() {
    let mut f = fixture();
    f.feed(Event::Remote(machines()));
    let v = screen(&f, 200, 60);
    assert!(v.contains("w2:p6") && v.contains("w2:p7"), "{v}");
    assert!(v.contains(" ▼ @gpu"), "an open band says it folds:\n{v}");
    click(&mut f, " @gpu");
    let v = screen(&f, 200, 60);
    assert!(!v.contains("w2:p6") && !v.contains("├─── diskann"), "{v}");
    assert!(
        v.contains(" ▶ @gpu (2)"),
        "the band says what it holds:\n{v}"
    );
    assert!(
        !v.lines().any(|l| l.contains("@gpu") && l.contains('▸')),
        "the folded mark is not the selection bar's:\n{v}"
    );
    assert!(f.app.remote_agents().is_empty(), "no folded row to land on");
    // `j` from the last local row has nowhere to go now.
    for _ in 0..f.app.agents().len() + 2 {
        f.tap("j");
    }
    assert!(!f.app.remote_mode());

    click(&mut f, " @gpu");
    let v = screen(&f, 200, 60);
    assert!(v.contains("w2:p6") && v.contains("w2:p7"), "{v}");
    assert!(v.contains(" ▼ @gpu (2)"), "{v}");
}

#[test]
fn folding_the_machine_the_cursor_is_on_takes_it_back_to_the_local_rows() {
    let mut f = fixture();
    f.feed(Event::Remote(machines()));
    click(&mut f, "w2:p7");
    assert!(f.app.remote_mode());
    click(&mut f, " @gpu");
    assert!(!f.app.remote_mode(), "nothing left to be on over there");
    assert!(f.app.selected().is_some());
}

#[test]
fn folding_local_leaves_its_band_and_the_cursor_goes_to_the_machines() {
    let mut f = fixture();
    let local = f.app.agents().len();
    f.feed(Event::Remote(machines()));
    click(&mut f, " @local");
    let v = screen(&f, 200, 60);
    assert!(
        v.contains(&format!(" ▶ @local ({local})")),
        "every local agent under one band:\n{v}"
    );
    assert!(f.app.agents().is_empty());
    assert!(!v.contains("check the handoff doc"), "{v}");
    assert!(
        f.app.remote_mode(),
        "the cursor moved to what is still drawn"
    );
    f.tap("k");
    assert!(f.app.remote_mode(), "no local row to go back to");
    // The header still counts every agent.
    assert!(v.lines().next().unwrap().contains("new"), "{v}");

    click(&mut f, " @local");
    assert_eq!(f.app.agents().len(), local);
    assert!(screen(&f, 200, 60).contains("check the handoff doc"));
}

/// `@local` has a band only beside a saved machine. Folded and then left
/// alone — the machine goes away — the list must not vanish with nothing to
/// click to bring it back.
#[test]
fn local_stays_open_while_there_is_no_band_to_click() {
    let mut f = fixture();
    let local = f.app.agents().len();
    f.feed(Event::Remote(machines()));
    click(&mut f, " @local");
    assert!(f.app.agents().is_empty());
    f.feed(Event::Remote(Vec::new()));
    assert_eq!(f.app.agents().len(), local, "unfolded with its band gone");
    assert!(!screen(&f, 200, 60).contains("@local"));
    // And folded again once the band is back, as it was left.
    f.feed(Event::Remote(machines()));
    assert!(f.app.agents().is_empty());
}

#[test]
fn a_machine_that_did_not_answer_is_not_a_band_to_fold() {
    let mut f = fixture();
    f.feed(Event::Remote(vec![RemoteMachine {
        label: "down".into(),
        error: Some("not reachable".into()),
        ..Default::default()
    }]));
    let v = screen(&f, 200, 60);
    assert!(
        v.contains("   @down") && !v.contains("▼ @down"),
        "no mark on a band that does not fold:\n{v}"
    );
    click(&mut f, " @down");
    assert!(!f.app.is_folded("down"));
}

/// Three machines of fifteen rows each: a list far taller than the screen.
fn many_machines() -> Vec<RemoteMachine> {
    ["a", "b", "c"]
        .iter()
        .map(|m| {
            let mut x = machines().remove(0);
            x.label = m.to_string();
            let row = x.agents[0].clone();
            x.agents = (1..=15)
                .map(|i| RemoteAgent {
                    pane_id: format!("w2:p{i}"),
                    id: format!("{m}-{i}"),
                    ..row.clone()
                })
                .collect();
            x
        })
        .collect()
}

/// Whatever band is clicked, and wherever the cursor was — on a local row,
/// deep in another machine, or back from the machines with the remote
/// cursor left far down — the band is still on the screen after it folds
/// and after it opens again, and the cursor sits beside it.
#[test]
fn a_band_clicked_to_fold_or_open_stays_on_the_screen() {
    let (w, h) = (120u16, 30u16);
    for band in ["local", "a", "b", "c"] {
        for steps in [0usize, 10, 20, 35, 47] {
            for back in [false, true] {
                let mut f = fixture();
                f.feed(Event::Remote(many_machines()));
                f.app.set_size(w, h);
                for _ in 0..steps {
                    f.tap("j");
                }
                if back {
                    f.tap("esc");
                }
                let name = format!(" @{band}");
                let shown = |f: &Fixture| {
                    screen(f, w, h)
                        .lines()
                        .any(|l| l.contains(&format!("{name} ")) || l.ends_with(&name))
                };
                if !shown(&f) {
                    continue; // not on the screen to be clicked from here
                }
                for turn in ["folds", "opens"] {
                    click(&mut f, &name);
                    assert!(
                        shown(&f),
                        "@{band} {turn} after {steps} j (esc {back}) and scrolls away:\n{}",
                        screen(&f, w, h)
                    );
                }
                assert!(!f.app.is_folded(band));
            }
        }
    }
}

/// A click on a band leaves the selection where it is while that row is
/// still drawn. One folded away goes beside the band: the row after it, or
/// the one before when nothing follows.
#[test]
fn folding_keeps_the_selection_unless_it_is_folded_away() {
    let mut f = fixture();
    f.feed(Event::Remote(many_machines()));
    let at = |f: &Fixture| {
        f.app
            .selected_remote()
            .map(|(m, a)| (m.label.clone(), a.id.clone()))
    };
    let row = |m: &str, id: &str| Some((m.to_string(), id.to_string()));
    let local = f.app.selected().map(|a| a.id.clone());
    for _ in 0..2 {
        click(&mut f, " @b");
        assert!(!f.app.remote_mode());
        assert_eq!(
            f.app.selected().map(|a| a.id.clone()),
            local,
            "a local row stays"
        );
    }
    click(&mut f, "w2:p3");
    assert_eq!(at(&f), row("a", "a-3"));
    for _ in 0..2 {
        click(&mut f, " @b");
        assert_eq!(at(&f), row("a", "a-3"), "a row of another machine stays");
    }
    click(&mut f, " @a");
    assert_eq!(
        at(&f),
        row("b", "b-1"),
        "folded away: the row after the band"
    );
    click(&mut f, " @a");
    assert_eq!(
        at(&f),
        row("b", "b-1"),
        "and it stays there as the band opens"
    );
    f.tap("esc");
    click(&mut f, " @local");
    assert_eq!(
        at(&f),
        row("a", "a-1"),
        "local folded away: the first machine row"
    );
    click(&mut f, " @local");
    assert_eq!(at(&f), row("a", "a-1"), "and kept as local opens");
}

/// The band clicked is drawn on the row it was clicked on, folded and
/// opened, though the list drawn around the cursor would have moved it — and
/// the selection stays. A key lets go: the list follows the cursor again.
#[test]
fn a_clicked_band_holds_its_row_until_a_key() {
    let (w, h) = (120u16, 50u16);
    let mut f = fixture();
    // Five machines, so the list still overflows with one of them folded.
    let mut machines = many_machines();
    for label in ["d", "e"] {
        let mut m = machines[0].clone();
        m.label = label.into();
        for a in &mut m.agents {
            a.id = a.id.replacen('a', label, 1);
        }
        machines.push(m);
    }
    f.feed(Event::Remote(machines));
    f.app.set_size(w, h);
    // Down to `c-1`, with `@b` above it on the screen.
    for _ in 0..32 {
        f.tap("j");
    }
    let band_row = |f: &Fixture| {
        screen(f, w, h)
            .lines()
            .position(|l| l.contains(" @b ") || l.ends_with(" @b"))
    };
    let selected = |f: &Fixture| {
        f.app
            .selected_remote()
            .map(|(m, a)| (m.label.clone(), a.id.clone()))
    };
    assert_eq!(selected(&f), Some(("c".to_string(), "c-1".to_string())));
    let before = band_row(&f).expect("@b on the screen");
    for turn in ["folds", "opens"] {
        click(&mut f, " @b");
        assert_eq!(
            band_row(&f),
            Some(before),
            "@b {turn}:\n{}",
            screen(&f, w, h)
        );
        assert_eq!(selected(&f), Some(("c".to_string(), "c-1".to_string())));
    }
    f.tap("j");
    let v = screen(&f, w, h);
    assert!(
        v.lines().any(|l| l.starts_with('▸')),
        "the cursor is followed again:\n{v}"
    );
}

fn ubuntu() -> tower_os::host::System {
    tower_os::host::System {
        os: "Ubuntu 26.04".into(),
        arch: "aarch64".into(),
        tower: "v0.57.4".into(),
    }
}

/// Each band says how many agents it has, then what the machine runs; a
/// folded one keeps its count, and `@local` says what this machine runs.
#[test]
fn a_band_counts_its_agents_and_says_what_the_machine_runs() {
    let mut f = fixture();
    let local = f.app.agents().len();
    let mut m = machines();
    m[0].system = Some(ubuntu());
    f.feed(Event::Remote(m));
    let v = screen(&f, 200, 60);
    assert!(
        v.contains(" ▼ @gpu (2) · Ubuntu 26.04 arm64 (herdr-tower v0.57.4)"),
        "{v}"
    );
    let ours = f.app.local_system().label();
    assert!(ours.ends_with("(herdr-tower v-test)"), "{ours}");
    assert!(
        v.contains(&format!(" ▼ @local ({local})")) && v.contains(&ours),
        "{v}"
    );
    click(&mut f, " @gpu");
    let v = screen(&f, 200, 60);
    assert!(
        v.contains(" ▶ @gpu (2) · Ubuntu 26.04 arm64"),
        "folded, the count stays:\n{v}"
    );
    // Opened again, a filter counts what it leaves.
    click(&mut f, " @gpu");
    f.tap("/");
    for c in "w2:p7".chars() {
        f.tap(&c.to_string());
    }
    let v = screen(&f, 200, 60);
    assert!(v.contains(" ▼ @gpu (1)"), "{v}");
}

/// A machine whose herdr-tower is too old to say what it runs shows its
/// count alone; one that stops answering keeps what it said last.
#[test]
fn an_old_or_lost_machine_says_what_it_can() {
    let mut f = with_remote(StubTower::default());
    f.feed(Event::Remote(machines()));
    let v = screen(&f, 200, 60);
    let band = v.lines().find(|l| l.contains("@gpu")).unwrap();
    assert!(
        !band.contains("herdr-tower") && band.contains("@gpu (2)"),
        "{band}"
    );

    let mut m = machines();
    m[0].system = Some(ubuntu());
    f.feed(Event::Remote(m));
    f.feed(Event::Remote(stale_machine()));
    let v = screen(&f, 200, 80);
    assert!(
        v.contains(
            "@gpu (2) · Ubuntu 26.04 arm64 (herdr-tower v0.57.4) · not reachable · last read"
        ),
        "{v}"
    );
}

/// A machine whose herdr-tower could not be read says why on its band, and
/// its rows are still there.
#[test]
fn a_band_says_why_its_herdr_tower_could_not_be_read() {
    let mut f = fixture();
    let mut m = machines();
    m[0].tower_trouble = Some("herdr-tower not installed".into());
    f.feed(Event::Remote(m));
    let v = screen(&f, 200, 60);
    assert!(v.contains(" ▼ @gpu (2) · herdr-tower not installed"), "{v}");
    assert!(v.contains("w2:p6"), "{v}");
}

/// A machine with no agents is still a band, carrying its load and what it
/// runs: nothing under it to fold. A filter that matches nothing on it
/// leaves it out.
#[test]
fn a_machine_without_agents_is_a_band_with_its_load() {
    let mut f = fixture();
    f.feed(Event::Remote(vec![RemoteMachine {
        label: "desk".into(),
        target: "desk".into(),
        memory: Some(tower_os::host::Memory {
            used: 61,
            total: 100,
        }),
        system: Some(ubuntu()),
        ..Default::default()
    }]));
    let v = screen(&f, 200, 60);
    let band = v.lines().find(|l| l.contains("@desk")).unwrap();
    assert!(
        band.starts_with("   @desk ")
            && band.contains("mem ■■■■■  61%")
            && band.contains("· Ubuntu 26.04 arm64"),
        "{band}"
    );
    assert!(v.contains(" ▼ @local"), "the machines are roots:\n{v}");
    click(&mut f, " @desk");
    assert!(!f.app.is_folded("desk"), "nothing to fold");
    f.tap("/");
    for c in "nowhere".chars() {
        f.tap(&c.to_string());
    }
    assert!(!screen(&f, 200, 60).contains("@desk"));
}

fn unseen(pane: &str, workspace: &str, other_user: bool) -> tower_ui::UnseenPane {
    tower_ui::UnseenPane {
        workspace: workspace.into(),
        pane_id: pane.into(),
        title: "Claude Code".into(),
        tab: String::new(),
        other_user,
    }
}

/// A pane that looks like an agent but that herdr there does not list is a
/// dim row under its workspace, after that workspace's agents, saying so —
/// and why, when herdr could not read what runs there. The cursor never
/// lands on it and the band does not count it.
#[test]
fn a_pane_herdr_does_not_see_as_an_agent_is_a_dim_row_never_selected() {
    let mut f = fixture();
    let mut m = machines();
    m[0].unseen = vec![
        unseen("w2:p9", "diskann", true),
        unseen("w3:p1", "[w3]~", false),
    ];
    f.feed(Event::Remote(m));
    let v = screen(&f, 200, 60);
    let lines: Vec<&str> = v.lines().collect();
    let at = |pane: &str| lines.iter().position(|l| l.contains(pane)).unwrap();
    assert_eq!(
        at("w2:p9"),
        at("w2:p7") + 1,
        "after its workspace's agents:\n{v}"
    );
    assert!(lines[at("w2:p7")].contains("├──── "), "{v}");
    assert!(
        lines[at("w2:p9")].contains("└──── ? unseen")
            && lines[at("w2:p9")].contains("Claude Code")
            && lines[at("w2:p9")].ends_with("not an agent to herdr — under sudo/su?"),
        "{v}"
    );
    assert!(v.contains("└─── [w3]~"), "its own workspace, last:\n{v}");
    assert!(
        lines[at("w3:p1")].ends_with("not an agent to herdr"),
        "no guess without a reason:\n{v}"
    );
    assert!(v.contains(" ▼ @gpu (2)"), "agents only in the count:\n{v}");
    let (w, h) = f.app.size();
    let drawn = screen_lines(&f.app, w.into(), h.into());
    let row = drawn
        .iter()
        .find(|l| line_text(l).contains("w2:p9"))
        .unwrap();
    let dead = Some(ratatui::style::Color::Rgb(0x55, 0x55, 0x55));
    assert!(
        row.spans
            .iter()
            .filter(|s| s.content.contains("unseen") || s.content.contains("w2:p9"))
            .all(|s| s.style.fg == dead),
        "dim: {row:?}"
    );

    assert_eq!(f.app.remote_agents().len(), 2, "only agents to land on");
    click(&mut f, "w2:p9");
    assert!(!f.app.remote_mode(), "a click on it selects nothing");
}

/// The filter reaches unseen panes, and keeps their machine for them; a
/// machine that stops answering keeps them with its agents.
#[test]
fn unseen_panes_are_filtered_and_kept_like_agents() {
    let mut f = fixture();
    let mut m = machines();
    m[0].unseen = vec![unseen("w3:p1", "[w3]~", true)];
    f.feed(Event::Remote(m));
    f.tap("/");
    for c in "code".chars() {
        f.tap(&c.to_string());
    }
    let v = screen(&f, 200, 60);
    assert!(v.contains("w3:p1") && !v.contains("w2:p6"), "{v}");
    assert!(
        v.contains(" ▼ @gpu ") && !v.contains("@gpu ("),
        "no agents left to count, a row to fold:\n{v}"
    );
    f.tap("esc");
    f.tap("esc");

    let mut m = stale_machine();
    m[0].unseen.clear();
    f.feed(Event::Remote(m));
    let v = screen(&f, 200, 80);
    assert!(
        v.contains("not reachable · last read") && v.contains("w3:p1"),
        "{v}"
    );
}
