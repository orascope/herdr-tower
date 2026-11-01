// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What each key does, against a backend that only remembers.
//!
//! A key press either finishes its work against the stub backend or hands back
//! an `Action` for the driver; the model is mutated in place, and each test
//! checks both what was done and what was asked for.

mod common;

use std::collections::{HashMap, HashSet};

use common::*;
use tower_state::{AgentState, DaemonBackend, DaemonSnapshot, LoopStore, DEFAULT_LOOP_MESSAGE};
use tower_ui::{Action, Event, LocalInfo, Pending, Screen};

#[test]
fn the_cursor_stops_at_both_ends() {
    let mut f = fixture();
    f.tap("k");
    assert_eq!(f.app.cursor(), 0, "up from the top stays");
    f.tap("j");
    f.tap("j");
    assert_eq!(f.app.cursor(), 1, "down past the end stays");
}

/// `L` offers the default message as a dim hint — not typed text, so `→`
/// takes it before `enter` arms it.
#[test]
fn l_offers_the_default_message_then_arms_only_the_selected_session() {
    let mut f = fixture();
    let mut selected_agent = agent("claude-loop", "claude", "z-ai", AgentState::DoneUnread);
    selected_agent.session_id = "conversation-loop".into();
    f.records(vec![selected_agent]);
    let selected = f.app.selected().unwrap().id.clone();

    assert_eq!(f.press("L"), Action::Nothing);
    assert_eq!(f.app.input().unwrap().value(), "", "a hint is not input");
    assert_eq!(f.app.input().unwrap().hint(), DEFAULT_LOOP_MESSAGE);
    assert_eq!(f.app.loop_input_idle_for().unwrap().as_secs(), 600);
    assert!(
        LoopStore::new(f.store.dir())
            .unwrap()
            .list()
            .unwrap()
            .is_empty(),
        "opening the composer is not consent to arm"
    );

    f.tap("right");
    f.tap("enter");
    let loops = LoopStore::new(f.store.dir()).unwrap().list().unwrap();
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0].agent_id, selected);
    assert_eq!(loops[0].idle_for_secs, 600);
    assert_eq!(loops[0].max_turns, None);
    assert_eq!(loops[0].message, DEFAULT_LOOP_MESSAGE);
    assert_eq!(f.tower.loop_starts.lock().unwrap().len(), 1);
    assert!(f.app.notice().contains("168h"));

    f.tap("L");
    assert!(LoopStore::new(f.store.dir())
        .unwrap()
        .list()
        .unwrap()
        .is_empty());
    assert!(f.app.notice().contains("off"));
}

#[test]
fn turning_a_loop_off_keeps_its_message_for_the_next_l_composer() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "z-ai", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    f.records(vec![a]);

    f.tap("L");
    // Typing over the hint replaces it: nothing to delete first.
    for c in "이전 감독 지시를 그대로 기억".chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
    f.tap("enter");
    f.tap("L");
    assert!(LoopStore::new(f.store.dir())
        .unwrap()
        .list()
        .unwrap()
        .is_empty());

    f.tap("L");
    assert_eq!(
        f.app.input().unwrap().hint(),
        "이전 감독 지시를 그대로 기억",
        "offered again, dim"
    );
}

#[test]
fn tab_cycles_the_loop_presets_and_backtab_reverses_them() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    f.records(vec![a]);

    f.tap("L");
    for (key, seconds) in [
        ("tab", 1_800),
        ("tab", 3_600),
        ("tab", 300),
        ("tab", 600),
        ("backtab", 300),
    ] {
        f.tap(key);
        assert_eq!(f.app.loop_input_idle_for().unwrap().as_secs(), seconds);
    }
    f.tap("right");
    f.tap("enter");
    let loops = LoopStore::new(f.store.dir()).unwrap().list().unwrap();
    assert_eq!(loops[0].idle_for_secs, 300);
    assert!(f.app.notice().contains("5m idle"));
}

/// #129: the pane's `loop_control` token is cleared before the lease is
/// armed, so a `done` left over from earlier cannot stop the new loop.
#[test]
fn the_control_token_is_cleared_before_the_loop_is_armed() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    f.records(vec![a]);
    *f.tower.loop_dir.lock().unwrap() = Some(f.store.dir().to_path_buf());

    f.tap("L");
    f.tap("right");
    f.tap("enter");
    assert_eq!(
        LoopStore::new(f.store.dir()).unwrap().list().unwrap().len(),
        1,
        "armed"
    );
    assert_eq!(
        *f.tower.prepared_armed.lock().unwrap(),
        [false],
        "cleared once, while nothing was armed yet"
    );
}

#[test]
fn the_loop_message_is_editable_and_escape_does_not_arm_it() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    f.records(vec![a]);

    f.tap("L");
    f.tap("end");
    for c in "\n이 세션만 확인하세요.".chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
    f.tap("enter");
    let loops = LoopStore::new(f.store.dir()).unwrap().list().unwrap();
    assert!(loops[0].message.ends_with("이 세션만 확인하세요."));

    f.tap("L");
    assert!(LoopStore::new(f.store.dir())
        .unwrap()
        .list()
        .unwrap()
        .is_empty());
    f.tap("L");
    f.tap("esc");
    assert!(f.app.input().is_none());
    assert!(LoopStore::new(f.store.dir())
        .unwrap()
        .list()
        .unwrap()
        .is_empty());
}

#[test]
fn an_identityless_agent_cannot_receive_a_lease_that_might_follow_pane_reuse() {
    let mut f = fixture();
    f.tap("L");
    assert!(f.app.err().unwrap_or_default().contains("session id"));
    assert!(LoopStore::new(f.store.dir())
        .unwrap()
        .list()
        .unwrap()
        .is_empty());
}

#[test]
fn a_session_identity_change_while_the_loop_composer_is_open_refuses_the_lease() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-one".into();
    f.records(vec![a.clone()]);
    f.tap("L");
    f.tap("right");

    a.session_id = "conversation-two".into();
    f.records(vec![a]);
    f.tap("enter");

    assert!(f.app.err().unwrap_or_default().contains("identity changed"));
    assert!(LoopStore::new(f.store.dir())
        .unwrap()
        .list()
        .unwrap()
        .is_empty());
}

#[test]
fn enter_marks_a_completion_read_and_moves_the_view() {
    let mut f = fixture();
    f.tap("j"); // the DONE_UNREAD row
    assert_eq!(
        f.press("enter"),
        Action::Quit,
        "the tower gets out of the way"
    );
    assert_eq!(
        f.store.load("claude-3").unwrap().state,
        AgentState::Idle,
        "jumping is reading"
    );
    assert_eq!(f.tower.focused(), vec!["p-claude-3"]);
}

#[test]
fn navigation_routes_a_named_backend_by_its_namespace() {
    let mut f = fixture();
    let mut a = agent("codex-named", "codex", "lab", AgentState::Idle);
    a.pane.backend = "b0123456789abcdef".into();
    f.records(vec![a]);

    f.tap("enter");
    assert_eq!(f.tower.focused(), vec!["b0123456789abcdef:p-codex-named"]);
}

/// A tower in a pane of its own is the dock, reached by clicking its workspace.
/// Quitting after the jump would leave a bare shell for the next click to find.
#[test]
fn enter_from_a_docked_tower_moves_the_view_and_keeps_the_tower() {
    let mut f = fixture_with(StubTower {
        self_pane: Some("p-dock".into()),
        ..Default::default()
    });
    f.records(vec![agent(
        "claude-3",
        "claude",
        "ai",
        AgentState::DoneUnread,
    )]);
    assert_eq!(f.press("enter"), Action::Refresh, "the tower stays");
    assert_eq!(f.tower.focused(), vec!["p-claude-3"]);
    assert_eq!(
        f.store.load("claude-3").unwrap().state,
        AgentState::Idle,
        "jumping is still reading"
    );
}

/// A failed jump from the dock is reported the same way as from a popup.
#[test]
fn a_failed_jump_from_the_dock_keeps_the_completion_unread() {
    let mut f = fixture_with(StubTower {
        self_pane: Some("p-dock".into()),
        focus_fails: true,
        ..Default::default()
    });
    f.records(vec![agent(
        "claude-3",
        "claude",
        "ai",
        AgentState::DoneUnread,
    )]);
    assert_eq!(f.press("enter"), Action::Refresh);
    assert_eq!(
        f.store.load("claude-3").unwrap().state,
        AgentState::DoneUnread
    );
    assert!(f.app.err().is_some(), "the failure is on screen");
}

/// Outside the backend, moving the view moves *somebody else's* screen — the
/// ssh incident. The terminal attaches instead, and the tower survives the
/// detach.
#[test]
fn enter_outside_the_backend_attaches_instead_of_moving_someone_elses_view() {
    let mut f = fixture_with(StubTower {
        inside: false,
        ..Default::default()
    });
    f.records(vec![agent(
        "claude-3",
        "claude",
        "ai",
        AgentState::DoneUnread,
    )]);
    let action = f.press("enter");
    assert_eq!(
        action,
        Action::Run {
            argv: vec!["herdr".into(), "attach".into()],
            // Both carried for the driver to do once the attach has actually
            // run. A command that cannot start must not clear the completion —
            // and must not move the view of every other client on the session
            // either.
            focus: Some("p-claude-3".to_string()),
            seen: Some("claude-3".to_string()),
        },
    );
    assert!(
        f.tower.focused().is_empty(),
        "no view was switched by building the command"
    );
    assert_eq!(
        f.store.load("claude-3").unwrap().state,
        AgentState::DoneUnread,
        "still unread until the child runs"
    );
}

#[test]
fn o_marks_read_without_moving() {
    let mut f = fixture();
    f.tap("j");
    assert_eq!(f.press("o"), Action::Refresh);
    assert_eq!(f.store.load("claude-3").unwrap().state, AgentState::Idle);
    assert!(f.tower.focused().is_empty());
}

#[test]
fn q_quits() {
    let mut f = fixture();
    assert_eq!(f.press("q"), Action::Quit);
}

/// A text field left to itself would swallow ctrl+c, and the only way out
/// would be knowing that esc worked.
#[test]
fn ctrl_c_quits_even_from_the_broadcast_field() {
    let mut f = fixture();
    f.tap("B");
    assert!(f.app.input().is_some());
    assert_eq!(f.press("ctrl+c"), Action::Quit);
}

/// Guidance about the last thing is stale the moment something else happens.
#[test]
fn a_notice_is_cleared_by_the_next_key() {
    let mut f = fixture();
    f.feed(Event::Notice(Ok("something happened".into())));
    assert_eq!(f.app.notice(), "something happened");
    f.tap("j");
    assert_eq!(f.app.notice(), "", "moving the cursor clears it");
}

#[test]
fn dead_rows_stay_in_the_store_but_not_in_the_tower() {
    let mut f = fixture();
    let mut dead = agent("claude-20", "claude", "work", AgentState::Dead);
    dead.session_id = "sid-20".into();
    dead.cwd = "/tmp/w".into();
    let live = agent("claude-9", "claude", "live", AgentState::Waiting);
    f.records(vec![dead, live]);

    assert_eq!(f.app.agents().len(), 1);
    assert_eq!(f.app.agents()[0].id, "claude-9");
    assert!(
        f.store.load("claude-20").is_ok(),
        "the explicit resume and restore commands still need the recovery record"
    );
}

