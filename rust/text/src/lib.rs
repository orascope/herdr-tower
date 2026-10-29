// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Measuring, padding, cutting. One width table, used everywhere.
//!
//! # The two-table problem this exists to end
//!
//! Padding a line with one width table and then cutting the finished line with
//! another goes wrong, and not hypothetically. A table that decides whether
//! ambiguous-width characters are two columns by sniffing
//! `LC_ALL`/`LC_CTYPE`/`LANG` and a table that is fixed disagree the moment the
//! locale is Korean:
//!
//! ```text
//!                 LANG=en_US.UTF-8      LANG=ko_KR.UTF-8
//! "● WORK"        sniffed 6, fixed 6    sniffed 7, fixed 6
//! "● WORK?"       sniffed 7, fixed 7    sniffed 8, fixed 7
//! "· idle"        sniffed 6, fixed 6    sniffed 7, fixed 6
//! "…"             sniffed 1, fixed 1    sniffed 2, fixed 1
//! ```
//!
//! So on a Korean-locale terminal the padding believes the state label is one
//! column wider than the cut does, and the full-width selection bar stops one
//! column short of the right edge. Nothing crashes and no test notices, which is
//! exactly why a bug like that survives.
//!
//! Everything here measures with `unicode-width`'s `width()` — ambiguous glyphs
//! count as one column, which is what the terminals this runs in actually do
//! with `●`, `◆`, `·` and `…`. The value of picking one is not that this table
//! is right about every glyph; it is that a pad and a truncation of the same
//! string can no longer disagree.
//!
//! # Cutting between glyphs
//!
//! A cut and a selection step through [`glyphs`] — extended grapheme clusters —
//! and count each glyph [`glyph_width`] columns. That is what counting each
//! character's own width already gave for everything but emoji sequences: a
//! heart with its U+FE0F selector is two columns, not one plus nothing, and a
//! family joined by zero-width joiners is two, not the sum of its people. A
//! control character counts none either way, as ratatui draws it.
//!
//! # Agreeing with the screen
//!
//! [`width`] is the one measurement, and what it counts is what ratatui draws.
//! Its rule is read off `ratatui_core::buffer::CellWidth` (0.1.2) and confirmed
//! by drawing into a `Buffer` (`tower_ui::text`'s tests):
//!
//! ```text
//!                       ratatui draws   unicode-width says
//! "\u{ff9e}" (dakuten)       1                0
//! "\u{ff76}\u{ff9e}"         2                1
//! "\u{644}\u{627}" (lam-alef) 2                1
//! "\t"                       0                1
//! ```
//!
//! So a glyph counts its own width, plus one for each halfwidth katakana
//! dakuten or handakuten in it — terminals draw those as a cell of their own
//! though Unicode gives them `Grapheme_Extend` — and a glyph holding a control
//! character counts none, because ratatui filters those out before drawing.
//! Measuring glyph by glyph is what settles lam-alef: the ligature is one
//! cluster of two columns, while the whole string read at once says one.

use time::{Duration, OffsetDateTime};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The columns `s` fills on screen.
///
/// The one measurement: a pad, a cut, a column layout and a selection all ask
/// this, so none of them can disagree with another — or with the screen, whose
/// rule this is (see the module docs).
pub fn width(s: &str) -> usize {
    glyphs(s).map(glyph_width).sum()
}

