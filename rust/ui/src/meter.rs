// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The btop-style meter both boards draw: a row of `■` cells, each used one in
//! the colour of its place on the row — green at the start, amber half way,
//! red at the end — and the unused ones dark grey.
//!
//! The usage board (`cli::usagecmd`) paints it as text with escapes; the
//! header's machine load paints it as spans. Both take their cells and their
//! colours from here, so a share looks the same wherever it is drawn.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Below this share, a figure is green.
pub const ORANGE_FROM: f64 = 50.0;
/// At or above this share, it is red. The same point where the context badge
/// turns red.
pub const RED_FROM: f64 = 80.0;

/// Where the meter's colour passes through each of the tower's three levels,
/// as a share of the row: green at the empty end, amber where
/// [`ORANGE_FROM`] starts, red from [`RED_FROM`] on.
pub const GRADIENT: [(f64, (u8, u8, u8)); 3] = [
    (0.0, (67, 181, 129)),
    (ORANGE_FROM, (250, 166, 26)),
    (RED_FROM, (240, 71, 71)),
];
/// A meter's cell that is not used yet.
pub const EMPTY_CELL: (u8, u8, u8) = (58, 58, 58);

/// The colour of the meter at `pct` of its width, blended between the
/// [`GRADIENT`] stops around it.
pub fn gradient(pct: f64) -> (u8, u8, u8) {
    let pct = pct.clamp(0.0, 100.0);
    let mut from = GRADIENT[0];
    for to in GRADIENT {
        if pct <= to.0 {
            let span = to.0 - from.0;
            let t = if span > 0.0 {
                (pct - from.0) / span
            } else {
                1.0
            };
            let mix =
                |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
            return (
                mix(from.1 .0, to.1 .0),
                mix(from.1 .1, to.1 .1),
                mix(from.1 .2, to.1 .2),
            );
        }
        from = to;
    }
    from.1
}

/// How many of `width` cells `used_pct` fills. Anything above zero fills at
/// least one, and anything below 100 leaves at least one empty, so 1% and 99%
/// never look like 0% and 100%.
pub fn filled(used_pct: f64, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let pct = used_pct.clamp(0.0, 100.0);
    let mut filled = ((pct / 100.0) * width as f64).round() as usize;
    if pct > 0.0 {
        filled = filled.max(1);
    }
    if pct < 100.0 {
        filled = filled.min(width.saturating_sub(1));
    }
    filled
}

/// The colour of cell `i` of a `width`-cell meter, used or not.
pub fn cell(i: usize, width: usize, used: bool) -> (u8, u8, u8) {
    if used {
        gradient((i as f64 + 0.5) / width.max(1) as f64 * 100.0)
    } else {
        EMPTY_CELL
    }
}

/// The colour a figure is printed in: green, then amber from
/// [`ORANGE_FROM`], red from [`RED_FROM`] — the tower's own three.
pub fn level(used_pct: f64) -> (u8, u8, u8) {
    if used_pct >= RED_FROM {
        GRADIENT[2].1
    } else if used_pct >= ORANGE_FROM {
        GRADIENT[1].1
    } else {
        GRADIENT[0].1
    }
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// A `width`-cell meter at `used_pct`, as spans.
pub fn spans(used_pct: f64, width: usize) -> Vec<Span<'static>> {
    let used = filled(used_pct, width);
    (0..width)
        .map(|i| Span::styled("■", Style::default().fg(rgb(cell(i, width, i < used)))))
        .collect()
}

