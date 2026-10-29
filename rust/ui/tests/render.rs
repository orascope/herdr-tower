// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What the screen says, as values — and once, through a real ratatui buffer.
//!
//! Everything but the last test reads [`tower_ui::view::screen_lines`], which is
//! the whole picture without a terminal. The last one draws into ratatui's
//! `TestBackend` to prove those values survive the trip into a buffer; nothing
//! here ever touches the terminal the test runner is using.

mod common;

use std::collections::{HashMap, HashSet};

use common::*;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;
use tower_state::{AgentState, LoopLease, LoopStore};
use tower_ui::view::{help_line, screen_lines};
use tower_ui::{line_text, Event, LocalInfo};
use tower_usage::{CtxInfo, DefaultModels, UsedPct};
use unicode_width::UnicodeWidthStr as _;

/// The whole screen as one string, for substring checks.
/// A band with no count and nothing known of what it runs.
fn band_of(
    machine: &str,
    fold: Option<bool>,
    load: tower_ui::HostLoad,
    note: &str,
) -> tower_ui::view::Band {
    tower_ui::view::Band {
        machine: machine.into(),
        fold,
        load,
        note: note.into(),
        ..Default::default()
    }
}

fn screen(f: &Fixture, width: u16, height: u16) -> String {
    screen_lines(&f.app, width as usize, height as usize)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_tower_never_previews_its_own_pane() {
    for backend in ["", "b0000000000000001"] {
        let target = tower_mux::target_key(backend, "p-tower");
        let mut f = fixture_with(StubTower {
            self_pane: Some(target.clone()),
            ..StubTower::default()
        });
        let mut a = agent("tower", "codex", "HERDR-TOWER", AgentState::Idle);
        a.pane.backend = backend.into();
        f.records(vec![a.clone()]);
        f.app.set_size(120, 40);
        let _ = f.app.on_event(Event::Preview {
            pane_id: target,
            body: "recursive tower footer".into(),
        });
        assert!(tower_ui::view::preview_request(&f.app).is_none());
        assert!(f.app.live_preview().is_none());
        assert!(!screen(&f, 120, 40).contains("recursive tower footer"));

        // Pane coordinates can repeat on another backend. That session is
        // still previewable, even when its title happens to name the tower.
        a.pane.backend = "b0000000000000002".into();
        f.records(vec![a]);
        let _ = f.app.on_event(Event::Preview {
            pane_id: "b0000000000000002:p-tower".into(),
            body: "other session output".into(),
        });
        assert_eq!(
            tower_ui::view::preview_request(&f.app).unwrap().0,
            "b0000000000000002:p-tower"
        );
        assert!(screen(&f, 120, 40).contains("other session output"));
    }
}

#[test]
fn preview_eligibility_handles_missing_dead_and_popup_targets() {
    // The expected decisions are explicit, independent of target_key.
    for (name, own, pane, state, captured, expected) in [
        (
            "own",
            Some("w0:p1"),
            "w0:p1",
            AgentState::Idle,
            "output",
            None,
        ),
        (
            "popup",
            None,
            "w0:p1",
            AgentState::Idle,
            "output",
            Some("output"),
        ),
        ("empty pane", None, "", AgentState::Idle, "output", None),
        ("dead", None, "w0:p1", AgentState::Dead, "output", None),
        (
            "blank capture",
            None,
            "w0:p1",
            AgentState::Idle,
            " \n\t",
            None,
        ),
        (
            "unicode",
            Some("w0:p1"),
            "w0:p2",
            AgentState::Idle,
            "세션 '界'",
            Some("세션 '界'"),
        ),
    ] {
        let mut f = fixture_with(StubTower {
            self_pane: own.map(str::to_owned),
            ..StubTower::default()
        });
        let mut a = agent("edge", "codex", "work", state);
        a.pane.pane_id = pane.into();
        f.records(vec![a]);
        f.feed(Event::Preview {
            pane_id: pane.into(),
            body: captured.into(),
        });
        assert_eq!(f.app.live_preview(), expected, "{name}");
        if pane.is_empty() || state == AgentState::Dead || name == "own" {
            assert!(tower_ui::view::preview_request(&f.app).is_none(), "{name}");
        } else {
            assert_eq!(
                tower_ui::view::preview_request(&f.app).unwrap().0,
                pane,
                "{name}"
            );
        }
    }
    let f = fixture_with(StubTower::default());
    assert!(f.app.live_preview().is_none());
    assert!(tower_ui::view::preview_request(&f.app).is_none());
}

#[test]
fn self_preview_stays_absent_across_generated_panes_and_screen_sizes() {
    let mut rng = tower_testrng::Rng::new(0x5e1f);
    for case in 0..32 {
        let backend = format!("b{:016x}", rng.below(65536));
        let pane = format!("w{}:p{}", rng.below(100), rng.below(100));
        let mut f = fixture_with(StubTower {
            self_pane: Some(format!("{backend}:{pane}")),
            ..StubTower::default()
        });
        let mut a = agent("generated", "codex", "work", AgentState::Idle);
        a.pane.backend = backend.clone();
        a.pane.pane_id = pane.clone();
        f.records(vec![a.clone()]);
        f.feed(Event::Preview {
            pane_id: format!("{backend}:{pane}"),
            body: "SELF_CAPTURE_SENTINEL".into(),
        });
        for width in [0, 1, 40, 80, 120, 220] {
            for height in [0, 1, 6, 12, 40, 80] {
                let frame = tower_ui::view::compose(&f.app, width, height);
                assert!(frame.preview.is_none(), "case {case}: {width}x{height}");
                assert!(frame.preview_area.is_none());
                assert!(!frame
                    .lines
                    .iter()
                    .map(line_text)
                    .any(|l| l.contains("SELF_CAPTURE_SENTINEL")));
            }
        }
        // A different pane on the same server must still be available.
        a.pane.pane_id.push_str("-other");
        let other = format!("{backend}:{pane}-other");
        f.records(vec![a]);
        f.feed(Event::Preview {
            pane_id: other.clone(),
            body: "OTHER_CAPTURE_SENTINEL".into(),
        });
        assert_eq!(tower_ui::view::preview_request(&f.app).unwrap().0, other);
        assert_eq!(f.app.live_preview(), Some("OTHER_CAPTURE_SENTINEL"));
    }
}

#[test]
fn preview_selection_round_trip_rejects_late_self_captures() {
    let mut f = fixture_with(StubTower {
        self_pane: Some("w0:p1".into()),
        ..StubTower::default()
    });
    let mut a = agent("selected", "codex", "work", AgentState::Idle);
    a.pane.pane_id = "w0:p2".into();
    f.records(vec![a.clone()]);
    f.feed(Event::Preview {
        pane_id: "w0:p2".into(),
        body: "OTHER_SESSION_MARKER".into(),
    });
    let original = screen(&f, 120, 40);
    assert!(original.contains("OTHER_SESSION_MARKER"));

    a.pane.pane_id = "w0:p1".into();
    f.records(vec![a.clone()]);
    f.feed(Event::Preview {
        pane_id: "w0:p1".into(),
        body: original.clone(),
    });
    // Read back the actual terminal buffer, rather than only screen_lines.
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| tower_ui::view::render(frame, frame.area(), &f.app))
        .unwrap();
    let read_back = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!read_back.contains("OTHER_SESSION_MARKER"));
    assert_eq!(read_back.matches("↑↓/jk move").count(), 1);

    a.pane.pane_id = "w0:p2".into();
    f.records(vec![a]);
    // A capture already in flight when the selection changed cannot put the
    // tower's screen under the other session's heading.
    f.feed(Event::Preview {
        pane_id: "w0:p1".into(),
        body: original.clone(),
    });
    assert!(f.app.live_preview().is_none());
    f.feed(Event::Preview {
        pane_id: "w0:p2".into(),
        body: "OTHER_SESSION_MARKER".into(),
    });
    assert_eq!(screen(&f, 120, 40), original);
}

#[test]
fn the_list_shows_what_the_tower_is_for() {
    let f = fixture();
    let v = screen(&f, 120, 40);
    for want in [
        "herdr-tower",
        "v-test",
        "◆ wait",
        "✔ done",
        "── collab-bot",
        "check the handoff doc",
        "TASK",
        "↑↓/jk",
    ] {
        assert!(v.contains(want), "the screen should say {want:?}:\n{v}");
    }
}

/// The column a header names starts at, read from the header itself so these
/// tests follow the layout rather than restating its widths.
fn column_at(header: &str, name: &str) -> usize {
    let byte = header
        .find(name)
        .unwrap_or_else(|| panic!("no {name} in {header:?}"));
    tower_text::width(&header[..byte])
}

/// The text of one row that starts at a column and runs to the next one.
fn cell_at(row: &str, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for ch in tower_text::glyphs(row) {
        let w = tower_text::glyph_width(ch);
        if col >= from && col < to {
            out.push_str(ch);
        }
        col += w;
    }
    out.trim().to_string()
}

/// The screen as text, and the column header in it.
fn text_and_header(f: &Fixture, w: u16, h: u16) -> (Vec<String>, String) {
    let texts: Vec<String> = screen_lines(&f.app, w.into(), h.into())
        .iter()
        .map(line_text)
        .collect();
    let header = texts
        .iter()
        .find(|l| l.contains("STATE") && l.contains("TASK"))
        .expect("a column header")
        .clone();
    (texts, header)
}

