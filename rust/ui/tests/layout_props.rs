// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What must hold of the layout at every size, checked at every width from 1
//! to 220 and a spread of heights, against generated screens: names long and
//! short, wide glyphs and narrow, session IDs of every shape, with and without
//! a preview, a question or an error on screen.
//!
//! Most of the layout bugs these guard against were found one width at a time,
//! in review — a long workspace name pushing the session ID off the preview
//! heading, a key guide cut off at 120 columns, a heading that overflowed at
//! 60. A property checked at every width finds the next one first.
//!
//! Hand-rolled rather than a property framework: the generator is seeded, so a
//! failure names its seed, width and height and replays exactly, and nothing is
//! added to the build.

mod common;

use std::collections::HashSet;

use common::*;
use crossterm::event::KeyCode;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tower_state::AgentState;
use tower_ui::view::compose;
use tower_ui::{clamp_line, line_text, pad_right, truncate_width, Event};
use unicode_width::UnicodeWidthStr;

/// xorshift64*: small, fast, and the same sequence on every machine.
struct Rng(tower_testrng::Rng);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(tower_testrng::Rng::new(seed))
    }

    fn below(&mut self, n: usize) -> usize {
        self.0.below(n)
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.below(from.len())]
    }

    /// Text of up to `max` pieces, narrow and wide glyphs mixed.
    fn text(&mut self, max: usize) -> String {
        const PIECES: &[&str] = &[
            "a", "Z", "fix", "review", "-", "/", " ", ".", "·", "…", "세", "션", "界", "🗼", "é",
            "PR #7", "auth-api",
        ];
        (0..self.below(max + 1))
            .map(|_| self.pick(PIECES))
            .collect()
    }

    fn session_id(&mut self) -> String {
        match self.below(6) {
            0 => String::new(),
            1 => "c376295e-95bf-45d2-9cd0-e26b0e837218".into(),
            2 => format!("01a0b8e0-99{:02x}-7af1-b920-ab518ce5d454", self.below(256)),
            3 => "conversation-loop".into(),
            4 => "세션아이디테스트12345".into(),
            _ => "short".into(),
        }
    }
}

/// A screen the generator made up, `seed` for replaying it.
fn scenario(seed: u64) -> Fixture {
    let mut rng = Rng::new(seed);
    let mut f = fixture();
    let states = [
        AgentState::Working,
        AgentState::Waiting,
        AgentState::DoneUnread,
        AgentState::Idle,
        AgentState::Error,
    ];
    let agents = (0..1 + rng.below(6))
        .map(|i| {
            let kind = rng.pick(&["claude", "codex", "gemini"]);
            let workspace = match rng.below(3) {
                0 => "work".to_string(),
                1 => "feat-auth-token-refresh-and-session-rotation".to_string(),
                _ => rng.text(12),
            };
            let mut a = agent(
                &format!("{kind}-{i}"),
                kind,
                &workspace,
                states[rng.below(5)],
            );
            a.title = rng.text(20);
            a.task = rng.text(20);
            a.label = if rng.chance(30) {
                "auth-api-3f2a91cc".into()
            } else {
                rng.text(6)
            };
            a.session_id = rng.session_id();
            a.cwd = format!("/Users/x/{}", rng.text(8));
            a
        })
        .collect::<Vec<_>>();
    // Parallax handoffs waiting: one more header line, with another
    // program's text of any width in it. A generator of its own, so the
    // screens the other properties were tuned on stay the ones they get.
    let mut rng2 = Rng::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5041_5241);
    if rng2.chance(30) {
        let handoffs = (0..1 + rng2.below(3))
            .map(|i| {
                parallax_handoff(
                    &format!("h{i}"),
                    &rng2.text(16),
                    rng2.pick(&["needs_human", "blocked", "waiting_approval", "novel"]),
                    if rng2.chance(50) {
                        tower_state::Attention::Urgent
                    } else {
                        tower_state::Attention::Action
                    },
                    &rng2.text(60),
                )
            })
            .collect();
        parallax_snapshot(&f.store, handoffs);
    }
    f.records(agents);
    // The preview is the selected agent's: the cursor starts on the first row
    // the list draws, which is not the first agent generated.
    let selected = f
        .app
        .selected()
        .expect("a selected agent")
        .pane
        .pane_id
        .clone();
    if rng.chance(60) {
        f.feed(Event::Preview {
            pane_id: selected,
            body: rng.text(40),
        });
    }
    match rng.below(4) {
        0 => f.tap("W"), // a question on the prompt line
        1 => f.feed(Event::Health(Err(format!(
            "{}\n  {}",
            rng.text(30),
            rng.text(30)
        )))),
        _ => {}
    }
    f
}

