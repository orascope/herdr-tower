// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The usage board inside the tower: its line under the header, and the
//! whole board in the preview's place on `u` — with a stand-in board that
//! records what the tower asked of it.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tower_ui::view::{compose, screen_lines};
use tower_ui::{line_text, Action, Adjust, Event, Usage, UsageFrame};

/// Three lines of board with a slider on the second and the fourth.
#[derive(Default)]
struct StubUsage {
    adjusted: Mutex<Vec<(usize, Adjust)>>,
    rendered_with: Mutex<Vec<Option<usize>>>,
}

impl Usage for StubUsage {
    fn refresh(&self, _force: bool) {}
    fn render(&self, editing: Option<usize>) -> UsageFrame {
        self.rendered_with.lock().unwrap().push(editing);
        let mark = |i: usize| if editing == Some(i) { "▸" } else { " " };
        UsageFrame {
            summary: "\x1b[1m▌claude\x1b[0m ◆5h ■■■■╎ 95%   ▌codex ⏸ held".into(),
            board: vec![
                "USAGE  updated 18:25".into(),
                format!("      ──────▼── {} claude ◆5h", mark(0)),
                " ◆5h  ■■■■■■■■  95% left".into(),
                format!("      ────▼──── {} claude ●7d", mark(1)),
                " ●7d  ■■■■■■■■  49% left".into(),
            ],
            sliders: vec![1, 3],
        }
    }
    fn adjust(&self, slider: usize, how: Adjust) -> Result<(), String> {
        self.adjusted.lock().unwrap().push((slider, how));
        Ok(())
    }
}

fn with_usage() -> (Fixture, Arc<StubUsage>) {
    let mut f = fixture();
    let u = Arc::new(StubUsage::default());
    f.app.set_usage(u.clone());
    f.app.set_size(140, 40);
    (f, u)
}

fn screen(f: &Fixture) -> Vec<String> {
    screen_lines(&f.app, 140, 40)
        .iter()
        .map(line_text)
        .collect()
}

#[test]
fn the_summary_sits_under_the_header_and_u_is_in_the_guide() {
    let (f, _) = with_usage();
    let s = screen(&f);
    assert!(s[0].starts_with("herdr-tower"), "{}", s[0]);
    assert!(s[1].contains("▌claude ◆5h ■■■■╎ 95%"), "{}", s[1]);
    assert!(s[1].contains("u usage"));
    assert!(s.last().unwrap().contains("u usage") || s.iter().any(|l| l.contains("· u usage ·")));
}

#[test]
fn without_a_board_the_tower_is_as_it_was() {
    let f = fixture();
    let s = screen_lines(&f.app, 140, 40)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>();
    assert!(!s.iter().any(|l| l.contains("u usage")));
}

#[test]
fn u_puts_the_board_in_the_previews_place_and_takes_it_away_again() {
    let (mut f, _) = with_usage();
    assert_eq!(f.press("u"), Action::Nothing);
    let s = screen(&f);
    assert!(s.iter().any(|l| l.starts_with("══ usage")), "{s:#?}");
    assert!(s.iter().any(|l| l.contains("49% left")));
    assert!(s.iter().any(|l| l.contains("u/esc back to preview")));
    assert!(
        !s[1].contains("u usage"),
        "the hint goes while the board shows"
    );
    // The list is still there above it.
    assert!(s.iter().any(|l| l.contains("◆ wait")));
    assert!(
        compose(&f.app, 140, 40).preview.is_none(),
        "no pane capture meanwhile"
    );

    f.tap("esc");
    let s = screen(&f);
    assert!(!s.iter().any(|l| l.starts_with("══ usage")));
    assert!(s.iter().any(|l| l.contains("u usage")));

    // `q` on the board goes back to the list; only then does it quit.
    f.tap("u");
    assert_eq!(f.press("q"), Action::Nothing);
    assert!(!screen(&f).iter().any(|l| l.starts_with("══ usage")));
    assert_eq!(f.press("q"), Action::Quit);
}

#[test]
fn the_arrows_choose_and_move_a_slider_and_j_k_still_move_the_list() {
    let (mut f, u) = with_usage();
    f.tap("u");
    f.tap("right");
    f.tap("down");
    f.tap("left");
    f.tap("x");
    f.tap("up");
    f.tap("right");
    assert_eq!(
        *u.adjusted.lock().unwrap(),
        [
            (0, Adjust::More),
            (1, Adjust::Less),
            (1, Adjust::Toggle),
            (0, Adjust::More),
        ]
    );
    assert!(
        u.rendered_with.lock().unwrap().contains(&Some(1)),
        "the chosen slider is drawn"
    );
    let before = f.app.selected().map(|a| a.id.clone());
    f.tap("j");
    assert_ne!(f.app.selected().map(|a| a.id.clone()), before);
}