/// Where TITLE, TASK and DIR start, on a screen wide enough for TITLE.
fn split_columns(header: &str) -> (usize, usize, usize) {
    (
        column_at(header, "TITLE"),
        column_at(header, "TASK"),
        column_at(header, "DIR"),
    )
}

/// TITLE sits left of TASK, and each shows its own thing: the pane's title,
/// and what the pane was asked. The TASK cell used to lead with the title too,
/// which printed the same words twice once TITLE had a column of its own.
#[test]
fn the_title_and_the_task_each_have_their_own_column() {
    let mut f = fixture();
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.title = "PR drift review".into();
    a.task = "look at the drift".into();
    f.records(vec![a]);

    let (texts, header) = text_and_header(&f, 200, 40);
    let (title_at, task_at, dir_at) = split_columns(&header);
    assert!(title_at < task_at, "TITLE is left of TASK: {header:?}");

    let row = texts
        .iter()
        .find(|l| l.contains("PR drift review"))
        .expect("the agent's row");
    assert_eq!(cell_at(row, title_at, task_at), "PR drift review");
    assert_eq!(cell_at(row, task_at, dir_at), "look at the drift");
    assert_eq!(
        row.matches("PR drift review").count(),
        1,
        "the title is not repeated in TASK: {row:?}"
    );
}

/// Nothing to show reads `N/A`, in both columns — a blank cell looked like a
/// rendering fault, and a row with both blank like a half-drawn one. Each row
/// is checked for its own cells: the titled one shows its title, the bare one
/// `N/A`.
#[test]
fn an_empty_title_or_task_reads_n_a() {
    let mut f = fixture();
    let bare = agent("codex-9", "codex", "work", AgentState::Idle);
    let mut titled = agent("codex-8", "codex", "work", AgentState::Idle);
    titled.title = "Fix tab restoration | ws-tab".into();
    f.records(vec![bare, titled]);

    let (texts, header) = text_and_header(&f, 200, 40);
    let (title_at, task_at, dir_at) = split_columns(&header);
    let rows: Vec<&String> = texts.iter().filter(|l| l.contains("codex")).collect();
    assert_eq!(rows.len(), 2, "{texts:#?}");
    let (titled, bare): (Vec<&String>, Vec<&String>) = rows
        .into_iter()
        .partition(|row| row.contains("Fix tab restoration"));
    let (titled, bare) = (titled[0], bare[0]);

    assert!(
        cell_at(titled, title_at, task_at).starts_with("Fix tab restoration"),
        "{titled:?}"
    );
    assert_eq!(cell_at(bare, title_at, task_at), "N/A", "{bare:?}");
    for row in [titled, bare] {
        assert_eq!(
            cell_at(row, task_at, dir_at),
            "N/A",
            "no assignment recorded: {row:?}"
        );
    }
}

/// A long title is cut short of the next column, never into it.
#[test]
fn a_long_title_stops_before_the_task_column() {
    let mut f = fixture();
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.title = "처리하십시오 Issue #2671 근본 원인 | nested-blob-copy-2671".into();
    a.task = "재개".into();
    f.records(vec![a]);

    let (texts, header) = text_and_header(&f, 200, 40);
    let task_at = column_at(&header, "TASK");
    let row = texts.iter().find(|l| l.contains("처리하십시오")).unwrap();
    assert_eq!(
        cell_at(row, task_at, task_at + 4),
        "재개",
        "TASK starts where its header does: {row:?}"
    );
    assert!(row.contains('…'), "the title was cut: {row:?}");
}

/// TITLE and TASK are always exposed side-by-side, even on narrow screens.
#[test]
fn the_title_and_task_are_always_exposed_even_on_narrow_screens() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    a.cwd = "/Users/x/parallax-browser/.claude/worktrees/feat".into();
    a.title = "PR drift review".into();
    a.task = "look at the drift".into();
    f.records(vec![a]);
    f.tap("L");
    f.tap("right");
    f.tap("enter");

    let (texts, header) = text_and_header(&f, 120, 40);
    assert!(header.contains("TITLE"), "TITLE at 120: {header:?}");
    assert!(header.contains("TASK"), "TASK at 120: {header:?}");
    let (title_at, task_at, dir_at) = split_columns(&header);
    let row = texts
        .iter()
        .find(|l| l.contains("PR drift review"))
        .expect("the agent's row");
    assert_eq!(cell_at(row, title_at, task_at), "PR drift review");
    assert_eq!(cell_at(row, task_at, dir_at), "look at the drift");

    let (wide_texts, _) = text_and_header(&f, 180, 40);
    let wide_row = wide_texts
        .iter()
        .find(|l| l.contains("PR drift review"))
        .expect("the agent's row");
    assert!(
        wide_row.contains("↻ 10m"),
        "the loop badge is drawn: {wide_row:?}"
    );
}

#[test]
fn the_pane_column_shows_the_pane_coordinate() {
    let mut f = fixture();
    let mut a = agent("claude-w1:p2", "claude", "work", AgentState::Idle);
    a.pane.pane_id = "w1:p2".into();
    a.title = "herdr sidebar".into();
    a.task = "fix review".into();
    let mut bare = agent("claude-none", "claude", "work", AgentState::Idle);
    bare.pane.pane_id = String::new();
    bare.title = "bare agent".into();
    f.records(vec![a, bare]);

    let (texts, header) = text_and_header(&f, 160, 40);
    assert!(header.contains("PANE"), "header contains PANE: {header:?}");
    let pane_at = column_at(&header, "PANE");
    let sid_at = column_at(&header, "SID");
    let title_at = column_at(&header, "TITLE");
    assert!(pane_at < sid_at, "PANE is before SID: {header:?}");
    assert!(sid_at < title_at, "SID is before TITLE: {header:?}");

    let row = texts
        .iter()
        .find(|l| l.contains("herdr sidebar"))
        .expect("agent row");
    assert_eq!(cell_at(row, pane_at, sid_at), "w1:p2");

    let bare_row = texts
        .iter()
        .find(|l| l.contains("bare agent"))
        .expect("bare row");
    assert_eq!(cell_at(bare_row, pane_at, sid_at), "N/A");
}

/// SID, not SESSION: `status` already has a SESSION column, and it holds the
/// workspace — the name `info` looks an agent up by. One header meaning two
/// things sent a copied session ID to `info`, which found nothing.
#[test]
fn the_sid_column_shows_the_short_session_id() {
    let mut f = fixture();
    let mut a = agent("claude-session", "claude", "work", AgentState::Idle);
    a.session_id = "c376295e-95bf-45d2-9cd0-e26b0e837218".into();
    a.title = "active agent".into();
    let mut bare = agent("claude-nosession", "claude", "work", AgentState::Idle);
    bare.title = "nosession agent".into();
    f.records(vec![a, bare]);

    let (texts, header) = text_and_header(&f, 160, 40);
    assert!(!header.contains("SESSION"), "{header:?}");
    let sid_at = column_at(&header, "SID");
    let title_at = column_at(&header, "TITLE");

    let row = texts
        .iter()
        .find(|l| l.contains("active agent"))
        .expect("agent row");
    assert_eq!(cell_at(row, sid_at, title_at), "c376295e");

    let bare_row = texts
        .iter()
        .find(|l| l.contains("nosession agent"))
        .expect("bare row");
    assert_eq!(cell_at(bare_row, sid_at, title_at), "N/A");
}

/// 🚨 SID costs nine columns, and at 120 — a laptop's popup, a split dock
/// pane — they came out of the folder and the badges after it. There the
/// column gives its room back; the preview heading still carries the ID.
#[test]
fn a_narrow_screen_gives_the_sid_column_back_to_the_badges() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "c376295e-95bf-45d2-9cd0-e26b0e837218".into();
    a.cwd = "/Users/x/parallax".into();
    a.title = "PR drift review".into();
    a.task = "look at the drift".into();
    f.records(vec![a]);

    let (narrow, header) = text_and_header(&f, 120, 40);
    assert!(!header.contains("SID"), "no SID at 120: {header:?}");
    let (title_at, task_at, dir_at) = split_columns(&header);
    let row = narrow
        .iter()
        .find(|l| l.contains("PR drift review"))
        .expect("the agent's row");
    assert!(!row.contains("c376295e"), "{row:?}");
    assert_eq!(cell_at(row, title_at, task_at), "PR drift review");
    assert_eq!(cell_at(row, task_at, dir_at), "look at the drift");

    let (_, wide_header) = text_and_header(&f, 140, 40);
    assert!(wide_header.contains("SID"), "SID at 140: {wide_header:?}");
    // Nine columns, SID's cell and its gap, and nothing else moved.
    assert_eq!(column_at(&wide_header, "DIR"), dir_at + 9);
}

/// The room SID gives back is for the badges: a loop badge that fits on a
/// 120-column row without SID, and would be cut off with it.
#[test]
fn a_narrow_screen_keeps_the_loop_badge_that_sid_would_push_off() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    a.cwd = "/w".into();
    a.title = "PR drift review".into();
    a.task = "look at the drift".into();
    f.records(vec![a]);
    f.tap("L");
    f.tap("right");
    f.tap("enter");

    let (texts, header) = text_and_header(&f, 120, 40);
    assert!(!header.contains("SID"), "{header:?}");
    let row = texts
        .iter()
        .find(|l| l.contains("PR drift review"))
        .expect("the agent's row");
    assert!(row.contains("↻ 10m"), "the loop badge is drawn: {row:?}");
    assert!(
        tower_text::width(row) + 9 > 120,
        "the badge would not have fit with SID: {row:?}"
    );
}

