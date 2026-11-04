// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The list in a narrow pane: STATE, the vendor's bar and a TITLE that takes all
//! the rest, rather than a grid cut at the right edge.

mod common;

use common::*;
use tower_ui::line_text;
use tower_ui::view::{column_header, screen_lines};

fn at(width: u16) -> Fixture {
    let mut f = fixture();
    f.app.set_size(width, 40);
    f
}

#[test]
fn a_narrow_header_names_only_what_it_shows() {
    for width in [40, 70, 89] {
        let head = line_text(&column_header(width));
        assert!(head.contains("STATE") && head.contains("TITLE"), "{head}");
        for gone in ["AGENT", "PANE", "TASK", "DIR", "SID"] {
            assert!(!head.contains(gone), "{width}: {head}");
        }
    }
}

/// The boundary: 90 columns has room for every column up to TASK.
#[test]
fn the_grid_comes_back_at_ninety_columns() {
    let head = line_text(&column_header(90));
    for name in ["STATE", "AGENT", "PANE", "TITLE", "TASK"] {
        assert!(head.contains(name), "{head}");
    }
}

/// Nothing is drawn past the edge at any narrow width, down to none at all.
#[test]
fn no_narrow_screen_overflows_or_panics() {
    for width in 0..90u16 {
        let f = at(width);
        for line in screen_lines(&f.app, width.into(), 40) {
            let drawn = tower_text::width(&line_text(&line));
            assert!(drawn <= width.into(), "{width}: {drawn}");
        }
    }
}

fn long_titled(id: &str) -> tower_state::Agent {
    let mut a = agent(id, "claude", "s", tower_state::AgentState::Working);
    a.pane.pane_id = "w1:p1".into();
    a.title = "a title much too long for the wide grid's thirty cells".into();
    a
}

fn with_rows(width: u16) -> Fixture {
    let mut f = at(width);
    f.records(vec![long_titled("one"), long_titled("two")]);
    f
}

fn rows(f: &Fixture, width: usize) -> Vec<String> {
    screen_lines(&f.app, width, 40)
        .iter()
        .map(line_text)
        .collect()
}

/// What the grid cut at thirty cells fits whole at 80, with the columns that
/// used to take its room gone.
#[test]
fn a_narrow_row_gives_the_title_the_room() {
    let f = with_rows(80);
    let screen = rows(&f, 80);
    let row = screen
        .iter()
        .find(|l| l.contains("a title much too long"))
        .unwrap_or_else(|| panic!("no row in {screen:#?}"));
    assert!(row.contains("thirty cells"), "{row}");
    assert!(!row.contains("w1:p1"), "{row}");
    assert!(!row.contains("claude"), "{row}");
}

/// The wide grid at the boundary still cuts the title in its own column.
#[test]
fn the_grid_keeps_its_columns_at_ninety() {
    let f = with_rows(90);
    let screen = rows(&f, 90);
    assert!(screen.iter().any(|l| l.contains("w1:p1")), "{screen:#?}");
}

/// Real rows stay inside the screen at every narrow width.
#[test]
fn no_narrow_row_overflows() {
    for width in 0..90u16 {
        let f = with_rows(width);
        for line in rows(&f, width.into()) {
            let drawn = tower_text::width(&line);
            assert!(drawn <= width.into(), "{width}: {drawn}: {line}");
        }
    }
}

/// See it: `cargo test -p tower_ui --test compact_list show -- --ignored --nocapture`.
#[test]
#[ignore]
fn show() {
    let f = with_rows(70);
    for line in rows(&f, 70).iter().take(9) {
        println!("|{line}|");
    }
}
