// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The checklist `herdr-tower restore` shows when a person types it with no
//! arguments.
//!
//! It exists because the alternative was a two-step ritual: run `--dry-run`,
//! read the IDs off the screen, retype the ones you want. The plan is already
//! on screen, so the choosing happens there.
//!
//! The event loop is here too, driven over `cbreak` rather than through a TUI
//! framework. `tower_ui` exists now, but it is one ratatui application with its own
//! screens; it exposes no "run this checklist and hand back the answer"
//! primitive, and pulling a whole alternate-screen app up for a nine-line list
//! would take over the terminal a person is reading a plan on, so the checklist
//! draws inline. [`Picker`] stays a seam so tests never touch a real terminal.
//!
//! The frame is plain text, with no styling. Colour would be worth adding; it
//! is not worth guessing at without a capability check.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use crate::restorecmd::RestoreItem;
use crate::shorten_home;

/// A keypress, in the vocabulary the checklist acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Down,
    Up,
    /// Toggle the row under the cursor.
    Toggle,
    /// Toggle every row: all on becomes all off, anything off becomes all on.
    ToggleAll,
    Confirm,
    Cancel,
}

impl Key {
    /// The key bindings, kept as a table so a future runtime translates its own
    /// key names once instead of scattering the mapping.
    ///
    /// Both vi keys and arrows, because this list is read under time pressure
    /// after a reboot and nobody should have to remember which.
    pub fn from_name(name: &str) -> Option<Key> {
        match name {
            "j" | "down" => Some(Key::Down),
            "k" | "up" => Some(Key::Up),
            " " | "space" | "x" => Some(Key::Toggle),
            "a" => Some(Key::ToggleAll),
            "enter" => Some(Key::Confirm),
            "q" | "esc" | "ctrl+c" => Some(Key::Cancel),
            _ => None,
        }
    }
}

/// Whether both ends of the terminal are really a terminal.
///
/// Both, not either: the checklist has to draw somewhere and read from
/// somewhere, and `herdr-tower restore | tee log` has a terminal on exactly one
/// of them.
pub fn stdio_is_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// How `restore` asks a person which sessions to bring back.
///
/// Two functions rather than one because the two questions are separable and
/// fail differently: "is anybody watching" is cheap and always answerable,
/// "which of these" needs a terminal. A test supplies both and never touches the
/// real one.
pub struct Picker<'a> {
    pub is_terminal: &'a dyn Fn() -> bool,
    /// `None` from this means the person cancelled. An empty `Vec` means they
    /// confirmed having chosen nothing, which is a different answer.
    pub choose: &'a dyn Fn(&[RestoreItem]) -> Option<Vec<RestoreItem>>,
}

static IS_TERMINAL: fn() -> bool = stdio_is_terminal;
static CHOOSE: fn(&[RestoreItem]) -> Option<Vec<RestoreItem>> = run_picker;

/// The real checklist, on the real terminal.
pub fn terminal_picker() -> Picker<'static> {
    Picker {
        is_terminal: &IS_TERMINAL,
        choose: &CHOOSE,
    }
}

/// Draws the checklist on the controlling terminal and reads the answer.
///
/// `None` covers cancellation *and* every failure. That collapse is correct
/// here: a checklist that could not be drawn has not been answered, and
/// restoring sessions nobody confirmed is the one outcome worse than restoring
/// none.
///
/// Written to `/dev/tty` rather than stdout so the frame goes to the terminal
/// even when the caller's own writer is something else — and read from there for
/// the same reason.
pub fn run_picker(items: &[RestoreItem]) -> Option<Vec<RestoreItem>> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(crate::cbreak::CONTROLLING_TTY)
        .ok()?;
    let guard = crate::cbreak::enter(Path::new(crate::cbreak::CONTROLLING_TTY)).ok()?;

    let _ = write!(tty, "{HIDE_CURSOR}");
    // The loop is its own function so that no exit from it — an answer, a
    // vanished terminal, a failed read — can skip putting the cursor back. A
    // hidden cursor outlives the process and leaves the user's shell looking
    // broken, and the terminal-restoring `guard` cannot cover it: cbreak and
    // cursor visibility are separate settings.
    let answer = drive(&mut tty, RestorePicker::new(items.to_vec()));
    let _ = write!(tty, "{SHOW_CURSOR}");
    let _ = tty.flush();
    let _ = guard.restore();
    answer
}