/// Three codex workers spawned together have UUIDv7 IDs that share their
/// first eight digits — a timestamp — so the column shows each one's tail.
#[test]
fn codex_workers_spawned_together_have_distinct_sids() {
    let mut f = fixture();
    let sids = [
        "01a0b8e0-9917-7af1-b920-ab518ce5d454",
        "01a0b8e0-99a2-7c03-8d11-4e0f97a2c1b6",
        "01a0b8e0-9a0c-7e58-a4f2-02d6b3e9f870",
    ];
    let workers = sids
        .iter()
        .enumerate()
        .map(|(i, sid)| {
            let mut a = agent(&format!("codex-{i}"), "codex", "work", AgentState::Working);
            a.session_id = (*sid).into();
            a.title = format!("worker {i}");
            a
        })
        .collect();
    f.records(workers);

    let (texts, header) = text_and_header(&f, 160, 40);
    let (sid_at, title_at) = (column_at(&header, "SID"), column_at(&header, "TITLE"));
    let shown: HashSet<String> = (0..sids.len())
        .map(|i| {
            let row = texts
                .iter()
                .find(|l| l.contains(&format!("worker {i}")))
                .expect("the worker's row");
            cell_at(row, sid_at, title_at)
        })
        .collect();
    assert_eq!(shown.len(), sids.len(), "{shown:?}");
    assert!(!shown.contains("01a0b8e0"), "{shown:?}");
}

/// The TITLE column reads `tab›title`: where the pane is, then what it is
/// doing. The title is what gets cut; a tab name long enough to crowd it out
/// is cut too, leaving the title a few columns.
#[test]
fn the_title_cell_leads_with_the_tab() {
    let row = |tab: &str, title: &str| {
        let mut f = fixture();
        let mut a = agent("claude-t", "claude", "work", AgentState::Idle);
        a.pane.tab = tab.into();
        a.title = title.into();
        f.records(vec![a]);
        screen(&f, 200, 20)
            .lines()
            .find(|l| l.contains("claude "))
            .expect("the row")
            .to_string()
    };
    assert!(row("pr-2752", "fix login").contains("pr-2752›fix login "));
    let long = row(
        "pr-2752",
        "rewrite the whole authentication layer from scratch",
    );
    assert!(long.contains("pr-2752›rewrite the whole a… "), "{long}");
    let crowded = row("a-tab-name-far-too-long-to-keep", "fix login");
    assert!(
        crowded.contains("a-tab-name-far-too…›fix log…"),
        "{crowded}"
    );
    assert!(
        row("pr-2752", "").contains("pr-2752 "),
        "a tab with no title"
    );
    assert!(
        row("", "fix login").contains("fix login") && !row("", "fix login").contains('›'),
        "no tab, the title alone"
    );
}

/// No yellow dot line anywhere: the summary and the preview are teal bands,
/// and the key guide's own line ends the preview.
#[test]
fn no_dot_line_is_drawn_anywhere() {
    let mut f = fixture();
    f.records(vec![agent(
        "claude-gap",
        "claude",
        "work",
        AgentState::Idle,
    )]);
    f.feed(Event::Preview {
        pane_id: "p-claude-gap".into(),
        body: "preview content".into(),
    });
    let text: Vec<String> = screen_lines(&f.app, 160, 40)
        .iter()
        .map(line_text)
        .collect();
    let heading = text
        .iter()
        .position(|l| l.contains(" ▼ ") && l.contains("preview"))
        .expect("the preview heading");
    let guide = text
        .iter()
        .position(|l| l.contains("↑↓/jk move"))
        .expect("the key guide");
    let columns = text
        .iter()
        .position(|l| l.contains("STATE") && l.contains("AGENT"))
        .expect("the column header");
    assert!(
        !text
            .iter()
            .any(|l| !l.is_empty() && l.chars().all(|c| c == '·')),
        "no dot line:\n{}",
        text.join("\n")
    );
    assert!(columns < heading && heading < guide);
}

/// The list runs straight into the preview's heading band — no dot line in
/// between — and the summary lines at the top sit on the same dark teal band,
/// ending where the column names begin.
#[test]
fn the_summary_and_the_preview_are_bands_not_dot_lines() {
    let mut f = fixture();
    f.records(vec![agent(
        "claude-gap",
        "claude",
        "work",
        AgentState::Idle,
    )]);
    f.feed(Event::Preview {
        pane_id: "p-claude-gap".into(),
        body: "preview content".into(),
    });
    let lines = screen_lines(&f.app, 160, 40);
    let heading = lines
        .iter()
        .position(|l| line_text(l).contains(" ▼ ") && line_text(l).contains("preview"))
        .expect("the preview heading");
    assert!(
        line_text(&lines[heading - 1]).contains("claude"),
        "the row, then the heading"
    );
    let columns = lines
        .iter()
        .position(|l| line_text(l).contains("STATE") && line_text(l).contains("AGENT"))
        .expect("the column header");
    // The preview's dark teal (theme::PREVIEW_BG).
    let band = Some(Color::Rgb(0x1f, 0x30, 0x3a));
    for (i, l) in lines[..columns].iter().enumerate() {
        assert!(
            l.spans.iter().all(|s| s.style.bg == band),
            "summary line {i} is on the band: {:?}",
            line_text(l)
        );
        assert_eq!(tower_ui::line_width(l), 160, "filled to the width");
    }
    assert!(
        lines[columns].spans.iter().all(|s| s.style.bg != band),
        "the column names are off the band"
    );
}

/// The whole session ID, so it can be read off the heading and handed to
/// `claude --resume` or `codex resume`.
#[test]
fn the_preview_heading_shows_the_whole_session_id_in_its_own_colour() {
    let mut f = fixture();
    let mut a = agent("claude-preview", "claude", "work", AgentState::Idle);
    a.session_id = "da5bd140-fc58-4111-a5fb-66d0181f3878".into();
    f.records(vec![a]);
    f.feed(Event::Preview {
        pane_id: "p-claude-preview".into(),
        body: "preview content".into(),
    });

    let lines = screen_lines(&f.app, 160, 40);
    let preview_heading = lines
        .iter()
        .find(|line| line_text(line).contains("preview"))
        .expect("the preview heading");
    let heading_text = line_text(preview_heading);
    assert!(
        heading_text.contains("preview · da5bd140-fc58-4111-a5fb-66d0181f3878 ═"),
        "heading should include the whole session ID: {heading_text}"
    );

    let sid_span = preview_heading
        .iter()
        .find(|span| span.content == "da5bd140-fc58-4111-a5fb-66d0181f3878")
        .expect("session_id span in preview heading");
    let preview_span = preview_heading
        .iter()
        .find(|span| span.content == "preview")
        .expect("the preview word");
    // Truecolour like the rest of the palette, and not the heading's blue.
    assert!(
        matches!(sid_span.style.fg, Some(Color::Rgb(..))),
        "{:?}",
        sid_span.style.fg
    );
    assert_ne!(sid_span.style.fg, preview_span.style.fg);
}

/// 🚨 The whole 36-character ID pushed a narrow heading past the width: the
/// ID was cut mid-UUID and the `═` rule vanished. A long workspace name did
/// the same, so the name is what gives way — and where even the ID alone does
/// not fit, the row's short ID stands in for it rather than half a UUID.
#[test]
fn a_narrow_preview_heading_keeps_its_id_and_its_rule() {
    let mut f = fixture();
    let mut a = agent(
        "claude-preview",
        "claude",
        "feat-auth-token-refresh-and-session-rotation",
        AgentState::Idle,
    );
    a.session_id = "da5bd140-fc58-4111-a5fb-66d0181f3878".into();
    f.records(vec![a]);
    f.feed(Event::Preview {
        pane_id: "p-claude-preview".into(),
        body: "preview content".into(),
    });

    let heading = screen_lines(&f.app, 60, 40)
        .iter()
        .map(line_text)
        .find(|l| l.starts_with(" ▼ ") && l.contains("preview"))
        .expect("the preview heading");
    assert_eq!(
        heading, " ▼ feat-a… preview · da5bd140-fc58-4111-a5fb-66d0181f3878 ══",
        "the whole ID fits at 60, so the name is what is cut"
    );

    for width in [40usize, 30] {
        let heading = screen_lines(&f.app, width, 40)
            .iter()
            .map(line_text)
            .find(|l| l.starts_with(" ▼ ") && l.contains("preview"))
            .expect("the preview heading");
        assert!(heading.contains("preview · da5bd140 ══"), "{heading:?}");
        assert!(
            heading.starts_with(" ▼ feat") && heading.contains("… preview"),
            "the name is cut, not dropped: {heading:?}"
        );
        assert_eq!(heading.width(), width, "{heading:?}");
    }

    // With no room for even a stub of it, the name goes and the ID stays.
    let heading = screen_lines(&f.app, 25, 40)
        .iter()
        .map(line_text)
        .find(|l| l.starts_with(" ▼ ") && l.contains("preview"))
        .expect("the preview heading");
    assert_eq!(heading, " ▼ preview · da5bd140 ═══", "{heading:?}");
}