/// Pads to a **display** width. Already-wider text is left alone rather than
/// truncated — a caller that wanted a cut asks for one.
pub fn pad_right(s: &str, width: usize) -> String {
    let w = crate::width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

/// Cuts to a display width, leaving room for `tail`.
///
/// `tail`'s own width is subtracted rather than assumed to be one column, which
/// is what a hardcoded `max - 1` gets wrong the moment the ellipsis is a wide
/// glyph. The only way out wider than `max` is a `tail` that is wider than
/// `max` on its own: it is added whole.
///
/// 🚨 A glyph at a time (see [`glyph_width`]), not a character at a time. `❤`
/// is one column but `❤` + U+FE0F is two and the selector alone none, so the
/// per-character sum let twenty hearts through a 24-column cut at 41 columns
/// wide, and a TITLE cell holding one overflowed its column. Cutting between
/// glyphs also keeps a heart with its selector, a letter with its accent and a
/// flag's halves together. One pass, and widths only ever add, so the first
/// glyph that does not fit ends it.
///
/// Whether it fits at all is asked the same way. Asked of the whole string, a
/// lam-alef ligature counted one column where the screen draws two, and a line
/// holding one was passed through uncut and drew wider than `max`. Glyph by
/// glyph is never less than the whole-string width either, so a caller that
/// measures the result with `.width()` still finds it within `max`.
pub fn truncate_width(s: &str, max: usize, tail: &str) -> String {
    if crate::width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(crate::width(tail));
    let mut out = String::new();
    let mut used = 0;
    for glyph in glyphs(s) {
        let w = glyph_width(glyph);
        if used + w > budget {
            break;
        }
        out.push_str(glyph);
        used += w;
    }
    out.push_str(tail);
    out
}

/// The tail of `s` that fits in `max` columns, cut between glyphs.
///
/// The mirror of [`truncate_width`], for a window that has to end at a fixed
/// point — an editor's caret — and can only give up text on its left. No marker:
/// the caller is showing the *end* of something, and a leading ellipsis would
/// cost a column of the little room there is.
pub fn tail_width(s: &str, max: usize) -> &str {
    let mut used = 0;
    let mut from = s.len();
    for (at, glyph) in s.grapheme_indices(true).rev() {
        let w = glyph_width(glyph);
        if used + w > max {
            break;
        }
        used += w;
        from = at;
    }
    &s[from..]
}

/// `s` in the pieces a terminal draws as one — extended grapheme clusters: a
/// heart and its variation selector, a letter and its accent, the people of a
/// family emoji, the two halves of a flag. What anything that walks a line
/// column by column steps through, [`glyph_width`] at a time.
pub fn glyphs(s: &str) -> impl Iterator<Item = &str> {
    s.graphemes(true)
}

/// The columns one glyph takes, as the screen draws it.
///
/// None if it holds a control character: a tab, an escape — ratatui filters
/// those out before drawing, so they take no column, though `unicode-width`
/// gives a tab one.
///
/// Otherwise its own width measured whole — so a heart with its variation
/// selector is two and a joined family two — plus one column for each halfwidth
/// katakana dakuten (U+FF9E) or handakuten (U+FF9F) in it. 🚨 That last term is
/// not a preference. Unicode gives both marks `Grapheme_Extend` and
/// `unicode-width` reports them as nothing, while terminals draw each as a
/// halfwidth cell of its own; ratatui compensates the same way
/// (`ratatui_core::buffer::CellWidth`, with references), and a measurement that
/// did not would leave a name with a dakuten in it drawing one column past its
/// cell.
pub fn glyph_width(glyph: &str) -> usize {
    if glyph.contains(char::is_control) {
        return 0;
    }
    glyph.width() + glyph.chars().filter(|c| is_sound_mark(*c)).count()
}

/// A halfwidth katakana dakuten or handakuten — the two marks a terminal draws
/// a cell for and `unicode-width` does not. Their combining forms (U+3099,
/// U+309A) are not these and keep their own width.
fn is_sound_mark(c: char) -> bool {
    c == '\u{ff9e}' || c == '\u{ff9f}'
}

/// Elapsed time, coarse on purpose — a control tower reads better with `5m`
/// than with a duration nobody can scan at a glance.
pub fn since(from: OffsetDateTime, now: OffsetDateTime) -> String {
    let d = now - from;
    if d < Duration::minutes(1) {
        "just now".into()
    } else if d < Duration::hours(1) {
        format!("{}m", d.whole_minutes())
    } else if d < Duration::days(1) {
        format!("{}h", d.whole_hours())
    } else {
        format!("{}d", d.whole_days())
    }
}

/// The same span, worded to stand on its own.
///
/// [`since`] is a bare width — `5m`, `2h` — because it sits in a column where
/// the heading says what it measures. Prose has no heading, and the callers that
/// glued one on ("{since} old", "updated {since} ago") read fine at five minutes
/// and turn into "just now old" and "updated just now ago" under a minute. This
/// one carries its own preposition, so there is nothing to glue.
pub fn age(from: OffsetDateTime, now: OffsetDateTime) -> String {
    let d = now - from;
    if d < Duration::minutes(1) {
        "just now".into()
    } else {
        format!("{} ago", since(from, now))
    }
}

/// The same span as a length, for a caller that supplies the preposition.
///
/// The third grammatical position, after [`since`]'s bare column and [`age`]'s
/// standalone prose: "for {}", "after {}", "within {}". Neither of the others
/// fits — `since` reads "for just now" and `age` reads "for just now ago" —
/// because under a minute both answer *when*, and a preposition has already
/// asked *how long*. `<1m` answers that, and it is the shortest of the three, so
/// no column that held "just now" can overflow on it.
pub fn span(from: OffsetDateTime, now: OffsetDateTime) -> String {
    let d = now - from;
    if d < Duration::minutes(1) {
        "<1m".into()
    } else {
        since(from, now)
    }
}

/// Shortens an absolute path's home prefix to `~`.
///
/// `$HOME` is read at call time rather than captured, so a test can set it and
/// a long-running tower picks up a change without restarting.
pub fn shorten_home(p: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return p.to_string();
    };
    if home.is_empty() {
        return p.to_string();
    }
    if p == home {
        return "~".into();
    }
    match p.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        // Windows spells it `C:\Users\u\proj`.
        None if cfg!(windows) => match p.strip_prefix(&format!("{home}\\")) {
            Some(rest) => format!("~\\{rest}"),
            None => p.to_string(),
        },
        None => p.to_string(),
    }
}

