// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The one-line editor behind broadcast and idle-loop messages.
//!
//! ratatui ships no input widget, and the tests say precisely how much of one
//! is needed: type, backspace, move the cursor left and right, insert in the
//! middle — and, for the `L`, `T` and `/` fields, a dim hint in the empty
//! field that `→` takes. That is this, and nothing more — selection, kill-ring
//! and history would be code with no caller.
//!
//! The cursor counts **characters**, not bytes and not graphemes. Bytes would
//! split a Korean syllable and panic; graphemes would need another dependency
//! to tell a combining mark from a base. Character indexing is also what the
//! recorded behaviour was measured against (`헬로` + Left + `고` = `헬고로`).

/// A single-line text field.
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
    /// A cap on length, so a pasted file cannot become one broadcast.
    limit: usize,
    /// A dim suggestion shown while the field is empty — an example, or the
    /// value used last time — fish-shell style. Never part of [`value`]: `→`
    /// or `End` takes it as text ([`adopt_hint`]), and anything else that
    /// edits or moves drops it. Only ever non-empty while `chars` is empty.
    ///
    /// [`value`]: TextInput::value
    /// [`adopt_hint`]: TextInput::adopt_hint
    hint: Vec<char>,
}

impl TextInput {
    pub fn new(limit: usize) -> Self {
        TextInput {
            chars: Vec::new(),
            cursor: 0,
            limit,
            hint: Vec::new(),
        }
    }

    /// Starts empty, with `hint` drawn dim in the field and the caret on its
    /// first character.
    ///
    /// The `L`, `T` and `/` fields open this way. They used to open either
    /// empty — nothing to say what shape of line `T` wants — or, for `L`, on
    /// the whole message as real text, which a person wanting a different one
    /// had to delete character by character. A hint is both: it shows what
    /// would go there, typing replaces it at once, and `→` keeps it.
    pub fn hinted(limit: usize, hint: &str) -> Self {
        TextInput {
            hint: hint.chars().take(limit).collect(),
            ..TextInput::new(limit)
        }
    }

    /// Starts with an editable value and the caret at its beginning.
    ///
    /// For text that is already the person's own — the active filter `/`
    /// reopens on, a schedule line sent back to be fixed — where a hint would
    /// be wrong: it is not a suggestion, and `enter` must still see it.
    /// Starting at the beginning makes the first words visible when a long
    /// line opens; `End` still jumps to its tail.
    pub fn prefilled(limit: usize, value: &str) -> Self {
        let chars: Vec<char> = value.chars().take(limit).collect();
        TextInput {
            chars,
            cursor: 0,
            limit,
            hint: Vec::new(),
        }
    }

    /// What was typed — never the hint, which is only a suggestion until it
    /// is taken. `enter` over a field showing nothing but its hint reads as
    /// empty, so a stray `enter` cannot send an example nobody chose.
    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Whether the dim hint is what the field shows.
    pub fn showing_hint(&self) -> bool {
        !self.hint.is_empty()
    }

    /// The hint, empty once it is gone.
    pub fn hint(&self) -> String {
        self.hint.iter().collect()
    }

    /// Takes the hint as text, with the caret at its end — `→` and `End`
    /// over a hint, the way fish accepts an autosuggestion. `false`, and
    /// nothing changes, when there is no hint showing.
    pub fn adopt_hint(&mut self) -> bool {
        if self.hint.is_empty() {
            return false;
        }
        self.chars = std::mem::take(&mut self.hint);
        self.cursor = self.chars.len();
        true
    }

    /// Drops the hint, leaving the field empty — the first edit over it, or a
    /// click on the field.
    pub fn dismiss_hint(&mut self) {
        self.hint.clear();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.hint.clear();
    }

    /// The value split around the caret: what is before it, the character
    /// under it, and what follows.
    ///
    /// The middle is a space when the caret is past the end, so the renderer can
    /// always draw it as one reversed cell. A caret drawn as a character
    /// *inserted* into the text instead would shift everything after it by a
    /// column as it moved, which reads as the text sliding under the cursor.
    ///
    /// While the hint shows, these are the hint's, with the caret on its first
    /// character: the renderer draws them dim (see [`showing_hint`]).
    ///
    /// [`showing_hint`]: TextInput::showing_hint
    pub fn parts(&self) -> (String, String, String) {
        let (chars, cursor) = self.shown();
        let before = shown_chars(&chars[..cursor]);
        match chars.get(cursor) {
            Some(c) => (
                before,
                shown_char(*c).to_string(),
                shown_chars(&chars[cursor + 1..]),
            ),
            None => (before, " ".into(), String::new()),
        }
    }

