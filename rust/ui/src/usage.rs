// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The usage board inside the tower: a line of it under the header, and the
//! whole of it — the guard's sliders included — in the preview's place on `u`.
//!
//! It used to be a process of its own (`herdr-tower usage --watch`) in a tab
//! the supervisor kept. The board itself — reading the figures, drawing the
//! bars, saving a floor — lives in the `cli` crate, which depends on this one,
//! so it comes in through [`Usage`], the way the backend comes in through
//! [`crate::Tower`]. What it hands over is text with colour escapes, the same
//! text the terminal board printed; [`ansi_line`] turns it into spans.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// How a key or a click changes a floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjust {
    /// Keep 1% less in reserve; reaching zero turns the guard off.
    Less,
    /// Keep 1% more in reserve.
    More,
    /// Off, or back to the default when off.
    Toggle,
    /// Where a click or a drag landed on the slider: the board's column,
    /// 1-based, as the terminal board counted it.
    At(u16),
}

/// The board as drawn at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageFrame {
    /// One line: every AI's windows as short bars.
    pub summary: String,
    /// The whole board, one entry per line.
    pub board: Vec<String>,
    /// The board lines (0-based) holding a slider, in the order the keys
    /// move through them.
    pub sliders: Vec<usize>,
}

/// What the tower asks of the usage board.
pub trait Usage: Send + Sync {
    /// Reads the figures again: from the network when they are older than
    /// the board's own cache allows, or whenever `force`. Slow — never on
    /// the draw path.
    fn refresh(&self, force: bool);
    /// The board as last read, with slider `editing` chosen. Fast.
    fn render(&self, editing: Option<usize>) -> UsageFrame;
    /// Changes the floor slider `slider` sets, and saves it.
    fn adjust(&self, slider: usize, how: Adjust) -> Result<(), String>;
}

/// One line of text with SGR escapes as spans: bold, dim, reset, and 24-bit
/// foreground and background. Anything else in an escape is dropped with it.
pub fn ansi_line(text: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut run = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("\x1b[") {
        run.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find(|c: char| c.is_ascii_alphabetic()) else {
            rest = "";
            break;
        };
        if !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        if after.as_bytes()[end] == b'm' {
            style = apply_sgr(style, &after[..end]);
        }
        rest = &after[end + 1..];
    }
    run.push_str(rest);
    if !run.is_empty() {
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params.split(';').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            39 => style.fg = None,
            49 => style.bg = None,
            c @ (38 | 48) if codes.get(i + 1) == Some(&2) && i + 4 < codes.len() => {
                let rgb = Color::Rgb(codes[i + 2] as u8, codes[i + 3] as u8, codes[i + 4] as u8);
                style = if c == 38 {
                    style.fg(rgb)
                } else {
                    style.bg(rgb)
                };
                i += 4;
            }
            _ => {}
        }
        i += 1;
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn escapes_become_styles_and_the_text_is_kept() {
        let l = ansi_line(
            "\x1b[1;38;2;67;181;129m95% left\x1b[0m  \x1b[38;2;138;138;138mresets\x1b[0m",
        );
        assert_eq!(text(&l), "95% left  resets");
        assert_eq!(l.spans[0].style.fg, Some(Color::Rgb(67, 181, 129)));
        assert!(l.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(l.spans[1].style, Style::default(), "reset");
        assert_eq!(l.spans[2].style.fg, Some(Color::Rgb(138, 138, 138)));
    }

    #[test]
    fn a_background_and_a_plain_line_come_through() {
        let l = ansi_line("\x1b[48;2;31;48;58m Fri \x1b[49m tail");
        assert_eq!(l.spans[0].style.bg, Some(Color::Rgb(31, 48, 58)));
        assert_eq!(l.spans[1].style.bg, None);
        assert_eq!(text(&ansi_line("plain")), "plain");
        assert_eq!(text(&ansi_line("")), "");
    }

    #[test]
    fn a_broken_or_foreign_escape_is_dropped_not_printed() {
        assert_eq!(text(&ansi_line("a\x1b[2Kb")), "ab");
        assert_eq!(text(&ansi_line("a\x1b[38;2;1m")), "a");
        assert_eq!(text(&ansi_line("a\x1b[")), "a");
    }
}