/// How many rows a frame occupies on a terminal `cols` wide.
///
/// Not `frame.lines().count()`. That counts *logical* lines, and the repaint
/// moves the cursor up by *physical* rows — so one soft-wrapped line and the
/// checklist redraws over itself, one row lower each keypress. An item row
/// carries a session name, a path and a launch command, so on a narrow window it
/// wraps whatever the labels say; counting what the terminal actually did is the
/// only version of this that does not need a promise about line lengths.
///
/// Display columns, not characters: a row of CJK occupies twice the rows a
/// character count would predict.
fn rows_used(frame: &str, cols: usize) -> usize {
    if cols == 0 {
        return frame.lines().count();
    }
    frame
        .lines()
        .map(|l| {
            let w = tower_text::width(l);
            if w == 0 {
                1
            } else {
                w.div_ceil(cols)
            }
        })
        .sum()
}

fn drive(tty: &mut std::fs::File, mut model: RestorePicker) -> Option<Vec<RestoreItem>> {
    let cols = crate::cbreak::columns_or_default(Path::new(crate::cbreak::CONTROLLING_TTY));
    let mut drawn = 0usize;
    let mut buf = [0u8; 8];
    loop {
        let frame = model.view();
        // Repaint in place: back up over the previous frame and clear from there
        // down. Whatever is on screen above it is left alone, which is the point
        // — the plan the user is choosing from is up there.
        if drawn > 0 {
            let _ = write!(tty, "\x1b[{drawn}A\x1b[J");
        }
        let _ = write!(tty, "{frame}");
        let _ = tty.flush();
        drawn = rows_used(&frame, cols);

        let n = tty.read(&mut buf).ok()?;
        if n == 0 {
            return None; // the terminal went away, so nothing was confirmed
        }
        let mut seq: Vec<u8> = buf[..n].to_vec();
        // A read is not guaranteed to carry a whole escape sequence. Over SSH the
        // first one can be the lone `0x1b`, which reads as Escape — so an arrow
        // key cancelled the checklist. When what arrived is only a *prefix*, wait
        // a tenth of a second for the rest; a real Escape has nothing following
        // it and falls through unchanged.
        while is_escape_prefix(&seq) {
            let mut more = [0u8; 16];
            let Ok(m) = crate::cbreak::read_with_deadline(
                Path::new(crate::cbreak::CONTROLLING_TTY),
                tty,
                &mut more,
                1,
            ) else {
                break;
            };
            if m == 0 {
                break; // nothing more is coming: this really was Escape
            }
            seq.extend_from_slice(&more[..m]);
        }
        if let Some(k) = key_from(&seq) {
            model.update(k);
        }
        if model.done() {
            return model.selected();
        }
    }
}

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

/// One read's worth of bytes, as a key.
///
/// A terminal delivers an escape sequence in one `read`, so a lone `0x1b` is a
/// real Escape and `0x1b [ A` is an arrow. Distinguishing them by a timeout is
/// what a TUI framework does; reading the burst is what makes a timer
/// unnecessary here.
///
/// `0x03` is handled even though `ISIG` is deliberately left on — see
/// `crate::cbreak` — because a terminal configured otherwise delivers it as a
/// byte, and Ctrl-C must never be a toggle.
/// Whether these bytes could still become a longer sequence.
///
/// Only the shapes this decoder recognizes: `ESC`, `ESC [` and `ESC O`. Anything
/// else is complete as it stands, and waiting on it would add a tenth of a second
/// to every ordinary keypress.
fn is_escape_prefix(b: &[u8]) -> bool {
    matches!(b, [0x1b] | [0x1b, b'['] | [0x1b, b'O'])
}

fn key_from(bytes: &[u8]) -> Option<Key> {
    match bytes {
        [0x1b] | [0x03, ..] => Some(Key::Cancel),
        // The second form is application cursor mode, which some terminals use
        // once a full-screen program has run.
        [0x1b, b'[', b'A', ..] | [0x1b, b'O', b'A', ..] => Some(Key::Up),
        [0x1b, b'[', b'B', ..] | [0x1b, b'O', b'B', ..] => Some(Key::Down),
        [b'\r', ..] | [b'\n', ..] => Some(Key::Confirm),
        [c, ..] => Key::from_name(&(*c as char).to_string()),
        [] => None,
    }
}

/// The checklist's state.
#[derive(Debug, Clone)]
pub struct RestorePicker {
    items: Vec<RestoreItem>,
    checked: Vec<bool>,
    cursor: usize,
    done: bool,
    cancelled: bool,
}