    /// The characters on screen and where the caret is among them: the hint
    /// from its start while it shows, the text otherwise.
    fn shown(&self) -> (&[char], usize) {
        if self.hint.is_empty() {
            (&self.chars, self.cursor)
        } else {
            (&self.hint, 0)
        }
    }

    /// The same three parts, narrowed to `width` display columns around the
    /// caret.
    ///
    /// The footer is clamped rather than wrapped, and this terminal goes down to
    /// 60 columns, so a message longer than the room left over used to have its
    /// tail — the caret included — cut off the right edge. An editor whose
    /// cursor is off screen looks like a dead key, and the character being typed
    /// is the one thing that must always be visible.
    ///
    /// The caret keeps up to half the budget of following text so moving left
    /// shows what comes next, and the rest goes to the text before it; whatever
    /// one side does not use, the other takes. At the end of a long message —
    /// which is where the caret spends its life — that means the window ends at
    /// the caret.
    ///
    /// A hint is windowed the same way, from its first character.
    pub fn window(&self, width: usize) -> (String, String, String) {
        let (chars, cursor) = self.shown();
        let (at, tail_from) = match chars.get(cursor) {
            Some(c) => (shown_char(*c).to_string(), cursor + 1),
            None => (" ".into(), chars.len()),
        };
        let at_w = tower_text::width(&at);
        if width <= at_w {
            return (String::new(), at, String::new());
        }
        let budget = width - at_w;

        // 🚨 Glyph by glyph, through `tower_text`, not character by character.
        // A per-character sum gives `❤` + U+FE0F one column where the screen
        // draws two, so the window it measured was a column wider than the room
        // it had — and what fell off the right edge was the caret, which is the
        // one thing this function exists to keep on screen.
        let ahead = shown_chars(&chars[tail_from..]);
        let behind = shown_chars(&chars[..cursor]);
        let after = tower_text::truncate_width(&ahead, budget / 2, "");
        let before = tower_text::tail_width(&behind, budget - tower_text::width(&after));
        // Whatever the left did not need, the right takes — so a caret at the
        // start of a long message fills the line rather than half of it.
        let after = tower_text::truncate_width(&ahead, budget - tower_text::width(before), "");
        (before.to_string(), at, after)
    }

    // Every edit and every move below drops the hint first, except `right`
    // and `end`, which take it. Typing over a hint starts from empty with the
    // character typed: the hint was a suggestion, not a prefix.

