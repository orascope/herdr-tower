// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The dim hint in the `L`, `T` and `/` fields: an example the first time, the
//! last value after that. It is not input — `enter` over it alone is an empty
//! field — `→` or `End` takes it, and typing replaces it.

mod common;

use common::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use tower_state::{AgentState, LoopStore, ScheduleStore, DEFAULT_LOOP_MESSAGE, HINTS_FILE};
use tower_ui::view::{compose, screen_lines};
use tower_ui::{line_text, App, Event, FILTER_EXAMPLE, SCHEDULE_EXAMPLE};

fn type_text(f: &mut Fixture, text: &str) {
    for c in text.chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
}

/// One agent with a native session, so `L` and `T` open their fields.
fn session_fixture() -> Fixture {
    let mut f = fixture();
    f.app.set_size(200, 40);
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.session_id = "c376295e-95bf-45d2-9cd0-e26b0e837218".into();
    f.records(vec![a]);
    f
}

fn hint(f: &Fixture) -> String {
    f.app.input().expect("a field is open").hint()
}

fn value(f: &Fixture) -> String {
    f.app.input().expect("a field is open").value()
}

fn loops(f: &Fixture) -> Vec<tower_state::LoopLease> {
    LoopStore::new(f.store.dir()).unwrap().list().unwrap()
}

fn schedules(f: &Fixture) -> Vec<tower_state::Schedule> {
    ScheduleStore::new(f.store.dir()).unwrap().list().unwrap()
}

/// The prompt line as drawn: its text, and the style of the span holding
/// `needle`.
fn prompt_span(f: &Fixture, needle: &str) -> (String, ratatui::style::Style) {
    let (w, h) = f.app.size();
    let lines = screen_lines(&f.app, w.into(), h.into());
    let line = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
        .unwrap_or_else(|| panic!("{needle:?} is on screen"));
    let span = line
        .spans
        .iter()
        .find(|s| s.content.contains(needle))
        .unwrap();
    (line_text(line), span.style)
}

fn click(f: &mut Fixture, row: u16) {
    f.feed(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 20,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }));
    f.feed(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 20,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }));
}

/// The first `L` shows the default message dim; `enter` over it alone arms
/// nothing and says how to take it, with the field still open.
#[test]
fn l_first_time_shows_the_example_and_enter_alone_does_not_arm() {
    let mut f = session_fixture();
    f.tap("L");
    assert_eq!(hint(&f), DEFAULT_LOOP_MESSAGE);
    assert_eq!(value(&f), "");

    f.tap("enter");
    assert!(f.app.input().is_some(), "the field stays open");
    assert!(loops(&f).is_empty(), "a hint is not consent to arm");
    assert!(f.app.notice().contains("→ to take"), "{}", f.app.notice());
    assert_eq!(
        hint(&f),
        DEFAULT_LOOP_MESSAGE,
        "and the hint is still there"
    );
    let (line, _) = prompt_span(&f, "→ to take");
    assert!(
        line.starts_with("⌨ loop"),
        "said on the field's line: {line:?}"
    );

    // Whitespace is no message either.
    type_text(&mut f, "   ");
    f.tap("enter");
    assert!(loops(&f).is_empty());
    assert!(f.app.input().is_some());

    // The next key takes the guidance away.
    f.tap("backspace");
    assert_eq!(f.app.notice(), "");
}

/// A session's remembered message comes back dim; `→` makes it text, drawn
/// as text, and `enter` arms it.
#[test]
fn l_offers_the_remembered_message_and_right_then_enter_arms_it() {
    let mut f = session_fixture();
    f.tap("L");
    type_text(&mut f, "keep going on the parser");
    f.tap("enter");
    assert_eq!(loops(&f)[0].message, "keep going on the parser");
    f.tap("L"); // off

    f.tap("L");
    assert_eq!(hint(&f), "keep going on the parser");
    assert_eq!(value(&f), "");
    let (_, dim) = prompt_span(&f, "eep going on the parser");
    assert!(dim.fg.is_some(), "the hint is drawn dim: {dim:?}");

    f.tap("right");
    assert_eq!(value(&f), "keep going on the parser");
    assert!(!f.app.input().unwrap().showing_hint());
    let (_, plain) = prompt_span(&f, "keep going on the parser");
    assert_eq!(plain.fg, None, "taken, it is drawn as text: {plain:?}");

    f.tap("enter");
    assert!(f.app.input().is_none());
    assert_eq!(loops(&f)[0].message, "keep going on the parser");
}

/// The `T` example is a line the parser takes: `→` `enter` on a first `T` is
/// a working schedule, and `enter` alone is none.
#[test]
fn t_example_taken_with_right_arms_that_schedule() {
    let mut f = session_fixture();
    f.tap("T");
    assert_eq!(hint(&f), SCHEDULE_EXAMPLE);
    f.tap("enter");
    assert!(f.app.input().is_some(), "the field stays open");
    assert!(schedules(&f).is_empty());
    assert!(f.app.notice().contains("→ to take"), "{}", f.app.notice());

    f.tap("right");
    f.tap("enter");
    assert!(f.app.input().is_none(), "{:?}", f.app.err());
    let all = schedules(&f);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].message, "check the build and report back");
    assert!(matches!(all[0].when, tower_state::When::Once { .. }));
}

