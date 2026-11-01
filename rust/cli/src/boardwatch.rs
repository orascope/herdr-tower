// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The board pane: `board --watch` in a split beside the advisor, opened by
//! the first `board send` or by `board open`.
//!
//! A pane is taken for this board's only on proof: the claim that made it
//! holds its pane id **and** terminal id (herdr reuses pane ids), and a command
//! is typed only into that pane seen at an idle shell. Anything else is split
//! afresh. herdr calls happen outside the board's lock; the lock guards only
//! the claim. See `docs/dispatch-board.md`, "The board pane".

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use herdrx::{Herdr, Pane, SplitDirection, SplitOptions};
use time::OffsetDateTime;
use tower_state::{Agent, Board, BoardStore, ItemStatus, PaneClaim, Progress};

use crate::boardcmd::{column_text, records_for, stale};

/// The token the running view keeps on its own pane.
pub const TOKEN: &str = "tower-board";
/// The metadata source the token is reported under.
pub const SOURCE: &str = "herdr-tower-board";
/// How long the token outlives the view's last refresh of it.
pub const TOKEN_TTL: Duration = Duration::from_secs(10);
/// How often the view redraws from the stored records.
pub const REDRAW_EVERY: Duration = Duration::from_secs(2);
/// The most time between the two presses of a double click.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How long a word on a click that went nowhere stays on the last line.
pub const NOTICE_FOR: Duration = Duration::from_secs(6);
/// How long a new split's shell gets to come up.
const SHELL_WAIT: Duration = Duration::from_secs(5);

/// Whether this pane runs a board view, and so is never the dock's tower.
/// The backend whose records the board can call current: `backend`, when the
/// supervisor's latest snapshot is recent and its pass refreshed that
/// backend's records. `None` draws the board as last seen, marked so.
pub fn observed(
    snapshot: Option<&tower_state::DaemonSnapshot>,
    backend: &str,
    now: OffsetDateTime,
) -> Option<String> {
    snapshot
        .is_some_and(|s| s.refreshed(backend, now))
        .then(|| backend.to_string())
}

pub fn is_board_pane(pane: &Pane) -> bool {
    pane.tokens.get(TOKEN).is_some_and(|v| !v.is_empty())
}

/// What an existing claim's pane turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// Something runs there — the view, or it starting.
    Open,
    /// Our pane, but what runs in it could not be read.
    Unknown,
    /// Our own pane, back at an idle shell: the view is typed (again).
    Restart,
    /// Not provably ours any more: gone, another terminal, or an agent in it.
    Stale,
}

/// Judges a claim against what herdr reports under its pane id.
/// `idle_shell` is `None` when the process could not be read.
pub fn judge(claim: &PaneClaim, pane: Option<&Pane>, idle_shell: Option<bool>) -> Found {
    let Some(pane) = pane else {
        return Found::Stale;
    };
    if claim.pane_id.is_empty()
        || claim.terminal_id.is_empty()
        || pane.terminal_id != claim.terminal_id
        || pane.has_agent()
    {
        return Found::Stale;
    }
    // The shell decides, not the token: a view that ended leaves its token
    // behind for up to its TTL, and an idle shell in our own terminal is the
    // stronger fact.
    match idle_shell {
        Some(true) => Found::Restart,
        // Something runs there — the view, or it starting — or we could not
        // tell: never split a second one over it.
        Some(false) => Found::Open,
        None => Found::Unknown,
    }
}

/// The command typed into the board pane. The state directory and the socket
/// travel on the command line too, not only in the split's environment: a
/// view restarted in an existing pane would otherwise inherit whatever that
/// shell has.
pub fn watch_command(exe: &Path, repo: &str, state_dir: &Path, socket: &Path) -> String {
    let q = |s: &str| tower_state::sh_quote(s);
    format!(
        "env HERDR_TOWER_STATE_DIR={} HERDR_SOCKET_PATH={} {} board --watch --repo {}",
        q(&state_dir.to_string_lossy()),
        q(&socket.to_string_lossy()),
        q(&exe.to_string_lossy()),
        q(repo)
    )
}

/// The environment the board pane is split with, so its view reads the same
/// store and talks to the same server as the advisor that opened it.
pub fn watch_env(state_dir: &Path, socket: &Path) -> HashMap<String, String> {
    [
        (
            "HERDR_TOWER_STATE_DIR".to_string(),
            state_dir.to_string_lossy().into_owned(),
        ),
        (
            "HERDR_SOCKET_PATH".to_string(),
            socket.to_string_lossy().into_owned(),
        ),
    ]
    .into()
}

/// What `open` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    AlreadyOpen(String),
    /// Our pane is there but could not be read; nothing was done to it.
    Unreadable(String),
    Started(String),
    /// Another process holds a fresh claim and is opening it.
    BeingOpened,
}

/// Where and how to open.
pub struct OpenSpec<'a> {
    pub repo: &'a str,
    pub backend: &'a str,
    pub caller: &'a str,
    pub exe: &'a Path,
    pub state_dir: &'a Path,
    pub socket: &'a Path,
    pub pid: u32,
}