/// Workspace, then tab, then herdr's own pane count — and an agent's state
/// has no say, so a row stays where it is when its agent starts or ends a
/// turn.
#[test]
fn the_tower_orders_workspaces_then_tabs_then_panes_whatever_their_state() {
    let mut f = fixture();
    let place = |id: &str, ws: &str, window: i64, pane: &str, state| {
        let mut a = agent(id, "claude", ws, state);
        a.pane.window = window;
        a.pane.pane_id = pane.into();
        a
    };
    let states = [
        AgentState::Waiting,
        AgentState::Idle,
        AgentState::Working,
        AgentState::DoneUnread,
        AgentState::Waiting,
        AgentState::Idle,
    ];
    let records = |shift: usize| {
        let st = |i: usize| states[(i + shift) % states.len()];
        vec![
            place("z-1", "zeta", 1, "w9:p2", st(0)),
            place("a-tab2", "alpha", 2, "w2:p3", st(1)),
            place("a-tab1-pJ0", "alpha", 1, "w2:pJ0", st(2)),
            place("a-tab1-p9", "alpha", 1, "w2:p9", st(3)),
            place("a-tab1-pJZ", "alpha", 1, "w2:pJZ", st(4)),
            place("a-tab1-p1B", "alpha", 1, "w2:p1B", st(5)),
        ]
    };
    let want = [
        "a-tab1-p9",
        "a-tab1-p1B",
        "a-tab1-pJZ",
        "a-tab1-pJ0",
        "a-tab2",
        "z-1",
    ];
    for shift in 0..states.len() {
        f.records(records(shift));
        let ids: Vec<&str> = f.app.agents().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, want, "states shifted by {shift}");
    }
}

/// Property: the order is the agents' places alone, so however the records
/// arrive — any rotation or reversal of them — the list comes out the same,
/// and reading it again changes nothing.
#[test]
fn the_order_does_not_depend_on_the_order_the_records_arrive_in() {
    let mut f = fixture();
    let place = |id: &str, ws: &str, window: i64, pane: &str| {
        let mut a = agent(id, "claude", ws, AgentState::Idle);
        a.pane.window = window;
        a.pane.pane_id = pane.into();
        a
    };
    let records = vec![
        place("b-2-p3", "beta", 2, "w3:p3"),
        place("a-1-p0", "alpha", 1, "w2:p0"),
        place("a-1-p11", "alpha", 1, "w2:p11"),
        place("a-1-pZ", "alpha", 1, "w2:pZ"),
        place("b-1-p9", "beta", 1, "w3:p9"),
        place("a-3-p2", "alpha", 3, "w2:p2"),
        place("a-1-p1", "alpha", 1, "w2:p1"),
    ];
    let want = [
        "a-1-p1", "a-1-pZ", "a-1-p0", "a-1-p11", "a-3-p2", "b-1-p9", "b-2-p3",
    ];
    for turn in 0..records.len() * 2 {
        let mut arrived = records.clone();
        arrived.rotate_left(turn % records.len());
        if turn >= records.len() {
            arrived.reverse();
        }
        f.records(arrived);
        let ids: Vec<&str> = f.app.agents().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, want, "arrival {turn}");
    }
}

// ── broadcast ────────────────────────────────────────────────────────────

/// Sidebar moves beat names. Legacy labels must not split a group,
/// and a missing heartbeat keeps the last order, including backend identity.
#[test]
fn workspace_order_follows_sidebar_moves_and_survives_a_missing_heartbeat() {
    let mut f = fixture();
    let mut z = agent("z-root", "codex", "[wZ]zeta", AgentState::Idle);
    z.pane.workspace_id = "wZ".into();
    let mut worker = agent("z-worker", "codex", "zeta", AgentState::Waiting);
    worker.pane.workspace_id = "wZ".into();
    let mut a = agent("a", "codex", "alpha", AgentState::Waiting);
    a.pane.workspace_id = "w2".into();
    let mut remote = agent("remote", "codex", "alpha", AgentState::Waiting);
    remote.pane.workspace_id = "b0123456789abcdef:w2".into();
    remote.pane.backend = "b0123456789abcdef".into();
    let agents = vec![a, remote, z, worker];
    let snapshot = |order: Vec<String>| DaemonSnapshot {
        pid: 42,
        version: "v-test".into(),
        started_at: t0(),
        updated_at: t0(),
        connected: true,
        error: String::new(),
        tab_roots: Vec::new(),
        workspace_order: order,
        workspace_display_labels: Default::default(),
        parents: Default::default(),
        latest_release: String::new(),
        backends: Vec::new(),
    };
    let event = |daemon: Option<tower_state::DaemonSnapshot>| Event::Records {
        agents: agents.clone(),
        branches: HashMap::new(),
        tab_roots: Default::default(),
        parents: Default::default(),
        daemon: daemon.map(Box::new),
        now: t0(),
    };
    f.feed(event(Some(snapshot(vec![
        "wZ".into(),
        "b0123456789abcdef:w2".into(),
        "w2".into(),
    ]))));
    let ids = |app: &tower_ui::App| {
        app.agents()
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&f.app), ["z-root", "z-worker", "remote", "a"]);
    assert!(f.app.agents()[..2].iter().all(|a| a.pane.session == "zeta"));
    f.feed(event(None));
    assert_eq!(ids(&f.app), ["z-root", "z-worker", "remote", "a"]);
    f.feed(event(Some(snapshot(vec![
        "w2".into(),
        "wZ".into(),
        "b0123456789abcdef:w2".into(),
    ]))));
    assert_eq!(ids(&f.app), ["a", "z-root", "z-worker", "remote"]);
}

#[test]
fn b_opens_the_broadcast_field() {
    let mut f = fixture();
    f.tap("B");
    assert!(f.app.input().is_some());
}

/// The field counts characters and can edit mid-string.
#[test]
fn the_broadcast_field_edits_at_the_cursor() {
    let mut f = fixture();
    f.tap("B");
    for c in "헬로".chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
    f.tap("left");
    f.feed(Event::Key(key_event("고")));
    assert_eq!(f.app.input().unwrap().value(), "헬고로");

    f.tap("backspace");
    assert_eq!(f.app.input().unwrap().value(), "헬로");
    f.tap("esc");
    assert!(f.app.input().is_none(), "esc abandons the message");
}

#[test]
fn an_empty_broadcast_is_a_cancel() {
    let mut f = fixture();
    f.tap("B");
    f.tap("enter");
    assert!(f.app.input().is_none());
    assert!(f.app.pending().is_none());
}

#[test]
fn confirming_a_broadcast_writes_to_every_session() {
    let mut f = fixture();
    f.tap("B");
    for c in "점검".chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
    f.tap("enter");
    assert_eq!(f.app.pending(), Some(&Pending::Broadcast("점검".into())));

    assert_eq!(f.press("y"), Action::Refresh);
    let sent = f.tower.sent();
    assert_eq!(sent.len(), 2, "both sessions: {sent:?}");
    assert!(sent.iter().all(|(_, text)| text == "점검"));
    assert!(
        f.app.notice().contains("2/2"),
        "the count is the receipt: {:?}",
        f.app.notice()
    );
}

/// Wake and close are instructions *about* the session-handoff discipline.
/// Telling an agent to "resume the session" in a folder with no `SESSION.md`
/// asks it to act on a file that does not exist, and it will invent one.
#[test]
fn wake_reaches_only_folders_with_the_handoff_discipline() {
    let with = handoff_dir();
    let without = tempfile::tempdir().unwrap();
    let mut f = fixture();
    let mut a = agent("claude-1", "claude", "disciplined", AgentState::Idle);
    a.cwd = with.path().to_string_lossy().into_owned();
    let mut b = agent("claude-2", "claude", "loose", AgentState::Idle);
    b.cwd = without.path().to_string_lossy().into_owned();
    f.records(vec![a, b]);

    f.tap("W");
    assert_eq!(f.app.pending(), Some(&Pending::Wake));
    f.tap("y");

    let sent = f.tower.sent();
    assert_eq!(sent.len(), 1, "only the disciplined folder: {sent:?}");
    assert_eq!(sent[0].1, tower_ui::WAKE_MESSAGE);
    assert!(f.app.notice().contains("1/1"));

    f.tap("C");
    f.tap("y");
    assert_eq!(f.tower.sent().last().unwrap().1, tower_ui::CLOSE_MESSAGE);
}

/// herdr refuses to write to an agent sitting at an approval dialog. Counting
/// the failure without the name would leave the one session a broadcast missed
/// — which is nearly always that one — unnamed.
#[test]
fn a_refused_session_is_named() {
    let mut f = fixture_with(StubTower {
        send_fails: vec!["p-claude-7".into()],
        ..Default::default()
    });
    f.records(vec![
        agent("claude-7", "claude", "blocked-one", AgentState::Waiting),
        agent("claude-3", "claude", "fine", AgentState::Idle),
    ]);
    f.tap("B");
    f.feed(Event::Key(key_event("x")));
    f.tap("enter");
    f.tap("y");
    assert!(f.app.notice().contains("1/2"));
    assert!(
        f.app.err().unwrap_or_default().contains("blocked-one"),
        "the missed session is named: {:?}",
        f.app.err()
    );
}

/// The receipt line after an answer, as drawn.
fn receipt(f: &Fixture) -> String {
    tower_ui::view::screen_lines(&f.app, 100, 30)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .find(|l| l.contains("sent"))
        .unwrap_or_default()
}

fn armed_broadcast(text: &str) -> Fixture {
    let mut f = fixture();
    f.tap("B");
    for c in text.chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
    f.tap("enter");
    f
}

/// A yes says, on screen, that the message went and to how many.
#[test]
fn a_confirmed_broadcast_shows_it_was_sent() {
    let mut f = armed_broadcast("점검");
    f.tap("y");
    let line = receipt(&f);
    assert!(line.starts_with("✔ \"점검\" sent to 2/2"), "{line:?}");
}

/// A no says, on screen, that nothing went — not a bare "cancelled".
#[test]
fn a_declined_broadcast_shows_it_was_not_sent() {
    let mut f = armed_broadcast("점검");
    f.tap("n");
    assert!(f.tower.sent().is_empty());
    let line = receipt(&f);
    assert!(line.starts_with("✖ not sent"), "{line:?}");
    assert!(line.contains("점검"), "names what was not sent: {line:?}");
}

/// `y` under a Korean input method arrives as `ㅛ`. Right after a message
/// typed in Korean that is what the key sends, and it means yes.
#[test]
fn y_under_a_korean_input_method_confirms() {
    let mut f = armed_broadcast("점검");
    f.feed(Event::Key(key_event("ㅛ")));
    assert_eq!(f.tower.sent().len(), 2);
    assert!(f.app.notice().starts_with('✔'), "{:?}", f.app.notice());
}

/// A broadcast every target refused is not a success with a small number.
#[test]
fn a_broadcast_nobody_took_is_marked_not_sent() {
    let mut f = fixture_with(StubTower {
        send_fails: vec!["p-claude-7".into()],
        ..Default::default()
    });
    f.records(vec![agent(
        "claude-7",
        "claude",
        "blocked",
        AgentState::Waiting,
    )]);
    f.tap("B");
    f.feed(Event::Key(key_event("x")));
    f.tap("enter");
    f.tap("y");
    assert!(f.app.notice().starts_with("✖"), "{:?}", f.app.notice());
    assert!(f.app.notice().contains("0/1"));
}