/// The last line `T` armed is offered next time — across a restart of the
/// tower too, which the dock's gets on every upgrade.
#[test]
fn t_offers_the_last_armed_line_even_after_a_restart() {
    let mut f = session_fixture();
    f.tap("T");
    type_text(&mut f, "+2h 테스트 돌려줘");
    f.tap("enter");
    assert_eq!(schedules(&f).len(), 1);
    f.tap("T"); // cancels it
    assert!(schedules(&f).is_empty());

    f.tap("T");
    assert_eq!(hint(&f), "+2h 테스트 돌려줘");
    f.tap("esc");

    // A line that failed is not remembered over the last good one.
    f.tap("T");
    type_text(&mut f, "whenever");
    f.tap("enter");
    assert!(f.app.err().is_some());
    f.tap("esc");

    assert!(f.store.dir().join(HINTS_FILE).exists());
    let home = tempfile::tempdir().unwrap();
    f.app = App::new(f.store.clone(), f.tower.clone(), settings(home.path()));
    f.app.set_size(200, 40);
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.session_id = "c376295e-95bf-45d2-9cd0-e26b0e837218".into();
    f.records(vec![a]);
    f.tap("T");
    assert_eq!(hint(&f), "+2h 테스트 돌려줘");
}

/// A `/` hint never filters; `→` takes it and it does. An active filter
/// reopens as text, and the last one used is the next hint.
#[test]
fn a_filter_hint_does_not_filter_until_taken() {
    let mut f = fixture();
    f.tap("/");
    assert_eq!(hint(&f), FILTER_EXAMPLE);
    assert_eq!(f.app.agents().len(), 2, "a hint never filters");
    f.tap("tab");
    assert_eq!(f.app.agents().len(), 2);

    f.tap("right");
    assert_eq!(f.app.filter(), FILTER_EXAMPLE, "taken, it filters");
    assert!(f.app.agents().is_empty(), "no codex in the fixture");
    f.tap("backspace");
    f.tap("backspace");
    f.tap("backspace");
    f.tap("backspace");
    f.tap("backspace");
    type_text(&mut f, "z-ai");
    f.tap("enter");
    assert_eq!(f.app.agents().len(), 1);

    // In force, it is the person's text, not a suggestion.
    f.tap("/");
    assert!(!f.app.input().unwrap().showing_hint());
    assert_eq!(value(&f), "z-ai");
    f.tap("esc");
    assert_eq!(f.app.filter(), "");

    // Used, so offered next — and still only offered.
    f.tap("/");
    assert_eq!(hint(&f), "z-ai");
    assert_eq!(f.app.agents().len(), 2);
    f.tap("enter");
    assert_eq!(f.app.filter(), "", "enter over a hint alone clears");
}

/// A click on the field focuses it: the hint goes and the field is empty.
/// A click anywhere else leaves it be.
#[test]
fn a_click_on_the_field_clears_the_hint() {
    let mut f = session_fixture();
    f.tap("L");
    f.app.remember_drawn();
    let (w, h) = f.app.size();
    let row = compose(&f.app, w.into(), h.into())
        .prompt_row
        .expect("the field's row");

    click(&mut f, 0);
    assert_eq!(hint(&f), DEFAULT_LOOP_MESSAGE, "a click elsewhere");
    click(&mut f, row as u16);
    assert!(f.app.input().is_some(), "still open");
    assert!(!f.app.input().unwrap().showing_hint());
    assert_eq!(value(&f), "");
    type_text(&mut f, "x");
    assert_eq!(value(&f), "x");
}

/// `tab` picks the loop's interval and the schedule's repetition; the hint
/// is left alone.
#[test]
fn tab_keeps_the_hint() {
    let mut f = session_fixture();
    f.tap("L");
    f.tap("tab");
    f.tap("backtab");
    f.tap("tab");
    assert_eq!(hint(&f), DEFAULT_LOOP_MESSAGE);
    assert_eq!(f.app.loop_input_idle_for().unwrap().as_secs(), 1_800);
    f.tap("esc");

    f.tap("T");
    f.tap("tab");
    assert_eq!(hint(&f), SCHEDULE_EXAMPLE);
    assert_eq!(
        f.app.schedule_input_repeat(),
        Some(tower_state::Repeat::Daily)
    );
}

/// `B` stays an empty field: a broadcast has no example worth suggesting.
#[test]
fn broadcast_has_no_hint() {
    let mut f = fixture();
    f.tap("B");
    assert!(!f.app.input().unwrap().showing_hint());
}

/// The hint and the key reminder after the field must not read as one text:
/// the hint is dim italic, a separator bar ends the field, and the reminder's
/// keys are bold like the key guide's.
#[test]
fn the_hint_and_the_key_reminder_are_told_apart() {
    use ratatui::style::Modifier;
    let mut f = session_fixture();
    f.tap("T");
    let lines = screen_lines(&f.app, 160, 40);
    let prompt = lines
        .iter()
        .find(|l| line_text(l).contains("check the build"))
        .expect("the schedule field");
    let hint = prompt
        .spans
        .iter()
        .find(|s| s.content.contains("check the build"))
        .expect("the hint");
    assert!(
        hint.style.add_modifier.contains(Modifier::ITALIC),
        "{hint:?}"
    );
    assert!(
        prompt.spans.iter().any(|s| s.content == " │ "),
        "{prompt:?}"
    );
    let tab = prompt
        .spans
        .iter()
        .find(|s| s.content == "tab")
        .expect("the tab key");
    assert!(tab.style.add_modifier.contains(Modifier::BOLD), "{tab:?}");
    assert_ne!(
        tab.style.fg, hint.style.fg,
        "keys are not the hint's colour"
    );
}