fn io(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Reads a pane, telling "no such pane" apart from "could not ask": only the
/// first may release a claim — a lost answer about a live pane would
/// otherwise lead to a second split beside it.
fn look(h: &Herdr, pane_id: &str) -> Result<Option<Pane>, String> {
    match h.pane(pane_id) {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.is_code("pane_not_found") => Ok(None),
        Err(e) => Err(format!("could not read pane {pane_id}: {e}")),
    }
}

/// Whether `pane_id` is still the terminal we made and sits at an idle shell.
fn ours_and_idle(h: &Herdr, pane_id: &str, terminal_id: &str) -> Option<bool> {
    let pane = look(h, pane_id).ok()??;
    if pane.terminal_id != terminal_id || pane.has_agent() {
        return Some(false);
    }
    h.process_info(pane_id)
        .ok()
        .map(|i| crate::usagetab::idle_shell(&i))
}

/// Opens the board pane beside `spec.caller`, or finds it open.
pub fn open(h: &Herdr, boards: &BoardStore, spec: &OpenSpec<'_>) -> Result<Opened, String> {
    let now = OffsetDateTime::now_utc();
    let board = boards.load(spec.repo, now).map_err(io)?;
    if let Some(claim) = board.pane_claim(spec.backend, spec.caller).cloned() {
        if claim.pane_id.is_empty() {
            // Pending, and its opener is alive (recovery drops a dead one).
            return Ok(Opened::BeingOpened);
        }
        let pane = look(h, &claim.pane_id)?;
        let idle = pane.as_ref().and_then(|_| {
            h.process_info(&claim.pane_id)
                .ok()
                .map(|i| crate::usagetab::idle_shell(&i))
        });
        match judge(&claim, pane.as_ref(), idle) {
            Found::Open => return Ok(Opened::AlreadyOpen(claim.pane_id)),
            Found::Unknown => return Ok(Opened::Unreadable(claim.pane_id)),
            Found::Restart => {
                return start(
                    h,
                    boards,
                    spec,
                    claim.id,
                    &claim.pane_id,
                    &claim.terminal_id,
                )
            }
            Found::Stale => {
                boards
                    .update(spec.repo, now, |b| Ok(b.release_pane(claim.id)))
                    .map_err(io)?
                    .map_err(io)?;
            }
        }
    }
    let Some(id) = boards
        .update(spec.repo, now, |b| {
            Ok(b.claim_pane(spec.backend, spec.caller, spec.pid, now))
        })
        .map_err(io)?
        .map_err(io)?
    else {
        return Ok(Opened::BeingOpened);
    };
    let split = h.split_pane(&SplitOptions {
        target_pane_id: Some(spec.caller.to_string()),
        direction: SplitDirection::Right,
        cwd: Some(spec.repo.to_string()),
        env: watch_env(spec.state_dir, spec.socket),
        ratio: Some(0.38),
        focus: false,
        ..SplitOptions::default()
    });
    let pane = match split {
        Ok(p) => p,
        Err(e) => {
            // A structured refusal made nothing. Anything else may have made
            // a pane whose answer was lost: the pending claim stays, and
            // recovery drops it once this process is gone.
            if matches!(e, herdrx::Error::Api(_)) {
                let _ = boards.update(spec.repo, now, |b| Ok(b.release_pane(id)));
            }
            return Err(format!("could not split a pane for the board: {e}"));
        }
    };
    let recorded = boards
        .update(spec.repo, now, |b| {
            Ok(b.set_pane(id, &pane.pane_id, &pane.terminal_id))
        })
        .map_err(io)?
        .map_err(io)?;
    if !recorded {
        // The claim was lost meanwhile, so this pane is nobody's. Closed only
        // while it is still the terminal we made at a bare shell.
        if ours_and_idle(h, &pane.pane_id, &pane.terminal_id) == Some(true) {
            let _ = h.close_pane(&pane.pane_id);
        }
        return Ok(Opened::BeingOpened);
    }
    start(h, boards, spec, id, &pane.pane_id, &pane.terminal_id)
}

/// Types the view into claim `id`'s pane: once its shell is idle, only while
/// it is still the terminal the claim made, and only by the one opener that
/// reserved the typing.
fn start(
    h: &Herdr,
    boards: &BoardStore,
    spec: &OpenSpec<'_>,
    id: u64,
    pane_id: &str,
    terminal_id: &str,
) -> Result<Opened, String> {
    let now = OffsetDateTime::now_utc();
    let mine = boards
        .update(spec.repo, now, |b| {
            Ok(b.take_typing(id, spec.pid, now, &tower_state::pid_alive))
        })
        .map_err(io)?
        .map_err(io)?;
    if !mine {
        return Ok(Opened::BeingOpened);
    }
    let typed = type_view(h, spec, pane_id, terminal_id);
    // The reservation ends only on an outcome that is certain: nothing was
    // typed, or the view was seen running. A command sent but not yet seen
    // keeps it until its window passes, so no other open types it again
    // while the shell is still getting to it.
    let certain = matches!(typed, Typed::Untouched(_) | Typed::Running);
    let now = OffsetDateTime::now_utc();
    let _ = boards.update(spec.repo, now, |b| {
        if matches!(typed, Typed::Running) {
            b.mark_started(id);
        }
        Ok(if certain {
            b.release_typing(id, spec.pid)
        } else {
            b.typing_unconfirmed(id, spec.pid, now)
        })
    });
    match typed {
        Typed::Running => Ok(Opened::Started(pane_id.to_string())),
        Typed::Untouched(e) => Err(e),
        Typed::NotSeen(e) => Err(e),
    }
}

/// How typing the view went.
enum Typed {
    /// Nothing reached the pane.
    Untouched(String),
    /// Sent, and the view was then seen running (a busy shell in our terminal).
    Running,
    /// Sent — or maybe sent — but the view was not seen running in time.
    NotSeen(String),
}

fn type_view(h: &Herdr, spec: &OpenSpec<'_>, pane_id: &str, terminal_id: &str) -> Typed {
    let waited = Instant::now();
    loop {
        match ours_and_idle(h, pane_id, terminal_id) {
            Some(true) => break,
            Some(false)
                if look(h, pane_id).ok().flatten().map(|p| p.terminal_id)
                    != Some(terminal_id.to_string()) =>
            {
                return Typed::Untouched(format!(
                    "the board pane {pane_id} is gone or no longer ours"
                ));
            }
            _ => {}
        }
        if waited.elapsed() > SHELL_WAIT {
            return Typed::Untouched(format!(
                "the board pane {pane_id} opened but its shell did not come up; `board open` starts it"
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let sent = h.run(
        pane_id,
        &watch_command(spec.exe, spec.repo, spec.state_dir, spec.socket),
    );
    let seen = Instant::now();
    loop {
        // A busy shell in our terminal, not the token: a view stopped just
        // before leaves its token behind for a while.
        let running = look(h, pane_id).ok().flatten().is_some_and(|p| {
            p.terminal_id == terminal_id
                && h.process_info(pane_id)
                    .is_ok_and(|i| !crate::usagetab::idle_shell(&i))
        });
        if running {
            return Typed::Running;
        }
        if seen.elapsed() > SHELL_WAIT {
            return Typed::NotSeen(match sent {
                Ok(()) => format!(
                    "the board view was typed into {pane_id} but not seen running; `board open` retries once the shell is idle"
                ),
                Err(e) => format!(
                    "could not tell whether the board view reached {pane_id} ({e}); `board open` retries once the shell is idle"
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Plants the view's token on its own pane.
pub fn claim_token(h: &Herdr, pane_id: &str) {
    let _ = h.report_metadata(&herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(SOURCE.to_string()),
        tokens: [(TOKEN.to_string(), "1".to_string())].into(),
        title: None,
        ttl: Some(TOKEN_TTL),
        seq: 0,
    });
}

/// The terminal's size as (columns, rows), or 80×24 when stdout is not one.
pub fn stdout_size() -> (usize, usize) {
    // A terminal that reports no area has none to draw in.
    tower_os::stdout_size().unwrap_or((80, 24))
}

/// A line of text for the view that fits `cols` columns: cleaned of control
/// characters and cut. For anything not built by [`render`] — an error, say.
pub fn fit_line(s: &str, cols: usize) -> String {
    if cols <= 1 {
        return String::new();
    }
    tower_text::truncate_cell(&tower_state::clean_text(s), cols - 1)
}

/// Double clicks, told apart from single ones. Terminals report presses
/// only, so two presses pointing at the same pane within [`DOUBLE_CLICK`] are
/// one double click. A single click does nothing: the board is mostly read,
/// and a click that only meant to focus the board pane must not leave it.
#[derive(Debug, Default)]
pub struct Clicks {
    last: Option<(String, Instant)>,
}

impl Clicks {
    /// A scroll or fold changes what a screen row means; the next click must
    /// start a new pair even if it points at the same worker pane.
    pub fn clear(&mut self) {
        self.last = None;
    }

    /// A press on a line pointing at `pane` (or at nothing). The pane to go
    /// to when this press completes a double click; the press after that
    /// starts afresh, so a triple click is one jump.
    pub fn press(&mut self, pane: Option<&str>, at: Instant) -> Option<String> {
        let Some(pane) = pane else {
            self.last = None;
            return None;
        };
        match self.last.take() {
            Some((p, t)) if p == pane && at.saturating_duration_since(t) <= DOUBLE_CLICK => Some(p),
            _ => {
                self.last = Some((pane.to_string(), at));
                None
            }
        }
    }
}

/// Puts `text` on the frame's last line: below the rows when the pane has
/// room, over the last one when it has not, so the frame never grows taller
/// than the pane. The line points nowhere.
pub fn put_notice(
    rows: &mut Vec<(String, Option<String>)>,
    text: &str,
    (cols, height): (usize, usize),
) {
    if height == 0 {
        return;
    }
    let line = (fit_line(text, cols), None);
    if rows.len() >= height {
        rows.truncate(height - 1);
    }
    rows.push(line);
}

/// Written once when the view starts: the scrollback, then the screen.
///
/// The pane may hold whatever ran there before — the shell's history, or a
/// board view from before frames stopped scrolling, whose copies would
/// otherwise still show when the pane is scrolled back.
pub const CLEAR_ALL: &str = "\x1b[3J\x1b[H\x1b[2J";

/// One frame for the terminal: the scrollback cleared, home, each line
/// overwritten in place and cleared to its end, then everything below cleared.
///
/// Never a newline after the last line and never more lines than `render`
/// was given room for, so the terminal never scrolls. Scrolling was the bug:
/// a board taller than its pane pushed each redraw's top into the scrollback,
/// where "clear screen" does not reach, and scrolling back showed the board
/// again and again.
///
/// The scrollback is still cleared on every frame, because a pane that gets
/// narrower re-wraps the last frame's lines and the terminal pushes the
/// overflow there itself. herdr then draws its own scrollbar beside the
/// board's, and that one stays until the scrollback is gone.
pub fn frame(lines: &[String]) -> String {
    let mut out = String::from("\x1b[3J\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        out.push_str(line);
        out.push_str("\x1b[K");
    }
    out.push_str("\x1b[J");
    out
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const STRIKE: &str = "\x1b[9m";
const HEADER_BG: &str = "\x1b[48;2;31;48;58m";
const HEADER_TITLE: &str = "\x1b[1;38;2;255;175;95m";
const HEADER_DETAIL: &str = "\x1b[38;2;208;208;208m";

/// Fill the header line across the pane, stopping a cell short of its right
/// edge so a terminal never wraps the frame into its scrollback.
fn header_line(text: &str, width: usize, color: bool, title: bool) -> String {
    let fitted = tower_text::truncate_cell(text, width);
    if !color {
        return fitted;
    }
    let padded = tower_text::pad_right(&fitted, width);
    let fg = if title { HEADER_TITLE } else { HEADER_DETAIL };
    format!("{HEADER_BG}{fg}{padded}{RESET}")
}

fn board_heading(
    name: &str,
    done: usize,
    total: usize,
    closed: usize,
    action: &str,
    width: usize,
) -> String {
    let prefix = "Board · ";
    let suffix = format!("  {done}/{total}  ✗ {closed} closed (x: {action})");
    let fixed = tower_text::width(prefix) + tower_text::width(&suffix);
    if fixed < width {
        return format!(
            "{prefix}{}{suffix}",
            tower_text::truncate_cell(name, width - fixed)
        );
    }
    let compact = format!("Board {done}/{total} · ✗ {closed} (x:{action})");
    if tower_text::width(&compact) <= width {
        compact
    } else {
        tower_text::truncate_cell(&format!("{done}/{total} ✗ {closed}"), width)
    }
}

/// `line`, already cut to `width - 1` columns, padded out to them and ended
/// with a scrollbar cell: `┃` on the thumb, `│` on the track.
fn with_bar(line: &str, width: usize, thumb: bool, color: bool) -> String {
    let pad = (width - 1).saturating_sub(visible_width(line));
    let glyph = match (thumb, color) {
        (true, _) => "┃".to_string(),
        (false, true) => format!("{DIM}│{RESET}"),
        (false, false) => "│".to_string(),
    };
    format!("{line}{}{glyph}", " ".repeat(pad))
}

/// The columns `line` takes once its escape sequences are left out.
fn visible_width(line: &str) -> usize {
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            plain.push(c);
        }
    }
    tower_text::width(&plain)
}

fn tone(p: Progress) -> &'static str {
    match p {
        Progress::Working => "\x1b[36m",
        Progress::Review => "\x1b[32m",
        Progress::Waiting | Progress::Errored => "\x1b[35m",
        Progress::SentUnknown | Progress::Replaced | Progress::Unclear | Progress::Ambiguous => {
            "\x1b[33m"
        }
        Progress::Gone => "\x1b[31m",
        _ => "",
    }
}

/// The colour of a pane id at the head of a row.
const PANE: &str = "\x1b[94m";

/// The colour of a priority tag.
fn priority_tone(p: u8) -> &'static str {
    match p {
        1 => "\x1b[1;31m",
        2 => "\x1b[33m",
        _ => DIM,
    }
}

/// Colours the priority tag and the `[pane]` at the head of an already-cut
/// row line. Only what is there whole is coloured: a tag the cut reached is
/// left plain rather than guessed at.
fn paint_lead(line: &str, item: &tower_state::BoardItem, who: Option<&str>) -> String {
    let num = format!("{:>2} ", item.n);
    let Some(mut rest) = line.strip_prefix(&num) else {
        return line.to_string();
    };
    let mut out = num.clone();
    if item.priority > 0 {
        let tag = format!("P{} ", item.priority);
        if let Some(r) = rest.strip_prefix(&tag) {
            out.push_str(&format!(
                "{}P{}{RESET} ",
                priority_tone(item.priority),
                item.priority
            ));
            rest = r;
        }
    }
    if let Some(w) = who {
        let tag = format!("{w} ");
        if let Some(r) = rest.strip_prefix(&tag) {
            out.push_str(&format!("{PANE}{w}{RESET} "));
            rest = r;
        }
    }
    out.push_str(rest);
    out
}

/// The view: the board as `show` prints it, fitted to `width` columns and at
/// most `height` lines, with finished rows struck through when `color` is on.
pub fn render(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
    width: usize,
    height: usize,
    color: bool,
) -> Vec<String> {
    render_with(
        board,
        records,
        now,
        observed,
        width,
        height,
        color,
        &Default::default(),
    )
}

/// [`render`] with the linked PRs' states.
#[allow(clippy::too_many_arguments)]
pub fn render_with(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
    width: usize,
    height: usize,
    color: bool,
    prs: &std::collections::HashMap<u32, String>,
) -> Vec<String> {
    render_rows(board, records, now, observed, width, height, color, prs)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// [`render_with`], each line with the pane its item is with: a click on any
/// line of an item's block goes there. `None` for the head, the fold lines
/// and an item with no pane (planned, or a label the tower cannot see).
#[allow(clippy::too_many_arguments)]
pub fn render_rows(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
    width: usize,
    height: usize,
    color: bool,
    prs: &std::collections::HashMap<u32, String>,
) -> Vec<(String, Option<String>)> {
    render_rows_scrolled(
        board, records, now, observed, width, height, color, prs, 0, false,
    )
    .rows
}

/// A screenful of the board. `offset` counts body lines, leaving the header
/// visible while every shown item and its note can be reached in a short pane.
pub struct Page {
    pub rows: Vec<(String, Option<String>)>,
    pub offset: usize,
    pub max_offset: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn render_rows_scrolled(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
    width: usize,
    height: usize,
    color: bool,
    prs: &std::collections::HashMap<u32, String>,
    offset: usize,
    show_closed: bool,
) -> Page {
    let paint = |code: &str, s: &str| {
        if color && !code.is_empty() {
            format!("{code}{s}{RESET}")
        } else {
            s.to_string()
        }
    };
    let done = board
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Done)
        .count();
    // A path and a task name are not ours to trust with the cursor.
    let name = tower_state::clean_text(
        Path::new(&board.repo)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&board.repo),
    );
    // One column short of the edge: a line that fills the last column leaves
    // some terminals waiting to wrap, and the next write lands a line lower.
    let width = width.saturating_sub(1);
    if width == 0 || height == 0 {
        return Page {
            rows: Vec::new(),
            offset: 0,
            max_offset: 0,
        };
    }
    // One header line: the title, with the scroll range appended when it fits.
    let mut title = format!("Board · {name}  {done}/{}", board.items.len());
    let mut head = vec![header_line(&title, width, color, true)];
    if observed.is_none() {
        head.push(paint(
            DIM,
            &tower_text::truncate_cell(
                "not refreshed by the supervisor — progress as last seen",
                width,
            ),
        ));
    }
    if board.items.is_empty() {
        head.push(paint(
            DIM,
            &tower_text::truncate_cell("no items — `board add \"…\"` from the advisor", width),
        ));
        head.truncate(height);
        return Page {
            rows: head.into_iter().map(|l| (l, None)).collect(),
            offset: 0,
            max_offset: 0,
        };
    }
    // Each item supplies its lines and the pane a click on those lines targets.
    // How many lines an item takes does not depend on `width`, so the body can
    // be built again a column narrower once it is known to need a scrollbar.
    let blocks_at = |width: usize| -> Vec<(bool, Vec<String>, Option<String>)> {
        let mut blocks: Vec<(bool, Vec<String>, Option<String>)> = board
            .items
            .iter()
            .map(|item| {
                let found = records_for(item, records);
                let c = tower_state::column(item, &found);
                let task = crate::boardcmd::task_cell(item);
                let text = crate::boardcmd::row_text(item, &found);
                // Number and text cut together, once: cut apart, a very narrow
                // pane got an ellipsis from each and drew wider than itself.
                let head_line = |mark: &str| {
                    tower_text::truncate_cell(&format!("{mark}{:>2} {text}", item.n), width)
                };
                // No task, no gap: the status line leads with the icon.
                let task = if task.is_empty() {
                    task
                } else {
                    format!("{task}  ")
                };
                let status = tower_text::truncate_cell(
                    &format!(
                        "   {task}{}{}",
                        column_text(&c, now, stale(item, observed)),
                        crate::boardcmd::pr_cell(item, prs)
                    ),
                    width,
                );
                let mut lines = Vec::new();
                if c.struck() {
                    lines.push(if color {
                        format!("{DIM}{STRIKE}{}{RESET}", head_line(""))
                    } else {
                        head_line("✗")
                    });
                    lines.push(paint(DIM, &status));
                } else {
                    let plain = head_line("");
                    lines.push(if color {
                        paint_lead(&plain, item, crate::boardcmd::who(item, &found).as_deref())
                    } else {
                        plain
                    });
                    lines.push(paint(tone(c.progress), &status));
                }
                if !item.note.is_empty() {
                    let note = format!("   ↳ {}", tower_state::clean_text(&item.note));
                    lines.push(paint(DIM, &tower_text::truncate_cell(&note, width)));
                }
                (c.struck(), lines, crate::boardcmd::pane_of(item, &found))
            })
            .collect();
        // Struck items sink below the live ones; the sort is stable, so each
        // group keeps the board's own order.
        blocks.sort_by_key(|(is_closed, _, _)| *is_closed);
        blocks
    };
    let blocks = blocks_at(width);
    let room = height.saturating_sub(head.len());
    let total: usize = blocks.iter().map(|(_, lines, _)| lines.len()).sum();
    let fold_closed = !show_closed && total > room;
    let closed = blocks.iter().filter(|(is_closed, _, _)| *is_closed).count();
    if closed > 0 && (fold_closed || show_closed) {
        title = board_heading(
            &name,
            done,
            board.items.len(),
            closed,
            if fold_closed { "show" } else { "hide" },
            width,
        );
        head[0] = header_line(&title, width, color, true);
    }
    let body_of =
        |blocks: Vec<(bool, Vec<String>, Option<String>)>| -> Vec<(String, Option<String>)> {
            blocks
                .into_iter()
                .filter(|(is_closed, _, _)| !fold_closed || !*is_closed)
                .flat_map(|(_, lines, pane)| {
                    lines.into_iter().map(move |line| (line, pane.clone()))
                })
                .collect()
        };
    let mut body = body_of(blocks);
    let max_offset = if room == 0 {
        0
    } else {
        body.len().saturating_sub(room)
    };
    let offset = offset.min(max_offset);
    if max_offset > 0 || (fold_closed && closed > 0) || (show_closed && closed > 0) {
        let end = (offset + room).min(body.len());
        let start = if room == 0 || body.is_empty() {
            0
        } else {
            offset + 1
        };
        let range = format!("  ↕ {start}-{end}/{}", body.len());
        if tower_text::width(&title) + tower_text::width(&range) <= width {
            head[0] = header_line(&format!("{title}{range}"), width, color, true);
        }
    }
    // Lines are hidden: the body gives up its last column to a scrollbar, the
    // same one the dock's list draws.
    let shown = room.min(body.len() - offset);
    let bar = if max_offset > 0 && width >= 2 {
        body = body_of(blocks_at(width - 1));
        tower_ui::view::scrollbar(
            body.len(),
            tower_ui::view::ListWindow {
                start: offset,
                end: offset + shown,
                above: offset,
                below: body.len() - offset - shown,
            },
        )
    } else {
        Vec::new()
    };
    let mut rows: Vec<(String, Option<String>)> = head.into_iter().map(|l| (l, None)).collect();
    for (i, (line, pane)) in body.into_iter().skip(offset).take(room).enumerate() {
        rows.push(match bar.get(i) {
            Some(Some(thumb)) => (with_bar(&line, width, *thumb, color), pane),
            _ => (line, pane),
        });
    }
    rows.truncate(height);
    Page {
        rows,
        offset,
        max_offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        backends: &[(&str, bool, bool)],
        at: OffsetDateTime,
    ) -> tower_state::DaemonSnapshot {
        tower_state::DaemonSnapshot {
            pid: 1,
            version: "v".into(),
            started_at: at,
            updated_at: at,
            connected: true,
            error: String::new(),
            tab_roots: Vec::new(),
            workspace_order: Vec::new(),
            workspace_display_labels: Default::default(),
            parents: Default::default(),
            latest_release: String::new(),
            backends: backends
                .iter()
                .map(|(id, connected, refreshed)| tower_state::DaemonBackend {
                    id: id.to_string(),
                    socket: String::new(),
                    connected: *connected,
                    error: String::new(),
                    refreshed: *refreshed,
                })
                .collect(),
        }
    }

    /// The pane calls its backend current exactly when the supervisor's
    /// snapshot says its latest pass refreshed it — no snapshot, an old one,
    /// or another backend's refresh leave the board "as last seen".
    #[test]
    fn the_pane_takes_current_from_the_supervisor() {
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let s = snapshot(&[("", true, true), ("lab", true, false)], at);
        assert_eq!(observed(Some(&s), "", at), Some(String::new()));
        assert_eq!(observed(Some(&s), "lab", at), None);
        assert_eq!(observed(None, "", at), None, "no supervisor");
        assert_eq!(
            observed(Some(&s), "", at + time::Duration::minutes(5)),
            None,
            "a supervisor that stopped writing"
        );
    }

    /// The pane only draws: its loop asks neither herdr for the records nor
    /// `gh` for PR states — those are the supervisor's.
    #[test]
    fn the_board_pane_leaves_herdr_and_gh_to_the_supervisor() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/rest.rs"),
        )
        .unwrap();
        let start = src
            .find("fn watch_board(")
            .expect("watch_board is in cmd/rest.rs");
        let body = &src[start..];
        let body = &body[..body[1..].find("\nfn ").map_or(body.len(), |i| i + 1)];
        for call in [
            "sync_named_backend",
            "sync_backend",
            "pr_states(repo,",
            "boardcmd::pr_states",
        ] {
            assert!(!body.contains(call), "watch_board calls {call}");
        }
        assert!(
            body.contains("boards.pr_states(repo)"),
            "it reads the kept states"
        );
    }

    fn pane(terminal: &str, agent: Option<&str>, token: bool) -> Pane {
        let mut v = serde_json::json!({
            "pane_id": "w1:p9",
            "terminal_id": terminal,
            "agent": agent,
        });
        if token {
            v["tokens"] = serde_json::json!({ TOKEN: "1" });
        }
        serde_json::from_value(v).unwrap()
    }

    fn claim(pane_id: &str, terminal: &str) -> PaneClaim {
        PaneClaim {
            id: 1,
            backend: String::new(),
            caller_pane: "w1:p1".into(),
            pid: 1,
            at: OffsetDateTime::UNIX_EPOCH,
            pane_id: pane_id.into(),
            terminal_id: terminal.into(),
            started: true,
            typing: None,
        }
    }

    #[test]
    fn only_our_own_terminal_is_ever_reused() {
        let c = claim("w1:p9", "term_A");
        assert_eq!(judge(&c, None, None), Found::Stale, "gone");
        assert_eq!(
            judge(&c, Some(&pane("term_B", None, false)), Some(true)),
            Found::Stale,
            "the pane id was reused by another terminal"
        );
        assert_eq!(
            judge(&c, Some(&pane("term_A", Some("codex"), false)), Some(true)),
            Found::Stale,
            "an agent lives there now"
        );
        assert_eq!(
            judge(
                &claim("w1:p9", ""),
                Some(&pane("term_A", None, false)),
                Some(true)
            ),
            Found::Stale,
            "no terminal proof"
        );
    }

    #[test]
    fn our_pane_is_open_restarted_or_left_alone() {
        let c = claim("w1:p9", "term_A");
        assert_eq!(
            judge(&c, Some(&pane("term_A", None, true)), Some(false)),
            Found::Open
        );
        assert_eq!(
            judge(&c, Some(&pane("term_A", None, false)), Some(true)),
            Found::Restart
        );
        assert_eq!(
            judge(&c, Some(&pane("term_A", None, false)), Some(false)),
            Found::Open,
            "something runs there: never split a second one"
        );
        assert_eq!(
            judge(&c, Some(&pane("term_A", None, false)), None),
            Found::Unknown,
            "not read: said so, nothing done"
        );
        assert_eq!(
            judge(&c, Some(&pane("term_A", None, true)), Some(true)),
            Found::Restart,
            "the view ended; its token outlives it"
        );
    }

    #[test]
    fn the_command_and_environment_are_explicit_and_quoted() {
        let cmd = watch_command(
            Path::new("/opt/it's/herdr-tower"),
            "/w/my repo",
            Path::new("/s d"),
            Path::new("/sock"),
        );
        assert_eq!(
            cmd,
            r#"env HERDR_TOWER_STATE_DIR='/s d' HERDR_SOCKET_PATH='/sock' '/opt/it'\''s/herdr-tower' board --watch --repo '/w/my repo'"#
        );
        let env = watch_env(Path::new("/s"), Path::new("/sock"));
        assert_eq!(env["HERDR_TOWER_STATE_DIR"], "/s");
        assert_eq!(env["HERDR_SOCKET_PATH"], "/sock");
    }

    #[test]
    fn the_view_strikes_finished_rows_and_colours_progress() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/dbstore");
        b.add(&["parser".into(), "tests".into()], now).unwrap();
        b.done(&[1], now).unwrap();
        let out = render(&b, &[], now, Some(""), 60, 40, true).join("\n");
        assert!(out.contains("Board · dbstore  1/2"), "{out}");
        assert!(out.contains(&format!("{DIM}{STRIKE} 1 parser")), "{out:?}");
        assert!(out.contains(" 2 tests"), "{out}");
        let plain = render(&b, &[], now, None, 60, 40, false).join("\n");
        assert!(!plain.contains('\x1b'), "{plain:?}");
        assert!(plain.contains("✗ 1 parser"), "{plain}");
        assert!(plain.contains("not refreshed by the supervisor"), "{plain}");
    }

    #[test]
    fn the_coloured_header_is_a_full_width_band() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let b = many(15, &[1, 2, 3, 4]);
        let rows = render(&b, &[], now, Some(""), 60, 12, true);
        let content = rows[0]
            .strip_prefix(&format!("{HEADER_BG}{HEADER_TITLE}"))
            .and_then(|line| line.strip_suffix(RESET))
            .expect("the header has its own background and text colour");
        assert_eq!(tower_text::width(content), 59, "fills the safe pane width");
        assert!(rows[0].contains("✗ 4 closed (x: show)"));
        assert!(rows[0].contains("1-11/22"));
        assert!(!rows[1].contains(HEADER_BG), "the body has no header band");
    }

    #[test]
    fn struck_items_sink_below_the_live_ones() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/dbstore");
        b.add(&["first".into(), "second".into(), "third".into()], now)
            .unwrap();
        b.done(&[1], now).unwrap();
        let plain = render(&b, &[], now, Some(""), 60, 40, false).join("\n");
        let at = |t: &str| plain.find(t).unwrap_or_else(|| panic!("{t} in {plain}"));
        assert!(
            at("second") < at("third") && at("third") < at("first"),
            "{plain}"
        );
        let text = crate::boardcmd::show_text(&b, &[], now, Some(""));
        let at = |t: &str| text.find(t).unwrap_or_else(|| panic!("{t} in {text}"));
        assert!(
            at("second") < at("third") && at("third") < at("first"),
            "{text}"
        );
    }

    #[test]
    fn a_status_line_without_a_task_leads_with_its_icon() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/dbstore");
        b.add(&["parser".into(), "tests".into()], now).unwrap();
        b.done(&[1], now).unwrap();
        let plain = render(&b, &[], now, Some(""), 60, 40, false).join("\n");
        assert!(plain.contains("\n   – done"), "{plain}");
        assert!(plain.contains("\n   ○ planned"), "{plain}");
        assert!(!plain.contains('—'), "{plain}");
        b.items[1].task = "i-tests".into();
        let plain = render(&b, &[], now, Some(""), 60, 40, false).join("\n");
        assert!(plain.contains("\n   i-tests  ○ planned"), "{plain}");
    }

    #[test]
    fn each_line_of_an_items_block_points_at_its_pane() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/dbstore");
        b.add(&["parser".into(), "tests".into()], now).unwrap();
        let pin = tower_state::PaneWorker {
            backend: String::new(),
            pane_id: "w4F:pY".into(),
            agent_id: String::new(),
            session_id: String::new(),
            born_at: None,
        };
        b.assign(
            1,
            tower_state::AssignTo::Send(tower_state::SendTo::Pane(pin)),
        )
        .unwrap();
        b.note(1, "round 2").unwrap();
        let rows = render_rows(&b, &[], now, Some(""), 60, 40, false, &Default::default());
        let at = |needle: &str| {
            rows.iter()
                .find(|(l, _)| l.contains(needle))
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| panic!("{needle}: {rows:?}"))
        };
        assert_eq!(at("Board · dbstore"), None);
        assert_eq!(at("1 [w4F:pY] parser"), Some("w4F:pY".into()));
        assert_eq!(at("↳ round 2"), Some("w4F:pY".into()));
        assert_eq!(at(" 2 tests"), None, "nobody has it");
        // The plain lines are render_with's, unchanged.
        let lines: Vec<String> = rows.into_iter().map(|(l, _)| l).collect();
        assert_eq!(lines, render(&b, &[], now, Some(""), 60, 40, false));
    }

    #[test]
    fn only_a_double_click_on_one_pane_goes_there() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut c = Clicks::default();
        assert_eq!(c.press(Some("w1:p1"), t0), None, "a single click stays");
        assert_eq!(c.press(Some("w1:p1"), ms(300)), Some("w1:p1".into()));
        assert_eq!(
            c.press(Some("w1:p1"), ms(400)),
            None,
            "a third press starts afresh"
        );
        let mut c = Clicks::default();
        c.press(Some("w1:p1"), t0);
        assert_eq!(c.press(Some("w1:p1"), ms(401)), None, "too slow");
        assert_eq!(
            c.press(Some("w1:p1"), ms(600)),
            Some("w1:p1".into()),
            "the slow one counts as a first"
        );
        let mut c = Clicks::default();
        c.press(Some("w1:p1"), t0);
        assert_eq!(c.press(Some("w1:p2"), ms(100)), None, "another item");
        c.press(Some("w1:p1"), ms(200));
        assert_eq!(c.press(None, ms(300)), None);
        assert_eq!(
            c.press(Some("w1:p1"), ms(350)),
            None,
            "a press on nothing in between"
        );
        c.press(Some("w1:p1"), ms(450));
        c.clear();
        assert_eq!(
            c.press(Some("w1:p1"), ms(500)),
            None,
            "scrolling or folding breaks a click pair"
        );
    }

    #[test]
    fn a_notice_takes_the_last_line_and_never_grows_the_frame() {
        let row = |s: &str| (s.to_string(), Some("w1:p1".to_string()));
        let mut rows = vec![row("a"), row("b")];
        put_notice(&mut rows, "could not go\x1b[2J", (40, 5));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], ("could not go[2J".to_string(), None));
        let mut full = vec![row("a"), row("b"), row("c")];
        put_notice(&mut full, "gone", (40, 3));
        assert_eq!(full.len(), 3);
        assert_eq!(full[2], ("gone".to_string(), None));
        let mut none = vec![row("a")];
        put_notice(&mut none, "gone", (40, 0));
        assert_eq!(none.len(), 1);
    }

    fn many(n: u32, closed: &[u32]) -> Board {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/dbstore");
        let texts: Vec<String> = (1..=n).map(|i| format!("item {i}")).collect();
        b.add(&texts, now).unwrap();
        b.done(closed, now).unwrap();
        b
    }

    #[test]
    fn a_board_taller_than_its_pane_never_exceeds_it() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let b = many(15, &[1, 2, 3, 4]);
        for height in [3, 5, 10, 17, 24, 40] {
            let lines = render(&b, &[], now, Some(""), 60, height, false);
            assert!(lines.len() <= height, "{height}: {}", lines.len());
        }
    }

    /// A body line without its scrollbar cell and the padding before it.
    fn text(line: &str) -> &str {
        line.trim_end_matches(['┃', '│']).trim_end()
    }

    #[test]
    fn visible_width_leaves_out_escapes_only() {
        // Edge cases: nothing, escapes alone, wide glyphs, a sequence cut off.
        let cases = [
            ("empty", "", 0),
            ("plain", "abc", 3),
            ("escapes only", "\x1b[0m\x1b[1;38;2;255;175;95m", 0),
            ("wide glyphs", "\x1b[2m한글\x1b[0m", 4),
            ("unterminated", "ab\x1b[38;2", 2),
            ("bare escape", "a\x1b", 1),
        ];
        for (name, line, want) in cases {
            assert_eq!(visible_width(line), want, "{name}");
        }
        // Property: painting never changes what shows.
        for code in [DIM, STRIKE, PANE, HEADER_BG, "\x1b[1;31m"] {
            for s in ["", "x", "↳ note", "P1 [w2:pE1] 한글"] {
                let painted = format!("{code}{s}{RESET}");
                assert_eq!(visible_width(&painted), tower_text::width(s), "{painted:?}");
            }
        }
    }

    #[test]
    fn a_bar_cell_round_trips_and_ends_every_width() {
        for width in 2..12 {
            for (len, line) in [0, 1, width - 1].into_iter().map(|n| (n, "a".repeat(n))) {
                for (thumb, color) in [(true, false), (false, false), (true, true), (false, true)] {
                    let out = with_bar(&line, width, thumb, color);
                    // Always one cell short of the pane, the bar in the last.
                    assert_eq!(visible_width(&out), width, "{width} {len} {out:?}");
                    // Round trip: the text comes back once the bar is taken off.
                    let plain = out.replace(DIM, "").replace(RESET, "");
                    assert_eq!(text(&plain), line, "{out:?}");
                    assert_eq!(plain.ends_with('┃'), thumb, "{out:?}");
                }
            }
        }
    }

    #[test]
    fn the_board_thumb_walks_down_with_the_offset() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let b = many(12, &[]);
        let page = |offset| {
            render_rows_scrolled(
                &b,
                &[],
                now,
                Some(""),
                30,
                8,
                false,
                &Default::default(),
                offset,
                false,
            )
        };
        let max = page(usize::MAX).max_offset;
        let mut last_at = 0;
        for offset in 0..=max {
            let p = page(offset);
            let bar: Vec<bool> = p.rows[1..].iter().map(|(l, _)| l.ends_with('┃')).collect();
            // One unbroken thumb, never moving back up as the offset grows.
            let at = bar.iter().position(|&t| t).expect("a thumb");
            let len = bar.iter().filter(|&&t| t).count();
            assert!(bar[at..at + len].iter().all(|&t| t), "{offset}: {bar:?}");
            assert!(at >= last_at, "{offset}: {at} < {last_at}");
            last_at = at;
            // Cross-check against the dock's bar for the same window.
            let shown = p.rows.len() - 1;
            let want: Vec<bool> = tower_ui::view::scrollbar(
                24,
                tower_ui::view::ListWindow {
                    start: offset,
                    end: offset + shown,
                    above: offset,
                    below: 24 - offset - shown,
                },
            )
            .into_iter()
            .map(|c| c == Some(true))
            .collect();
            assert_eq!(bar, want, "{offset}");
        }
        assert!(page(0).rows[1].0.ends_with('┃'));
        assert!(page(max).rows.last().unwrap().0.ends_with('┃'));
    }

    #[test]
    fn an_overflowing_board_has_a_scrollbar() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let b = many(15, &[]);
        let page = |offset, color| {
            render_rows_scrolled(
                &b,
                &[],
                now,
                Some(""),
                40,
                12,
                color,
                &Default::default(),
                offset,
                false,
            )
        };
        // Every body line ends in the column before the pane's last: the
        // thumb at the top for the first lines, at the bottom for the last.
        let top = page(0, false);
        let bar: String = top.rows[1..]
            .iter()
            .map(|(l, _)| {
                assert_eq!(tower_text::width(l), 39, "{l:?}");
                l.chars().last().unwrap()
            })
            .collect();
        assert!(bar.starts_with('┃') && bar.ends_with('│'), "{bar}");
        assert!(
            !top.rows[0].0.ends_with(['┃', '│']),
            "the header has no bar"
        );
        let bottom = page(usize::MAX, false);
        let bar: String = bottom.rows[1..]
            .iter()
            .map(|(l, _)| l.chars().last().unwrap())
            .collect();
        assert!(bar.starts_with('│') && bar.ends_with('┃'), "{bar}");
        // Coloured lines are padded by what shows, not by their escapes.
        for (l, _) in &page(0, true).rows[1..] {
            assert_eq!(visible_width(l), 39, "{l:?}");
        }
        // A board that fits keeps every column for its text.
        let fits = render_rows_scrolled(
            &b,
            &[],
            now,
            Some(""),
            40,
            40,
            false,
            &Default::default(),
            0,
            false,
        );
        assert!(fits.rows.iter().all(|(l, _)| !l.ends_with(['┃', '│'])));
    }

    #[test]
    fn scrolling_reaches_all_open_items_and_closed_items_can_be_shown() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let b = many(15, &[1, 2, 3, 4]);
        // 2 header + 15 blocks of 2 = 32 lines: all fit in 40.
        let all = render(&b, &[], now, Some(""), 60, 40, false);
        assert_eq!(all.len(), 31);
        assert!(all.iter().any(|l| l.contains("✗ 1 item 1")));
        let first = render_rows_scrolled(
            &b,
            &[],
            now,
            Some(""),
            60,
            12,
            false,
            &Default::default(),
            0,
            false,
        );
        assert_eq!(first.rows.len(), 12);
        assert_eq!(first.max_offset, 11);
        assert!(first.rows[0].0.contains("✗ 4 closed (x: show)"));
        assert!(text(&first.rows[1].0).ends_with("item 5"));
        assert!(first.rows[0].0.contains("1-11/22"));
        let last = render_rows_scrolled(
            &b,
            &[],
            now,
            Some(""),
            60,
            12,
            false,
            &Default::default(),
            usize::MAX,
            false,
        );
        assert_eq!(last.offset, 11);
        assert!(last.rows.iter().any(|(l, _)| text(l).ends_with("item 15")));
        assert!(last.rows[0].0.contains("12-22/22"));
        let expanded = render_rows_scrolled(
            &b,
            &[],
            now,
            Some(""),
            60,
            12,
            false,
            &Default::default(),
            usize::MAX,
            true,
        );
        assert_eq!(expanded.max_offset, 19);
        assert!(expanded.rows[0].0.contains("✗ 4 closed (x: hide)"));
        // Struck items sit below every live one, so the end of the scroll has them.
        assert!(expanded.rows.iter().any(|(l, _)| l.contains("✗ 1 item 1")));
        let texts: Vec<&str> = expanded.rows.iter().skip(1).map(|(l, _)| text(l)).collect();
        let last_live = texts.iter().rposition(|l| l.ends_with("item 15"));
        let first_struck = texts.iter().position(|l| l.contains("✗"));
        assert!(last_live.is_none() || first_struck.is_none() || last_live < first_struck);
    }

    #[test]
    fn narrow_header_keeps_item_and_closed_counts() {
        let heading = board_heading("a_very_long_repository_name", 33, 53, 36, "show", 34);
        assert!(tower_text::width(&heading) <= 34);
        assert!(heading.contains("33/53"), "{heading}");
        assert!(heading.contains("✗ 36"), "{heading}");
    }

    #[test]
    fn a_scrolled_item_keeps_its_click_target() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = many(5, &[]);
        b.assign(
            5,
            tower_state::AssignTo::Send(tower_state::SendTo::Pane(tower_state::PaneWorker {
                backend: String::new(),
                pane_id: "w2:pE1".into(),
                agent_id: String::new(),
                session_id: String::new(),
                born_at: None,
            })),
        )
        .unwrap();
        b.note(5, "last note").unwrap();
        let page = render_rows_scrolled(
            &b,
            &[],
            now,
            Some(""),
            60,
            5,
            false,
            &Default::default(),
            usize::MAX,
            false,
        );
        assert_eq!(page.rows.len(), 5);
        assert_eq!(text(&page.rows[4].0), "   ↳ last note");
        assert_eq!(page.rows[4].1.as_deref(), Some("w2:pE1"));
    }

    #[test]
    fn the_view_starts_by_clearing_the_scrollback_and_the_screen() {
        assert!(CLEAR_ALL.starts_with("\x1b[3J"), "scrollback first");
        assert!(CLEAR_ALL.ends_with("\x1b[2J"));
    }

    #[test]
    fn a_frame_overwrites_in_place_and_never_scrolls() {
        let f = frame(&["a".into(), "b".into()]);
        assert_eq!(f, "\x1b[3J\x1b[Ha\x1b[K\r\nb\x1b[K\x1b[J");
        assert!(!f.ends_with('\n'));
    }

    #[test]
    fn every_frame_clears_what_a_narrowed_pane_wrapped_into_the_scrollback() {
        for lines in [vec![], vec!["a".to_string()], vec!["a".into(), "b".into()]] {
            let f = frame(&lines);
            assert!(f.starts_with("\x1b[3J"), "scrollback first: {f:?}");
            assert_eq!(f.matches("\x1b[3J").count(), 1);
            assert!(
                !f.contains("\x1b[2J"),
                "the screen is overwritten, never blanked"
            );
        }
    }

    #[test]
    fn tiny_and_empty_panes_get_nothing_wider_or_taller_than_themselves() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let b = many(5, &[1]);
        assert!(render(&b, &[], now, Some(""), 80, 0, false).is_empty());
        assert!(render(&b, &[], now, Some(""), 0, 20, false).is_empty());
        for cols in [2, 5, 10, 21] {
            for line in render(&b, &[], now, None, cols, 20, false) {
                assert!(tower_text::width(&line) < cols, "{cols}: {line:?}");
            }
        }
        assert_eq!(render(&b, &[], now, Some(""), 80, 1, false).len(), 1);
        let tiny = render_rows_scrolled(
            &b,
            &[],
            now,
            Some(""),
            80,
            1,
            false,
            &Default::default(),
            usize::MAX,
            false,
        );
        assert_eq!(tiny.max_offset, 0);
        assert!(tiny.rows[0].0.contains("0-0/"));
    }

    #[test]
    fn an_error_line_is_cleaned_and_fitted() {
        let line = fit_line("cannot read the board: /a/very/long/path\nsecond", 20);
        assert!(!line.contains('\n'), "{line:?}");
        assert!(tower_text::width(&line) < 20, "{line:?}");
        assert_eq!(fit_line("anything", 1), "");
        assert_eq!(fit_line("anything", 0), "");
    }

    #[test]
    fn a_repo_or_task_name_cannot_move_the_cursor() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/evil\nname\x1b[2J");
        b.add(&["x".into()], now).unwrap();
        let t = b
            .reserve_send(
                1,
                "t\n\x1b[H",
                "/w/wt/t",
                tower_state::SendFlags::default(),
                1,
                now,
            )
            .unwrap();
        b.apply(t, tower_state::Outcome::Unknown, now).unwrap();
        for line in render(&b, &[], now, Some(""), 60, 20, false) {
            assert!(!line.contains('\n') && !line.contains('\x1b'), "{line:?}");
        }
    }

    #[test]
    fn a_rows_pane_and_priority_are_coloured_and_its_width_unchanged() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/dbstore");
        b.add(&["#3212 jsonb".into()], now).unwrap();
        b.assign(1, tower_state::AssignTo::Label("w2:pBR".into()))
            .unwrap();
        b.set_priority(1, 1).unwrap();
        let lines = render(&b, &[], now, Some(""), 60, 20, true);
        let row = lines.iter().find(|l| l.contains("jsonb")).unwrap();
        assert!(
            row.contains(&format!(
                "\x1b[1;31mP1{RESET} {PANE}[w2:pBR]{RESET} #3212 jsonb"
            )),
            "{row:?}"
        );
        let plain = render(&b, &[], now, Some(""), 60, 20, false);
        let plain_row = plain.iter().find(|l| l.contains("jsonb")).unwrap();
        assert_eq!(plain_row, " 1 P1 [w2:pBR] #3212 jsonb");
        // A cut through the tag leaves it plain rather than half-coloured.
        let narrow = render(&b, &[], now, Some(""), 10, 20, true);
        let cut = narrow.iter().find(|l| l.starts_with(" 1 ")).unwrap();
        assert!(!cut.contains(PANE), "{cut:?}");
    }

    #[test]
    fn a_narrow_pane_still_fits() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut b = Board::new("/w/dbstore");
        b.add(
            &["a very long item text that will not fit in a narrow pane at all".into()],
            now,
        )
        .unwrap();
        for line in render(&b, &[], now, Some(""), 30, 40, false) {
            assert!(tower_text::width(&line) <= 30, "{line:?}");
        }
    }
}