/// Wake and close decline with the same receipt.
#[test]
fn a_declined_wake_shows_it_was_not_sent() {
    let mut f = fixture();
    f.tap("W");
    f.tap("n");
    assert!(f.tower.sent().is_empty());
    assert!(
        f.app.notice().starts_with("✖ not sent"),
        "{:?}",
        f.app.notice()
    );
}

#[test]
fn lazygit_refuses_a_folder_that_is_not_a_repository() {
    let mut f = fixture_with(StubTower {
        git: false,
        ..Default::default()
    });
    f.install_tool("lazygit");
    let mut a = agent("claude-7", "claude", "notarepo", AgentState::Idle);
    a.cwd = "/w/plain".into();
    f.records(vec![a]);
    assert_eq!(f.press("g"), Action::Nothing);
    let err = f.app.err().unwrap_or_default();
    assert!(err.contains("notarepo is not a git repository"), "{err}");
}

/// No `lazygit` anywhere the search looks: the missing tool is reported
/// before the folder is even considered.
#[test]
fn a_missing_lazygit_is_said_before_the_folder_is_looked_at() {
    let mut f = fixture_with(StubTower {
        git: false,
        ..Default::default()
    });
    let mut a = agent("claude-7", "claude", "notarepo", AgentState::Idle);
    a.cwd = "/w/plain".into();
    f.records(vec![a]);
    assert_eq!(f.press("g"), Action::Nothing);
    let err = f.app.err().unwrap_or_default();
    assert!(err.contains("lazygit is not installed"), "{err}");
}

// ── events ───────────────────────────────────────────────────────────────

/// Rows within a workspace are sorted by state, so a state change can move the
/// selected row underneath a fixed index. The highlight follows the agent,
/// because `enter` moves the user's view to whichever row it names.
#[test]
fn a_refresh_keeps_the_highlight_on_the_same_agent() {
    let mut f = fixture();
    let mut first = agent("claude-3", "claude", "work", AgentState::DoneUnread);
    first.pane.pane_id = "w2:p3".into();
    let mut second = agent("claude-7", "claude", "work", AgentState::Waiting);
    second.pane.pane_id = "w2:p7".into();
    f.records(vec![second.clone(), first.clone()]);
    f.tap("j");
    assert_eq!(f.app.selected().unwrap().id, "claude-7");

    // A new pane lower in the count puts a row above the selected one.
    let mut new = agent("claude-1", "claude", "work", AgentState::Idle);
    new.pane.pane_id = "w2:p1".into();
    f.feed(Event::Records {
        agents: vec![first, second, new],
        branches: HashMap::new(),
        tab_roots: Default::default(),
        parents: Default::default(),
        daemon: None,
        now: t0(),
    });
    assert_eq!(f.app.cursor(), 2, "the row moved down");
    assert_eq!(
        f.app.selected().unwrap().id,
        "claude-7",
        "the highlight follows the agent, not the index"
    );
}

#[test]
fn a_refresh_clamps_the_cursor_when_the_agent_is_gone() {
    let mut f = fixture();
    f.tap("j");
    f.feed(Event::Records {
        agents: vec![agent(
            "claude-7",
            "claude",
            "collab-bot",
            AgentState::Waiting,
        )],
        branches: HashMap::new(),
        tab_roots: Default::default(),
        parents: Default::default(),
        daemon: None,
        now: t0(),
    });
    assert_eq!(f.app.cursor(), 0);
    assert_eq!(f.app.agents().len(), 1);
}

#[test]
fn a_fresh_supervisor_heartbeat_records_a_live_herdr_connection() {
    let mut f = fixture();
    assert!(!f.app.watching());
    let now = t0();
    let agents = f.app.agents().to_vec();
    assert_eq!(
        f.app.on_event(Event::Records {
            agents,
            branches: HashMap::new(),
            tab_roots: Default::default(),
            parents: Default::default(),
            daemon: Some(Box::new(DaemonSnapshot {
                pid: 42,
                version: "v-daemon".into(),
                started_at: now,
                updated_at: now,
                connected: true,
                error: String::new(),
                tab_roots: Vec::new(),
                workspace_order: Vec::new(),
                workspace_display_labels: Default::default(),
                parents: Default::default(),
                latest_release: String::new(),
                backends: vec![
                    DaemonBackend {
                        id: String::new(),
                        socket: "/default/herdr.sock".into(),
                        connected: true,
                        error: String::new(),
                        refreshed: false,
                    },
                    DaemonBackend {
                        id: "b0123456789abcdef".into(),
                        socket: "/sessions/lab/herdr.sock".into(),
                        connected: true,
                        error: String::new(),
                        refreshed: false,
                    },
                ],
            })),
            now,
        }),
        Action::Nothing
    );
    assert!(f.app.watching());
    assert_eq!(f.app.version(), "v-daemon");
    assert_eq!(f.app.backend_count(), 2);
}

#[test]
fn local_readings_reach_the_rows() {
    let mut f = fixture();
    let mut wired = HashMap::new();
    wired.insert("collab-bot".to_string(), "⌁".to_string());
    f.feed(Event::Local(LocalInfo {
        wired,
        ..Default::default()
    }));
    assert_eq!(
        f.app.local().wired.get("collab-bot").map(String::as_str),
        Some("⌁")
    );
}

/// A key release is not a key press. Terminals speaking the kitty keyboard
/// protocol send both, and acting on each would jump twice from a tower that
/// has already quit.
#[test]
fn a_key_release_does_nothing() {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    let mut f = fixture();
    let release = KeyEvent::new_with_kind_and_state(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
        KeyEventState::NONE,
    );
    assert_eq!(f.app.on_event(Event::Key(release)), Action::Nothing);
}

/// A warning about the machine's state outlives a passing storage error.
///
/// It goes up before the first frame — a state store shared with another
/// server — and nothing the screen does can
/// make those go away. Carried on the storage-health channel it did not last:
/// the first unreadable record replaced it, and the recovery that followed
/// cleared it, leaving the user with no sign of a condition that still holds.
#[test]
fn a_startup_warning_survives_a_storage_error_and_its_recovery() {
    let mut f = fixture();
    f.app
        .set_startup_note("the state store is shared with another server".into());
    assert_eq!(
        f.app.err(),
        Some("the state store is shared with another server")
    );

    // A transient failure speaks first, because it is the more immediate of the
    // two…
    f.feed(Event::Health(Err(
        "the state store could not be read".into()
    )));
    assert_eq!(f.app.err(), Some("the state store could not be read"));

    // …and when it clears, what was already wrong is still wrong.
    f.feed(Event::Health(Ok(())));
    assert_eq!(
        f.app.err(),
        Some("the state store is shared with another server"),
        "the startup warning was cleared by an unrelated recovery"
    );
}

/// What happens when the attach is actually running, and what does not happen
/// when it never starts.
///
/// Both halves used to be done while the command was still being built. The
/// completion was cleared for an attach that failed — the person never saw the
/// result, and the unread marker was the only thing that would have brought them
/// back to it — and the selection went out to a session every client follows, so
/// a desk on the other end of an ssh connection had its pane changed for an
/// attach that might never happen. herdr has no client-scoped selection to do
/// better than that; what is left is not doing it for nothing.
#[test]
fn an_attach_selects_and_reads_only_once_it_is_running() {
    let mut f = fixture_with(StubTower {
        inside: false,
        ..Default::default()
    });
    f.records(vec![agent(
        "claude-3",
        "claude",
        "ai",
        AgentState::DoneUnread,
    )]);

    // Pressing the key does neither.
    let action = f.press("enter");
    assert!(matches!(action, Action::Run { .. }));
    assert!(f.tower.focused().is_empty(), "nobody's view has moved");
    assert_eq!(
        f.store.load("claude-3").unwrap().state,
        AgentState::DoneUnread,
        "the completion is still unread"
    );

    // The driver, once the child is up.
    assert_eq!(f.app.attached(Some("p-claude-3"), Some("claude-3")), None);
    assert_eq!(f.tower.focused(), vec!["p-claude-3"]);
    assert_eq!(f.store.load("claude-3").unwrap().state, AgentState::Idle);
}

/// A selection that is refused keeps the completion unread, and says so.
///
/// The pane was not reached, so the result was not seen, so the marker that
/// would bring somebody back to it has to stay. And the message is the whole
/// sentence: the driver ends the child on this, rather than leaving somebody
/// attached to a pane they did not ask for with the reason stuck behind it.
#[test]
fn a_refused_selection_leaves_the_completion_unread() {
    let mut f = fixture_with(StubTower {
        inside: false,
        focus_fails: true,
        ..Default::default()
    });
    f.records(vec![agent(
        "claude-3",
        "claude",
        "ai",
        AgentState::DoneUnread,
    )]);

    assert_eq!(
        f.app.attached(Some("p-claude-3"), Some("claude-3")),
        Some("could not reach p-claude-3, so the attach was cancelled: focus refused".to_string()),
        "the driver puts this on the error line"
    );
    assert_eq!(
        f.store.load("claude-3").unwrap().state,
        AgentState::DoneUnread
    );
}

/// The wake and close instructions are named, not quoted back.
///
/// Their bytes are the agents' interface and stay in the words the agents were
/// taught — which are not the words the person typed. They pressed `W`. Echoing
/// the payload puts a phrase nobody wrote into a line that is otherwise theirs.
#[test]
fn a_built_in_instruction_is_named_when_it_is_echoed() {
    let mut f = fixture_with(StubTower::default());
    f.records(vec![agent("claude-3", "claude", "ai", AgentState::Idle)]);

    f.tap("W");
    f.tap("y");
    let notice = f.app.notice();
    assert!(notice.contains("the wake instruction"), "{notice}");
    assert!(
        !notice.contains(tower_ui::WAKE_MESSAGE),
        "the agents' own words reached the person: {notice}"
    );
}

// ── mouse ────────────────────────────────────────────────────────────────

fn mouse(kind: crossterm::event::MouseEventKind, row: u16) -> Event {
    Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column: 10,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    })
}

fn left_click(row: u16) -> Event {
    mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        row,
    )
}

/// Draws the screen, as the driver does before any click can land, and returns
/// the line an agent's row is on — found the way a person finds it.
fn line_of(f: &mut Fixture, text: &str) -> u16 {
    let (w, h) = f.app.size();
    let line = tower_ui::view::screen_lines(&f.app, w.into(), h.into())
        .iter()
        .position(|l| tower_ui::line_text(l).contains(text))
        .unwrap_or_else(|| panic!("{text:?} is not on screen")) as u16;
    f.app.remember_drawn();
    line
}

#[test]
fn a_click_selects_the_row_under_it_without_leaving() {
    let mut f = fixture();
    assert_eq!(f.app.cursor(), 0);
    let row = line_of(&mut f, "check the handoff doc");
    assert_eq!(f.app.on_event(left_click(row)), Action::Nothing);
    assert_eq!(f.app.selected().unwrap().id, "claude-3");
    assert!(f.tower.focused().is_empty(), "one click only selects");
}