/// A `wt new` worker's generated name is not one somebody chose: the TITLE
/// cell shows what the CLI says the worker is doing.
#[test]
fn a_worker_row_titles_itself_with_the_cli_title_not_its_generated_name() {
    let mut f = fixture();
    let mut a = agent("claude-1", "claude", "work", AgentState::Working);
    a.label = "auth-api-3f2a91cc".into();
    a.title = "Wiring the token refresh".into();
    a.task = "wire up the token refresh".into();
    f.records(vec![a]);

    let (texts, header) = text_and_header(&f, 160, 40);
    let (title_at, task_at, _) = split_columns(&header);
    let row = texts
        .iter()
        .find(|l| l.contains("Wiring the token refresh"))
        .expect("the worker's row");
    assert_eq!(cell_at(row, title_at, task_at), "Wiring the token refresh");
    assert!(!row.contains("auth-api-3f2a91cc"), "{row:?}");
}

#[test]
fn the_title_column_prefers_the_pane_label_over_terminal_title() {
    let mut f = fixture();
    let mut a = agent("claude-1", "claude", "work", AgentState::Idle);
    a.label = "PR #7".into();
    a.title = "herdr sidebar 탭 우클릭 비활성화".into();
    a.task = "/code-review xhigh 7".into();
    f.records(vec![a]);

    let (texts, header) = text_and_header(&f, 160, 40);
    let (title_at, task_at, dir_at) = split_columns(&header);
    let row = texts.iter().find(|l| l.contains("PR #7")).expect("the row");
    assert_eq!(cell_at(row, title_at, task_at), "PR #7");
    assert_eq!(cell_at(row, task_at, dir_at), "/code-review xhigh 7");
}

#[test]
fn an_armed_idle_loop_is_visible_on_its_agent_row() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    f.records(vec![a]);
    f.tap("L");
    f.tap("right");
    f.tap("enter");
    let v = screen(&f, 180, 40);
    assert!(
        v.contains("↻ 10m · 0 ·"),
        "the armed row needs a loop badge:\n{v}"
    );
    assert!(
        v.contains("L idle loop"),
        "the toggle must be discoverable:\n{v}"
    );
}

