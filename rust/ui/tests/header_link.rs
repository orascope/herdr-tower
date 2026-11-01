// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The link at the header's right end: the project's page, or a newer
//! release once the supervisor has seen one, opened in the browser on a click.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use common::*;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tower_state::DaemonSnapshot;
use tower_ui::view::{compose, screen_lines, REPO_URL};
use tower_ui::{line_text, Action, Event, Usage, UsageFrame};

const PROJECT: &str = "★ github.com/orascope/herdr-tower ↗";

struct OneLine;

impl Usage for OneLine {
    fn refresh(&self, _force: bool) {}
    fn render(&self, _editing: Option<usize>) -> UsageFrame {
        UsageFrame {
            summary: "▌claude ◆5h ■■■■╎ 95%".into(),
            board: vec!["USAGE".into()],
            sliders: Vec::new(),
        }
    }
    fn adjust(&self, _slider: usize, _how: tower_ui::Adjust) -> Result<(), String> {
        Ok(())
    }
}

fn sized(width: u16) -> Fixture {
    let mut f = fixture();
    f.app.set_size(width, 40);
    f
}

fn with_usage(width: u16) -> Fixture {
    let mut f = sized(width);
    f.app.set_usage(Arc::new(OneLine));
    f
}

/// A heartbeat from a supervisor at `version` that has read `latest`.
fn supervisor(f: &mut Fixture, version: &str, latest: &str) {
    supervisor_at(f, version, latest, t0());
}

/// The same heartbeat, read `now` — 30 seconds after it or later, it is stale.
fn supervisor_at(f: &mut Fixture, version: &str, latest: &str, now: time::OffsetDateTime) {
    let daemon = DaemonSnapshot {
        pid: 1,
        version: version.into(),
        started_at: t0(),
        updated_at: t0(),
        connected: true,
        error: String::new(),
        tab_roots: Vec::new(),
        workspace_order: Vec::new(),
        workspace_display_labels: Default::default(),
        parents: Default::default(),
        backends: Vec::new(),
        latest_release: latest.into(),
    };
    let agents = f.app.all_agents().to_vec();
    f.feed(Event::Records {
        agents,
        branches: HashMap::new(),
        tab_roots: HashSet::new(),
        parents: Default::default(),
        daemon: Some(Box::new(daemon)),
        now,
    });
}

fn line(f: &Fixture, width: usize, row: usize) -> String {
    line_text(&screen_lines(&f.app, width, 40)[row])
}

fn click(f: &mut Fixture, row: usize, col: usize) -> Action {
    f.app.remember_drawn();
    f.app.on_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    }))
}

/// On the usage line, flush right one cell in from the edge, and a click
/// anywhere on it — first cell to last — opens the project's page.
#[test]
fn the_project_link_sits_at_the_usage_lines_right_end_and_opens_the_page() {
    let mut f = with_usage(140);
    let hit = compose(&f.app, 140, 40)
        .header_link
        .expect("room for the link");
    assert_eq!(hit.row, 1, "the usage line");
    assert_eq!(hit.cols.end, 139);
    assert_eq!(hit.url, REPO_URL);
    let text = line(&f, 140, 1);
    assert!(text.trim_end().ends_with(PROJECT), "{text}");
    assert!(text.contains("u usage"), "the reminder stays: {text}");
    for col in [hit.cols.start, hit.cols.end - 1] {
        assert_eq!(click(&mut f, 1, col), Action::Open(REPO_URL.into()));
    }
    assert_ne!(
        click(&mut f, 1, hit.cols.end),
        Action::Open(REPO_URL.into())
    );
}

/// Without a usage line the first line carries it.
#[test]
fn without_a_usage_line_the_first_line_carries_it() {
    let f = sized(200);
    let hit = compose(&f.app, 200, 40)
        .header_link
        .expect("room for the link");
    assert_eq!(hit.row, 0);
    assert!(line(&f, 200, 0).trim_end().ends_with(PROJECT));
}

/// Too narrow for it beside the line's own text with a gap: not drawn, and
/// nothing there to click.
#[test]
fn a_link_with_no_room_is_neither_drawn_nor_clickable() {
    let f = with_usage(60);
    assert!(compose(&f.app, 60, 40).header_link.is_none());
    assert!(!line(&f, 60, 1).contains("github.com"));
    // A fresh tower per click: one on `u usage` opens the board, whose line
    // is shorter and would then have room.
    for col in 0..60 {
        let mut f = with_usage(60);
        assert!(
            !matches!(click(&mut f, 1, col), Action::Open(_)),
            "col {col}"
        );
    }
}