impl RestorePicker {
    /// Starts with everything checked, so a bare Enter does what `restore` did
    /// before the checklist existed: bring all of them back.
    pub fn new(items: Vec<RestoreItem>) -> Self {
        let checked = vec![true; items.len()];
        RestorePicker {
            items,
            checked,
            cursor: 0,
            done: false,
            cancelled: false,
        }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn update(&mut self, k: Key) {
        match k {
            Key::Down => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                }
            }
            Key::Up => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            Key::Toggle => {
                if let Some(c) = self.checked.get_mut(self.cursor) {
                    *c = !*c;
                }
            }
            Key::ToggleAll => {
                let all = self.checked.iter().all(|c| *c);
                self.checked.iter_mut().for_each(|c| *c = !all);
            }
            Key::Confirm => self.done = true,
            Key::Cancel => {
                self.done = true;
                self.cancelled = true;
            }
        }
    }

    /// The checked rows in their original order, or `None` if cancelled.
    ///
    /// `Some(vec![])` and `None` are different answers and the caller says
    /// different things about them: nothing picked, against cancelled.
    pub fn selected(&self) -> Option<Vec<RestoreItem>> {
        if self.cancelled {
            return None;
        }
        Some(
            self.items
                .iter()
                .zip(&self.checked)
                .filter(|(_, c)| **c)
                .map(|(it, _)| it.clone())
                .collect(),
        )
    }

    /// One frame.
    pub fn view(&self) -> String {
        let id_width = self
            .items
            .iter()
            .map(|it| it.agent.id.chars().count())
            .max()
            .unwrap_or(0);

        let mut out = String::from("Pick the sessions to restore\n\n");
        for (i, it) in self.items.iter().enumerate() {
            let box_ = if self.checked[i] { "[x]" } else { "[ ]" };
            let cursor = if i == self.cursor { "› " } else { "  " };
            out.push_str(&format!(
                "{cursor}{box_} {:<id_width$}  {} {}  {}  ← {}\n",
                it.agent.id,
                it.verb(),
                it.agent.pane.session,
                shorten_home(&it.agent.cwd),
                it.cmd,
            ));
        }
        // Kept inside 60 columns, with room for two-digit counts on both sides.
        //
        // The repaint counts physical rows now (see `rows_used`), so a wrapped
        // line no longer corrupts the checklist. This stays short anyway: the
        // guide is the one line a person reads to find out what the keys do, and
        // a guide that wraps buries half of itself. 57 columns at `12/34`.
        let n = self.checked.iter().filter(|c| **c).count();
        out.push_str(&format!("\n{n}/{} on  ", self.items.len()));
        for (key, text) in [
            ("space", "pick"),
            ("a", "all"),
            ("j/k", "move"),
            ("enter", "go"),
            ("q", "quit"),
        ] {
            out.push_str(&format!("{key} {text}  "));
        }
        out.push('\n');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restorecmd::tests::dead_agent;

    fn items() -> Vec<RestoreItem> {
        vec![
            RestoreItem {
                agent: dead_agent("claude-6", "claude", "herdr-tower-dev", 0, "/tmp"),
                new_session: true,
                cmd: "claude".into(),
            },
            RestoreItem {
                agent: dead_agent("claude-7", "claude", "demo-b", 0, "/tmp"),
                new_session: true,
                cmd: "claude".into(),
            },
            RestoreItem {
                agent: dead_agent("codex-9", "codex", "demo-b", 2, "/tmp"),
                new_session: false,
                cmd: "codex".into(),
            },
        ]
    }

    fn drive(keys: &[&str]) -> RestorePicker {
        let mut m = RestorePicker::new(items());
        for k in keys {
            m.update(Key::from_name(k).unwrap_or_else(|| panic!("no binding for {k:?}")));
        }
        m
    }

    fn ids(m: &RestorePicker) -> Vec<String> {
        m.selected()
            .unwrap_or_default()
            .into_iter()
            .map(|it| it.agent.id)
            .collect()
    }

    /// Everything starts checked, so unchecking one and confirming keeps the
    /// rest — the default is "bring them all back".
    #[test]
    fn unchecking_one_row_leaves_the_others_selected() {
        let m = drive(&["space", "enter"]);
        assert!(m.done() && !m.cancelled());
        assert_eq!(ids(&m), ["claude-7", "codex-9"]);
    }

    #[test]
    fn the_cursor_moves_by_vi_keys_and_by_arrows_alike() {
        let m = drive(&["j", "down", "space", "k", "space", "enter"]);
        assert_eq!(ids(&m), ["claude-6"]);
    }

    /// `a` is a toggle, not a "select all": pressing it when everything is on
    /// clears the list, and pressing it again brings it back.
    #[test]
    fn the_all_key_inverts_rather_than_selecting() {
        let m = drive(&["a"]);
        assert!(ids(&m).is_empty(), "{:?}", ids(&m));

        let mut m = m;
        for k in ["j", "space", "a"] {
            m.update(Key::from_name(k).unwrap());
        }
        assert_eq!(
            ids(&m).len(),
            3,
            "one row on means the next `a` turns all on"
        );
    }

    /// Cancelling restores nothing, and says so differently from confirming an
    /// empty selection — a `restore` that silently did nothing after a reboot
    /// is indistinguishable from one that crashed.
    #[test]
    fn cancelling_is_distinguishable_from_choosing_nothing() {
        for k in ["q", "esc", "ctrl+c"] {
            let m = drive(&[k]);
            assert!(m.done() && m.cancelled(), "{k}");
            assert!(m.selected().is_none(), "{k}");
        }
        let m = drive(&["a", "enter"]);
        assert!(
            m.selected().is_some_and(|v| v.is_empty()),
            "confirming an empty checklist is an answer, not a cancellation"
        );
    }

    /// The cursor cannot leave the list, in either direction.
    #[test]
    fn the_cursor_stops_at_both_ends() {
        let m = drive(&["k", "k", "space", "enter"]);
        assert_eq!(ids(&m), ["claude-7", "codex-9"], "up from the first row");

        let m = drive(&["j", "j", "j", "j", "space", "enter"]);
        assert_eq!(ids(&m), ["claude-6", "claude-7"], "down past the last row");
    }

    /// The frame has to show, per row, whether it is checked, which record it
    /// is, where it goes and what will run there — and the keys, because
    /// nothing else on screen says them.
    #[test]
    fn the_frame_shows_each_row_and_the_key_bindings() {
        let v = drive(&["j", "space"]).view();
        for want in [
            "[x] claude-6",
            "[ ] claude-7",
            "demo-b",
            "← codex",
            "space",
            "enter",
        ] {
            assert!(v.contains(want), "the frame never says {want:?}:\n{v}");
        }
    }

    /// The key table has to cover what a terminal actually sends: arrows arrive
    /// as escape sequences, Enter as CR or LF depending on the terminal, and a
    /// lone Escape is a cancellation rather than the start of one.
    #[test]
    fn a_terminals_own_byte_sequences_map_onto_the_keys() {
        assert_eq!(key_from(b"\x1b[A"), Some(Key::Up));
        assert_eq!(key_from(b"\x1b[B"), Some(Key::Down));
        assert_eq!(key_from(b"\x1bOA"), Some(Key::Up));
        assert_eq!(key_from(b"\x1b"), Some(Key::Cancel));
        assert_eq!(key_from(b"\x03"), Some(Key::Cancel));
        assert_eq!(key_from(b"\r"), Some(Key::Confirm));
        assert_eq!(key_from(b"\n"), Some(Key::Confirm));
        assert_eq!(key_from(b" "), Some(Key::Toggle));
        assert_eq!(key_from(b"j"), Some(Key::Down));
        assert_eq!(key_from(b"Z"), None, "an unbound key does nothing");
        assert_eq!(key_from(b""), None);
    }

    /// An empty list must not index anything. `restore` never opens the
    /// checklist with nothing in it, but a model that panics on the empty case
    /// is one bad guard away from taking the command down.
    #[test]
    fn an_empty_list_survives_every_key() {
        let mut m = RestorePicker::new(Vec::new());
        for k in [
            Key::Down,
            Key::Up,
            Key::Toggle,
            Key::ToggleAll,
            Key::Confirm,
        ] {
            m.update(k);
        }
        assert!(m.selected().is_some_and(|v| v.is_empty()));
        assert!(m.view().contains("0/0 on"));
    }
    /// The key guide fits a narrow terminal, and that is a correctness
    /// requirement rather than a nicety.
    ///
    /// The guide is the one line a person reads to find out what the keys do, so
    /// it is kept off the wrap regardless of what the repaint can cope with — a
    /// guide that wraps buries half of itself. It was 60 columns in Korean and
    /// 66 after translation; it is 57 now.
    #[test]
    fn the_key_guide_fits_inside_sixty_columns() {
        // Two-digit counts on both sides, which is the widest the line gets
        // before the item list itself is unreadably long.
        let many: Vec<RestoreItem> = std::iter::repeat_with(|| RestoreItem {
            agent: dead_agent("claude-6", "claude", "demo", 0, "/tmp"),
            new_session: true,
            cmd: "claude".into(),
        })
        .take(34)
        .collect();
        let mut m = RestorePicker::new(many);
        for _ in 0..22 {
            m.update(Key::from_name("space").unwrap());
            m.update(Key::from_name("j").unwrap());
        }

        let guide = m
            .view()
            .lines()
            .last()
            .expect("the guide is the last line")
            .to_string();
        assert!(
            guide.starts_with("12/34 on"),
            "two-digit counts on both sides: {guide:?}"
        );
        assert!(
            tower_text::width(&guide) <= 60,
            "{} columns — it wraps, and the repaint counts rows it cannot see: {guide:?}",
            tower_text::width(&guide)
        );
    }
    /// The repaint's whole contract: move the cursor up by as many rows as the
    /// terminal actually used. A count of logical lines is only the same number
    /// when nothing wrapped, which is a promise no row carrying a path and a
    /// launch command can make.
    #[test]
    fn a_wrapped_row_costs_the_repaint_a_row() {
        assert_eq!(rows_used("abc\ndef\n", 80), 2);
        // 100 columns over an 80-column terminal is two rows, not one.
        let long = "x".repeat(100);
        assert_eq!(rows_used(&format!("{long}\n"), 80), 2);
        assert_eq!(rows_used(&format!("{long}\n"), 100), 1, "exactly full");
        assert_eq!(rows_used(&format!("{long}\n"), 99), 2, "one over");
        // A blank line is still a row.
        assert_eq!(rows_used("\n\n", 80), 2);
        // Columns, not characters: CJK draws two columns each.
        assert_eq!(rows_used(&format!("{}\n", "한".repeat(50)), 80), 2);
        // No width to divide by, so the old count is all there is.
        assert_eq!(rows_used("abc\ndef\n", 0), 2);
    }

    /// The case that sent this back: an ordinary row, on the narrowest terminal
    /// this is meant to support, after the labels grew in translation.
    #[test]
    fn an_ordinary_row_is_counted_where_it_wraps_at_sixty() {
        let m = RestorePicker::new(items());
        let frame = m.view();
        let row = frame
            .lines()
            .find(|l| l.contains("claude-6"))
            .expect("the checklist draws the row");
        assert!(
            tower_text::width(row) > 60,
            "the row this guards is the one that wraps: {row:?} is {} columns",
            tower_text::width(row)
        );
        // Three item rows, one of which wraps, must not be counted as three.
        assert!(
            rows_used(&frame, 60) > frame.lines().count(),
            "a wrapped frame costs more rows than it has lines"
        );
        // And at a width nothing wraps at, the two agree again.
        assert_eq!(rows_used(&frame, 200), frame.lines().count());
    }
}