/// The glyphs of `line` in columns `from..to`.
///
/// 🚨 By glyph, through `tower_text`, not by character with a table of its own.
/// A model that measured differently from the code under test agreed with it
/// only for the glyphs both got right — and the ones they disagreed about (a
/// heart with its selector, a halfwidth sound mark, lam-alef) are exactly the
/// ones the layout gets wrong when it is wrong.
fn columns(line: &str, from: usize, to: usize) -> String {
    let mut col = 0;
    tower_text::glyphs(line)
        .filter(|glyph| {
            let here = col;
            col += tower_text::glyph_width(glyph);
            (from..to).contains(&here)
        })
        .collect()
}

fn key_name(key: KeyCode) -> String {
    match key {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        other => panic!("the guide has no key {other:?}"),
    }
}

/// Every key the list's guide offers.
fn list_keys() -> HashSet<KeyCode> {
    let mut keys: HashSet<KeyCode> = "jkigLTWCBrq/[]=".chars().map(KeyCode::Char).collect();
    keys.insert(KeyCode::Enter);
    keys.insert(KeyCode::Up);
    keys.insert(KeyCode::Down);
    keys
}

const SEEDS: u64 = 12;
const HEIGHTS: [usize; 4] = [6, 12, 24, 40];

/// The widths one seed checks.
///
/// Every narrow width, where the layout's own thresholds are, and a sample of
/// the rest — a different sample per seed, so the seeds together still sweep the
/// range while one seed stays quick. 🚨 Every width used to be checked for every
/// seed and height: 12 × 4 × 220 compositions, which had grown to 23 s in one
/// test. The narrow end is exhaustive because that is where the layout changes
/// shape (a heading cut to its `…`, a preview that cannot open, a column that
/// drops out); past that, width is a smooth parameter and sampling it finds the
/// same faults.
fn widths_for(seed: u64) -> Vec<usize> {
    let mut rng = tower_testrng::Rng::new(seed ^ 0x_5A3D_1D7A);
    let mut widths: Vec<usize> = (1..=16).collect();
    widths.extend([20, 24, 40, 60, 80, 120, 200, 220]);
    widths.extend((0..10).map(|_| 17 + rng.below(204)));
    widths.sort_unstable();
    widths.dedup();
    widths
}

#[test]
fn every_line_fits_the_screen_and_the_screen_is_filled() {
    for seed in 0..SEEDS {
        let f = scenario(seed);
        for height in HEIGHTS {
            for width in widths_for(seed) {
                let c = compose(&f.app, width, height);
                assert_eq!(c.lines.len(), height, "seed {seed} {width}x{height}");
                for (y, line) in c.lines.iter().enumerate() {
                    assert!(
                        tower_ui::line_width(line) <= width,
                        "seed {seed} {width}x{height}: line {y} is {} wide: {:?}",
                        tower_ui::line_width(line),
                        line_text(line)
                    );
                }
            }
        }
    }
}

/// Every key the frame says is somewhere is on screen, inside the width, not
/// on top of another, and on the very characters that name it.
#[test]
fn every_key_is_where_its_name_is_drawn() {
    for seed in 0..SEEDS {
        let f = scenario(seed);
        for height in HEIGHTS {
            for width in widths_for(seed) {
                let c = compose(&f.app, width, height);
                let text: Vec<String> = c.lines.iter().map(line_text).collect();
                let mut taken: HashSet<(usize, usize)> = HashSet::new();
                for k in &c.keys {
                    let at = format!("seed {seed} {width}x{height}: {k:?}");
                    assert!(k.row < height && k.from < k.to && k.to <= width, "{at}");
                    for col in k.from..k.to {
                        assert!(taken.insert((k.row, col)), "{at} overlaps another key");
                    }
                    let drawn = columns(&text[k.row], k.from, k.to);
                    if let Some(words) = drawn.strip_prefix(' ') {
                        // The words after a key, which the width may have cut
                        // down to the space before them — that space is part
                        // of the button too.
                        let cut = k.to + 1 >= width;
                        assert!(!words.trim().is_empty() || cut, "{at}: {drawn:?}");
                        assert!(!words.contains(" · "), "{at}: {drawn:?}");
                        continue;
                    }
                    let name = key_name(k.key);
                    // A key cut by the width shows the part that fits.
                    assert!(
                        name.starts_with(&drawn) && !drawn.is_empty(),
                        "{at}: {drawn:?}"
                    );
                }
            }
        }
    }
}