/// A newer release takes the link's place and opens its own page; the same
/// version, an older one, or none read yet keeps the project's link.
#[test]
fn a_newer_release_takes_the_links_place() {
    let mut f = with_usage(140);
    supervisor(&mut f, "v0.61.6", "v0.61.7");
    let hit = compose(&f.app, 140, 40).header_link.expect("room");
    assert!(line(&f, 140, 1).trim_end().ends_with("v0.61.7 available ↗"));
    assert_eq!(
        click(&mut f, hit.row, hit.cols.start),
        Action::Open(format!("{REPO_URL}/releases/tag/v0.61.7"))
    );
    for latest in ["v0.61.6", "v0.61.5", ""] {
        supervisor(&mut f, "v0.61.6", latest);
        assert!(
            line(&f, 140, 1).trim_end().ends_with(PROJECT),
            "latest {latest:?}"
        );
    }
}

/// The link belongs to the list: another screen draws none and opens nothing.
#[test]
fn only_the_list_has_it() {
    let mut f = with_usage(140);
    let row = compose(&f.app, 140, 40).header_link.unwrap().row;
    let _ = f.app.on_event(Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('i'),
        KeyModifiers::NONE,
    )));
    assert_ne!(f.app.screen(), &tower_ui::Screen::List);
    assert!(compose(&f.app, 140, 40).header_link.is_none());
    assert!(!matches!(click(&mut f, row, 130), Action::Open(_)));
}

/// Edge: a heartbeat gone stale — the supervisor stopped — offers nothing,
/// one second either side of the 30-second line.
#[test]
fn a_stale_heartbeat_offers_no_release() {
    let mut f = with_usage(140);
    supervisor_at(
        &mut f,
        "v0.61.6",
        "v0.61.7",
        t0() + time::Duration::seconds(29),
    );
    assert!(line(&f, 140, 1).trim_end().ends_with("v0.61.7 available ↗"));
    supervisor_at(
        &mut f,
        "v0.61.6",
        "v0.61.7",
        t0() + time::Duration::seconds(30),
    );
    assert!(line(&f, 140, 1).trim_end().ends_with(PROJECT));
    assert_eq!(f.app.newer_release(), None);
}

/// Properties over every width, for both kinds of link, with and without a
/// usage line: when drawn, the link ends one cell in from the edge, spans
/// exactly its text's width, keeps a gap of at least three cells from the
/// line's own text, and the drawn cells read back as the link (cross check
/// of the hit against the screen); once it fits at a width it fits at every
/// wider one (monotonic); and it is never drawn wider than the screen.
#[test]
fn the_link_lays_out_consistently_at_every_width() {
    for usage in [true, false] {
        for newer in [false, true] {
            let mut f = if usage { with_usage(400) } else { sized(400) };
            if newer {
                supervisor(&mut f, "v0.61.6", "v0.61.7");
            }
            let want = if newer {
                "v0.61.7 available ↗"
            } else {
                PROJECT
            };
            let want_w = tower_text::width(want);
            let mut fitted_at = None;
            for width in 0..=400usize {
                let Some(hit) = compose(&f.app, width, 40).header_link else {
                    assert!(fitted_at.is_none(), "fit at {fitted_at:?}, lost at {width}");
                    continue;
                };
                fitted_at.get_or_insert(width);
                let ctx = format!("usage {usage} newer {newer} width {width}");
                assert_eq!(hit.row, usize::from(usage), "{ctx}");
                assert_eq!(hit.cols.end + 1, width, "{ctx}");
                assert_eq!(hit.cols.len(), want_w, "{ctx}");
                let text = line(&f, width, hit.row);
                assert!(tower_text::width(&text) <= width, "{ctx}");
                // The cells before the hit's end, read off the screen.
                let mut drawn = String::new();
                let mut used = 0;
                for c in text.chars() {
                    let w = tower_text::width(&c.to_string());
                    if used + w > hit.cols.end {
                        break;
                    }
                    used += w;
                    drawn.push(c);
                }
                assert!(drawn.ends_with(want), "{ctx}: {text}");
                let own = drawn[..drawn.len() - want.len()].trim_end();
                assert!(
                    tower_text::width(own) + 3 <= hit.cols.start,
                    "{ctx}: gap lost: {text}"
                );
            }
            assert!(fitted_at.is_some(), "never fit up to 400 (usage {usage})");
        }
    }
}