fn left_click_at(f: &mut Fixture, row: u16, at: std::time::Instant) -> Action {
    let Event::Mouse(m) = left_click(row) else {
        unreachable!()
    };
    f.app.on_mouse(m, at)
}

#[test]
fn a_double_click_jumps_like_enter() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    let t = std::time::Instant::now();
    assert_eq!(left_click_at(&mut f, row, t), Action::Nothing);
    assert_eq!(
        left_click_at(&mut f, row, t + std::time::Duration::from_millis(200)),
        Action::Quit
    );
    assert_eq!(f.tower.focused(), vec!["p-claude-3"]);
    assert_eq!(
        f.store.load("claude-3").unwrap().state,
        AgentState::Idle,
        "jumping by mouse is reading too"
    );
}

/// A real mouse sends a release after every press; it must not break the pair.
#[test]
fn releases_between_the_clicks_still_make_a_double_click() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    let t = std::time::Instant::now();
    let ms = std::time::Duration::from_millis;
    let up = |f: &mut Fixture, at| {
        let Event::Mouse(m) = mouse(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            row,
        ) else {
            unreachable!()
        };
        f.app.on_mouse(m, at)
    };
    let _ = left_click_at(&mut f, row, t);
    let _ = up(&mut f, t + ms(50));
    assert_eq!(left_click_at(&mut f, row, t + ms(150)), Action::Quit);
}

/// A click elsewhere in between means the two clicks on the row were separate.
#[test]
fn a_click_elsewhere_in_between_is_not_a_double_click() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    let t = std::time::Instant::now();
    let ms = std::time::Duration::from_millis;
    let _ = left_click_at(&mut f, row, t);
    let _ = left_click_at(&mut f, 0, t + ms(100)); // the header
    assert_eq!(left_click_at(&mut f, row, t + ms(200)), Action::Nothing);
    assert!(f.tower.focused().is_empty());
}

/// So does a key or a wheel turn.
#[test]
fn a_key_or_the_wheel_in_between_is_not_a_double_click() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    let t = std::time::Instant::now();
    let ms = std::time::Duration::from_millis;

    let _ = left_click_at(&mut f, row, t);
    f.tap("r");
    assert_eq!(left_click_at(&mut f, row, t + ms(100)), Action::Nothing);

    let _ = left_click_at(&mut f, row, t + ms(1000));
    let Event::Mouse(wheel) = mouse(crossterm::event::MouseEventKind::ScrollUp, row) else {
        unreachable!()
    };
    let _ = f.app.on_mouse(wheel, t + ms(1050));
    let row = line_of(&mut f, "check the handoff doc");
    let _ = left_click_at(&mut f, row, t + ms(1100));
    assert!(f.tower.focused().is_empty());
}

/// The list can re-sort between a draw and a click. The click means the agent
/// the person saw on that line, not whichever one has moved there since.
#[test]
fn a_click_selects_the_agent_that_was_drawn_there() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    let mut moved = agent("claude-3", "claude", "a-first", AgentState::DoneUnread);
    moved.task = "check the handoff doc".into();
    let mut other = agent("claude-7", "claude", "collab-bot", AgentState::Waiting);
    other.task = "awaiting approval".into();
    f.records(vec![moved, other]); // claude-3's workspace now sorts first
    assert_ne!(
        f.app.agents()[1].id,
        "claude-3",
        "the model has moved on from the frame"
    );
    f.feed(left_click(row));
    assert_eq!(f.app.selected().unwrap().id, "claude-3");
}

/// A click on the line of an agent that has since gone does nothing.
#[test]
fn a_click_on_an_agent_that_has_gone_does_nothing() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    let mut stay = agent("claude-7", "claude", "collab-bot", AgentState::Waiting);
    stay.task = "awaiting approval".into();
    f.records(vec![stay]);
    assert_eq!(f.app.on_event(left_click(row)), Action::Nothing);
    assert_eq!(f.app.selected().unwrap().id, "claude-7");
}

/// A second click long after the first is a person clicking again, not a
/// request to leave.
#[test]
fn a_slow_second_click_only_selects() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    let t = std::time::Instant::now();
    f.feed(left_click(row));
    let late = t + tower_ui::DOUBLE_CLICK + std::time::Duration::from_millis(300);
    assert_eq!(left_click_at(&mut f, row, late), Action::Nothing);
    assert!(f.tower.focused().is_empty());
}

/// Two quick clicks on *different* rows are two selections.
#[test]
fn quick_clicks_on_two_rows_do_not_jump() {
    let mut f = fixture();
    let t = std::time::Instant::now();
    let first = line_of(&mut f, "awaiting approval");
    let second = line_of(&mut f, "check the handoff doc");
    assert_eq!(left_click_at(&mut f, first, t), Action::Nothing);
    assert_eq!(
        left_click_at(&mut f, second, t + std::time::Duration::from_millis(100)),
        Action::Nothing
    );
    assert_eq!(f.app.selected().unwrap().id, "claude-3");
    assert!(f.tower.focused().is_empty());
}

/// A triple click is a double click and then the first half of another.
#[test]
fn a_third_quick_click_does_not_jump_twice() {
    let mut f = fixture_with(StubTower {
        self_pane: Some("p-dock".into()),
        ..Default::default()
    });
    f.records(vec![agent(
        "claude-3",
        "claude",
        "ai",
        AgentState::DoneUnread,
    )]);
    let row = line_of(&mut f, "claude");
    let t = std::time::Instant::now();
    let ms = std::time::Duration::from_millis;
    let _ = left_click_at(&mut f, row, t);
    assert_eq!(left_click_at(&mut f, row, t + ms(100)), Action::Refresh);
    assert_eq!(left_click_at(&mut f, row, t + ms(200)), Action::Nothing);
    assert_eq!(f.tower.focused(), vec!["p-claude-3"]);
}

#[test]
fn a_click_on_a_rule_or_the_header_does_nothing() {
    let mut f = fixture();
    for row in [0, line_of(&mut f, "── collab-bot")] {
        assert_eq!(f.app.on_event(left_click(row)), Action::Nothing);
        assert_eq!(f.app.cursor(), 0, "row {row}");
    }
    assert!(f.tower.focused().is_empty());
}

#[test]
fn the_wheel_moves_the_selection() {
    let mut f = fixture();
    f.feed(mouse(crossterm::event::MouseEventKind::ScrollDown, 5));
    assert_eq!(f.app.cursor(), 1);
    f.feed(mouse(crossterm::event::MouseEventKind::ScrollDown, 5));
    assert_eq!(f.app.cursor(), 1, "stops at the end");
    f.feed(mouse(crossterm::event::MouseEventKind::ScrollUp, 5));
    assert_eq!(f.app.cursor(), 0);
}

/// A click must not answer a question it cannot see.
#[test]
fn a_click_is_ignored_while_a_confirmation_waits() {
    let mut f = fixture();
    let row = line_of(&mut f, "check the handoff doc");
    f.tap("W");
    assert!(f.app.pending().is_some());
    f.feed(left_click(row));
    f.feed(left_click(row));
    assert!(f.tower.focused().is_empty());
    assert!(f.app.pending().is_some(), "still waiting for y/n");
}

/// Rows further down a list longer than the screen are still hit exactly, with
/// the `↑ more` line and the repeated rule above them.
#[test]
fn a_click_lands_on_the_drawn_row_in_a_scrolled_list() {
    let mut f = fixture();
    let mut agents = Vec::new();
    for i in 0..30 {
        let mut a = agent(&format!("c-{i:02}"), "claude", "ws", AgentState::Idle);
        a.task = format!("task number {i:02}");
        a.cwd = format!("/w/{i:02}");
        agents.push(a);
    }
    f.records(agents);
    f.app.set_size(120, 20);
    for _ in 0..25 {
        f.tap("j");
    }
    let row = line_of(&mut f, "task number 22");
    f.feed(left_click(row));
    assert_eq!(f.app.selected().unwrap().task, "task number 22");
}

// ── the key guide, clicked ───────────────────────────────────────────────

/// Draws the screen and returns where `text` is in the key guide at the bottom —
/// found the way a person finds it, on the guide line and nowhere else.
fn guide_at(f: &mut Fixture, text: &str) -> (u16, u16) {
    let (w, h) = f.app.size();
    let lines: Vec<String> = tower_ui::view::screen_lines(&f.app, w.into(), h.into())
        .iter()
        .map(tower_ui::line_text)
        .collect();
    f.app.remember_drawn();
    let guide_rows: HashSet<usize> = tower_ui::view::drawn_keys(&f.app)
        .iter()
        .map(|k| k.row)
        .collect();
    let (row, line) = lines
        .iter()
        .enumerate()
        .rev()
        // Only the guide's own lines — a wrapped guide's last line can hold a
        // single entry with no ` · ` on it, and an error line under it or a
        // row above it can hold the same words.
        .filter(|(row, _)| guide_rows.contains(row))
        .find(|(_, l)| l.contains(text))
        .unwrap_or_else(|| panic!("{text:?} is not in the key guide:\n{}", lines.join("\n")));
    let byte = line.find(text).unwrap();
    let col = tower_text::width(&line[..byte]);
    (row as u16, col as u16)
}

/// Wide enough for the whole list guide, which runs past 120 columns.
fn wide_fixture() -> Fixture {
    let mut f = fixture();
    f.app.set_size(200, 40);
    f
}

fn press_at(row: u16, column: u16, kind: crossterm::event::MouseEventKind) -> Event {
    Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    })
}

/// Clicks `text` in the key guide, as the driver would after a draw.
fn click_guide(f: &mut Fixture, text: &str) -> Action {
    let (row, col) = guide_at(f, text);
    f.app.on_event(press_at(
        row,
        col,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
    ))
}

fn release(f: &mut Fixture) {
    f.feed(press_at(
        0,
        0,
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ));
}

#[test]
fn a_click_on_the_key_guide_presses_that_key() {
    let mut f = wide_fixture();
    assert_eq!(click_guide(&mut f, "r refresh"), Action::Refresh);
    release(&mut f);
    // The words after a key are part of the same button.
    assert_eq!(click_guide(&mut f, "refresh"), Action::Refresh);
}

/// The filter's key is `/` itself, which elsewhere in the guide separates
/// keys: drawn whole, and a click on it opens the field.
#[test]
fn a_click_on_slash_filter_in_the_guide_opens_the_filter() {
    let mut f = wide_fixture();
    assert_eq!(click_guide(&mut f, "/ filter"), Action::Nothing);
    assert!(f.app.filtering());
}

/// `↑↓/jk` is four keys drawn as one entry: each arrow and each letter
/// presses its own.
#[test]
fn clicking_the_arrows_and_j_and_k_in_the_guide_moves_the_selection() {
    let mut f = wide_fixture();
    assert_eq!(f.app.cursor(), 0);
    let (row, col) = guide_at(&mut f, "↑↓/jk");
    let down = crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left);
    let up = crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left);
    let click = |f: &mut Fixture, at: u16| {
        f.feed(press_at(row, col + at, down));
        f.feed(press_at(row, col + at, up));
    };
    click(&mut f, 1);
    assert_eq!(f.app.cursor(), 1, "↓ moves down");
    click(&mut f, 0);
    assert_eq!(f.app.cursor(), 0, "↑ moves up");
    click(&mut f, 3);
    assert_eq!(f.app.cursor(), 1, "j moves down");
    click(&mut f, 4);
    assert_eq!(f.app.cursor(), 0, "k moves up");
    click(&mut f, 2);
    assert_eq!(f.app.cursor(), 0, "the / presses nothing");
}