#[test]
fn r_on_the_board_refreshes_it_at_most_every_half_minute() {
    let (mut f, _) = with_usage();
    f.tap("u");
    assert_eq!(f.press("r"), Action::RefreshUsage);
    assert_eq!(f.press("r"), Action::Nothing);
    assert!(f.app.notice().contains("again in"));
    // Off the board, `r` is the list's.
    f.tap("u");
    assert_eq!(f.press("r"), Action::Refresh);
}

#[test]
fn a_click_on_a_slider_sets_its_floor_where_it_landed() {
    let (mut f, u) = with_usage();
    f.tap("u");
    let frame = compose(&f.app, 140, 40);
    f.app.remember_frame(frame);
    let s = screen(&f);
    let row = s
        .iter()
        .position(|l| l.contains("claude ●7d"))
        .expect("the second slider is drawn");
    let click = |kind| MouseEvent {
        kind,
        column: 10,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    };
    f.feed(Event::Mouse(click(MouseEventKind::Down(MouseButton::Left))));
    f.feed(Event::Mouse(click(MouseEventKind::Drag(MouseButton::Left))));
    assert_eq!(
        *u.adjusted.lock().unwrap(),
        [(1, Adjust::At(11)), (1, Adjust::At(11))],
        "a 0-based screen column is the board's 1-based one"
    );
}

#[test]
fn a_new_reading_redraws_the_board() {
    let (mut f, u) = with_usage();
    let before = u.rendered_with.lock().unwrap().len();
    f.feed(Event::Usage);
    assert_eq!(u.rendered_with.lock().unwrap().len(), before + 1);
}

#[test]
fn the_docks_tower_is_not_quit_by_q_or_esc_and_says_why() {
    let mut f = kept_fixture();
    assert_eq!(f.press("q"), Action::Nothing);
    assert!(f.app.notice().contains("the dock keeps the tower"));
    assert_eq!(f.press("esc"), Action::Nothing);
    let s = screen_lines(&f.app, 120, 40)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>();
    assert!(
        !s.iter().any(|l| l.contains("q quit")),
        "not offered in the guide"
    );
    // Ctrl-C still ends it, for whoever means it.
    assert_eq!(f.press("ctrl+c"), Action::Quit);
    // A tower opened any other way still quits on q.
    assert_eq!(fixture().press("q"), Action::Quit);
}