/// `pct` as the figure after a meter (`  42%`), in its level's colour. Three
/// digits wide, so what follows does not move as the figure changes, and
/// figures stacked on the machine bands line up.
pub fn figure(pct: f64) -> Span<'static> {
    Span::styled(
        format!(" {:>3.0}%", pct.clamp(0.0, 100.0)),
        Style::default()
            .fg(rgb(level(pct)))
            .add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_runs_green_to_amber_to_red_along_the_row() {
        assert_eq!(gradient(0.0), (67, 181, 129));
        assert_eq!(gradient(ORANGE_FROM), (250, 166, 26));
        assert_eq!(gradient(RED_FROM), (240, 71, 71));
        assert_eq!(gradient(100.0), (240, 71, 71));
        assert_eq!(gradient(25.0), (159, 174, 78));
    }

    /// Property: never more than the width, monotonic, and only 0% empty and
    /// only 100% full.
    #[test]
    fn a_meter_fills_monotonically_and_only_the_ends_are_empty_or_full() {
        for width in 1..=8 {
            let mut last = 0;
            for tenth in 0..=1000 {
                let pct = f64::from(tenth) / 10.0;
                let n = filled(pct, width);
                assert!(n <= width && n >= last, "{width} {pct}");
                last = n;
                if width > 1 {
                    assert_eq!(n == 0, pct == 0.0, "{width} {pct}");
                    assert_eq!(n == width, pct == 100.0, "{width} {pct}");
                }
            }
        }
        assert_eq!(filled(50.0, 0), 0);
        assert_eq!(filled(-5.0, 4), 0);
        assert_eq!(filled(500.0, 4), 4);
    }

    #[test]
    fn spans_are_one_cell_each_and_colour_their_place_not_their_fill() {
        let low = spans(10.0, 5);
        let full = spans(100.0, 5);
        assert_eq!(low.len(), 5);
        assert!(low.iter().all(|s| s.content == "■"));
        assert_eq!(low[0].style.fg, full[0].style.fg, "first cell is as green");
        assert_eq!(low[4].style.fg, Some(rgb(EMPTY_CELL)));
        assert_eq!(full[4].style.fg, Some(rgb(gradient(90.0))));
    }

    /// Property: a figure is five cells whatever the share, so nothing after
    /// it moves, and its level only rises with the share.
    #[test]
    fn a_figure_is_always_five_cells_and_its_level_only_rises() {
        let rank = |c: Option<Color>| {
            GRADIENT
                .iter()
                .position(|g| Some(rgb(g.1)) == c)
                .expect("one of the three levels")
        };
        let mut last = 0;
        for tenth in -100..=1500 {
            let pct = f64::from(tenth) / 10.0;
            let f = figure(pct);
            assert_eq!(f.content.chars().count(), 5, "{pct}: {:?}", f.content);
            let r = rank(f.style.fg);
            assert!(r >= last, "{pct}");
            last = r;
        }
    }

    /// Cross-check: away from the ends, the cells filled are the share of the
    /// width rounded, worked out on its own; and the used cells are exactly
    /// the first [`filled`] of the spans.
    #[test]
    fn the_fill_is_the_rounded_share_and_the_spans_show_it() {
        for width in 2..=12usize {
            for pct in 0..=100u32 {
                let pct = f64::from(pct);
                let n = filled(pct, width);
                let naive = (pct * width as f64 / 100.0).round() as usize;
                if naive >= 1 && naive < width {
                    assert_eq!(n, naive, "{pct}% of {width}");
                }
                let lit = spans(pct, width)
                    .iter()
                    .take_while(|s| s.style.fg != Some(rgb(EMPTY_CELL)))
                    .count();
                assert_eq!(lit, n, "{pct}% of {width}");
            }
        }
    }

    /// Cross-check: the gradient passes through each stop's own colour, and a
    /// figure printed at a stop wears that same colour.
    #[test]
    fn the_gradient_meets_each_level_at_its_stop() {
        for (at, colour) in GRADIENT {
            assert_eq!(gradient(at), colour, "{at}");
            assert_eq!(level(at), colour, "{at}");
        }
        assert_eq!(gradient(-20.0), GRADIENT[0].1, "below the row");
        assert_eq!(
            cell(0, 0, true),
            gradient(50.0),
            "a zero width does not divide by zero"
        );
    }

    #[test]
    fn a_figure_wears_its_level() {
        assert_eq!(figure(12.4).content, "  12%");
        assert_eq!(figure(5.0).content, "   5%");
        assert_eq!(figure(49.9).style.fg, Some(rgb(GRADIENT[0].1)));
        assert_eq!(figure(50.0).style.fg, Some(rgb(GRADIENT[1].1)));
        assert_eq!(figure(80.0).style.fg, Some(rgb(GRADIENT[2].1)));
        assert_eq!(figure(140.0).content, " 100%");
    }
}