#[test]
fn a_click_between_two_keys_presses_nothing() {
    let mut f = wide_fixture();
    let (row, col) = guide_at(&mut f, " · r refresh");
    let action = f.app.on_event(press_at(
        row,
        col + 1, // the `·`
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
    ));
    assert_eq!(action, Action::Nothing);
    assert_eq!(f.app.pressed_key(), None);
}

/// Every screen has a guide, and every guide can be clicked — including the
/// way back, which is the key a mouse user most needs.
#[test]
fn the_guide_is_clickable_on_every_screen() {
    let mut f = wide_fixture();
    let _ = click_guide(&mut f, "i detail");
    assert!(matches!(f.app.screen(), Screen::Info(_)));
    let _ = click_guide(&mut f, "close");
    assert_eq!(f.app.screen(), &Screen::List);
}

/// The entry under the pointer is reversed while the button is down, so the
/// press can be seen, and drawn plainly again once it comes up.
#[test]
fn a_clicked_key_is_reversed_until_the_button_comes_up() {
    use ratatui::style::Modifier;
    let reversed = |f: &Fixture| -> Vec<String> {
        let (w, h) = f.app.size();
        tower_ui::view::screen_lines(&f.app, w.into(), h.into())
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.to_string())
            .collect()
    };
    let mut f = wide_fixture();
    assert!(reversed(&f).is_empty());

    assert_eq!(click_guide(&mut f, "refresh"), Action::Refresh);
    assert_eq!(
        reversed(&f),
        ["r", " ", "refresh"],
        "the whole entry, as one button"
    );

    release(&mut f);
    assert!(reversed(&f).is_empty(), "letting go restores it");
}

/// A key that changes the screen is followed by a different guide, where the
/// same entry is a different key — so nothing is held there.
#[test]
fn a_key_that_changes_the_screen_holds_nothing_down() {
    let mut f = wide_fixture();
    let _ = click_guide(&mut f, "i detail");
    assert!(matches!(f.app.screen(), Screen::Info(_)));
    assert_eq!(f.app.pressed_key(), None);
}

/// A release the terminal never sent must not leave a key held for good: the
/// next press lets go of it.
#[test]
fn the_next_press_lets_go_of_a_key_whose_release_was_lost() {
    let mut f = wide_fixture();
    let _ = click_guide(&mut f, "refresh");
    assert!(f.app.pressed_key().is_some());
    f.feed(left_click(0)); // the header
    assert_eq!(f.app.pressed_key(), None);
}

/// 🚨 A click must not answer a confirmation it cannot read the answer of, nor
/// type the letter it lands on into a message. `W` asks; a click on `q quit`
/// afterwards neither quits nor says yes.
#[test]
fn a_click_on_the_guide_cannot_answer_a_confirmation_or_type() {
    let mut f = wide_fixture();
    let _ = click_guide(&mut f, "W wake all");
    assert_eq!(f.app.pending(), Some(&Pending::Wake));
    release(&mut f);
    assert_eq!(click_guide(&mut f, "q quit"), Action::Nothing);
    assert_eq!(f.app.pending(), Some(&Pending::Wake), "still asking");
    f.tap("n");

    let _ = click_guide(&mut f, "B broadcast");
    assert!(f.app.input().is_some(), "the message field is open");
    release(&mut f);
    assert_eq!(click_guide(&mut f, "q quit"), Action::Nothing);
    assert_eq!(f.app.input().map(|i| i.value()), Some(String::new()));
}

/// 🚨 A guide wider than the screen wraps rather than losing its end. At 120
/// columns — a laptop's popup — `B broadcast · r refresh · q quit` used to be
/// cut off: not on screen, and not clickable either.
#[test]
fn a_narrow_screen_wraps_the_guide_and_keeps_every_key() {
    for width in [120u16, 60] {
        let mut f = fixture();
        f.app.set_size(width, 40);
        let _ = guide_at(&mut f, "↑↓/jk");
        let keys = tower_ui::view::drawn_keys(&f.app);
        assert!(keys.iter().all(|k| k.to <= usize::from(width)), "{keys:?}");
        let rows: std::collections::HashSet<_> = keys.iter().map(|k| k.row).collect();
        assert!(
            rows.len() >= 2,
            "wrapped onto more than one line at {width}"
        );
        for key in ['B', 'r', 'q'] {
            assert!(
                keys.iter()
                    .any(|k| k.key == crossterm::event::KeyCode::Char(key)),
                "`{key}` is on screen at {width} columns: {keys:?}"
            );
        }
        assert_eq!(click_guide(&mut f, "q quit"), Action::Quit, "at {width}");
    }
}

// ── T: a scheduled message for the selected session ──────────────────────

fn type_text(f: &mut Fixture, text: &str) {
    for c in text.chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
}

fn with_session_fixture() -> Fixture {
    let mut f = wide_fixture();
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.session_id = "c376295e-95bf-45d2-9cd0-e26b0e837218".into();
    a.title = "the scheduled one".into();
    f.records(vec![a]);
    f
}

fn schedules(f: &Fixture) -> Vec<tower_state::Schedule> {
    tower_state::ScheduleStore::new(f.store.dir())
        .unwrap()
        .list()
        .unwrap()
}

/// `T`, a time and a message, `tab` for daily, `enter`: a daily schedule for
/// this session's conversation.
#[test]
fn t_schedules_a_message_for_the_selected_session() {
    let mut f = with_session_fixture();
    f.tap("T");
    assert!(f.app.input().is_some(), "the schedule field is open");
    assert_eq!(
        f.app.schedule_input_repeat(),
        Some(tower_state::Repeat::Once)
    );
    type_text(&mut f, "19:00 세션 마감하자");
    f.tap("tab");
    assert_eq!(
        f.app.schedule_input_repeat(),
        Some(tower_state::Repeat::Daily)
    );
    f.tap("enter");
    assert!(f.app.input().is_none());

    let all = schedules(&f);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].message, "세션 마감하자");
    assert_eq!(
        all[0].target,
        tower_state::ScheduleTarget::Session {
            id: "c376295e-95bf-45d2-9cd0-e26b0e837218".into(),
            origin: "claude-1".into(),
        }
    );
    assert_eq!(
        all[0].when,
        tower_state::When::Daily {
            at: tower_state::ClockTime::parse("19:00").unwrap()
        }
    );
    assert!(f.app.notice().contains("daily 19:00"), "{}", f.app.notice());
    assert_eq!(
        f.tower.loop_starts.lock().unwrap().len(),
        1,
        "the supervisor is what sends it"
    );
}

/// A pane whose CLI has not reported its conversation yet gets no schedule:
/// named by its pane, it would pass to whatever herdr next put there.
#[test]
fn t_refuses_a_row_without_a_session_id() {
    let mut f = wide_fixture();
    f.records(vec![agent("claude-1", "claude", "work", AgentState::Idle)]);
    f.tap("T");
    assert!(f.app.input().is_none(), "no field opened");
    assert!(
        f.app.err().is_some_and(|e| e.contains("session id")),
        "{:?}",
        f.app.err()
    );
    assert!(schedules(&f).is_empty());
}

/// `/clear` gives the pane a new conversation: the schedule is not delivered
/// there, but the row still shows it and `T` still cancels it.
#[test]
fn a_schedule_outlives_its_conversation_on_the_row_it_came_from() {
    let mut f = with_session_fixture();
    f.tap("T");
    type_text(&mut f, "+2h 테스트");
    f.tap("enter");
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.session_id = "0199aaaa-bbbb-7ccc-8ddd-eeeeffff0000".into();
    a.title = "the scheduled one".into();
    f.records(vec![a]);
    f.tap("T");
    assert!(f.app.input().is_none(), "no field: it cancelled");
    assert!(schedules(&f).is_empty());
}

/// `+30m` is a one-off; a line that does not start with a time goes back
/// into the field with what is wrong on the error line.
#[test]
fn a_line_without_a_time_stays_in_the_field() {
    let mut f = with_session_fixture();
    f.tap("T");
    type_text(&mut f, "hello");
    f.tap("enter");
    assert!(f.app.input().is_some(), "still open, to be fixed");
    assert!(
        f.app.err().is_some_and(|e| e.contains("not a time")),
        "{:?}",
        f.app.err()
    );
    assert!(schedules(&f).is_empty());

    f.tap("home");
    type_text(&mut f, "+30m ");
    f.tap("enter");
    assert!(f.app.input().is_none());
    assert!(f.app.err().is_none(), "{:?}", f.app.err());
    let all = schedules(&f);
    assert!(matches!(all[0].when, tower_state::When::Once { .. }));
    assert_eq!(all[0].message, "hello");
}

/// `T` on a row that already has a scheduled message cancels it, as `L`
/// turns a loop off.
#[test]
fn t_again_cancels_the_sessions_schedule() {
    let mut f = with_session_fixture();
    f.tap("T");
    type_text(&mut f, "+2h 테스트");
    f.tap("enter");
    assert_eq!(schedules(&f).len(), 1);
    f.tap("T");
    assert!(f.app.input().is_none(), "no field: it cancelled");
    assert!(schedules(&f).is_empty());
    assert!(f.app.notice().contains("cancelled"), "{}", f.app.notice());
}

/// The row wears its next time, and the header counts every schedule.
#[test]
fn a_scheduled_row_shows_its_time_and_the_header_counts_them() {
    let mut f = with_session_fixture();
    f.tap("T");
    type_text(&mut f, "+2h 테스트");
    f.tap("enter");
    let (w, h) = f.app.size();
    let text: Vec<String> = tower_ui::view::screen_lines(&f.app, w.into(), h.into())
        .iter()
        .map(tower_ui::line_text)
        .collect();
    let row = text
        .iter()
        .find(|l| l.contains("the scheduled one"))
        .expect("the row");
    assert!(row.contains("⏰ "), "{row:?}");
    assert!(text[0].contains("⏰ 1 · next"), "{:?}", text[0]);
}

/// The field says what it wants and which repetition is picked.
#[test]
fn the_schedule_field_says_what_it_wants() {
    let mut f = with_session_fixture();
    f.tap("T");
    f.tap("tab");
    f.tap("tab");
    let (w, h) = f.app.size();
    let footer = tower_ui::view::screen_lines(&f.app, w.into(), h.into())
        .iter()
        .map(tower_ui::line_text)
        .find(|l| l.contains("⏰"))
        .expect("the prompt line");
    assert!(footer.contains("once daily [weekdays]"), "{footer:?}");
    assert!(footer.contains("19:00 or +30m"), "{footer:?}");
}

// ── copying from the preview ─────────────────────────────────────────────

const PREVIEW_SID: &str = "da5bd140-fc58-4111-a5fb-66d0181f3878";

/// One agent selected, its pane captured, the frame drawn and remembered.
fn previewing(body: &str) -> Fixture {
    previewing_in(wide_fixture(), body)
}