#[test]
fn the_loop_composer_shows_the_default_message_and_selected_interval() {
    let mut f = fixture();
    let mut a = agent("claude-loop", "claude", "work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    f.records(vec![a]);
    f.tap("L");

    let v = screen(&f, 120, 40);
    assert!(v.contains("⌨ loop 5m [10m] 30m 60m:"), "{v}");
    assert!(v.contains("Proceed with the next approved task"), "{v}");

    f.tap("tab");
    let v = screen(&f, 120, 40);
    assert!(v.contains("⌨ loop 5m 10m [30m] 60m:"), "{v}");
}

#[test]
fn an_empty_tower_says_where_to_look() {
    let mut f = fixture();
    f.feed(Event::Records {
        agents: Vec::new(),
        branches: HashMap::new(),
        tab_roots: HashSet::new(),
        parents: Default::default(),
        daemon: None,
        now: t0(),
    });
    let v = screen(&f, 120, 40);
    assert!(v.contains("running in herdr"), "{v}");
    assert!(v.contains("no sessions"), "the tally says so too:\n{v}");
}

/// The defaults are not in the header any more — their room went to the
/// machine's load — except for the one worth acting on: Claude left pinned to
/// Fable, so every new session runs at the top tier and the weekly allowance
/// drains without anybody choosing it.
#[test]
fn only_a_fable_default_is_in_the_header_and_as_a_warning() {
    let mut f = fixture();
    f.feed(Event::Local(LocalInfo {
        def_models: Some(DefaultModels {
            claude: "opus".into(),
            codex: "gpt-5.6-sol high".into(),
            gemini: String::new(),
        }),
        ..Default::default()
    }));
    let v = screen(&f, 200, 40);
    for gone in ["default model", "▌Claude", "▌Codex gpt-5.6-sol", "▌Gemini"] {
        assert!(!v.contains(gone), "{gone:?} is no longer shown:\n{v}");
    }
    assert!(tower_ui::view::fable_warning(&f.app).is_none());

    f.feed(Event::Local(LocalInfo {
        def_models: Some(DefaultModels {
            claude: "claude-fable-5".into(),
            ..Default::default()
        }),
        ..Default::default()
    }));
    let first = screen(&f, 200, 40).lines().next().unwrap().to_string();
    assert!(first.contains("⚠ ▌Claude Fable 5 default"), "{first}");
    let warning = tower_ui::view::fable_warning(&f.app).unwrap();
    let red = Some(Color::Rgb(0xf0, 0x47, 0x47));
    for want in ["⚠ ", " Fable 5 default"] {
        let span = warning.iter().find(|s| s.content == want).expect(want);
        assert_eq!(span.style.fg, red, "{want:?}");
    }
}

#[test]
fn a_row_carries_the_model_and_the_context_share() {
    let mut f = fixture();
    let mut ctx = HashMap::new();
    ctx.insert(
        "claude-7".to_string(),
        CtxInfo {
            model: "Opus 5 (1M context)".into(),
            used_pct: Some(UsedPct::Exact(42.0)),
            ts: Some(at(-10)),
        },
    );
    f.feed(Event::Local(LocalInfo {
        ctx,
        ..Default::default()
    }));
    let v = screen(&f, 200, 40);
    assert!(v.contains("Opus 5 (1M context)"), "{v}");
    assert!(v.contains("ctx 42%"), "{v}");
}

/// A guess shown with the authority of a measurement is how somebody plans a
/// session around a number that was never measured.
#[test]
fn an_estimated_context_share_wears_a_tilde() {
    let mut f = fixture();
    let mut ctx = HashMap::new();
    ctx.insert(
        "claude-7".to_string(),
        CtxInfo {
            model: "gemini-3.6-flash".into(),
            used_pct: Some(UsedPct::approx(7.0)),
            ts: Some(t0()),
        },
    );
    f.feed(Event::Local(LocalInfo {
        ctx,
        ..Default::default()
    }));
    assert!(screen(&f, 200, 40).contains("ctx ~7%"));
}

/// The rule names a workspace and opens every group, the first one included.
///
/// The vendor rule it replaced was a separator, so the first group had nothing
/// to be separated from and went bare. This one is a heading with indented rows
/// under it, and a bare first group would indent those rows under nothing.
#[test]
fn a_rule_opens_every_workspace_including_the_first() {
    let mut f = fixture();
    f.records(vec![
        agent("claude-7", "claude", "collab-bot", AgentState::Waiting),
        agent("gemini-9", "gemini", "g", AgentState::Idle),
    ]);
    let v = screen(&f, 120, 40);
    assert!(
        v.contains("── collab-bot"),
        "the first group has a rule:\n{v}"
    );
    assert!(v.contains("── g "), "and so does the second:\n{v}");
    assert!(
        !v.contains("── gemini"),
        "the vendor is a column now, not a group:\n{v}"
    );
}

/// A workspace name is typed by a person and can be any length, unlike the
/// short fixed vendor names the rule replaced.
///
/// It cannot overflow the screen: `screen_lines` clamps every line it returns to
/// the width, so a name wider than the terminal is cut with the rest. Pinned
/// because the rule builds its own trailing dashes with `saturating_sub`, which
/// stops the dashes at zero and would otherwise be the only thing standing
/// between a long name and a wrapped line.
#[test]
fn a_long_workspace_name_cannot_widen_the_screen() {
    let mut f = fixture();
    f.records(vec![agent(
        "claude-7",
        "claude",
        &"very-long-workspace-name-".repeat(8),
        AgentState::Waiting,
    )]);
    for width in [40usize, 60, 80, 120] {
        for l in screen_lines(&f.app, width, 40) {
            assert!(
                l.width() <= width,
                "a {}-column line at width {width}: {:?}",
                l.width(),
                line_text(&l)
            );
        }
    }
}

/// A workspace with more agents than fit scrolls, and the rule must not scroll
/// away with them: the rows below it are indented under nothing, and with the
/// SESSION column gone nothing on them says which workspace they are in.
#[test]
fn a_viewport_that_starts_inside_a_group_repeats_its_rule() {
    let mut f = fixture();
    let many: Vec<_> = (0..40)
        .map(|i| {
            agent(
                &format!("claude-{i}"),
                "claude",
                "collab-bot",
                AgentState::Idle,
            )
        })
        .collect();
    f.records(many);
    for _ in 0..30 {
        f.tap("j");
    }

    let v = screen(&f, 120, 24);
    assert!(
        v.contains("── collab-bot"),
        "the rule is still on screen with the cursor 30 rows down:\n{v}"
    );
    assert!(v.contains("↑ "), "and the list really is scrolled:\n{v}");
}

/// A dead record keeps the label it had when it died, but recovery records are
/// not tower rows and therefore cannot put a stale name on a live group.
#[test]
fn a_dead_record_cannot_name_a_live_group() {
    let mut f = fixture();
    let mut stale = agent("claude-dead", "claude", "old-name", AgentState::Dead);
    stale.pane.workspace_id = "w2".into();
    let mut live = agent("codex-live", "codex", "new-name", AgentState::Working);
    live.pane.workspace_id = "w2".into();
    f.records(vec![stale, live]);

    let v = screen(&f, 120, 40);
    assert_eq!(
        f.app.agents().len(),
        1,
        "only the live row reaches the view"
    );
    assert!(v.contains("── [w2]new-name"), "{v}");
    assert!(!v.contains("old-name"), "{v}");
}

/// The scanner's fallback ID and a real name equal to an ID look identical.
/// Rendering follows the server's display labels rather than guessing which
/// semantic name was absent. Labels survive a missing heartbeat too.
#[test]
fn sidebar_labels_distinguish_unnamed_workspaces_from_names_equal_to_ids() {
    let mut f = fixture();
    let mut unnamed = agent("unnamed", "codex", "w2", AgentState::Idle);
    unnamed.pane.workspace_id = "w2".into();
    let mut named = agent("named", "codex", "w3", AgentState::Idle);
    named.pane.workspace_id = "b0123456789abcdef:w3".into();
    named.pane.backend = "b0123456789abcdef".into();
    let mut dock = agent("dock", "codex", tower_ui::DOCK_LABEL, AgentState::Idle);
    dock.pane.workspace_id = "w4Z".into();
    let agents = vec![named, dock, unnamed];
    let daemon = tower_state::DaemonSnapshot {
        pid: 42,
        version: "v-test".into(),
        started_at: t0(),
        updated_at: t0(),
        connected: true,
        error: String::new(),
        tab_roots: Vec::new(),
        workspace_order: vec!["w2".into(), "b0123456789abcdef:w3".into(), "w4Z".into()],
        workspace_display_labels: [
            ("w2".into(), "[w2]".into()),
            ("b0123456789abcdef:w3".into(), "[w3]w3".into()),
            ("w4Z".into(), tower_ui::DOCK_LABEL.into()),
        ]
        .into(),
        parents: Default::default(),
        backends: Vec::new(),
    };
    let event = |daemon| Event::Records {
        agents: agents.clone(),
        branches: HashMap::new(),
        tab_roots: HashSet::new(),
        parents: Default::default(),
        daemon,
        now: t0(),
    };
    f.feed(event(Some(daemon)));
    let selected = f.app.selected().unwrap().pane.pane_id.clone();
    f.feed(Event::Preview {
        pane_id: selected,
        body: "worker output".into(),
    });
    f.feed(event(None));
    let v = screen(&f, 120, 50);
    assert!(v.contains("── [w2] "), "{v}");
    assert!(!v.contains("[w2]w2"), "{v}");
    assert!(v.contains("── [w3]w3 "), "{v}");
    assert!(v.contains("── 🗼 HERDR-TOWER 🗼 "), "{v}");
    assert!(!v.contains("[w4Z]"), "{v}");
    assert!(v.contains(" ▼ [w2] preview"), "{v}");
}

/// A pane whose workspace herdr no longer reports still has to land under a
/// heading rather than one with a hole in it.
#[test]
fn a_workspaceless_row_gets_a_rule_of_its_own() {
    let mut f = fixture();
    f.records(vec![agent("claude-7", "claude", "", AgentState::Idle)]);
    let v = screen(&f, 120, 40);
    assert!(v.contains("── (no workspace)"), "{v}");
}

/// Agent rows form workspace -> tab branches, a tab's panes in herdr's count.
/// The selection marker replaces the branch's first cell without shifting
/// STATE.
#[test]
fn rows_form_a_session_tree_under_their_workspace() {
    let mut f = fixture();
    let mut watcher = agent("watcher", "claude", "collab-bot", AgentState::Idle);
    watcher.pane.workspace_id = "w2".into();
    watcher.pane.window = 1;
    // Herdr recycles pane coordinates. The later worker can therefore receive
    // a numerically smaller ID than the supervising pane.
    watcher.pane.pane_id = "w2:p5Z".into();
    let mut worker = agent("worker", "codex", "collab-bot", AgentState::Waiting);
    worker.pane.workspace_id = "w2".into();
    worker.pane.window = 1;
    worker.pane.pane_id = "w2:p50".into();
    let mut standalone = agent("standalone", "gemini", "collab-bot", AgentState::Working);
    standalone.pane.workspace_id = "w2".into();
    standalone.pane.window = 2;
    standalone.pane.pane_id = "w2:p4".into();
    f.records_with_roots(
        vec![standalone, worker, watcher],
        HashSet::from(["w2:p5Z".to_string(), "w2:p4".to_string()]),
    );

    assert_eq!(
        f.app
            .agents()
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        vec!["watcher", "worker", "standalone"],
        "tab 1 before tab 2, and in tab 1 p5Z before p50 (`0` is herdr's last digit)"
    );
    let lines: Vec<String> = screen(&f, 120, 40).lines().map(str::to_string).collect();
    let rule = lines
        .iter()
        .position(|l| l.starts_with("── [w2]collab-bot"))
        .expect("a rule");
    for l in &lines[rule + 1..rule + 4] {
        let cols: Vec<char> = l.chars().collect();
        assert_ne!(cols[6], ' ', "STATE starts after the six-cell tree: {l:?}");
    }
    assert!(
        lines[rule + 1].starts_with("▸─┬─  "),
        "selected root: {lines:?}"
    );
    assert!(
        lines[rule + 2].starts_with("│ └── "),
        "nested worker: {lines:?}"
    );
    assert!(
        lines[rule + 3].starts_with("└──── "),
        "last root: {lines:?}"
    );
}

/// A worker stays in its own tab, in pane order, and says whom it works
/// under: `↳pC` at the head of its TITLE for a coordinator in the same
/// workspace, `↳w4:pC` for one in another. No row moves for a link.
#[test]
fn a_worker_stays_in_its_tab_and_marks_its_coordinator() {
    let in_tab = |id: &str, pane: &str, window: i64, state: AgentState| {
        let mut a = agent(id, "claude", "goose", state);
        a.pane.workspace_id = "w4".into();
        a.pane.window = window;
        a.pane.pane_id = pane.into();
        a.title = format!("{id} title");
        a
    };
    let agents = || {
        vec![
            in_tab("lead", "w4:pC", 9, AgentState::Idle),
            in_tab("a", "w4:pY", 16, AgentState::Waiting),
            in_tab("b", "w4:pZ", 17, AgentState::Idle),
            in_tab("c", "w4:pQ", 18, AgentState::Idle),
            in_tab("alone", "w4:pS", 14, AgentState::Idle),
        ]
    };
    let tree = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(w, c)| (w.to_string(), c.to_string()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let mut f = fixture();
    // a and b under lead, c under b.
    f.records_with_tree(
        agents(),
        HashSet::new(),
        tree(&[("w4:pY", "w4:pC"), ("w4:pZ", "w4:pC"), ("w4:pQ", "w4:pZ")]),
    );
    let order: Vec<&str> = f.app.agents().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(
        order,
        ["lead", "alone", "a", "b", "c"],
        "tab order, links or not"
    );
    let v = screen(&f, 160, 40);
    let row = |id: &str| {
        v.lines()
            .find(|l| l.contains(&format!("{id} title")))
            .unwrap_or_else(|| panic!("{id}:\n{v}"))
            .to_string()
    };
    assert!(row("a").contains("↳pC a title"), "{}", row("a"));
    assert!(row("b").contains("↳pC b title"), "{}", row("b"));
    assert!(
        row("c").contains("↳pZ c title"),
        "its own coordinator, not the top: {}",
        row("c")
    );
    assert!(!row("lead").contains('↳'), "{}", row("lead"));
    assert!(!row("alone").contains('↳'), "{}", row("alone"));
    // The mark takes room from the title, not from the columns after it.
    let col = |l: &str, needle: &str| l[..l.find(needle).unwrap()].width();
    assert_eq!(
        col(&row("a"), "w4:pY"),
        col(&row("lead"), "w4:pC"),
        "PANE stays put"
    );

    // A coordinator in another workspace is named whole.
    let mut f = fixture();
    let mut away = in_tab("away", "w2:pA", 9, AgentState::Idle);
    away.pane.workspace_id = "w2".into();
    away.pane.session = "elsewhere".into();
    let mut list = agents();
    list.push(away);
    f.records_with_tree(list, HashSet::new(), tree(&[("w2:pA", "w4:pC")]));
    let v = screen(&f, 160, 40);
    let away = v.lines().find(|l| l.contains("away title")).unwrap();
    assert!(away.contains("↳w4:pC away title"), "{away}");

    // A coordinator that is not a live agent here marks nothing.
    let mut f = fixture();
    f.records_with_tree(agents(), HashSet::new(), tree(&[("w4:pY", "w4:pGONE")]));
    assert!(!screen(&f, 160, 40).contains('↳'));
}

/// Edge: a link from a pane to itself is not a coordinator — the row
/// carries no mark rather than pointing at itself.
#[test]
fn a_pane_linked_to_itself_marks_nothing() {
    let mut a = agent("solo", "claude", "goose", AgentState::Idle);
    a.pane.workspace_id = "w4".into();
    a.pane.pane_id = "w4:pC".into();
    a.title = "solo title".into();
    let mut f = fixture();
    f.records_with_tree(
        vec![a],
        HashSet::new(),
        [("w4:pC".to_string(), "w4:pC".to_string())]
            .into_iter()
            .collect(),
    );
    let v = screen(&f, 160, 40);
    assert!(v.contains("solo title") && !v.contains('↳'), "{v}");
}

/// Property: whatever the fold state, the name's width and the screen's,
/// a band fills the width exactly, its name starts in one column and its
/// meters in another, so bands stacked on the screen read as a table.
#[test]
fn every_band_fills_its_width_and_keeps_its_columns() {
    let load = tower_ui::HostLoad {
        cpu_pct: Some(42.0),
        memory: Some(tower_os::host::Memory { used: 1, total: 2 }),
    };
    for width in [30usize, 60, 120] {
        for name in ["a", "local", "db-host-02"] {
            let mut columns = Vec::new();
            for fold in [Some(false), Some(true), None] {
                let line =
                    tower_ui::view::machine_root(&band_of(name, fold, load, "note"), 10, width);
                assert_eq!(line.width(), width, "{name} {fold:?} at {width}");
                let text = line_text(&line);
                let at = |needle: &str| text.find(needle).map(|i| text[..i].width());
                columns.push((at(&format!("@{name}")), at("cpu")));
            }
            assert!(
                columns.windows(2).all(|w| w[0] == w[1]),
                "{name} at {width}: {columns:?}"
            );
        }
    }
    // Edge: a screen narrower than the band still gets no more than its width.
    for width in 0..4 {
        let line = tower_ui::view::machine_root(&band_of("desk", Some(true), load, ""), 4, width);
        assert!(line.width() <= width, "{width}");
    }
}

/// The highlighted row is a bar across the whole screen. Sizing it by measuring
/// its own text invites a width table that disagrees with the one used to cut
/// the line; this one is padded to the area and never measured twice.
#[test]
fn the_selection_bar_reaches_the_right_edge() {
    let f = fixture();
    for width in [40usize, 60, 80, 120] {
        let lines = screen_lines(&f.app, width, 40);
        let bar = lines
            .iter()
            .find(|l| line_text(l).starts_with('▸'))
            .unwrap_or_else(|| panic!("no selection bar at width {width}"));
        assert_eq!(
            bar.width(),
            width,
            "the bar stops short at width {width}: {:?}",
            line_text(bar)
        );
    }
}

/// A line wider than the terminal wraps, and one wrapped line pushes an
/// alternate-screen layout off the bottom.
#[test]
fn no_line_exceeds_the_terminal_width() {
    let mut f = fixture();
    let mut a = agent("claude-7", "claude", "collab-bot", AgentState::Waiting);
    a.task = "awaiting approval — 아주 긴 한글 작업 설명이 들어간다".into();
    a.cwd = "/Users/jordan/ai-folder/dev/very/long/path/that/overflows/narrow/terminal".into();
    let mut b = agent("claude-3", "claude", "ai", AgentState::DoneUnread);
    b.cwd = "/Users/jordan/VSCodeWorkspace/another/deeply/nested/project/folder".into();
    f.records(vec![a, b]);

    for width in [20usize, 40, 60, 80] {
        for line in screen_lines(&f.app, width, 24) {
            assert!(
                line.width() <= width,
                "width {width}: a line is {} columns: {:?}",
                line.width(),
                line_text(&line)
            );
        }
    }
}

/// More agents than rows: the screen still fits, and the highlighted one is
/// always on it.
#[test]
fn the_screen_fits_and_keeps_the_cursor_visible() {
    let mut f = fixture();
    let mut many = Vec::new();
    for i in 0..20 {
        many.push(agent(
            &format!("claude-{i:02}"),
            "claude",
            &format!("sess-{i}"),
            AgentState::Waiting,
        ));
    }
    f.records(many);

    for cursor in [0usize, 10, 19] {
        for _ in 0..cursor {
            f.tap("j");
        }
        let selected = f.app.selected().unwrap().pane.session.clone();
        let lines = screen_lines(&f.app, 100, 15);
        assert!(lines.len() <= 15, "cursor={cursor}: {} lines", lines.len());
        let v = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            v.contains('▸'),
            "cursor={cursor}: the bar is off screen:\n{v}"
        );
        assert!(
            v.contains(&selected),
            "cursor={cursor}: the selected session is off screen:\n{v}"
        );
        // Back to the top for the next round.
        for _ in 0..cursor {
            f.tap("k");
        }
    }
}

/// The key guide is checked as a value: at any real terminal width the whole
/// list is wider than the screen and the tail is cut.
#[test]
fn the_help_line_lists_the_keys() {
    let text = line_text(&help_line(&[
        ("g", "git"),
        ("L", "idle loop"),
        ("T", "schedule"),
    ]));
    for want in ["git", "idle loop", "schedule"] {
        assert!(text.contains(want), "{text}");
    }
    let f = fixture();
    let v = screen(&f, 200, 40);
    for want in ["git", "schedule", "broadcast"] {
        assert!(v.contains(want), "the guide should list {want:?}:\n{v}");
    }
    // Off the guide, as a key that still works: `o` marks read without
    // jumping (enter does both).
    assert!(!v.contains(" o read"), "`o` is off the guide:\n{v}");
}

/// A confirmation that is armed but invisible is the worst of both.
///
/// If the detail card drew no footer, a `W` pressed while the card was open
/// would arm a broadcast to every session that nothing on screen mentioned —
/// and the next `y` would send it.
#[test]
fn a_confirmation_is_visible_from_every_screen() {
    let mut f = fixture();
    f.tap("i");
    assert!(matches!(f.app.screen(), tower_ui::Screen::Info(_)));
    f.tap("W");
    let v = screen(&f, 200, 40);
    assert!(
        v.contains("[y/N]"),
        "the prompt has to be on the screen that armed it:\n{v}"
    );

    // And at the narrowest width this renderer supports. `screen_lines` clamps
    // a footer rather than wrapping it, so a prompt that puts its answer at the
    // end loses the answer — and an armed action reads as a key that did
    // nothing.
    let narrow = screen(&f, 60, 40);
    assert!(
        narrow.contains("[y/N]"),
        "the answer fell off a 60-column screen:\n{narrow}"
    );
}

/// The panel is a screenshot with a name on it, and the name has to be right.
///
/// A capture whose pane is never checked would, after moving the cursor, show
/// the previous agent's screen under the new agent's title until the next
/// capture landed.
#[test]
fn the_preview_belongs_to_the_highlighted_row() {
    let mut f = fixture();
    f.feed(Event::Preview {
        pane_id: "p-claude-7".into(),
        body: "the first agent's screen".into(),
    });
    let lines = screen_lines(&f.app, 120, 40);
    let preview_heading = lines
        .iter()
        .find(|line| line_text(line).contains("collab-bot preview"))
        .expect("the preview heading");
    let heading_text = line_text(preview_heading);
    assert!(heading_text.starts_with(" ▼ collab-bot preview ═"));
    for label in ["collab-bot", "preview"] {
        let span = preview_heading
            .iter()
            .find(|span| span.content == label)
            .unwrap_or_else(|| panic!("missing {label:?} in the preview heading"));
        assert_eq!(span.style.fg, Some(Color::Rgb(0x5f, 0xaf, 0xff)));
        assert_eq!(span.style.bg, Some(Color::Rgb(0x1f, 0x30, 0x3a)));
    }

    let v = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(v.contains("collab-bot preview"), "{v}");
    assert!(v.contains("the first agent's screen"), "{v}");
    let preview_body = lines
        .iter()
        .find(|line| line_text(line).contains("the first agent's screen"))
        .expect("preview body");
    assert!(
        preview_body
            .spans
            .iter()
            .all(|span| span.style.bg == Some(Color::Rgb(0x1f, 0x30, 0x3a))),
        "the entire preview row needs the preview background"
    );

    f.tap("j"); // now looking at claude-3, whose pane has not been captured
    let v = screen(&f, 120, 40);
    assert!(
        !v.contains("the first agent's screen"),
        "the other agent's screen is not this agent's preview:\n{v}"
    );
}

#[test]
fn the_selected_sessions_configured_loop_message_wears_the_loop_colour() {
    let mut f = fixture_with(StubTower::default());
    let mut a = agent("codex-loop", "codex", "loop-work", AgentState::Idle);
    a.session_id = "conversation-loop".into();
    let mut lease = LoopLease::new(
        &a,
        t0(),
        std::time::Duration::from_secs(30 * 60),
        std::time::Duration::from_secs(168 * 60 * 60),
        None,
        String::new(),
    );
    lease.message = "이 세션의 설정된 지시만 이어가세요.".into();
    LoopStore::new(f.store.dir()).unwrap().arm(lease).unwrap();
    f.records(vec![a]);
    f.feed(Event::Preview {
        pane_id: "p-codex-loop".into(),
        body: "current work remains visible".into(),
    });

    let lines = screen_lines(&f.app, 120, 40);
    let loop_line = lines
        .iter()
        .find(|line| line_text(line).contains("이 세션의 설정된 지시"))
        .expect("configured loop instruction above the preview");
    assert!(line_text(loop_line).starts_with("↻ LOOP 30m · "));
    // The loop's own colour, not `Waiting`'s yellow and not `DoneUnread`'s
    // accent: a loop is attached to an agent, not a state it is in.
    assert_eq!(
        loop_line.spans[0].style.fg,
        Some(Color::Rgb(0xe0, 0x7b, 0xb8))
    );
    let v = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(v.contains("current work remains visible"), "{v}");
}

/// The values reach a real buffer intact — and the buffer is ratatui's own test
/// backend, never the terminal this test is running in.
#[test]
fn the_tower_draws_into_a_buffer() {
    let mut f = fixture();
    f.app.set_size(100, 20);
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|frame| tower_ui::view::render(frame, frame.area(), &f.app))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let painted: String = buffer
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(painted.contains("herdr-tower"), "{painted}");
    assert!(painted.contains("collab-bot"), "{painted}");

    // The bar's background is what makes the selected row readable at a glance.
    let bar_row = (0..20)
        .find(|y| buffer[(0, *y)].symbol() == "▸")
        .expect("a selection bar somewhere on the screen");
    assert_eq!(
        buffer[(99, bar_row)].bg,
        Color::Rgb(0x3a, 0x3a, 0x3a),
        "the highlight has to reach the last column"
    );
}