/// With room for the guide, all of it is on screen: it wraps rather than
/// losing keys off its end.
#[test]
fn the_whole_guide_is_on_screen_whenever_it_has_room() {
    for seed in 0..SEEDS {
        let f = scenario(seed);
        for width in widths_for(seed).into_iter().filter(|w| *w >= 16) {
            let c = compose(&f.app, width, 40);
            let keys: HashSet<KeyCode> = c.keys.iter().map(|k| k.key).collect();
            assert_eq!(keys, list_keys(), "seed {seed} at {width} columns");
        }
    }
}

/// A line said to hold an agent holds one, and every line that does is in the
/// frame.
#[test]
fn every_agent_line_is_a_drawn_row() {
    for seed in 0..SEEDS {
        let f = scenario(seed);
        for height in HEIGHTS {
            for width in widths_for(seed) {
                let c = compose(&f.app, width, height);
                assert_eq!(c.agents.len(), height, "seed {seed} {width}x{height}");
                for (y, i) in c.agents.iter().enumerate() {
                    let Some(i) = *i else { continue };
                    assert!(i < f.app.agents().len(), "seed {seed} {width}x{height}");
                    assert!(
                        !line_text(&c.lines[y]).trim().is_empty(),
                        "seed {seed} {width}x{height}: line {y} is blank"
                    );
                }
            }
        }
    }
}

/// The preview heading runs exactly to the edge, and keeps the selected
/// session's ID — whole where it fits, short where it does not — whenever the
/// line has room for ` ▼ preview · <id> ══`.
#[test]
fn the_preview_heading_fills_the_line_and_keeps_the_id() {
    for seed in 0..SEEDS {
        let f = scenario(seed);
        let selected = f.app.selected().expect("a selected agent");
        let short = selected.short_session_id().to_string();
        for width in widths_for(seed) {
            let c = compose(&f.app, width, 40);
            let Some(heading) = c
                .lines
                .iter()
                .map(line_text)
                .find(|l| l.starts_with(" ▼ ") && l.contains("preview"))
            else {
                continue;
            };
            let at = format!("seed {seed} at {width}: {heading:?}");
            if width >= " ▼ preview ".width() + 2 {
                assert_eq!(heading.width(), width, "{at}");
            }
            let room = " ▼ preview · ".width() + short.width() + " ══".width();
            if !short.is_empty() && width >= room {
                assert!(
                    heading.contains(&format!("preview · {short}"))
                        || heading.contains(selected.session_id.trim()),
                    "{at}"
                );
            }
        }
    }
}

/// The preview asks for exactly the rows drawn under its heading.
#[test]
fn the_preview_request_matches_the_rows_it_is_given() {
    for seed in 0..SEEDS {
        let f = scenario(seed);
        for height in HEIGHTS {
            for width in widths_for(seed).into_iter().filter(|w| *w >= 30) {
                let c = compose(&f.app, width, height);
                let text: Vec<String> = c.lines.iter().map(line_text).collect();
                let Some(heading) = text.iter().position(|l| l.starts_with(" ▼ ")) else {
                    continue;
                };
                let (_, rows) = c.preview.clone().expect("a drawn preview was asked for");
                // The footer starts at the first line a key is on, or at the
                // prompt/error line above it.
                let footer = c.keys.iter().map(|k| k.row).min().unwrap_or(height);
                assert!(
                    heading + 1 + usize::from(rows) <= footer,
                    "seed {seed} {width}x{height}: {rows} rows asked under the heading at {heading}, footer at {footer}"
                );
            }
        }
    }
}