/// The one previewed pane, on whatever fixture the caller has already sized.
///
/// Said once: [`previewing`] and [`previewing_on`] differ only in the fixture they
/// start from, and a second copy of this body is a second thing to keep in step
/// with `Event::Preview`.
fn previewing_in(mut f: Fixture, body: &str) -> Fixture {
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.session_id = PREVIEW_SID.into();
    f.records(vec![a]);
    f.feed(Event::Preview {
        pane_id: "p-claude-1".into(),
        body: body.into(),
    });
    f.app.remember_drawn();
    f
}

/// The screen as text, one string per row.
fn drawn_text(f: &Fixture) -> Vec<String> {
    let (w, h) = f.app.size();
    tower_ui::view::screen_lines(&f.app, w.into(), h.into())
        .iter()
        .map(tower_ui::line_text)
        .collect()
}

/// Where `needle` starts on screen: its row, and its display column.
fn find(f: &Fixture, needle: &str) -> (u16, u16) {
    use unicode_width::UnicodeWidthStr;
    drawn_text(f)
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            let at = line.find(needle)?;
            Some((row as u16, line[..at].width() as u16))
        })
        .unwrap_or_else(|| panic!("{needle:?} is not on screen"))
}

fn left(
    kind: fn(crossterm::event::MouseButton) -> crossterm::event::MouseEventKind,
) -> crossterm::event::MouseEventKind {
    kind(crossterm::event::MouseButton::Left)
}

/// A click on the session ID in the preview's heading copies the whole ID —
/// what `claude --resume` takes.
#[test]
fn clicking_the_preview_heading_session_id_copies_it() {
    let mut f = previewing("hello");
    let (row, col) = find(&f, PREVIEW_SID);
    f.feed(press_at(
        row,
        col + 3,
        left(crossterm::event::MouseEventKind::Down),
    ));
    f.feed(press_at(
        row,
        col + 3,
        left(crossterm::event::MouseEventKind::Up),
    ));
    assert_eq!(
        *f.tower.copied.lock().unwrap(),
        vec![PREVIEW_SID.to_string()]
    );
    assert!(
        f.app.notice().contains("copied session id"),
        "{}",
        f.app.notice()
    );
}