/// A row wears what is true of it: the Discord mark for a session a bot drives,
/// and the branch when the folder is a worktree.
#[test]
fn a_row_wears_its_marks() {
    let mut f = fixture();
    let mut a = agent("claude-7", "claude", "collab-bot", AgentState::Waiting);
    a.cwd = "/w/feature".into();
    let mut branches = HashMap::new();
    branches.insert("/w/feature".to_string(), "feat/tower".to_string());
    f.feed(Event::Records {
        agents: vec![a],
        branches,
        tab_roots: HashSet::new(),
        parents: Default::default(),
        daemon: None,
        now: t0(),
    });
    let mut wired = HashMap::new();
    wired.insert("collab-bot".to_string(), "⌁운영".to_string());
    f.feed(Event::Local(LocalInfo {
        wired,
        ..Default::default()
    }));

    let v = screen(&f, 200, 40);
    assert!(
        v.contains("⌁운영"),
        "the Discord mark carries its label:\n{v}"
    );
    assert!(v.contains("⎇ feat/tower"), "{v}");
    assert!(!v.contains("#52"), "no PR looked up yet:\n{v}");

    f.feed(Event::Prs(HashMap::from([("/w/feature".to_string(), 52)])));
    let v = screen(&f, 200, 40);
    assert!(
        v.contains("⎇ feat/tower #52"),
        "the open PR sits beside the branch:\n{v}"
    );
}

