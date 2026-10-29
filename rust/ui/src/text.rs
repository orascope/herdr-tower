// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The ratatui-bound half of the drawing helpers.
//!
//! Measuring, padding and cutting live in [`tower_text`], which exists so a pad and
//! a truncation of the same string cannot disagree — see its module docs for
//! the two-table bug that motivated it. What stays here is what needs
//! `ratatui::Line`, and therefore cannot.

pub use tower_text::{
    age, glyph_width, glyphs, pad_right, shorten_home, since, span, title_case, truncate_width,
    width,
};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The columns a whole line fills, span by span, by [`width`].
///
/// 🚨 Not `Line::width`, which ratatui sums with `unicode-width` over each
/// span's whole string. That reading is one column short for every lam-alef
/// ligature and one long for every tab, so `clamp_line` measuring it handed a
/// line back uncut that then drew wider than the cell it was given — found by a
/// generated test against a ratatui `Buffer` (`clamp_line(Line::from("لالالا"),
/// 3)`).
pub fn line_width(line: &Line<'_>) -> usize {
    line.iter().map(|span| width(&span.content)).sum()
}

/// Cuts a styled line to `width`, marking the cut with `…`.
///
/// The ellipsis keeps its span's style, so a truncated orange badge does not
/// end in a grey ellipsis. Nothing needs closing afterwards: ratatui carries
/// style beside the text, so a severed span cannot leak into the next line the
/// way a severed ANSI escape did.
///
/// `width == 0` is a screen with no columns — the answer is an empty line, not
/// a panic.
pub fn clamp_line(line: &Line<'static>, width: usize) -> Line<'static> {
    if line_width(line) <= width {
        return line.clone();
    }
    if width == 0 {
        return Line::default();
    }
    let budget = width - 1; // the ellipsis takes the last column
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut last_style = Style::default();
    for span in line.iter() {
        last_style = span.style;
        let sw = crate::text::width(&span.content);
        if used + sw <= budget {
            used += sw;
            spans.push(span.clone());
            continue;
        }
        let head = truncate_width(&span.content, budget - used, "");
        used += crate::text::width(&head);
        if !head.is_empty() {
            spans.push(Span::styled(head, span.style));
        }
        break;
    }
    // A wide character straddling the boundary can leave a column unspent;
    // pad it so the ellipsis still lands on the last column rather than one
    // short of it, which would make a clamped line narrower than the screen.
    if used < budget {
        spans.push(Span::styled(" ".repeat(budget - used), Style::default()));
    }
    spans.push(Span::styled("…", last_style));
    Line::from(spans)
}

/// The plain text of a line, for tests and for the plain-text detail card.
///
/// Public because assertions about the rendered screen live on this: a `Line`
/// is the value a substring check on the screen is really about, and the
/// checks stay readable when they can still compare strings.
pub fn line_text(line: &Line<'_>) -> String {
    line.iter().map(|s| s.content.as_ref()).collect()
}

/// The glyphs of `s` that start inside the display columns `cols` — what a
/// selection over those columns of a drawn line holds. A wide glyph belongs to
/// the column it starts on. Stepped through by [`glyphs`], [`glyph_width`] at
/// a time, so `❤` + U+FE0F is two columns here as on screen, rather than the
/// one-and-nothing its characters count separately, and a tab none, as
/// before.
pub fn columns(s: &str, cols: std::ops::Range<usize>) -> String {
    let mut at = 0;
    let mut out = String::new();
    for glyph in glyphs(s) {
        if cols.contains(&at) {
            out.push_str(glyph);
        }
        at += glyph_width(glyph);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    /// Cross validation against the screen itself: each generated string is
    /// drawn into a ratatui buffer, and the columns counted here must be the
    /// ones it fills — in total, and glyph by glyph where each one starts. A
    /// cut must fit the columns it was given once drawn, and a selection must
    /// copy what the columns it covers hold. The pieces are the ones a
    /// per-character count got wrong — a variation selector, an accent, a
    /// flag, a family — beside plain and wide ones, lam and alef, a tab and an
    /// escape (which take no column), and the two halfwidth katakana sound
    /// marks, which ratatui draws a cell for and `unicode-width` does not.
    /// Nothing is left out any more: every disagreement between this table and
    /// the screen that #45 listed is a case in here.
    #[test]
    fn widths_and_selections_agree_with_what_ratatui_draws() {
        const PIECES: [&str; 17] = [
            "a",
            " ",
            "가",
            "🗼",
            "❤\u{fe0f}",
            "e\u{301}",
            "🇰🇷",
            "👨\u{200d}👩\u{200d}👧",
            "✈\u{fe0f}",
            "ل",
            "ا",
            "\t",
            "\u{1b}",
            "…",
            "\u{ff9e}",
            "\u{ff9f}",
            "\u{ff76}",
        ];
        let draw = |s: &str| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 400, 1));
            let (x, _) = buf.set_stringn(0, 0, s, 400, ratatui::style::Style::default());
            (buf, usize::from(x))
        };
        let mut rng = tower_testrng::Rng::new(0x5eed_fe0f);
        for _ in 0..300 {
            let s: String = (0..rng.below(16))
                .map(|_| PIECES[rng.below(PIECES.len())])
                .collect();
            let (buf, filled) = draw(&s);
            assert_eq!(glyphs(&s).map(glyph_width).sum::<usize>(), filled, "{s:?}");

            let mut at = 0;
            for glyph in glyphs(&s) {
                if glyph_width(glyph) > 0 {
                    assert_eq!(buf[(at as u16, 0)].symbol(), glyph, "{s:?} at {at}");
                }
                at += glyph_width(glyph);
            }

            for max in 1..=filled {
                let (_, cut) = draw(&truncate_width(&s, max, "…"));
                assert!(cut <= max, "{s:?} cut to {max} drew {cut}");
            }

            let (a, b) = (rng.below((filled) + 2), rng.below((filled) + 2));
            let mut want = String::new();
            let mut at = 0;
            for glyph in glyphs(&s) {
                if (a..b).contains(&at) {
                    if glyph_width(glyph) > 0 {
                        assert_eq!(buf[(at as u16, 0)].symbol(), glyph);
                    }
                    want.push_str(glyph);
                }
                at += glyph_width(glyph);
            }
            assert_eq!(columns(&s, a..b), want, "{s:?} over {a}..{b}");
        }
    }

    /// The columns a line fills and the cells it fills them with, as ratatui
    /// draws it — **span by span**, which is how production draws — and the
    /// oracle for the clamp tests below. A reading off the screen, so nothing in
    /// this module is checked against another measurement out of this module.
    ///
    /// 🚨 Not the joined text. A grapheme cluster can be split across two spans
    /// (a span ending in `ا`, the next beginning with a sound mark), and then the
    /// screen has two symbols where the joined string has one. `clamp_line`
    /// measures span by span for that reason, so its oracle must draw that way
    /// too, or the test and the code would be answering different questions.
    fn screen_line(line: &Line<'_>) -> (Buffer, usize) {
        let mut buf = Buffer::empty(Rect::new(0, 0, 400, 1));
        let (x, _) = buf.set_line(0, 0, line, 400);
        (buf, usize::from(x))
    }

    /// Where each of a line's drawn glyphs starts, and how many columns it
    /// takes. Span by span, as the screen draws it.
    fn starts(line: &Line<'_>) -> std::collections::HashMap<usize, usize> {
        let mut out = std::collections::HashMap::new();
        let mut at = 0;
        for span in line.iter() {
            for glyph in glyphs(&span.content) {
                let w = glyph_width(glyph);
                if w > 0 {
                    out.insert(at, w);
                }
                at += w;
            }
        }
        out
    }

    /// The characters of a line, run together. Spelled out rather than calling
    /// [`line_text`], which is itself under test here.
    fn joined(line: &Line<'_>) -> String {
        line.iter().map(|s| s.content.as_ref()).collect()
    }

    /// A clamped line fills the width it was given, and what it keeps is what
    /// the screen already showed in those columns.
    ///
    /// Both halves have gone wrong. A wide glyph straddling the boundary left
    /// the line one column short of the edge — the pad in [`clamp_line`] is
    /// that fix, and nothing pinned it for text of mixed widths across several
    /// spans. And a cut landing inside a glyph draws a different character than
    /// the one that was there, which a length check cannot see. The oracle is a
    /// ratatui buffer rather than `Line::width`: that width is the same
    /// `unicode_width` reading [`clamp_line`] does itself, so it would agree
    /// with a wrong answer.
    #[test]
    fn a_clamped_line_fills_its_width_and_keeps_what_the_screen_showed() {
        // Every piece, including the ones this test used to leave out because
        // the module measured them differently from the screen: a tab and an
        // escape (no column), lam and alef (a two-column ligature the whole
        // string reads as one), and a halfwidth sound mark (a cell the string
        // reading gives nothing). They are in because `clamp_line` now measures
        // the way the screen draws, and a line of them is the proof.
        const PIECES: [&str; 15] = [
            "a",
            " ",
            "z",
            "가",
            "나",
            "🗼",
            "❤\u{fe0f}",
            "e\u{301}",
            "🇰🇷",
            "👨\u{200d}👩\u{200d}👧",
            "\t",
            "\u{1b}",
            "ل",
            "ا",
            "\u{ff9e}",
        ];
        let mut rng = tower_testrng::Rng::new(0x5eed_c1a3);
        for _ in 0..200 {
            let line = Line::from(
                (0..3)
                    .map(|i| {
                        let s: String = (0..rng.below(5))
                            .map(|_| PIECES[rng.below(PIECES.len())])
                            .collect();
                        let style = if i == 1 {
                            Style::default().fg(Color::Red)
                        } else {
                            Style::default()
                        };
                        Span::styled(s, style)
                    })
                    .collect::<Vec<_>>(),
            );
            let text = joined(&line);
            let (before, filled) = screen_line(&line);
            assert_eq!(
                line_width(&line),
                filled,
                "{line:?} is measured as it is drawn"
            );
            for width in 0..=24 {
                let cut = clamp_line(&line, width);
                let got = joined(&cut);
                if filled <= width {
                    assert_eq!(got, text, "{text:?} fits in {width}");
                    continue;
                }
                if width == 0 {
                    assert_eq!(cut.iter().count(), 0, "no columns, no spans: {text:?}");
                    continue;
                }
                let (after, drawn) = screen_line(&cut);
                assert_eq!(drawn, width, "{text:?} clamped to {width} drew {got:?}");
                // 🚨 Read off the screen, without guessing which span is the
                // pad. Both earlier readings guessed and both were wrong: "a
                // trailing space the text does not have" breaks when a glyph
                // *holds* a space (`" \u{ff9e}"` is one two-column cluster), and
                // "a span that is a prefix of the original's span at that index"
                // calls the pad body whenever that span begins with a space.
                //
                // What the screen says is enough. Up to the first column that
                // differs, the clamp kept what was there; from it, only the pad
                // and the marker may follow; and the cut is as long as it could
                // be, because whatever glyph starts at that column does not fit
                // in the columns left before the marker.
                let last = width - 1;
                let first_diff = (0..width)
                    .find(|c| after[(*c as u16, 0)].symbol() != before[(*c as u16, 0)].symbol())
                    .unwrap_or(width);
                for c in first_diff..last {
                    assert_eq!(
                        after[(c as u16, 0)].symbol(),
                        " ",
                        "{text:?} clamped to {width}: column {c} is neither kept nor pad: {got:?}"
                    );
                }
                assert_eq!(
                    after[(last as u16, 0)].symbol(),
                    "…",
                    "{text:?} clamped to {width} must end in its marker: {got:?}"
                );
                if let Some(room) = starts(&line).get(&first_diff) {
                    assert!(
                        first_diff + room > last,
                        "{text:?} clamped to {width}: a {room}-column glyph fitted at {first_diff} and was dropped: {got:?}"
                    );
                }
            }
        }
    }

    /// The narrow widths, where there is no room for the text and barely any
    /// for the marker. A popup two columns wide is a real screen — the tower
    /// draws into whatever herdr gives it — and every one of these answers with
    /// a line rather than a panic or an overflow.
    #[test]
    fn clamping_to_a_column_or_none_answers_with_a_line() {
        let wide = Line::from(vec![Span::raw("가나다")]); // six columns
        let none = clamp_line(&wide, 0);
        assert_eq!(none.width(), 0);
        assert_eq!(none.iter().count(), 0, "a screen with no columns");
        assert_eq!(joined(&clamp_line(&wide, 1)), "…", "the marker alone");
        // One column of a two-column glyph: the glyph cannot go in, and the
        // marker still lands on the last column rather than one short of it.
        assert_eq!(joined(&clamp_line(&wide, 2)), " …");
        assert_eq!(clamp_line(&wide, 2).width(), 2);

        // Exactly at the width: handed back whole, spans and all.
        let two = Line::from(vec![
            Span::styled("가나", Style::default()),
            Span::raw("ab"),
        ]);
        assert_eq!(two.width(), 6);
        let same = clamp_line(&two, 6);
        assert_eq!(same.iter().count(), 2, "a line that fits is not rebuilt");
        assert_eq!(joined(&same), "가나ab");
        // One column less, and the second span goes rather than half of it.
        assert_eq!(joined(&clamp_line(&two, 5)), "가나…");
        // Two less, and the cut falls inside 나 — which means before it.
        assert_eq!(joined(&clamp_line(&two, 4)), "가 …");

        // An empty line is already within every width, including none.
        assert_eq!(joined(&clamp_line(&Line::default(), 0)), "");
        let blanks = Line::from(vec![Span::raw(""), Span::raw("")]);
        assert_eq!(joined(&clamp_line(&blanks, 0)), "");
    }

    /// Every assertion about a drawn screen reads its line through
    /// [`line_text`], so a separator or a dropped span here would quietly
    /// weaken all of them. A string written into a line comes back out of it
    /// unchanged, however the line was cut into spans — including cuts inside a
    /// grapheme cluster, where a joiner is all that is between the halves.
    #[test]
    fn the_text_of_a_line_is_its_spans_run_together() {
        for whole in ["", "a", "가나 ab", "❤\u{fe0f}e\u{301}🇰🇷", "  두 칸  "] {
            assert_eq!(line_text(&Line::from(whole.to_string())), whole);
            for at in 0..=whole.len() {
                if !whole.is_char_boundary(at) {
                    continue;
                }
                let line = Line::from(vec![
                    Span::raw(whole[..at].to_string()),
                    Span::styled(whole[at..].to_string(), Style::default().fg(Color::Red)),
                ]);
                assert_eq!(line_text(&line), whole, "{whole:?} split at {at}");
            }
        }
        assert_eq!(line_text(&Line::default()), "", "no spans, no text");
    }
}