/// A drag over the preview selects the way a terminal does — from where it
/// started to where it ends, whole lines between — draws it reversed, and
/// copies it when the button comes up.
#[test]
fn dragging_over_the_preview_copies_what_it_covers() {
    let mut f = previewing("alpha line\nbeta line\ngamma line");
    let (r0, c0) = find(&f, "alpha");
    let (r2, c2) = find(&f, "gamma");
    f.feed(press_at(
        r0,
        c0,
        left(crossterm::event::MouseEventKind::Down),
    ));
    f.feed(press_at(
        r2,
        c2 + 4,
        left(crossterm::event::MouseEventKind::Drag),
    ));

    let (w, h) = f.app.size();
    let lines = tower_ui::view::screen_lines(&f.app, w.into(), h.into());
    let reversed = |row: u16| -> String {
        lines[usize::from(row)]
            .spans
            .iter()
            .filter(|s| {
                s.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert_eq!(reversed(r0).trim_end(), "alpha line");
    assert_eq!(reversed(r2), "gamma", "up to and including the end column");

    f.feed(press_at(
        r2,
        c2 + 4,
        left(crossterm::event::MouseEventKind::Up),
    ));
    assert_eq!(
        *f.tower.copied.lock().unwrap(),
        vec!["alpha line\nbeta line\ngamma".to_string()]
    );
    assert!(
        f.app.notice().contains("copied 3 lines"),
        "{}",
        f.app.notice()
    );
    assert!(
        f.app.selection().is_none(),
        "the highlight goes with the button"
    );
}

/// A click in the preview is not a selection, and is not a click on a row.
#[test]
fn a_click_in_the_preview_copies_nothing() {
    let mut f = previewing("alpha line");
    let cursor = f.app.cursor();
    let (row, col) = find(&f, "alpha");
    f.feed(press_at(
        row,
        col,
        left(crossterm::event::MouseEventKind::Down),
    ));
    f.feed(press_at(
        row,
        col,
        left(crossterm::event::MouseEventKind::Up),
    ));
    assert!(f.tower.copied.lock().unwrap().is_empty());
    assert_eq!(f.app.cursor(), cursor);
}

/// [`previewing`], on a backend of the test's own.
fn previewing_on(tower: StubTower, body: &str) -> Fixture {
    let mut f = fixture_with(tower);
    f.app.set_size(200, 40);
    previewing_in(f, body)
}

/// Presses at `from`, drags to `to` and lets go there, and returns what was
/// copied by it, if anything.
fn drag(f: &mut Fixture, from: (u16, u16), to: (u16, u16)) -> Option<String> {
    use crossterm::event::MouseEventKind::{Down, Drag, Up};
    let before = f.tower.copied.lock().unwrap().len();
    f.feed(press_at(from.0, from.1, left(Down)));
    f.feed(press_at(to.0, to.1, left(Drag)));
    f.feed(press_at(to.0, to.1, left(Up)));
    let copied = f.tower.copied.lock().unwrap();
    assert!(copied.len() <= before + 1, "one release, one copy at most");
    copied.get(before).cloned()
}

/// A selection is where it starts and ends, not which way the hand moved:
/// dragging up and to the left over the same cells copies exactly what the
/// drag down and to the right does, on one row and across several.
#[test]
fn dragging_backwards_copies_what_dragging_forwards_does() {
    let mut f = previewing("alpha line\nbeta line\ngamma line");
    let (r0, c0) = find(&f, "alpha");
    let (rb, cb) = find(&f, "beta");
    let (r2, c2) = find(&f, "gamma");
    for (from, to, want) in [
        ((r0, c0), (r2, c2 + 4), "alpha line\nbeta line\ngamma"),
        ((rb, cb + 2), (rb, cb + 6), "ta li"),
        ((r0, c0 + 6), (rb, cb + 3), "line\nbeta"),
    ] {
        assert_eq!(drag(&mut f, from, to).as_deref(), Some(want), "forwards");
        assert_eq!(drag(&mut f, to, from).as_deref(), Some(want), "backwards");
    }
}

/// A selection on one line copies what it covers, trailing spaces off, and
/// counts it in characters — a Korean syllable is one character though it is
/// two columns, and a drag ending on either of its columns takes it whole.
#[test]
fn a_one_line_drag_counts_characters_and_drops_trailing_spaces() {
    for (body, needle, span, want) in [
        ("alpha beta   end", "alpha", 12, "alpha beta"),
        ("세션 id", "세션", 6, "세션 id"),
        ("세션 id", "세션", 1, "세"),
        ("세션 id", "세션", 2, "세션"),
        ("세션 id", "세션", 3, "세션"),
    ] {
        let mut f = previewing(body);
        let (row, col) = find(&f, needle);
        let copied = drag(&mut f, (row, col), (row, col + span));
        assert_eq!(copied.as_deref(), Some(want), "{body:?} over {span}");
        let count = want.chars().count();
        assert_eq!(
            f.app.notice(),
            format!("copied {count} characters from the preview"),
            "{body:?}"
        );
    }
}

/// Blank preview is not text: a drag over nothing but the padding under the
/// pane's last line copies nothing and claims nothing.
#[test]
fn a_drag_over_blank_preview_copies_nothing() {
    let mut f = previewing("alpha line");
    let (row, _) = find(&f, "alpha");
    assert_eq!(drag(&mut f, (row + 1, 0), (row + 3, 40)), None);
    assert!(!f.app.notice().contains("copied"), "{}", f.app.notice());
    assert!(f.app.selection().is_none());
}

/// A drag that runs off the preview — up into the list, off the right edge,
/// down into the key guide — selects up to the preview's edge instead of
/// losing the selection or reaching text that is not the pane's.
#[test]
fn a_drag_past_the_preview_edge_stops_at_the_edge() {
    let mut f = previewing("alpha line\nbeta line\ngamma line");
    let (r0, c0) = find(&f, "alpha");
    let (rb, cb) = find(&f, "beta");
    let (r2, c2) = find(&f, "gamma");
    let (_, h) = f.app.size();
    // Up into the header, to its first column: from the preview's first cell.
    assert_eq!(
        drag(&mut f, (r2, c2 + 4), (0, 0)).as_deref(),
        Some("alpha line\nbeta line\ngamma")
    );
    // Off the right edge of the screen: to the end of the row.
    assert_eq!(
        drag(&mut f, (rb, cb), (rb, u16::MAX)).as_deref(),
        Some("beta line")
    );
    // Down past the bottom and off the right: to the end of the preview, with
    // nothing from the dot line or the key guide below it.
    let all = drag(&mut f, (r0, c0), (h - 1, u16::MAX)).expect("a copy");
    assert_eq!(all.trim_end(), "alpha line\nbeta line\ngamma line");
    assert!(!all.contains('·') && !all.contains("j/k"), "{all:?}");
}

/// A drag that did not start in the preview is not a selection: a press on a
/// list row that then sweeps over the preview chooses that row, and copies
/// nothing when it lets go.
#[test]
fn a_drag_that_started_outside_the_preview_selects_nothing() {
    use crossterm::event::MouseEventKind::{Down, Drag, Up};
    let mut f = previewing("alpha line\nbeta line");
    let (row, _) = find(&f, "claude");
    let (r0, c0) = find(&f, "alpha");
    f.feed(press_at(row, 20, left(Down)));
    f.feed(press_at(r0, c0 + 4, left(Drag)));
    assert!(
        f.app.selection().is_none(),
        "no selection without a press in it"
    );
    f.feed(press_at(r0 + 1, c0 + 4, left(Drag)));
    f.feed(press_at(r0 + 1, c0 + 4, left(Up)));
    assert!(f.tower.copied.lock().unwrap().is_empty());
    // And a drag with no press at all — its press went to another program.
    f.feed(press_at(r0, c0, left(Drag)));
    f.feed(press_at(r0, c0 + 4, left(Up)));
    assert!(f.tower.copied.lock().unwrap().is_empty());
    assert!(f.app.selection().is_none());
}

/// A clipboard that refuses says so, for a selection and for the session ID
/// alike — a notice claiming the text was copied would send the person off to
/// paste nothing.
#[test]
fn a_refused_copy_is_reported_not_claimed() {
    use crossterm::event::MouseEventKind::{Down, Up};
    let mut f = previewing_on(
        StubTower {
            copy_fails: true,
            ..Default::default()
        },
        "alpha line",
    );
    let (r0, c0) = find(&f, "alpha");
    assert_eq!(drag(&mut f, (r0, c0), (r0, c0 + 4)), None);
    assert_eq!(*f.tower.copy_tries.lock().unwrap(), ["alpha"]);
    assert_eq!(f.app.err(), Some("could not copy: no clipboard"));
    assert!(!f.app.notice().contains("copied"), "{}", f.app.notice());
    assert!(
        f.app.selection().is_none(),
        "the highlight goes all the same"
    );

    // A fixture of its own: the drag's error would otherwise still be
    // showing, and a click that never tried to copy would pass as refused.
    let mut f = previewing_on(
        StubTower {
            copy_fails: true,
            ..Default::default()
        },
        "alpha line",
    );
    assert_eq!(f.app.err(), None);
    let (row, col) = find(&f, PREVIEW_SID);
    f.feed(press_at(row, col, left(Down)));
    f.feed(press_at(row, col, left(Up)));
    assert_eq!(*f.tower.copy_tries.lock().unwrap(), [PREVIEW_SID]);
    assert_eq!(f.app.err(), Some("could not copy: no clipboard"));
    assert!(!f.app.notice().contains("copied"), "{}", f.app.notice());
}

/// The ID in the heading is a button exactly as wide as the ID drawn: its
/// first and last columns copy the whole ID — even where the heading only had
/// room for the short one — and the columns either side of it copy nothing.
#[test]
fn the_session_id_is_a_button_exactly_as_wide_as_the_id_drawn() {
    use crossterm::event::MouseEventKind::{Down, Up};
    let short = &PREVIEW_SID[..8];
    for (width, drawn) in [(200, PREVIEW_SID), (40, short)] {
        let mut f = previewing("hello");
        f.app.set_size(width, 40);
        f.app.remember_drawn();
        let text = drawn_text(&f);
        assert!(
            width == 200 || !text.iter().any(|l| l.contains(PREVIEW_SID)),
            "the whole ID does not fit at {width}"
        );
        let (row, col) = find(&f, drawn);
        let last = col + drawn.len() as u16 - 1;
        for (at, copies) in [
            (col - 1, false),
            (col, true),
            (last, true),
            (last + 1, false),
        ] {
            f.tower.copied.lock().unwrap().clear();
            f.feed(press_at(row, at, left(Down)));
            f.feed(press_at(row, at, left(Up)));
            let copied = f.tower.copied.lock().unwrap().clone();
            let want = if copies {
                vec![PREVIEW_SID.to_string()]
            } else {
                Vec::new()
            };
            assert_eq!(copied, want, "column {at} at {width} ({col}..={last})");
        }
    }
}

/// xorshift64*, so every generated drag replays from its case number.
struct Rng(tower_testrng::Rng);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(tower_testrng::Rng::new(seed))
    }

    fn below(&mut self, n: usize) -> usize {
        self.0.below(n)
    }

    /// A pane's worth of lines, narrow and wide glyphs and runs of spaces.
    fn body(&mut self) -> String {
        const PIECES: &[&str] = &["a", "fix", " ", "   ", "-", "세", "션", "界", "é", "…"];
        let lines: Vec<String> = (0..1 + self.below(12))
            .map(|_| {
                (0..self.below(30))
                    .map(|_| PIECES[self.below(PIECES.len())])
                    .collect()
            })
            .collect();
        format!("x{}", lines.join("\n"))
    }
}

/// Any drag over the preview — forwards or backwards, starting halfway
/// through a wide glyph, running off any edge — copies exactly what it drew
/// reversed, and exactly what a cell-by-cell reading of the text drawn says
/// lies between its ends: every cell from the start to the end in reading
/// order, the end included, each line's trailing spaces off, nothing when
/// that is blank or the press never moved.
#[test]
fn every_drag_copies_what_it_drew_reversed_and_nothing_else() {
    use crossterm::event::MouseEventKind::{Down, Drag, Up};
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthChar;

    let mut f = previewing("x");
    let mut rng = Rng::new(0x0D2A_6C0D);
    let mut copies = 0;
    for case in 0..400 {
        let width = [200u16, 120, 80, 47, 20][case % 5];
        f.app.set_size(width, 40);
        f.feed(Event::Preview {
            pane_id: "p-claude-1".into(),
            body: rng.body(),
        });
        f.app.remember_drawn();
        let w = usize::from(width);
        let area = tower_ui::view::compose(&f.app, w, 40)
            .preview_area
            .expect("room for the preview");
        let rows = area.rows.clone();
        let anchor = (rows.start + rng.below(rows.len()), rng.below(w));
        let head = (rng.below(42), rng.below(w + 6));
        let at = format!("case {case} at {width}: {anchor:?} to {head:?}");

        f.tower.copied.lock().unwrap().clear();
        f.feed(press_at(anchor.0 as u16, anchor.1 as u16, left(Down)));
        f.feed(press_at(head.0 as u16, head.1 as u16, left(Drag)));
        let screen = tower_ui::view::screen_lines(&f.app, w, 40);
        f.feed(press_at(head.0 as u16, head.1 as u16, left(Up)));
        let copied = f.tower.copied.lock().unwrap().first().cloned();

        // The cells between the ends, in reading order, read off the text
        // drawn — the drag's end pulled inside the preview first.
        let end = (head.0.clamp(rows.start, rows.end - 1), head.1.min(w - 1));
        let (from, to) = if anchor <= end {
            (anchor, end)
        } else {
            (end, anchor)
        };
        let naive: Vec<String> = (from.0..=to.0)
            .map(|row| {
                let mut col = 0;
                let mut kept = String::new();
                for c in area.text[row - rows.start].chars() {
                    if (row, col) >= from && (row, col) <= to {
                        kept.push(c);
                    }
                    col += c.width().unwrap_or(0);
                }
                kept.trim_end().to_string()
            })
            .collect();
        let naive = naive.join("\n");
        let want = (anchor != end && !naive.trim().is_empty()).then_some(naive);
        assert_eq!(copied, want, "{at}");

        // What was drawn reversed on those rows, read back the same way.
        let reversed: Vec<String> = (from.0..=to.0)
            .map(|row| {
                let lit: String = screen[row]
                    .spans
                    .iter()
                    .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
                    .map(|s| s.content.as_ref())
                    .collect();
                lit.trim_end().to_string()
            })
            .collect();
        if let Some(copied) = &copied {
            copies += 1;
            assert_eq!(*copied, reversed.join("\n"), "{at}");
        }
        assert!(f.app.selection().is_none(), "{at}");
    }
    assert!(copies > 100, "only {copies} of the drags copied anything");
}

// ── preview size ─────────────────────────────────────────────────────────

/// The row of the preview's heading band, if one is drawn.
fn preview_heading_row(f: &Fixture) -> Option<usize> {
    drawn_text(f)
        .iter()
        .position(|l| l.starts_with(" ▼ ") && l.contains("preview"))
}

/// Sixty sessions — more than the 40-row screen holds — with the selected
/// one's pane captured, drawn once so the mouse knows where things are.
fn long_list_previewing() -> Fixture {
    let mut f = fixture();
    let agents: Vec<_> = (0..60)
        .map(|i| agent(&format!("a{i:02}"), "claude", "work", AgentState::Idle))
        .collect();
    f.records(agents);
    let pane = f
        .app
        .selected()
        .expect("a session is selected")
        .pane
        .pane_id
        .clone();
    f.feed(Event::Preview {
        pane_id: pane,
        body: "captured text".into(),
    });
    f.app.remember_drawn();
    f
}

/// Pressing the preview's heading and dragging it moves the boundary: the
/// preview follows the pointer while the button is down, and the height is
/// saved once, when it comes up.
#[test]
fn dragging_the_preview_heading_resizes_the_preview() {
    let mut f = previewing("hello");
    let top = preview_heading_row(&f).expect("the preview is drawn");
    let down = top as u16 + 5;
    let a = f.app.on_event(press_at(
        top as u16,
        1,
        left(crossterm::event::MouseEventKind::Down),
    ));
    assert_eq!(a, Action::Nothing);
    let a = f.app.on_event(press_at(
        down,
        1,
        left(crossterm::event::MouseEventKind::Drag),
    ));
    assert_eq!(a, Action::Nothing, "nothing is saved mid-drag");
    f.app.remember_drawn();
    assert_eq!(
        preview_heading_row(&f),
        Some(top + 5),
        "the heading followed the pointer"
    );
    let rows = f.app.preview_rows().expect("the drag chose a height");
    let a = f.app.on_event(press_at(
        down,
        1,
        left(crossterm::event::MouseEventKind::Up),
    ));
    assert_eq!(a, Action::SavePreviewRows(Some(rows)));
    assert!(
        f.app.selection().is_none(),
        "a resize is not a text selection"
    );
}

/// Where the preview's folded band is drawn, if it is.
fn preview_band_row(f: &Fixture) -> Option<usize> {
    drawn_text(f)
        .iter()
        .position(|l| l.starts_with(" ▲ ") && l.contains("preview"))
}

/// Presses and lets go on `row` without moving: a click.
fn click_row(f: &mut Fixture, row: usize) -> Action {
    let row = row as u16;
    let _ = f.app.on_event(press_at(
        row,
        2,
        left(crossterm::event::MouseEventKind::Down),
    ));
    f.app
        .on_event(press_at(row, 2, left(crossterm::event::MouseEventKind::Up)))
}

/// A list too long for the screen gives the preview no rows by default, but
/// it still shows as a folded band at the foot, and is pulled up from there.
#[test]
fn a_long_list_pulls_the_preview_up_from_its_band() {
    let mut f = long_list_previewing();
    assert_eq!(
        preview_heading_row(&f),
        None,
        "the automatic split gives the list everything"
    );
    let band = preview_band_row(&f).expect("the folded preview's band") as u16;
    assert!(
        tower_ui::view::preview_request(&f.app).is_none(),
        "nothing is captured for a folded preview"
    );
    let _ = f.app.on_event(press_at(
        band,
        2,
        left(crossterm::event::MouseEventKind::Down),
    ));
    let _ = f.app.on_event(press_at(
        band - 12,
        2,
        left(crossterm::event::MouseEventKind::Drag),
    ));
    let a = f.app.on_event(press_at(
        band - 12,
        2,
        left(crossterm::event::MouseEventKind::Up),
    ));
    assert_eq!(a, Action::SavePreviewRows(Some(13)));
    f.app.remember_drawn();
    let top = preview_heading_row(&f).expect("the preview is drawn now");
    assert_eq!(top, usize::from(band) - 12);
    assert!(drawn_text(&f)[top + 1..]
        .iter()
        .any(|l| l.contains("captured text")));
}

/// A click on a long list's band opens the preview at half the body: the
/// automatic split would give it nothing to go back to.
#[test]
fn a_click_on_a_long_lists_band_opens_the_preview_at_half() {
    let mut f = long_list_previewing();
    let band = preview_band_row(&f).expect("the folded preview's band");
    let a = click_row(&mut f, band);
    let Action::SavePreviewRows(Some(rows)) = a else {
        panic!("a chosen height to remember: {a:?}");
    };
    f.app.remember_drawn();
    let top = preview_heading_row(&f).expect("the preview is open");
    assert!(rows > tower_ui::view::PREVIEW_MIN, "{rows}");
    assert!(preview_band_row(&f).is_none());
    // Its heading folds it again, keeping the height it was given.
    assert_eq!(click_row(&mut f, top), Action::Nothing);
    f.app.remember_drawn();
    assert!(preview_heading_row(&f).is_none());
    let band = preview_band_row(&f).expect("folded back to its band");
    assert_eq!(f.app.preview_rows(), Some(rows));
    assert_eq!(
        click_row(&mut f, band),
        Action::Nothing,
        "the height is kept"
    );
    f.app.remember_drawn();
    assert_eq!(preview_heading_row(&f), Some(top));
}

/// An automatic preview folds to its band on a click on its heading, captures
/// nothing while folded, and opens to the automatic split again.
#[test]
fn a_click_on_the_preview_heading_folds_and_opens_it() {
    let mut f = previewing("hello");
    let top = preview_heading_row(&f).expect("an automatic preview");
    assert!(tower_ui::view::preview_request(&f.app).is_some());
    assert_eq!(click_row(&mut f, top), Action::Nothing);
    assert!(f.app.preview_folded());
    f.app.remember_drawn();
    assert!(preview_heading_row(&f).is_none());
    let band = preview_band_row(&f).expect("the folded band");
    assert!(
        drawn_text(&f)[band].contains(PREVIEW_SID),
        "the band still names the session"
    );
    assert!(tower_ui::view::preview_request(&f.app).is_none());
    assert_eq!(
        click_row(&mut f, band),
        Action::Nothing,
        "nothing to remember"
    );
    assert!(!f.app.preview_folded());
    assert_eq!(f.app.preview_rows(), None, "back to the automatic split");
    f.app.remember_drawn();
    assert_eq!(preview_heading_row(&f), Some(top));
}

/// Folded, a tall chosen preview comes back at its height on `]`, not at
/// the smallest one.
#[test]
fn the_open_key_restores_a_folded_previews_height() {
    let mut f = long_list_previewing();
    for _ in 0..4 {
        let _ = f.press("]");
        f.app.remember_drawn();
    }
    assert_eq!(f.app.preview_rows(), Some(13));
    let top = preview_heading_row(&f).unwrap();
    let _ = click_row(&mut f, top);
    f.app.remember_drawn();
    assert!(preview_band_row(&f).is_some());
    assert_eq!(f.press("]"), Action::Nothing);
    f.app.remember_drawn();
    assert_eq!(f.app.preview_rows(), Some(13));
    assert_eq!(preview_heading_row(&f), Some(top));
}

/// A band pressed and dragged down, where there is no height to choose,
/// is a drag that came to nothing — not a click that opens it.
#[test]
fn dragging_a_band_down_neither_opens_nor_sizes_it() {
    let mut f = long_list_previewing();
    let band = preview_band_row(&f).unwrap() as u16;
    use crossterm::event::MouseEventKind::{Down, Drag, Up};
    let _ = f.app.on_event(press_at(band, 2, left(Down)));
    let _ = f.app.on_event(press_at(band + 1, 2, left(Drag)));
    let a = f.app.on_event(press_at(band + 1, 2, left(Up)));
    assert_eq!(a, Action::Nothing);
    assert_eq!(f.app.preview_rows(), None);
    f.app.remember_drawn();
    assert_eq!(
        preview_band_row(&f),
        Some(usize::from(band)),
        "still folded"
    );
}

/// `=` unfolds as well as forgetting the height: folded with none chosen,
/// it has nothing to save but brings the automatic preview back.
#[test]
fn equals_unfolds_a_folded_preview() {
    let mut f = previewing("hello");
    let top = preview_heading_row(&f).unwrap();
    let _ = click_row(&mut f, top);
    f.app.remember_drawn();
    assert!(f.app.preview_folded());
    assert_eq!(f.press("="), Action::Nothing, "no height to forget");
    assert!(!f.app.preview_folded());
    f.app.remember_drawn();
    assert_eq!(preview_heading_row(&f), Some(top));
}

/// A screen too short to spare a row for it draws no band, and the keys and
/// a click there change nothing.
#[test]
fn a_screen_too_short_for_the_band_draws_none() {
    let mut f = long_list_previewing();
    let (w, _) = f.app.size();
    for h in 1..12u16 {
        f.app.set_size(w, h);
        f.app.remember_drawn();
        let text = drawn_text(&f);
        assert_eq!(text.len(), usize::from(h));
        let c = tower_ui::view::compose(&f.app, w.into(), h.into());
        if c.split.is_none() {
            assert!(preview_band_row(&f).is_none(), "height {h}");
            assert!(c.preview.is_none(), "height {h}");
        }
    }
}

/// Folding and opening again leaves the screen as it was, at every size —
/// for an automatic preview and for one opened from a long list's band.
#[test]
fn folding_and_opening_restores_the_screen_at_every_size() {
    for long in [false, true] {
        for (w, h) in [(80u16, 24u16), (120, 30), (200, 40), (60, 50), (160, 18)] {
            let mut f = if long {
                long_list_previewing()
            } else {
                previewing("hello\nworld")
            };
            f.app.set_size(w, h);
            f.app.remember_drawn();
            if long {
                let Some(band) = preview_band_row(&f) else {
                    continue;
                };
                let _ = click_row(&mut f, band);
                f.app.remember_drawn();
            }
            let Some(top) = preview_heading_row(&f) else {
                continue;
            };
            let before = drawn_text(&f);
            let _ = click_row(&mut f, top);
            f.app.remember_drawn();
            let band = preview_band_row(&f).expect("folded to its band");
            assert_eq!(
                drawn_text(&f).len(),
                before.len(),
                "{w}x{h}: the band keeps the screen's height"
            );
            let _ = click_row(&mut f, band);
            f.app.remember_drawn();
            assert_eq!(drawn_text(&f), before, "{w}x{h} long={long}");
        }
    }
}

/// Whatever the state — automatic, chosen, folded by hand, or folded by a
/// long list — a capture is asked for exactly when an open preview is drawn,
/// and for the rows under its heading.
#[test]
fn a_capture_is_asked_for_exactly_when_the_preview_is_open() {
    for long in [false, true] {
        let mut f = if long {
            long_list_previewing()
        } else {
            previewing("hello")
        };
        let mut seen = std::collections::HashSet::new();
        for step in ["", "]", "]", "click", "click", "[", "[", "[", "="] {
            match step {
                "" => {}
                "click" => {
                    let row = preview_heading_row(&f).or_else(|| preview_band_row(&f));
                    if let Some(row) = row {
                        let _ = click_row(&mut f, row);
                    }
                }
                key => {
                    let _ = f.press(key);
                }
            }
            f.app.remember_drawn();
            let (w, h) = f.app.size();
            let c = tower_ui::view::compose(&f.app, w.into(), h.into());
            let heading = preview_heading_row(&f);
            let band = preview_band_row(&f);
            assert!(
                heading.is_none() || band.is_none(),
                "open and folded at once after {step:?}"
            );
            match (heading, &c.preview) {
                (Some(_), Some((_, rows))) => {
                    let split = c.split.expect("an open preview has a divider");
                    assert_eq!(usize::from(*rows), split.preview_h - 1, "after {step:?}");
                }
                (None, None) => {}
                other => panic!("long={long} after {step:?}: {other:?}"),
            }
            seen.insert((heading.is_some(), band.is_some()));
        }
        assert!(seen.contains(&(true, false)), "long={long}: never open");
        assert!(seen.contains(&(false, true)), "long={long}: never folded");
    }
}

/// The band is one full-width line at the foot of the body, right above the
/// key guide, naming the session with its fold mark on both ends.
#[test]
fn the_band_fills_its_line_at_the_foot() {
    use unicode_width::UnicodeWidthStr;
    let mut f = long_list_previewing();
    let band = preview_band_row(&f).unwrap();
    let text = drawn_text(&f);
    let (w, _) = f.app.size();
    assert_eq!(text[band].width(), usize::from(w), "{:?}", text[band]);
    assert!(text[band].trim_end().ends_with('▲'), "{:?}", text[band]);
    assert!(text[band + 1].contains("move"), "the key guide comes next");
    let _ = f.press("]");
    f.app.remember_drawn();
    assert!(drawn_text(&f)[preview_heading_row(&f).unwrap()]
        .trim_end()
        .ends_with('▼'));
}

/// Pressing the heading and moving only along its row is still a click.
#[test]
fn a_press_that_stays_on_the_heading_row_is_a_click() {
    let mut f = previewing("hello");
    let top = preview_heading_row(&f).unwrap() as u16;
    use crossterm::event::MouseEventKind::{Down, Drag, Up};
    let _ = f.app.on_event(press_at(top, 2, left(Down)));
    let _ = f.app.on_event(press_at(top, 9, left(Drag)));
    let _ = f.app.on_event(press_at(top, 9, left(Up)));
    assert!(f.app.preview_folded());
    assert_eq!(f.app.preview_rows(), None, "no height was chosen");
}

/// `]` opens or grows the preview, `[` shrinks it, `=` goes back to the
/// automatic split; each change asks the driver to remember it.
#[test]
fn the_bracket_keys_size_the_preview_and_equals_resets_it() {
    let mut f = long_list_previewing();
    assert_eq!(
        f.press("]"),
        Action::SavePreviewRows(Some(4)),
        "opened at the smallest useful height"
    );
    f.app.remember_drawn();
    assert!(preview_heading_row(&f).is_some());
    assert_eq!(f.press("]"), Action::SavePreviewRows(Some(7)));
    f.app.remember_drawn();
    assert_eq!(f.press("["), Action::SavePreviewRows(Some(4)));
    f.app.remember_drawn();
    // Already at the smallest: it folds to its band, with nothing to save.
    assert_eq!(f.press("["), Action::Nothing);
    assert!(f.app.preview_folded());
    f.app.remember_drawn();
    assert!(preview_heading_row(&f).is_none());
    assert!(preview_band_row(&f).is_some());
    assert_eq!(f.press("]"), Action::Nothing, "opened to the height it had");
    assert_eq!(f.app.preview_rows(), Some(4));
    assert!(!f.app.preview_folded());
    f.app.remember_drawn();
    assert!(preview_heading_row(&f).is_some());
    assert_eq!(f.press("="), Action::SavePreviewRows(None));
    f.app.remember_drawn();
    assert_eq!(preview_heading_row(&f), None);
    assert_eq!(f.press("="), Action::Nothing, "already automatic");
}

/// However far it is dragged, the preview leaves the list a few rows to
/// steer with, and is never shorter than a heading and three lines.
#[test]
fn a_dragged_preview_stays_in_bounds() {
    let mut f = previewing("hello");
    let top = preview_heading_row(&f).unwrap() as u16;
    let _ = f.app.on_event(press_at(
        top,
        1,
        left(crossterm::event::MouseEventKind::Down),
    ));
    let _ = f
        .app
        .on_event(press_at(0, 1, left(crossterm::event::MouseEventKind::Drag)));
    let tallest = f.app.preview_rows().unwrap();
    let _ = f.app.on_event(press_at(
        39,
        1,
        left(crossterm::event::MouseEventKind::Drag),
    ));
    assert_eq!(f.app.preview_rows(), Some(4));
    let _ = f
        .app
        .on_event(press_at(39, 1, left(crossterm::event::MouseEventKind::Up)));
    f.app.remember_drawn();
    let (_, h) = f.app.size();
    assert!(
        tallest + tower_ui::view::LIST_KEEPS_SIZED < usize::from(h),
        "{tallest}"
    );
}

/// An overflowing list draws a scrollbar in its last column: a thumb at the
/// top while the first rows are shown.
#[test]
fn an_overflowing_list_has_a_scrollbar() {
    let f = long_list_previewing();
    let text = drawn_text(&f);
    let bars: Vec<char> = text
        .iter()
        .filter_map(|l| l.chars().last())
        .filter(|c| *c == '┃' || *c == '│')
        .collect();
    assert!(bars.len() > 10, "{bars:?}");
    assert_eq!(bars[0], '┃', "the thumb starts at the top");
    assert_eq!(*bars.last().unwrap(), '│', "and does not reach the bottom");
}