/// `"claude"` → `"Claude"`, for the provider headings.
///
/// Not a per-word title case, which mangles anything with punctuation in it.
/// These keys are single lowercase ASCII words, so only the first character
/// matters and the rest is left exactly as it came — a provider named
/// `antigravity_v2` must not come back as `Antigravity_V2`.
pub fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Cuts a status-table cell.
///
/// Distinct from [`truncate_width`] by one column, and the difference is not an
/// accident: a cell that fits may be `max` wide while a cell that had to be cut
/// stops at `max - 1`. Keeping the asymmetry matters because the column is sized
/// from its widest cell — get it wrong and every row shifts together, which is
/// invisible in isolation and obvious against the recorded output.
///
/// Charging the ellipsis to `max` instead of to `max - 1` is what shipped. The
/// freed column admitted one more double-width Korean character *and* the
/// ellipsis, so every truncated cell came back two columns too wide.
///
/// Whether it fits is asked with [`width`], glyph by glyph, like everything else
/// here: the whole-string measure let a lam-alef or a halfwidth sound mark
/// through uncut and wider than `max`.
pub fn truncate_cell(s: &str, max: usize) -> String {
    if crate::width(s) <= max {
        return s.to_string();
    }
    truncate_width(s, max.saturating_sub(1), "…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mins: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::minutes(mins)
    }

    /// The whole point of the crate: one string, padded and cut, cannot
    /// disagree with itself. Two width tables from different libraries could
    /// not pass this.
    #[test]
    fn a_cut_cell_is_never_wider_than_asked_by_the_one_width_table() {
        // Lam-alef and a halfwidth katakana with its sound mark: the
        // whole-string measure counts them narrower than the screen draws.
        for s in ["لا لا لا لا لا لا", "ｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞ", "S1 원격 카탈로그 장애 🔴 → 전체"]
        {
            for max in 1..12 {
                let cut = truncate_cell(s, max);
                assert!(width(&cut) <= max, "{s:?} at {max}: {cut:?}");
            }
        }
    }

    #[test]
    fn a_pad_and_a_cut_of_one_string_agree() {
        for s in [
            "● WORK",
            "● WORK?",
            "· idle",
            "…",
            "⎇ main",
            "한글 작업", // CJK on purpose: this is the width case
            "abc",
        ] {
            let w = s.width();
            assert_eq!(
                pad_right(s, w).width(),
                w,
                "padding to its own width: {s:?}"
            );
            assert_eq!(
                truncate_width(s, w, "…").width(),
                w,
                "cutting at its own width leaves it alone: {s:?}"
            );
        }
    }

    #[test]
    fn padding_counts_columns_not_characters() {
        assert_eq!("승인 대기".width(), 9, "4 CJK chars plus a space");
        assert_eq!(pad_right("승인 대기", 12).width(), 12);
        assert_eq!(pad_right("abcdefghi", 12).width(), 12);
        assert_eq!(pad_right("too wide already", 4), "too wide already");
    }

    /// The tail pays for itself, whatever it is.
    #[test]
    fn a_cut_leaves_room_for_its_own_tail() {
        assert_eq!(truncate_width(&"가".repeat(30), 40, "…").width(), 39);
        assert_eq!(truncate_width(&"a".repeat(60), 40, "…").width(), 40);
        // A wide tail costs two columns, not the one a hardcoded `max - 1`
        // would have assumed.
        assert_eq!(truncate_width(&"a".repeat(60), 40, "→").width(), 40);
        assert_eq!(truncate_width(&"a".repeat(60), 40, "…가").width(), 40);
        assert_eq!(truncate_width("짧다", 40, "…"), "짧다");
    }

    /// 🚨 A character that widens the one before it. `❤` is one column and
    /// `❤` with U+FE0F is two, so a cut that summed each character's width let
    /// twenty of them through a 24-column cut at 41 columns. A cut now stays
    /// within its width, and a glyph — a heart with its selector, a letter with
    /// its accent, a flag, a family — is kept or dropped whole. Everything
    /// else counts as it did character by character: lam and alef a column
    /// each, a tab none.
    #[test]
    fn a_cut_keeps_or_drops_each_glyph_whole() {
        let hearts = "❤\u{fe0f}".repeat(20);
        let cut = truncate_width(&hearts, 24, "…");
        assert!(cut.width() <= 24, "{cut:?}");
        assert_eq!(cut, format!("{}…", "❤\u{fe0f}".repeat(11)));

        for (s, max, want) in [
            ("e\u{301}e\u{301}e\u{301}e\u{301}", 3, "e\u{301}e\u{301}…"),
            ("ab❤\u{fe0f}cd", 4, "ab…"),
            ("ab🇰🇷cd", 4, "ab…"),
            ("👨\u{200d}💻👨\u{200d}💻", 3, "👨\u{200d}💻…"),
            ("لالالا", 2, "ل…"),
            ("\t\t\tabcdef", 3, "\t\t\tab…"),
            ("", 0, ""),
        ] {
            assert_eq!(truncate_width(s, max, "…"), want, "{s:?} at {max}");
        }
        for (glyph, w) in [
            ("a", 1),
            ("가", 2),
            ("❤", 1),
            ("❤\u{fe0f}", 2),
            ("e\u{301}", 1),
            ("🇰🇷", 2),
            ("👨\u{200d}👩\u{200d}👧", 2),
            ("ل", 1),
            ("\t", 0),
            ("\u{1b}", 0),
        ] {
            assert_eq!(glyph_width(glyph), w, "{glyph:?}");
        }
        assert_eq!(
            glyphs("a❤\u{fe0f}e\u{301}🇰🇷").collect::<Vec<_>>(),
            ["a", "❤\u{fe0f}", "e\u{301}", "🇰🇷"]
        );
    }

    /// Property, against a brute force: for generated strings of plain,
    /// wide, combined, joined, flag, ligature and control characters and every
    /// width up to past them, a cut that has to cut is the input's own start
    /// plus the tail, ends between glyphs, is no wider than the budget, and
    /// keeps as much as fits — the longest such start within the budget.
    /// Linear in what it reads: a long run of zero-width glyphs costs no more
    /// than any other.
    #[test]
    fn a_cut_is_the_longest_whole_start_that_fits() {
        let pieces = [
            "a",
            "Z",
            " ",
            "가",
            "🗼",
            "❤\u{fe0f}",
            "e\u{301}",
            "🇰🇷",
            "👨\u{200d}💻",
            "ل",
            "ا",
            "\t",
            "\u{200b}",
            "…",
        ];
        let sum = |s: &str| glyphs(s).map(glyph_width).sum::<usize>();
        let mut rng = tower_testrng::Rng::new(0xfe0f);
        for _ in 0..400 {
            let s: String = (0..rng.below(12))
                .map(|_| pieces[rng.below(pieces.len())])
                .collect();
            // Every place a cut may end: between glyphs.
            let mut ends = vec![0];
            ends.extend(s.grapheme_indices(true).map(|(i, g)| i + g.len()));
            for max in 0..=sum(&s) + 2 {
                let cut = truncate_width(&s, max, "…");
                if sum(&s) <= max {
                    assert_eq!(cut, s, "fits at {max}");
                    continue;
                }
                let kept = cut.strip_suffix('…').expect("the tail");
                let budget = max.saturating_sub(1);
                assert!(sum(kept) <= budget, "{s:?} at {max}: {cut:?}");
                let longest = ends
                    .iter()
                    .map(|&i| &s[..i])
                    .filter(|start| sum(start) <= budget)
                    .max_by_key(|start| start.len())
                    .unwrap_or_default();
                assert_eq!(kept, longest, "{s:?} at {max}");
            }
        }

        let zero_width_run = format!("{}{}", "\u{200b}".repeat(40_000), "a".repeat(100));
        let started = std::time::Instant::now();
        let cut = truncate_width(&zero_width_run, 30, "…");
        assert_eq!(cut.width(), 30);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "one pass, not one per glyph: {:?}",
            started.elapsed()
        );
    }

    /// The table-cell rule is deliberately one column tighter.
    #[test]
    fn a_cut_cell_is_one_column_tighter_than_a_cell_that_fits() {
        let exact = "가".repeat(20); // 40 columns
        assert_eq!(truncate_cell(&exact, 40), exact, "fits, so untouched");
        assert_eq!(truncate_cell(&"가".repeat(30), 40).width(), 39);
        assert_eq!(truncate_cell(&"a".repeat(60), 40).width(), 39);
    }

    #[test]
    fn elapsed_time_is_coarse() {
        let t = at(0);
        assert_eq!(since(t, t), "just now");
        assert_eq!(since(t, t + Duration::seconds(59)), "just now");
        assert_eq!(since(t, t + Duration::minutes(5)), "5m");
        assert_eq!(since(t, t + Duration::hours(5)), "5h");
        assert_eq!(since(t, t + Duration::days(3)), "3d");
    }

    #[test]
    fn a_home_prefix_becomes_a_tilde() {
        std::env::set_var("HOME", "/Users/fixture");
        assert_eq!(shorten_home("/Users/fixture"), "~");
        assert_eq!(shorten_home("/Users/fixture/proj"), "~/proj");
        assert_eq!(
            shorten_home("/Users/fixturely/proj"),
            "/Users/fixturely/proj",
            "a sibling that merely starts with the same letters is not home"
        );
        assert_eq!(shorten_home("/tmp/elsewhere"), "/tmp/elsewhere");
    }

    /// A per-word title case would touch `_v2`; these keys are one word and the
    /// rest must survive untouched.
    #[test]
    fn title_case_only_touches_the_first_character() {
        assert_eq!(title_case("antigravity"), "Antigravity");
        assert_eq!(title_case("antigravity_v2"), "Antigravity_v2");
        assert_eq!(title_case(""), "");
    }

    /// `age` stands on its own, which is the whole reason it exists.
    ///
    /// Callers used to glue a word onto `since` — "{since} old", "updated
    /// {since} ago" — and that reads fine at five minutes and turns into "just
    /// now old" under a minute. Fresh data is the *common* case for both of
    /// them, so the broken wording was what a person saw most.
    #[test]
    fn a_span_that_carries_its_own_preposition() {
        let t = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        assert_eq!(age(t, t), "just now");
        assert_eq!(age(t, t + Duration::seconds(59)), "just now");
        assert_eq!(age(t, t + Duration::minutes(5)), "5m ago");
        assert_eq!(age(t, t + Duration::hours(2)), "2h ago");
        assert_eq!(age(t, t + Duration::days(3)), "3d ago");
    }

    #[test]
    fn a_span_that_follows_someone_elses_preposition() {
        let t = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        // The case the other two cannot serve: "for just now" and "for just now
        // ago" are both wrong, because the preposition already asked how long.
        assert_eq!(span(t, t), "<1m");
        assert_eq!(span(t, t + Duration::seconds(59)), "<1m");
        assert_eq!(span(t, t + Duration::minutes(5)), "5m");
        assert_eq!(span(t, t + Duration::hours(2)), "2h");
        assert_eq!(span(t, t + Duration::days(3)), "3d");
        // Never wider than what `since` would have put in the same column.
        for d in [Duration::seconds(0), Duration::seconds(59)] {
            assert!(span(t, t + d).width() <= since(t, t + d).width());
        }
    }
}