/// The detail screen's guide follows the same rule as the list's: the dock's
/// tower offers no `q quit` there either, since `q` would only say it cannot;
/// a tower opened any other way still offers it.
#[test]
fn the_docks_detail_screen_does_not_offer_quit() {
    let guide = |kept: bool| {
        let mut f = if kept { kept_fixture() } else { fixture() };
        f.app.set_size(120, 40);
        f.records(vec![agent(
            "claude-1",
            "claude",
            "api",
            tower_state::AgentState::Waiting,
        )]);
        assert_eq!(f.press("i"), Action::Nothing);
        assert!(matches!(f.app.screen(), tower_ui::Screen::Info(_)));
        screen_lines(&f.app, 120, 40)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let docked = guide(true);
    assert!(docked.contains("close"), "{docked}");
    assert!(!docked.contains("q quit"), "{docked}");
    let plain = guide(false);
    assert!(
        plain.contains("close") && plain.contains("q quit"),
        "{plain}"
    );
}

fn mouse_at(kind: MouseEventKind, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 2,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn header_usage_reminder_opens_the_board_on_click() {
    let (mut f, _) = with_usage();
    let (row, cols) = compose(&f.app, 140, 40)
        .header_usage
        .expect("visible reminder");
    f.app.remember_drawn();
    f.feed(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: cols.start as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(f.app.usage_view());
    assert!(compose(&f.app, 140, 40).header_usage.is_none());
}

#[test]
fn truncated_header_usage_reminder_is_not_clickable() {
    let (mut f, _) = with_usage();
    f.app.set_size(20, 40);
    assert!(compose(&f.app, 20, 40).header_usage.is_none());
    f.app.remember_drawn();
    f.feed(mouse_at(MouseEventKind::Down(MouseButton::Left), 1));
    assert!(!f.app.usage_view());
}

/// Choosing a session on the list is asking to see it: `j`/`k`, a click on its
/// row and the wheel each put its preview back in the board's place. `↑↓`
/// stay the board's, choosing a slider.
#[test]
fn choosing_a_session_brings_its_preview_back() {
    let (mut f, _) = with_usage();
    f.tap("u");
    f.tap("down");
    assert!(f.app.usage_view(), "↑↓ choose a slider and keep the board");
    f.tap("u");

    for key in ["j", "k"] {
        f.tap("u");
        assert!(f.app.usage_view());
        f.tap(key);
        assert!(!f.app.usage_view(), "{key} brings the preview back");
    }

    f.tap("u");
    let row = screen(&f)
        .iter()
        .position(|l| l.contains("check the handoff doc"))
        .expect("the row is drawn beside the board") as u16;
    f.app.remember_drawn();
    f.feed(mouse_at(MouseEventKind::Down(MouseButton::Left), row));
    assert!(
        !f.app.usage_view(),
        "a click on a row brings the preview back"
    );
    assert_eq!(f.app.selected().unwrap().task, "check the handoff doc");

    for wheel in [MouseEventKind::ScrollDown, MouseEventKind::ScrollUp] {
        f.tap("u");
        f.feed(mouse_at(wheel, 0));
        assert!(!f.app.usage_view(), "{wheel:?} brings the preview back");
    }
}

/// With the board in the preview's place, no row is marked selected — the
/// board is about no one session. The mark comes back with the preview, on
/// the row it was on.
#[test]
fn the_board_takes_the_selection_mark_off_the_list() {
    let (mut f, _) = with_usage();
    let marked = |f: &Fixture| {
        screen(f)
            .iter()
            .filter(|l| l.starts_with('▸') && (l.contains("◆ wait") || l.contains("✔ done")))
            .count()
    };
    assert_eq!(marked(&f), 1, "{:#?}", screen(&f));
    let chosen = f.app.selected().unwrap().id.clone();

    f.tap("u");
    assert_eq!(marked(&f), 0, "{:#?}", screen(&f));

    f.tap("u");
    assert_eq!(marked(&f), 1);
    assert_eq!(f.app.selected().unwrap().id, chosen);
}

fn load(cpu: Option<f64>, used: Option<u64>) -> tower_ui::HostLoad {
    tower_ui::HostLoad {
        cpu_pct: cpu,
        memory: used.map(|used| tower_os::host::Memory { used, total: 100 }),
    }
}

#[test]
fn the_machines_load_ends_the_first_line_and_leaves_the_usage_line_alone() {
    let (mut f, _) = with_usage();
    f.feed(Event::Host(load(Some(23.4), Some(61))));
    let s = screen(&f);
    assert!(s[0].starts_with("herdr-tower"), "{}", s[0]);
    assert!(
        s[0].trim_end()
            .ends_with("·  cpu ■■■■■  23%  mem ■■■■■  61%"),
        "{}",
        s[0]
    );
    assert!(!s[1].contains("cpu"), "{}", s[1]);
    assert!(s[1].contains("▌claude ◆5h ■■■■╎ 95%"), "{}", s[1]);
    assert!(s[1].contains("u usage"), "{}", s[1]);
}

#[test]
fn the_machines_load_is_drawn_only_once_it_is_read() {
    let (mut f, _) = with_usage();
    assert!(
        !screen(&f)[0].contains("cpu"),
        "nothing before the first reading"
    );
    // The first reading has memory but no processor share yet: that needs two.
    f.feed(Event::Host(load(None, Some(40))));
    let line = screen(&f)[0].clone();
    assert!(!line.contains("cpu"), "{line}");
    assert!(line.contains("mem ■■■■■  40%"), "{line}");
    f.feed(Event::Host(load(None, None)));
    assert!(!screen(&f)[0].contains("mem"));
}

#[test]
fn without_a_board_the_machines_load_is_still_on_the_first_line() {
    let mut f = fixture();
    f.feed(Event::Host(load(Some(5.0), Some(90))));
    let s = screen_lines(&f.app, 140, 40)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>();
    assert!(s[0].starts_with("herdr-tower"), "{}", s[0]);
    assert!(s[0].contains("cpu ■■■■■   5%  mem ■■■■■  90%"), "{}", s[0]);
    assert!(!s.iter().any(|l| l.contains("u usage")), "no board to open");
}

#[test]
fn the_machines_meters_fill_and_colour_as_the_usage_meters_do() {
    let (mut f, _) = with_usage();
    f.feed(Event::Host(load(Some(100.0), Some(1))));
    let lines = screen_lines(&f.app, 140, 40);
    let cells: Vec<_> = lines[0]
        .spans
        .iter()
        .filter(|s| s.content == "■")
        .map(|s| s.style.fg)
        .collect();
    // The first line holds no other meter; these are the machine's.
    assert_eq!(cells.len(), 10, "two meters of five");
    let rgb = |(r, g, b)| Some(ratatui::style::Color::Rgb(r, g, b));
    let full = &cells[..5];
    let low = &cells[5..];
    for (i, colour) in full.iter().enumerate() {
        assert_eq!(*colour, rgb(tower_ui::meter::cell(i, 5, true)), "cpu {i}");
    }
    assert_eq!(low[0], full[0], "1% still shows one cell, as green");
    assert!(low[1..]
        .iter()
        .all(|c| *c == rgb(tower_ui::meter::EMPTY_CELL)));
}