    pub fn insert(&mut self, c: char) {
        self.hint.clear();
        if self.chars.len() >= self.limit {
            return;
        }
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Deletes the character before the caret. A no-op at the start, rather
    /// than wrapping around to the end.
    pub fn backspace(&mut self) {
        self.hint.clear();
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.chars.remove(self.cursor);
    }

    /// Deletes the character under the caret (Delete / `ctrl+d`).
    pub fn delete(&mut self) {
        self.hint.clear();
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.hint.clear();
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// One character right — or, over a hint, the whole hint taken as text.
    pub fn right(&mut self) {
        if self.adopt_hint() {
            return;
        }
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.hint.clear();
        self.cursor = 0;
    }

    /// To the end — or, over a hint, the whole hint taken as text.
    pub fn end(&mut self) {
        if self.adopt_hint() {
            return;
        }
        self.cursor = self.chars.len();
    }
}

/// A physical newline cannot live inside a ratatui `Line`: it would change the
/// geometry behind the renderer's back. Keep it in the stored value, but show a
/// visible single-cell marker while the one-line editor is open.
fn shown_char(c: char) -> char {
    if c == '\n' {
        '↵'
    } else {
        c
    }
}

fn shown_chars(chars: &[char]) -> String {
    chars.iter().copied().map(shown_char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> TextInput {
        let mut t = TextInput::new(200);
        for c in s.chars() {
            t.insert(c);
        }
        t
    }

    fn shown(w: &(String, String, String)) -> String {
        format!("{}{}{}", w.0, w.1, w.2)
    }

    #[test]
    fn a_message_that_fits_is_not_windowed_at_all() {
        let t = typed("hello");
        assert_eq!(t.window(40), t.parts());
        assert_eq!(shown(&t.window(40)), "hello ");
    }

    /// 🚨 The window never asks for a column it does not have, whatever the text
    /// is made of — checked against a ratatui buffer, which is what draws it.
    ///
    /// The pieces are the ones a per-character measurement got wrong: a heart
    /// with its variation selector (one column by characters, two on screen), a
    /// halfwidth sound mark (none by `unicode-width`, a cell on screen), a
    /// lam-alef ligature, a newline (drawn as `↵`), and wide and plain text. Any
    /// of them could push the caret off the right edge, which is the failure this
    /// function exists to prevent.
    #[test]
    fn a_window_fits_the_columns_it_was_given_however_the_text_is_made() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        const PIECES: [&str; 8] = [
            "a",
            "가",
            "🗼",
            "❤\u{fe0f}",
            "\u{ff76}\u{ff9e}",
            "\u{644}\u{627}",
            "\n",
            " ",
        ];
        // 🚨 Three spans, as `view::footer_lines` draws them — the caret is a
        // reversed cell of its own. ratatui measures each span on its own, so a
        // cluster the caret splits (a heart on one side of it, its variation
        // selector on the other) is two symbols on screen, and joining the parts
        // into one string before drawing would measure something nobody draws.
        let drawn = |(before, at, after): &(String, String, String)| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
            let line = ratatui::text::Line::from(vec![
                ratatui::text::Span::raw(before.clone()),
                ratatui::text::Span::raw(at.clone()),
                ratatui::text::Span::raw(after.clone()),
            ]);
            let (x, _) = buf.set_line(0, 0, &line, 200);
            usize::from(x)
        };
        let mut rng = tower_testrng::Rng::new(0x1_04_7e_57);
        for round in 0..300 {
            let text: String = (0..rng.below(12))
                .map(|_| PIECES[rng.below(PIECES.len())])
                .collect();
            let mut t = typed(&text);
            for _ in 0..rng.below(12) {
                t.left();
            }
            for width in 1..=12 {
                let w = t.window(width);
                let caret = tower_text::width(&w.1);
                if width >= caret {
                    assert!(
                        drawn(&w) <= width,
                        "round {round}: {text:?} at width {width} drew {} columns: {w:?}",
                        drawn(&w)
                    );
                } else {
                    // A caret wider than the whole room is kept whole and
                    // nothing joins it: `clamp_line` cuts the footer line from
                    // there, which is the only place that can know what else is
                    // on the line. Giving the caret up instead would leave an
                    // editor with no cursor, which reads as a dead key.
                    assert_eq!(
                        (w.0.as_str(), w.2.as_str()),
                        ("", ""),
                        "round {round}: {text:?} at width {width}"
                    );
                    assert_eq!(drawn(&w), caret, "round {round}: {text:?}");
                }
                assert!(!w.1.is_empty(), "round {round}: {text:?} lost its caret");
            }
        }
    }

    /// The case that sent this back: the footer is clamped, not wrapped, so a
    /// message longer than the room left over used to lose its tail — and the
    /// caret with it. The caret is the one cell that must survive.
    #[test]
    fn the_caret_survives_a_message_longer_than_the_line() {
        let t = typed(&"x".repeat(120));
        for width in [4, 10, 31, 60] {
            let w = t.window(width);
            assert_eq!(w.1, " ", "the caret cell at width {width}");
            assert!(
                tower_text::width(&shown(&w)) <= width,
                "width {width}: {:?} is {} columns",
                shown(&w),
                tower_text::width(&shown(&w))
            );
        }
        // Typing at the end, the window ends at the caret: what was just typed
        // is what is on screen.
        let w = t.window(10);
        assert_eq!(shown(&w), "xxxxxxxxx ");
    }

    #[test]
    fn a_caret_in_the_middle_keeps_context_on_both_sides() {
        let mut t = typed(&"abcdefghij".repeat(6));
        for _ in 0..30 {
            t.left();
        }
        let (before, at, after) = t.window(11);
        assert!(!before.is_empty(), "text before the caret");
        assert!(!after.is_empty(), "and after it");
        assert_eq!(at, "a", "the caret is on the character it is on");
        assert_eq!(tower_text::width(&format!("{before}{at}{after}")), 11);
    }

    #[test]
    fn a_caret_at_the_start_fills_the_line_rightwards() {
        let mut t = typed(&"x".repeat(120));
        t.home();
        let (before, at, after) = t.window(10);
        assert_eq!(before, "", "nothing precedes it");
        assert_eq!(at, "x");
        // The left took none of the budget, so the right takes all of it.
        assert_eq!(tower_text::width(&format!("{before}{at}{after}")), 10);
    }

    /// Columns, not characters: a window that counted CJK as one would overflow
    /// the line by its own width again.
    #[test]
    fn a_wide_character_counts_as_the_two_columns_it_draws() {
        let t = typed(&"한".repeat(40));
        let w = t.window(9);
        assert!(tower_text::width(&shown(&w)) <= 9, "{:?}", shown(&w));
        // 8 columns of text plus the one-column caret cell: a ninth column
        // would need half a syllable.
        assert_eq!(shown(&w), "한한한한 ");
    }

    #[test]
    fn a_width_with_no_room_for_anything_still_draws_the_caret() {
        let mut t = typed("abc");
        // Past the end, the caret is the blank cell `parts` also gives it.
        assert_eq!(t.window(1), (String::new(), " ".into(), String::new()));
        assert_eq!(t.window(0), (String::new(), " ".into(), String::new()));
        t.home();
        assert_eq!(t.window(1), (String::new(), "a".into(), String::new()));
        assert_eq!(t.window(0), (String::new(), "a".into(), String::new()));
    }

    /// The hint is drawn, caret on its first character, and is not the value:
    /// `enter` over it reads as an empty field.
    #[test]
    fn a_hint_is_shown_from_its_start_but_is_not_the_value() {
        let t = TextInput::hinted(100, "19:00 check");
        assert!(t.showing_hint());
        assert!(t.is_empty());
        assert_eq!(t.value(), "");
        assert_eq!(t.hint(), "19:00 check");
        assert_eq!(
            t.parts(),
            ("".into(), "1".into(), "9:00 check".into()),
            "the caret is on its first character"
        );
        assert_eq!(t.window(40), t.parts());
        assert_eq!(shown(&t.window(5)), "19:00");
    }

    /// `→` and `End` take the hint as text, caret at its end; after that it is
    /// ordinary text, and `→` moves again.
    #[test]
    fn right_and_end_take_the_hint_as_text() {
        for take in [TextInput::right as fn(&mut TextInput), TextInput::end] {
            let mut t = TextInput::hinted(100, "codex");
            take(&mut t);
            assert!(!t.showing_hint());
            assert_eq!(t.value(), "codex");
            assert_eq!(t.parts(), ("codex".into(), " ".into(), "".into()));
            t.insert('!');
            assert_eq!(t.value(), "codex!");
            t.home();
            t.right();
            assert_eq!(t.parts().0, "c", "an ordinary move now");
        }
    }

    /// Typing, Backspace, Delete, Home and Left drop the hint: what is typed
    /// starts from empty, not after the suggestion.
    #[test]
    fn every_other_edit_or_move_drops_the_hint() {
        let mut t = TextInput::hinted(100, "codex");
        t.insert('z');
        assert!(!t.showing_hint());
        assert_eq!(t.value(), "z", "typing replaces the hint");

        type Op = fn(&mut TextInput);
        let ops: [(&str, Op); 4] = [
            ("backspace", TextInput::backspace),
            ("delete", TextInput::delete),
            ("home", TextInput::home),
            ("left", TextInput::left),
        ];
        for (name, op) in ops {
            let mut t = TextInput::hinted(100, "codex");
            op(&mut t);
            assert!(!t.showing_hint(), "{name} drops the hint");
            assert_eq!(t.value(), "", "{name} leaves the field empty");
            assert_eq!(t.parts(), ("".into(), " ".into(), "".into()), "{name}");
            t.right();
            assert_eq!(t.value(), "", "{name}: nothing left to take");
        }

        let mut t = TextInput::hinted(100, "codex");
        t.dismiss_hint();
        assert!(!t.showing_hint(), "a click on the field drops it too");
    }

    /// A hint longer than the field could hold is cut to it, so taking it can
    /// never make a value past the limit.
    #[test]
    fn a_hint_is_held_to_the_limit() {
        let mut t = TextInput::hinted(3, "codex");
        t.end();
        assert_eq!(t.value(), "cod");
    }

    #[test]
    fn a_prefilled_multiline_message_is_one_editable_visual_line() {
        let mut t = TextInput::prefilled(100, "first\nsecond");
        assert_eq!(t.value(), "first\nsecond");
        assert_eq!(shown(&t.window(20)), "first↵second");
        t.end();
        t.insert('!');
        assert_eq!(t.value(), "first\nsecond!");
    }
}