fn remote_agent(pane: &str, status: &str, ws: &str) -> tower_ui::RemoteAgent {
    tower_ui::RemoteAgent {
        workspace: ws.into(),
        pane_id: pane.into(),
        agent: "codex".into(),
        status: status.into(),
        id: format!("codex-{pane}"),
        loop_state: None,
        schedules: Vec::new(),
        title: "Diagnose startup".into(),
        tab: String::new(),
        session_id: "f385489f-aaaa".into(),
        cwd: "/home/alice/work".into(),
    }
}

/// Agents on a saved machine are drawn under the list in the local rows'
/// columns, a rule per workspace naming the machine; the cursor never lands on
/// one, and a machine that did not answer says so.
#[test]
fn agents_on_saved_machines_are_drawn_under_the_list_and_never_selected() {
    use tower_ui::RemoteMachine;
    let mut f = fixture();
    let before = f.app.cursor();
    let agents_before = f.app.agents().len();
    f.feed(Event::Remote(vec![
        RemoteMachine {
            label: "buildbox".into(),
            target: "u@buildbox".into(),
            agents: vec![
                remote_agent("w5M:p2", "working", "partitions"),
                remote_agent("w5M:p3", "idle", "partitions"),
                remote_agent("w7:p1", "done", "[w7]"),
            ],
            error: None,
            stale_since: None,
            ..Default::default()
        },
        RemoteMachine {
            label: "db-host-02".into(),
            target: "db-host-02".into(),
            agents: Vec::new(),
            error: Some("not reachable".into()),
            stale_since: None,
            ..Default::default()
        },
        RemoteMachine {
            label: "quiet".into(),
            target: "quiet".into(),
            agents: Vec::new(),
            error: None,
            stale_since: None,
            ..Default::default()
        },
    ]));
    let v = screen(&f, 200, 80);
    assert!(v.contains("\n ▼ @buildbox "), "{v}");
    assert!(v.contains("├─── partitions ──"), "{v}");
    assert!(
        v.contains("└─── [w7] ──"),
        "a rule per workspace, the last closing the trunk:\n{v}"
    );
    assert!(v.contains("\n   @db-host-02 · not reachable"), "{v}");
    assert!(
        v.contains("\n   @quiet ") && !v.contains("@quiet ("),
        "an empty machine is its band alone, with no count and nothing to fold:\n{v}"
    );
    assert!(
        v.contains("\n ▼ @local "),
        "the local machine is a root too:\n{v}"
    );
    assert_eq!(
        v.matches("@local").count(),
        1,
        "once, not per workspace:\n{v}"
    );
    assert!(
        v.contains("│ ├──── ● working ▌codex"),
        "rows hang off the trunk:\n{v}"
    );
    assert!(v.contains("│ └──── · idle"), "{v}");
    assert!(
        v.contains("  └──── ✔ done"),
        "under the last workspace, no trunk:\n{v}"
    );
    assert!(v.contains("f385489f"), "the short session id:\n{v}");
    assert!(v.contains("Diagnose startup"), "{v}");
    assert_eq!(f.app.cursor(), before);
    assert_eq!(f.app.agents().len(), agents_before);
}

/// A machine that reports its load carries it on its band, after its name and
/// before the note: the header's meters, on the band's blue.
#[test]
fn a_machines_band_carries_its_load() {
    use tower_os::host::{CpuTicks, Memory};
    let mut f = fixture();
    let machine = |cpu, cpu_before, memory| tower_ui::RemoteMachine {
        label: "desk".into(),
        target: "desk".into(),
        agents: vec![remote_agent("w5M:p2", "working", "partitions")],
        cpu,
        cpu_before,
        memory,
        ..Default::default()
    };
    let mem = Some(Memory {
        used: 61,
        total: 100,
    });
    // The first read: memory, but no share yet.
    f.feed(Event::Remote(vec![machine(
        Some(CpuTicks {
            busy: 10,
            total: 100,
        }),
        None,
        mem,
    )]));
    let v = screen(&f, 200, 80);
    // `@desk (1)` is padded to `@local (2)`, the widest name on screen.
    assert!(v.contains("\n ▼ @desk (1)   mem ■■■■■  61% "), "{v}");
    assert!(!v.contains("cpu ■"), "{v}");
    f.feed(Event::Remote(vec![machine(
        Some(CpuTicks {
            busy: 33,
            total: 200,
        }),
        Some(CpuTicks {
            busy: 10,
            total: 100,
        }),
        mem,
    )]));
    let v = screen(&f, 200, 80);
    assert!(
        v.contains("\n ▼ @desk (1)   cpu ■■■■■  23%  mem ■■■■■  61% "),
        "{v}"
    );
    // @local's band carries this machine's load too, in the same column.
    f.feed(Event::Host(tower_ui::HostLoad {
        cpu_pct: Some(87.0),
        memory: Some(Memory {
            used: 70,
            total: 100,
        }),
    }));
    let v = screen(&f, 200, 80);
    let band = |name: &str| v.lines().find(|l| l.starts_with(name)).unwrap().to_string();
    let (local, desk) = (band(" ▼ @local"), band(" ▼ @desk"));
    assert!(
        local.starts_with(" ▼ @local (2)  cpu ■■■■■  87%  mem ■■■■■  70%"),
        "{local}"
    );
    for word in ["cpu", "mem"] {
        assert_eq!(
            local[..local.find(word).unwrap()].width(),
            desk[..desk.find(word).unwrap()].width(),
            "{word} lines up:\n{local}\n{desk}"
        );
    }

    let load = tower_ui::HostLoad {
        cpu_pct: Some(23.0),
        memory: mem,
    };
    let line =
        tower_ui::view::machine_root(&band_of("desk", Some(false), load, "R to control"), 4, 80);
    let band = ratatui::style::Color::Rgb(0x1d, 0x4e, 0xd8);
    assert!(
        line.spans.iter().all(|s| s.style.bg == Some(band)),
        "the meters sit on the band"
    );
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.starts_with(" ▼ @desk  cpu ■■■■■  23%  mem ■■■■■  61% · R to control"),
        "{text}"
    );
    assert_eq!(line.width(), 80);
}

/// The pane column starts where the local rows' does, so the two read as one
/// table.
#[test]
fn a_remote_row_lines_up_with_the_local_rows() {
    let mut f = fixture();
    let mut a = agent("one", "codex", "s", AgentState::Working);
    a.pane.pane_id = "w1:p1".into();
    f.records(vec![a]);
    f.feed(Event::Remote(vec![tower_ui::RemoteMachine {
        label: "gpu".into(),
        target: "u@gpu".into(),
        agents: vec![remote_agent("w5M:p2", "working", "partitions")],
        error: None,
        stale_since: None,
        ..Default::default()
    }]));
    let v = screen(&f, 200, 80);
    let col = |pane: &str| {
        let line = v.lines().find(|l| l.contains(pane)).unwrap();
        line[..line.find(pane).unwrap()].width()
    };
    assert_eq!(col("w5M:p2"), col("w1:p1"), "{v}");
}

/// Machine names are blue, and only once something remote is on screen does
/// a local rule name its machine at all.
#[test]
fn a_machine_is_a_blue_band_and_local_is_one_only_beside_a_remote() {
    let f = fixture();
    let v = screen(&f, 200, 80);
    assert!(
        !v.contains("@local"),
        "nothing remote, nothing to tell apart:\n{v}"
    );

    let line = tower_ui::view::machine_root(
        &band_of("gpu", None, Default::default(), "not reachable"),
        3,
        60,
    );
    let name = line.spans.iter().find(|s| s.content == " @gpu").unwrap();
    let band = ratatui::style::Color::Rgb(0x1d, 0x4e, 0xd8);
    assert_eq!(name.style.bg, Some(band));
    assert_eq!(
        name.style.fg,
        Some(ratatui::style::Color::Rgb(0xff, 0xff, 0xff))
    );
    assert!(
        line.spans.iter().all(|s| s.style.bg == Some(band)),
        "the whole line is the band"
    );
    assert_eq!(line.width(), 60, "the band reaches the edge");
}