/// What the frame says about the preview is what it drew, at every size: its
/// text rows sit under the heading, with no dot line above or below it; the text a
/// selection copies from is those rows as drawn, character for character; and
/// the session ID's columns land on the ID itself, whole or short, with the
/// whole ID behind them for a click to copy.
#[test]
fn the_preview_area_is_what_was_drawn_under_its_heading() {
    // What was actually exercised, per height and per width band. 🚨 A raw frame
    // count was the guard here, and it only said "many frames" — it could not
    // tell a run that swept one height from one that covered them all, and it
    // had to be re-tuned the moment the widths were sampled. These say what the
    // run has to have reached.
    let mut seen = 0;
    let mut by_height = std::collections::BTreeMap::<usize, usize>::new();
    let mut narrow = 0;
    let mut wide = 0;
    for seed in 0..SEEDS {
        let f = scenario(seed);
        let selected = f.app.selected().expect("a selected agent");
        let whole = selected.session_id.trim().to_string();
        let short = selected.short_session_id().to_string();
        for height in HEIGHTS {
            for width in widths_for(seed) {
                let c = compose(&f.app, width, height);
                let text: Vec<String> = c.lines.iter().map(line_text).collect();
                let at = format!("seed {seed} {width}x{height}");
                let Some(area) = &c.preview_area else {
                    assert!(
                        !text.iter().any(|l| l.starts_with(" ▼ ")),
                        "{at}: a heading with no area"
                    );
                    continue;
                };
                seen += 1;
                *by_height.entry(height).or_default() += 1;
                if width <= 40 {
                    narrow += 1;
                } else {
                    wide += 1;
                }
                // No loop is armed here, so the text starts right under the
                // heading — which, below three columns, is cut to its `…`.
                assert!(area.rows.start >= 2, "{at}: {:?}", area.rows);
                let heading = area.rows.start - 1;
                assert!(
                    text[heading].starts_with(" ▼") || width < 3,
                    "{at}: {:?} over the text",
                    text[heading]
                );
                let dots = "·".repeat(width);
                // The list runs straight into the heading band: no line above.
                assert_ne!(text[heading - 1], dots, "{at}: no dot line above");
                assert!(area.rows.start < area.rows.end, "{at}: {:?}", area.rows);
                assert!(area.rows.end < height, "{at}: {:?}", area.rows);
                assert_ne!(text[area.rows.end], dots, "{at}: no closing dot line");
                assert_eq!(area.text, text[area.rows.clone()].to_vec(), "{at}");
                match &area.sid {
                    None => assert!(whole.is_empty(), "{at}: the ID lost its columns"),
                    Some(sid) => {
                        assert_eq!(sid.id, whole, "{at}");
                        assert_eq!(sid.row, heading, "{at}");
                        assert!(sid.cols.end <= width, "{at}: {:?}", sid.cols);
                        if !sid.cols.is_empty() {
                            let drawn = columns(&text[heading], sid.cols.start, sid.cols.end);
                            // A heading too narrow for even the short ID is
                            // cut, and the cut's `…` stands where the ID ran
                            // off the edge.
                            let drawn = match drawn.strip_suffix('…') {
                                Some(kept) if sid.cols.end == width => kept.to_string(),
                                _ => drawn,
                            };
                            assert!(
                                whole.starts_with(&drawn) || short.starts_with(&drawn),
                                "{at}: {drawn:?} under the ID's columns"
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(seen > 200, "only {seen} frames had a preview");
    assert!(narrow > 20, "only {narrow} narrow frames had a preview");
    assert!(wide > 20, "only {wide} wide frames had a preview");
    assert_eq!(
        by_height.keys().copied().collect::<Vec<_>>(),
        HEIGHTS
            .iter()
            .copied()
            .filter(|h| *h > 6)
            .collect::<Vec<_>>(),
        "every height that can hold a preview must have drawn one: {by_height:?}"
    );
}

/// The three measurements everything above is built on: a cut never runs past
/// its width, a pad reaches it exactly, and a line clamped to a width fits —
/// for text of narrow and wide glyphs alike, across several spans.
#[test]
fn cuts_pads_and_clamps_keep_to_their_width() {
    let mut rng = Rng::new(0xC0FFEE);
    for _ in 0..1500 {
        let s = rng.text(24);
        for max in 0..40 {
            for tail in ["…", ""] {
                let cut = truncate_width(&s, max, tail);
                if tower_text::width(&s) <= max {
                    assert_eq!(cut, s, "{s:?} fits in {max}");
                } else if max >= tower_text::width(tail) {
                    assert!(
                        tower_text::width(&cut) <= max,
                        "{s:?} cut to {max}: {cut:?}"
                    );
                    assert!(cut.ends_with(tail), "{s:?} cut to {max}: {cut:?}");
                }
            }
            let padded = pad_right(&s, max);
            assert_eq!(
                tower_text::width(&padded),
                tower_text::width(&s).max(max),
                "{s:?} padded to {max}"
            );
            assert!(padded.starts_with(&s));
        }
        let line = Line::from(vec![
            Span::raw(rng.text(8)),
            Span::styled(rng.text(8), Style::default().bold()),
            Span::raw(rng.text(8)),
        ]);
        for width in 0..40 {
            let clamped = clamp_line(&line, width);
            assert!(
                clamped.width() <= width,
                "{:?} at {width}",
                line_text(&line)
            );
            if line.width() <= width {
                assert_eq!(line_text(&clamped), line_text(&line));
            }
        }
    }
}