#[cfg(test)]
mod fragment_tests {
    use super::*;

    /// A read is not guaranteed to carry a whole escape sequence.
    ///
    /// Over SSH the first one can be the lone `0x1b`, and reading that as Escape
    /// cancelled the checklist the moment somebody pressed an arrow key. The
    /// decoder itself cannot fix that — it is the caller that has to wait — but
    /// it can say which byte strings are still incomplete, and that is what the
    /// loop keys off.
    #[test]
    fn an_incomplete_escape_sequence_is_recognized_as_incomplete() {
        for partial in [&b"\x1b"[..], &b"\x1b["[..], &b"\x1bO"[..]] {
            assert!(
                is_escape_prefix(partial),
                "{partial:?} could still become an arrow key"
            );
        }
        // Complete sequences, and ordinary keys, must not make the loop wait a
        // tenth of a second on every press.
        for complete in [
            &b"\x1b[A"[..],
            &b"\x1b[B"[..],
            &b"\x1bOA"[..],
            &b"\r"[..],
            &b" "[..],
            &b"q"[..],
            &b"\x03"[..],
            &b""[..],
        ] {
            assert!(!is_escape_prefix(complete), "{complete:?} is not a prefix");
        }
    }

    /// Reassembly reaches the same key as a single read would.
    #[test]
    fn a_split_arrow_key_decodes_once_reassembled() {
        assert_eq!(key_from(b"\x1b"), Some(Key::Cancel), "on its own, Escape");
        // The loop appends until the sequence is no longer a prefix.
        let mut seq = b"\x1b".to_vec();
        assert!(is_escape_prefix(&seq));
        seq.extend_from_slice(b"[");
        assert!(is_escape_prefix(&seq));
        seq.extend_from_slice(b"A");
        assert!(!is_escape_prefix(&seq));
        assert_eq!(key_from(&seq), Some(Key::Up));
    }
}