/// A folder that is no tower worktree has no branch to show, and its open PR
/// still says itself.
#[test]
fn an_open_pr_shows_without_a_worktree_branch() {
    let mut f = fixture();
    let mut a = agent("claude-7", "claude", "collab-bot", AgentState::Waiting);
    a.cwd = "/w/plain".into();
    f.feed(Event::Records {
        agents: vec![a],
        branches: HashMap::new(),
        tab_roots: HashSet::new(),
        parents: Default::default(),
        daemon: None,
        now: t0(),
    });
    f.feed(Event::Prs(HashMap::from([("/w/plain".to_string(), 7)])));
    let v = screen(&f, 200, 40);
    assert!(v.contains(" #7"), "{v}");
    assert!(!v.contains("⎇"), "{v}");
}

/// The card shows herdr's coordinate and nothing it cannot measure.
///
/// No window number derived from a tab ID and no pid, which `pane.list` never
/// returns — either would be a fabricated number printed as if measured.
#[test]
fn the_detail_card_reports_only_what_herdr_knows() {
    let mut f = fixture();
    let mut a = agent("claude-7", "claude", "collab-bot", AgentState::Waiting);
    a.cwd = "/w/app".into();
    a.session_id = "0192abcd-ef01-2345-6789-abcdef012345".into();
    a.ask = "approval needed".into();
    f.records(vec![a]);

    f.tap("i");
    let v = screen(&f, 200, 40);
    for want in [
        "herdr-tower — detail",
        "collab-bot",
        "herdr",
        "p-claude-7",
        "approval needed",
        "Discord    not connected",
        "session    0192abcd → if it dies",
    ] {
        assert!(v.contains(want), "the card should say {want:?}:\n{v}");
    }
    assert!(
        !v.contains("pid"),
        "no pid is reported, because none is known"
    );

    // `i` again closes it, and so does esc.
    f.tap("i");
    assert_eq!(f.app.screen(), &tower_ui::Screen::List);
}

/// A warning of several lines gets a row each.
///
/// A startup warning can be a heading, a command to run, and what to do
/// afterwards. Carried as a single `Line` the newlines were drawn as characters
/// rather than breaks, so the commands the warning exists to hand over were
/// collapsed into the heading — and the row count that reserves footer space
/// only ever counted one.
#[test]
fn a_multi_line_warning_is_not_collapsed_into_one_row() {
    let mut f = fixture();
    f.app.set_startup_note(
        "The state store is shared with another server:\n  mv ~/.local/state/shared ~/.local/state/herdr-tower\n  Once that is tidied up, run herdr-tower init again."
            .into(),
    );

    let v = screen(&f, 100, 40);
    assert!(
        v.contains("⚠ The state store is shared with another server:"),
        "{v}"
    );
    assert!(
        v.contains("\n  mv ~/.local/state/shared ~/.local/state/herdr-tower\n"),
        "the command needs a row of its own to be read, let alone pasted:\n{v}"
    );
    assert!(
        v.contains("\n  Once that is tidied up, run herdr-tower init again."),
        "{v}"
    );
    assert!(
        !v.lines().any(|l| l.matches("mv ").count() > 1),
        "no row holds two of them:\n{v}"
    );
}

/// Fresh data does not read as "just now old".
///
/// The detail card glued a word onto the elapsed span. Under a minute that
/// span is the words "just now", and fresh is the common case — so the broken
/// wording was the one most often on screen.
#[test]
fn a_span_under_a_minute_is_not_suffixed() {
    let f = fixture();
    let v = screen(&f, 120, 40);
    assert!(!v.contains("just now old"), "{v}");
    assert!(!v.contains("just now ago"), "{v}");
}

/// An editor whose cursor is off screen looks like a dead key.
///
/// `screen_lines` clamps the footer to the width instead of wrapping it, and
/// this renderer supports 60 columns. A message longer than the room left after
/// the label used to have its tail cut off the right edge — the caret included —
/// so the field looked frozen while it was still taking every keystroke.
#[test]
fn the_broadcast_caret_stays_on_screen_at_sixty_columns() {
    let mut f = fixture();
    f.tap("B");
    for c in "deploy the card refresh and tell me when it is live".chars() {
        f.feed(Event::Key(key_event(&c.to_string())));
    }
    assert_eq!(
        f.app.input().unwrap().value().chars().count(),
        51,
        "the whole message is in the field"
    );

    let v = screen(&f, 60, 24);
    let line = v
        .lines()
        .find(|l| l.contains("broadcast:"))
        .expect("the footer draws the editor");
    assert!(line.width() <= 60, "{:?} is {} columns", line, line.width());
    // The tail of what was typed, and the caret cell after it, are both on the
    // line: the window ends at the caret rather than at the message's start.
    assert!(
        line.contains("when it is live"),
        "the end of the message is what is shown:\n{line}"
    );

    // And it holds while the caret moves back through the message.
    for i in 0..51 {
        f.tap("left");
        let v = screen(&f, 60, 24);
        let line = v.lines().find(|l| l.contains("broadcast:")).unwrap();
        assert!(line.width() <= 60, "after {i} lefts: {line:?}");
        let (_, at, _) = f.app.input().unwrap().window(60 - "⌨ broadcast: ".width());
        assert!(
            line.contains(&at),
            "the caret's character is on screen after {i} lefts:\n{line}"
        );
    }
}

/// The version belongs in the header of a tower a person opened — the popup
/// that `prefix+a` throws up, or a plain `herdr-tower` — because that is the
/// copy they are looking at and the one they would report a bug against.
///
/// Everywhere, including the dock: that is the one people reach by clicking.
#[test]
fn the_header_carries_the_version_everywhere() {
    let popup = fixture_with(StubTower::default());
    let lines = screen_lines(&popup.app, 120, 40);
    let header = line_text(&lines[0]);
    assert!(
        header.contains("v-test"),
        "a popup shows which build it is: {header:?}"
    );

    // 🚨 And in the dock, which is how most people open this — one click in
    // the sidebar. Hiding the build there hid it from the surface actually
    // being looked at.
    let docked = fixture_with(StubTower {
        self_pane: Some("w9:p1".into()),
        ..StubTower::default()
    });
    let lines = screen_lines(&docked.app, 120, 40);
    let header = line_text(&lines[0]);
    assert!(
        header.contains("v-test"),
        "the dock shows its build too: {header:?}"
    );
}

/// The driver lays each frame out once and asks it everything: which agent is
/// on which line, where the guide's keys are, what the preview captures next.
/// Each answer is checked against what the frame actually draws — not against
/// the helpers, which are now the same layout and would agree with any mistake.
#[test]
fn one_layout_answers_every_question_about_the_frame() {
    use crossterm::event::KeyCode;
    let mut f = fixture();
    // Wide enough for the whole key guide, so every key in it is checked.
    f.app.set_size(200, 40);
    let mut a = agent("claude-preview", "claude", "work", AgentState::Idle);
    a.session_id = "da5bd140-fc58-4111-a5fb-66d0181f3878".into();
    a.title = "the preview agent".into();
    let mut b = agent("claude-zz-other", "claude", "work", AgentState::Idle);
    b.title = "the other agent".into();
    f.records(vec![a, b]);
    f.feed(Event::Preview {
        pane_id: "p-claude-preview".into(),
        body: "preview content".into(),
    });
    let (w, h) = f.app.size();
    let composed = tower_ui::view::compose(&f.app, w.into(), h.into());
    let text: Vec<String> = composed.lines.iter().map(line_text).collect();

    // Every line said to hold an agent draws that agent's row.
    let rows: Vec<_> = composed
        .agents
        .iter()
        .enumerate()
        .filter_map(|(y, i)| i.map(|i| (y, i)))
        .collect();
    assert_eq!(rows.len(), 2, "{text:#?}");
    for (y, i) in rows {
        let title = &f.app.agents()[i].title;
        assert!(
            text[y].contains(title.as_str()),
            "line {y} is {:?}, not {title:?}",
            text[y]
        );
    }

    // Every key sits where its name is drawn.
    let at = |row: usize, from: usize, to: usize| -> String {
        let mut col = 0;
        tower_text::glyphs(&text[row])
            .filter(|glyph| {
                let here = col;
                col += tower_text::glyph_width(glyph);
                (from..to).contains(&here)
            })
            .collect()
    };
    assert!(!composed.keys.is_empty());
    for k in &composed.keys {
        let drawn = at(k.row, k.from, k.to);
        // The words after a key are its button too: a space, then the words.
        if let Some(words) = drawn.strip_prefix(' ') {
            assert!(!words.is_empty() && !words.contains(" · "), "{drawn:?}");
            continue;
        }
        let name = match k.key {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Up => "↑".into(),
            KeyCode::Down => "↓".into(),
            other => panic!("unexpected key {other:?}"),
        };
        assert_eq!(drawn, name, "key {:?} at {}..{}", k.key, k.from, k.to);
    }

    // The preview asks for the selected pane, and for exactly the rows drawn
    // under its heading — everything down to the key guide.
    assert_eq!(f.app.selected().unwrap().id, "claude-preview");
    let (pane, want) = composed.preview.clone().expect("room for a preview");
    assert_eq!(pane, "p-claude-preview");
    let heading = text
        .iter()
        .position(|l| l.starts_with(" ▼ "))
        .expect("heading");
    let footer_starts = text.iter().rposition(|l| l.contains("↑↓/jk move")).unwrap();
    assert!(
        !text[footer_starts - 1].starts_with('·'),
        "no dot line closes it"
    );
    assert_eq!(usize::from(want), footer_starts - heading - 1);
}
