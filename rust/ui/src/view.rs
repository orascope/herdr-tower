// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Turning state into lines. No terminal anywhere in here.
//!
//! ratatui's `Line`/`Span` are plain data — text on one side, style on the
//! other — so the whole screen can be produced here and inspected without a
//! terminal. [`screen_lines`] is the whole picture as values; [`render`] is
//! three lines that hand those values to a `Paragraph`.
//!
//! That split is deliberate. "The selection bar reaches the right edge", "the
//! task column is 30 wide", "a group divider appears between kinds but not
//! before the first one" and "the cursor row is always on screen" would
//! otherwise only be checkable by rendering the whole thing to a string and
//! searching it. Here they are properties of functions that take numbers and
//! return values, and the terminal is only involved in the one test that proves
//! the values reach the buffer intact.
//!
//! Two pieces of arithmetic are deliberately absent:
//!
//!   - The selection bar does not measure itself. Computing
//!     `width - 2 - width_of(state)` to know how much padding the bar needs is
//!     the calculation that has to be right for the bar to reach the edge — and
//!     it invites a width table that disagrees with the one used to cut the
//!     line (see [`crate::text`]). [`selection_bar`] patches a background onto
//!     the spans that already exist and pads to the area width, so there is no
//!     second measurement to get wrong.
//!   - There are no `height - 10` or `5 + agents + groups + 5` fudge constants.
//!     Those are what a renderer that hands back one string needs, to guess how
//!     many rows its own header and footer will occupy. Here the header and
//!     footer are built first and counted, so the body gets exactly what is
//!     left.

use std::ops::Range;

use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use time::OffsetDateTime;
use tower_state::{Agent, AgentState, DEFAULT_LOOP_MESSAGE, NO_WORKSPACE};

use crate::app::{App, Pending, Screen};
use crate::text::{clamp_line, pad_right, shorten_home, since, truncate_width};
// Named apart from the many `width: usize` parameters in this file, which would
// shadow it — every text measurement here is this one.
use crate::text::width as text_width;
use crate::theme;

/// Column widths, matching the header row above them. Kept together because
/// they only make sense as a set: changing one without the header shears the
/// whole table.
const W_STATE: usize = 9;
/// `opencode`, the longest kind name, so no row is wider than its header.
const W_KIND: usize = 8;
const W_PANE: usize = 7;
/// Exactly the short session ID, so it is never cut.
const W_SID: usize = tower_state::SHORT_SESSION_ID;
/// TITLE gets the wider cell: the name a person gave the pane, or else the
/// CLI's own terminal title, which it rewrites every turn — claude a summary
/// of the turn, codex the task and its branch.
const W_TITLE: usize = 30;
const W_TASK: usize = 24;
/// The tree under a workspace rule. Six cells fit both a root (`├─┬─`) and a
/// worker (`│ └──`) without moving the STATE column between hierarchy levels.
const GUTTER: usize = 6;
const W_PANE_TEXT: usize = W_PANE;
const W_SID_TEXT: usize = W_SID;
/// Each text is cut two columns short of its cell so a truncated one never
/// touches the next column.
const W_TITLE_TEXT: usize = W_TITLE - 2;
const W_TASK_TEXT: usize = W_TASK - 2;
/// The narrowest list that gets a SID column.
///
/// SID costs the row nine columns, and at 120 — a laptop's popup, a split dock
/// pane — those come out of the folder and the badges after it. It is the one
/// column such a screen can do without: the detail card (`i`) always has the
/// selected pane's short ID, and the preview heading its whole one whenever
/// there is room for a preview.
const SID_COLUMN_FROM: usize = 140;

/// Whether a list this wide has a SID column. See [`SID_COLUMN_FROM`].
fn sid_column(width: usize) -> bool {
    width >= SID_COLUMN_FROM
}

/// What an empty TITLE or TASK cell says. Blank read as a rendering fault, and
/// a row whose cells are all blank could not be told from a half-drawn one.
const NOT_KNOWN: &str = "N/A";

/// The preview needs a title rule plus three rows to be worth the space. An
/// armed loop uses one of those rows for its instruction.
pub const PREVIEW_MIN: usize = 4;
/// Rows set aside for lines between the list, the preview and the key guide:
/// none. The summary and the preview are teal bands, and the key guide's own
/// line ends the preview — every yellow dot line is gone (2026-09-26).
pub const PREVIEW_RULES: usize = 0;

/// `line` on the preview's background, filled out to `width`.
fn banded(line: Line<'static>, width: usize) -> Line<'static> {
    let bg = theme::preview_background();
    let mut spans: Vec<Span<'static>> = line.spans.into_iter().map(|s| s.patch_style(bg)).collect();
    let used: usize = spans.iter().map(|s| text_width(&s.content)).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), bg));
    }
    Line::from(spans)
}
/// The title rule plus forty rows. More than that and the preview is reading
/// the pane rather than glancing at it, and the list it was meant to sit under
/// has become the smaller half of the screen.
const PREVIEW_MAX: usize = 41;

// ── labels ───────────────────────────────────────────────────────────────

/// The state cell's text, padded to a fixed width.
///
/// The padding is the point. A label written straight into the row would put
/// every column to the right of a short word further along than on its
/// neighbours — the table would shear on exactly the rows the tower exists to
/// draw attention to.
///
/// The words are herdr's own (`working`, `idle`, `done`), so a row reads the
/// same here as in herdr's sidebar and `prefix+g` list. A working agent whose
/// hook has gone quiet trades its dot for `?` rather than gaining a suffix, so
/// it keeps the column's width.
pub fn state_label(a: &Agent, now: OffsetDateTime) -> &'static str {
    match a.state {
        AgentState::Working if a.stale(now) => "? working",
        AgentState::Working => "● working",
        AgentState::Waiting => "◆ wait   ",
        AgentState::DoneUnread => "✔ done   ",
        AgentState::Error => "✖ err    ",
        AgentState::Idle => "· idle   ",
        AgentState::Dead => "  dead   ",
    }
}

/// The short word for a state in the header tally.
fn short_name(s: AgentState) -> &'static str {
    match s {
        AgentState::Waiting => "wait",
        AgentState::DoneUnread => "new",
        AgentState::Error => "error",
        AgentState::Working => "working",
        _ => "idle",
    }
}

/// The header tally: how many agents are in each state that matters.
///
/// Ordered by urgency rather than by count, so the leftmost number is always
/// the one that needs a person.
pub fn summary(agents: &[Agent]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for s in [
        AgentState::Waiting,
        AgentState::DoneUnread,
        AgentState::Error,
        AgentState::Working,
        AgentState::Idle,
    ] {
        // Counted by scanning rather than tallied into a map: `AgentState` is
        // deliberately not `Hash`, and five passes over a list that never holds
        // more than a few dozen rows costs nothing worth a trait impl on
        // somebody else's type.
        let n = agents.iter().filter(|a| a.state == s).count();
        if n == 0 {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{} {n}", short_name(s)),
            theme::state_badge(s),
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled("no sessions", theme::header()));
    }
    spans
}

// ── header ───────────────────────────────────────────────────────────────

/// A warning when Claude's default is pinned to Fable: every new claude then
/// runs at the top tier and the weekly allowance drains without anybody
/// choosing it.
///
/// Only the warning is in the header. The defaults themselves used to be a
/// whole section there (`default model ▌Claude opus · ▌Codex … · ▌Gemini
/// auto`), which took the room the machine's load now has; a default that is
/// not Fable is nothing to act on from the tower. The Discord card still lists
/// all three.
pub fn fable_warning(app: &App) -> Option<Vec<Span<'static>>> {
    let d = app.local().def_models.as_ref()?;
    if !tower_usage::is_fable(&d.claude) {
        return None;
    }
    let mut spans = vec![Span::styled("⚠ ", theme::level("red"))];
    spans.extend(theme::vendor_spans("claude", "Claude".to_string()));
    spans.push(Span::styled(
        format!(" {} default", tower_usage::pretty_model(&d.claude)),
        theme::level("red"),
    ));
    Some(spans)
}

/// `◆ parallax  Research · needs you · Pick a plan  +1 more` — the header's
/// line for the most urgent Parallax handoff.
///
/// The text is another program's, so it is cleaned of controls here as well
/// as where it was written: the snapshot is a file anybody with the account
/// can edit. A redacted handoff has no title and is named by its status.
pub fn parallax_line(top: &tower_state::ParallaxHandoff, more: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled("◆ parallax  ", theme::state(AgentState::Waiting)),
        Span::raw(tower_state::clean_text(&top.workspace).into_owned()),
        Span::styled(format!(" · {}", top.status_label()), theme::help()),
    ];
    if !top.title.is_empty() {
        spans.push(Span::raw(format!(
            " · {}",
            tower_state::clean_text(&top.title)
        )));
    }
    if more > 0 {
        spans.push(Span::styled(format!("  +{more} more"), theme::help()));
    }
    Line::from(spans)
}

pub fn header_lines(app: &App) -> Vec<Line<'static>> {
    let mut first = vec![Span::styled("herdr-tower", theme::title())];
    // 🚨 In the dock too. The dock is how this is opened — one click in the
    // sidebar — so hiding the build there hides it from the surface people
    // actually look at, and "which build is this" is the first question a bug
    // report has to answer.
    if !app.version().trim().is_empty() {
        first.push(Span::styled(
            format!(" {}", app.version().trim()),
            theme::help(),
        ));
    }
    if app.backend_count() > 1 {
        first.push(Span::styled(
            format!(" · {} backends", app.backend_count()),
            theme::help(),
        ));
    }
    first.push(Span::raw("  "));
    // The whole count, not what `/` left: a WAIT filtered out is still one.
    first.extend(summary(app.all_agents()));
    // Parallax Browser's handoffs that wait on the person, in WAIT's colours:
    // the same kind of thing — somebody stopped and needs an answer — asked
    // from another program. Not counted into WAIT, which is agents this tower
    // can jump to.
    let parallax = app.parallax_waiting();
    if !parallax.is_empty() {
        first.push(Span::raw("  "));
        first.push(Span::styled(
            format!("parallax {}", parallax.len()),
            theme::state(AgentState::Waiting),
        ));
    }
    if let Some(indicator) = schedules_indicator(app) {
        first.push(Span::styled("  ·  ", theme::help()));
        first.push(indicator);
    }
    // The machine's own load, in the usage board's meters: how busy the
    // machine running the agents is, beside what they are doing.
    if let Some(host) = host_spans(app.host_load(), theme::header()) {
        first.push(Span::styled("  ·  ", theme::help()));
        first.extend(host);
    }
    if let Some(warning) = fable_warning(app) {
        first.push(Span::styled("  ·  ", theme::help()));
        first.extend(warning);
    }
    let mut lines = vec![Line::from(first)];
    // The most urgent of them by name, one line and only while there is one:
    // the count says that something waits, this says where to go. Parallax
    // is answered in Parallax, so there is no key here — the tower only
    // shows it.
    if let Some(top) = parallax.first() {
        lines.push(parallax_line(top, parallax.len() - 1));
    }
    // Every AI's windows, as short bars — the usage board's one line. All of
    // it is `u` away.
    if let Some(frame) = app.usage_frame().filter(|f| !f.summary.is_empty()) {
        let mut line = crate::usage::ansi_line(&frame.summary);
        if !app.usage_view() {
            line.spans.push(Span::raw("   "));
            line.spans.extend(reminder_spans("u usage"));
        }
        lines.push(line);
    }
    lines
}

/// Cells in each of the machine's meters: as wide as a window's on the usage
/// line beside them.
const HOST_METER: usize = 5;

/// `cpu ■■■■■ 23%  mem ■■■■■ 61%`: the share in use, not what is left — the
/// opposite of the AIs' windows, because a machine is read by how busy it is.
/// A part that could not be read is left out; `None` when both are. `label`
/// is the style of the `cpu`/`mem` words, which sit on whatever is behind them.
fn host_spans(load: crate::app::HostLoad, label: Style) -> Option<Vec<Span<'static>>> {
    let parts: Vec<(&str, f64)> = [
        ("cpu", load.cpu_pct),
        ("mem", load.memory.map(|m| m.used_pct())),
    ]
    .into_iter()
    .filter_map(|(label, pct)| Some((label, pct?)))
    .collect();
    if parts.is_empty() {
        return None;
    }
    let mut spans = Vec::new();
    for (i, (name, pct)) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("{name} "), label));
        spans.extend(crate::meter::spans(pct, HOST_METER));
        spans.push(crate::meter::figure(pct));
    }
    Some(spans)
}

/// The column names, over the indented rows.
///
/// There is no workspace column: the rule above each group names the
/// workspace once, and repeating it on every row underneath spent twenty
/// columns saying what the reader is already looking at. The room goes to the
/// folder, which is the column that was being cut.
///
/// SID is the CLI's conversation, not the workspace — `status` calls the
/// workspace SESSION, and `info` looks one up by it, so this column does not
/// borrow that name.
pub fn column_header(width: usize) -> Line<'static> {
    let mut text = pad_right("PANE", W_PANE + 1);
    if sid_column(width) {
        text += &pad_right("SID", W_SID + 1);
    }
    text += &(pad_right("TITLE", W_TITLE + 1) + &pad_right("TASK", W_TASK + 1));
    Line::from(Span::styled(
        format!(
            "{}{}{}{}",
            pad_right("", GUTTER),
            pad_right("STATE", W_STATE + 1),
            // One more cell for the vendor's bar, which hangs left of the
            // heading so the name starts under `AGENT`.
            pad_right(" AGENT", W_KIND + 2),
            text,
        ) + "DIR·SINCE",
        theme::header(),
    ))
}

// ── rows ─────────────────────────────────────────────────────────────────

/// `[model · ctx 42% · 10m]` — what is answering, how full it is, how fresh
/// that reading is.
///
/// The `~` on an estimate comes from [`UsedPct`] itself rather than from a flag
/// this function has to remember to check.
pub fn ctx_badge(app: &App, a: &Agent) -> Vec<Span<'static>> {
    let Some(info) = app.ctx_of(&a.id) else {
        return Vec::new();
    };
    let mut parts: Vec<Span<'static>> = Vec::new();
    if !info.model.is_empty() {
        let style = if tower_usage::is_fable(&info.model) {
            theme::level("yellow").add_modifier(Modifier::BOLD)
        } else {
            theme::model()
        };
        parts.push(Span::styled(info.model.clone(), style));
    }
    if let Some(used) = info.used_pct {
        parts.push(Span::styled(
            format!("ctx {}", used.label()),
            theme::ctx(used.value()),
        ));
    }
    if let Some(ts) = info.ts {
        parts.push(Span::styled(since(ts, app.now()), theme::header()));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    let mut out = vec![Span::styled("[", theme::header())];
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::styled(" · ", theme::header()));
        }
        out.push(p);
    }
    out.push(Span::styled("]", theme::header()));
    out
}

/// One text cell of a row: cut to `text` columns, padded to `width`, and a
/// dimmed `N/A` when there is nothing to put in it.
fn cell(text: &str, width: usize, text_width: usize) -> Span<'static> {
    if text.trim().is_empty() {
        return Span::styled(pad_right(NOT_KNOWN, width), theme::header());
    }
    Span::raw(pad_right(&truncate_width(text, text_width, "…"), width))
}

/// Between the tab and the pane's title in the TITLE cell.
const TAB_SEP: &str = "›";
/// The room a tab name leaves for the title, however long the tab — or the
/// title's own width, when the title is narrower than this (#34).
const TITLE_KEEP: usize = 8;

/// The TITLE cell: `tab›title`.
///
/// The tab says where the pane is, which is what the eye looks for down the
/// column, so the title is what gets cut first. A tab name long enough to
/// crowd the title out is cut too, down to [`TITLE_KEEP`] columns of title —
/// or the title's own width, when that is less: a short title needs no more,
/// and holding back the rest cut tabs that fit whole beside it (#34).
///
/// Neither half shows the `[w2:t3] ` / `[w2:p1] ` ID prefix the tower names
/// its tabs and panes with: the PANE column already says where the row is,
/// and eight columns of coordinate would push the title out of a cell this
/// narrow. The scan stores both without it; stripping again here covers a
/// record written before that, which keeps its old tab name until the tab's
/// label next reaches it.
fn title_cell(a: &Agent) -> Vec<Span<'static>> {
    title_cell_in(a, W_TITLE, W_TITLE_TEXT)
}

/// [`title_cell`] for a worker: `↳pEJ` — the coordinator it works under —
/// before the tab and title, which take what is left of the cell. The mark
/// is what is left of the session tree: rows stay in pane order, and who
/// works under whom is read here.
fn worker_title_cell(a: &Agent, coordinator: &str) -> Vec<Span<'static>> {
    let mark = format!("↳{coordinator} ");
    let mark = truncate_width(&mark, W_TITLE_TEXT, "…");
    let shift = text_width(&mark);
    let mut spans = vec![Span::styled(mark, theme::header())];
    spans.extend(title_cell_in(
        a,
        W_TITLE.saturating_sub(shift),
        W_TITLE_TEXT.saturating_sub(shift),
    ));
    spans
}

/// [`title_cell`] in a cell `width` wide holding `text` columns of text.
fn title_cell_in(a: &Agent, width: usize, text: usize) -> Vec<Span<'static>> {
    let tab = herdrx::strip_id_prefix(a.pane.tab.trim()).trim();
    // A pane named only `[w2:p1]` has no name of its own: its live terminal
    // title is what the cell shows then, as for a pane with no name at all.
    let title = match herdrx::strip_id_prefix(a.pane_title().trim()).trim() {
        "" => a.title.trim(),
        named => named,
    };
    if tab.is_empty() {
        if title.is_empty() {
            return vec![cell(title, width, text)];
        }
        let title = truncate_width(title, text, "…");
        return vec![Span::styled(pad_right(&title, width), theme::pane_title())];
    }
    if title.is_empty() {
        let tab = truncate_width(tab, text, "…");
        return vec![Span::styled(pad_right(&tab, width), theme::pane_title())];
    }
    let tab_room = text.saturating_sub(text_width(TAB_SEP) + text_width(title).min(TITLE_KEEP));
    let tab = truncate_width(tab, tab_room, "…");
    let title_room = text.saturating_sub(text_width(&tab) + text_width(TAB_SEP));
    let title = truncate_width(title, title_room, "…");
    let used = text_width(&tab) + text_width(TAB_SEP) + text_width(&title);
    vec![
        Span::styled(tab, theme::pane_title()),
        Span::styled(TAB_SEP, theme::tab_sep()),
        Span::styled(title, theme::pane_title()),
        Span::raw(" ".repeat(width.saturating_sub(used))),
    ]
}

/// The AGENT cell: the vendor's bar, then the kind's name padded to the
/// column — one cell of bar and [`W_KIND`] of name for every kind, so the
/// columns after it stay under their headings.
fn kind_cell(kind: &str) -> [Span<'static>; 2] {
    theme::vendor_spans(kind, pad_right(kind, W_KIND))
}

/// A state label as spans: the badge covers the text only, not the padding
/// that keeps the next column in line.
fn state_spans(label: impl Into<String>, state: AgentState) -> Vec<Span<'static>> {
    let label: String = label.into();
    let text = label.trim_end();
    let pad = label[text.len()..].to_string();
    let mut spans = vec![Span::styled(text.to_string(), theme::state_badge(state))];
    if !pad.is_empty() {
        spans.push(Span::raw(pad));
    }
    spans
}

/// One agent's row, before any selection styling.
fn row_with_gutter(app: &App, a: &Agent, gutter: String, width: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(gutter, theme::header())];
    spans.extend(state_spans(state_label(a, app.now()), a.state));
    spans.push(Span::raw(" "));
    spans.extend(kind_cell(&a.kind));
    spans.extend([
        Span::raw(" "),
        cell(a.pane_id(), W_PANE, W_PANE_TEXT),
        Span::raw(" "),
    ]);
    if sid_column(width) {
        spans.extend([
            cell(a.short_session_id(), W_SID, W_SID_TEXT),
            Span::raw(" "),
        ]);
    }
    match app.coordinator_of(a) {
        Some(c) => spans.extend(worker_title_cell(a, c)),
        None => spans.extend(title_cell(a)),
    }
    spans.extend([Span::raw(" "), cell(a.assignment(), W_TASK, W_TASK_TEXT)]);
    spans.extend([Span::raw(format!(
        " {} · {}",
        shorten_home(&a.cwd),
        since(a.state_since, app.now()),
    ))]);
    if let Some(mark) = app.local().wired.get(&a.pane.session) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(mark.clone(), theme::discord()));
    }
    if let Some(br) = app.branch_of(&a.cwd) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("⎇ {br}"), theme::title()));
    }
    if let Some(pr) = app.pr_of(&a.cwd) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("#{pr}"), theme::title()));
    }
    if let Some(lease) = app.loop_lease(&a.id) {
        spans.push(Span::raw(" "));
        let paused = !lease.paused_reason.is_empty();
        let badge = if paused {
            format!("↻ {}m · paused", lease.idle_for_secs / 60)
        } else {
            format!(
                "↻ {}m · {} · {}h",
                lease.idle_for_secs / 60,
                lease.sent_count,
                lease.remaining_hours(app.now())
            )
        };
        spans.push(Span::styled(badge, theme::loop_badge(paused)));
    }
    if let Some(badge) = schedule_badge(app, a) {
        spans.push(Span::raw(" "));
        spans.push(badge);
    }
    let badge = ctx_badge(app, a);
    if !badge.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(badge);
    }
    Line::from(spans)
}

/// Turns a row into the highlighted bar: a marker, the same spans with a
/// background patched on, and padding out to the edge.
///
/// Patching keeps every colour the row already had, so the selected row needs
/// no second, colourless set of cell helpers just to size its padding — the
/// padding here is `width` minus a width the caller never has to compute.
pub fn selection_bar(line: &Line<'static>, width: usize) -> Line<'static> {
    let bg = Style::default()
        .bg(theme::SEL_BG)
        .add_modifier(Modifier::BOLD);
    let gutter_tail: String = line
        .spans
        .first()
        .map(|span| span.content.chars().skip(1).collect())
        .unwrap_or_else(|| " ".repeat(GUTTER.saturating_sub(1)));
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled(format!("▸{gutter_tail}"), theme::selected())];
    for (i, s) in line.iter().enumerate() {
        // The first span is the row's tree gutter; the marker replaced its
        // first cell while retaining the branch shape in the remaining cells.
        if i == 0 {
            continue;
        }
        spans.push(s.clone().patch_style(bg));
    }
    let used: usize = spans.iter().map(|s| text_width(&s.content)).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), theme::selected()));
    }
    Line::from(spans)
}

/// The rule that opens a workspace's group (`── 10.parallax ─────…`).
///
/// Unlike the vendor rule it replaced, this one is drawn before the *first*
/// group as well. A vendor rule was a separator — the first group had nothing to
/// be separated from — but this one is a heading, and the rows below it are
/// indented under it. Leaving it off the first group would indent those rows
/// under nothing.
///
/// The rule reaches the right edge; a repeat count one short of the width
/// would leave a ragged column down the side of every divider.
pub fn workspace_divider(session: &str, width: usize) -> Line<'static> {
    let name = if session.is_empty() {
        NO_WORKSPACE
    } else {
        session
    };
    let used = 4 + text_width(name);
    Line::from(vec![
        Span::styled("── ", theme::header()),
        Span::styled(name.to_string(), theme::title()),
        Span::raw(" "),
        Span::styled("─".repeat(width.saturating_sub(used)), theme::header()),
    ])
}

/// What a machine's band says; see [`machine_root`].
#[derive(Debug, Clone, Default)]
pub struct Band {
    /// The machine's label, `local` for this one.
    pub machine: String,
    /// How many agents are under the band, folded or not; `None` on a
    /// machine with none to show.
    pub count: Option<usize>,
    /// `Some(true)` folded, `Some(false)` open, `None` nothing to fold.
    pub fold: Option<bool>,
    pub load: crate::app::HostLoad,
    /// What it runs, [`tower_os::host::System::label`]; empty when unknown.
    pub system: String,
    /// Why it is not answering, or since when, after everything else.
    pub note: String,
}

impl Band {
    /// `@desk (6)`: what the band's meters are lined up after.
    fn name(&self) -> String {
        match self.count {
            Some(n) => format!("@{} ({n})", self.machine),
            None => format!("@{}", self.machine),
        }
    }
}

/// A machine as a root of the tree: ` ▼ @gpu (6)  cpu … · Ubuntu 26.04
/// arm64 (herdr-tower v0.57.3)`, a band of [`theme::machine`] blue across
/// the whole width, so where one machine's rows start cannot be missed. Drawn
/// only once a saved machine is on screen — `@local` too.
///
/// After the name, how many agents it has. A machine that reported its load
/// carries it next, in the header's meters, `@local` included. The name is
/// padded to `name_width` cells first, so every band's meters start in one
/// column and read down the screen as a table. Then what it runs, and last
/// any note.
///
/// A band that folds says so before the name: `▼` while its rows are open,
/// `▶` while they are folded — not the selection bar's `▸`, so a folded band
/// is not read as the selected row. One with nothing under it to fold gets a
/// blank there instead, so every name starts in the same column.
pub fn machine_root(band: &Band, name_width: usize, width: usize) -> Line<'static> {
    let glyph = match band.fold {
        Some(true) => " ▶",
        Some(false) => " ▼",
        None => "  ",
    };
    let mut spans = vec![
        Span::styled(glyph, theme::machine()),
        Span::styled(format!(" @{}", band.machine), theme::machine()),
    ];
    if let Some(n) = band.count {
        spans.push(Span::styled(format!(" ({n})"), theme::machine_note()));
    }
    if let Some(host) = host_spans(band.load, theme::machine_note()) {
        let pad = name_width.saturating_sub(text_width(&band.name()));
        spans.push(Span::styled(" ".repeat(pad + 2), theme::machine_note()));
        let bg = Style::default().bg(theme::MACHINE_BG);
        spans.extend(host.into_iter().map(|s| s.patch_style(bg)));
    }
    for note in [&band.system, &band.note] {
        if !note.is_empty() {
            spans.push(Span::styled(format!(" · {note}"), theme::machine_note()));
        }
    }
    let used: usize = spans.iter().map(|s| text_width(&s.content)).sum();
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            theme::machine_note(),
        ));
    }
    clamp_line(&Line::from(spans), width)
}

/// `@local`'s band.
fn local_band(app: &App) -> Band {
    Band {
        machine: crate::app::LOCAL_MACHINE.to_string(),
        count: Some(app.local_matched()),
        fold: Some(app.is_folded(crate::app::LOCAL_MACHINE)),
        load: app.host_load(),
        system: app.local_system().label(),
        note: String::new(),
    }
}

/// A saved machine's band, with `n` agents shown under it.
fn remote_band(m: &crate::RemoteMachine, n: usize, fold: Option<bool>, note: String) -> Band {
    Band {
        machine: m.label.clone(),
        count: (n > 0).then_some(n),
        fold,
        load: m.load(),
        system: m.system.as_ref().map(|s| s.label()).unwrap_or_default(),
        note,
    }
}

/// The widest band name on screen, `@local (N)` included: where every band's
/// meters start.
fn band_name_width(app: &App) -> usize {
    app.remote_shown()
        .iter()
        .map(|(m, agents)| text_width(&remote_band(m, agents.len(), None, String::new()).name()))
        .chain([text_width(&local_band(app).name())])
        .max()
        .unwrap_or_default()
}

/// `line` hung off its machine's trunk: `├─`/`└─` before a workspace rule,
/// `│ ` or blanks before the rows under it.
fn branch(prefix: &str, line: Line<'static>, width: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_string(), theme::header())];
    spans.extend(line.spans);
    clamp_line(&Line::from(spans), width)
}

/// The name to put on a group's rule.
///
/// The app filters recovery records before they reach the view, so every member
/// is live and has been refreshed by the scanner. The first member's label is
/// therefore the current label for the whole group.
fn group_label(app: &App, start: usize) -> String {
    workspace_label(app, &app.agents()[start])
}

/// Prefer the observed sidebar label, including an unnamed workspace's `[wX]`.
/// Older heartbeats fall back to the semantic name and stable workspace ID.
fn workspace_label(app: &App, agent: &Agent) -> String {
    if let Some(label) = app.workspace_display_label(&agent.pane.workspace_id) {
        if !label.is_empty() {
            return label.to_string();
        }
        let id = agent
            .pane
            .workspace_id
            .rsplit(':')
            .next()
            .unwrap_or_default();
        return herdrx::workspace_id_label(id, "");
    }
    if agent.pane.session == crate::dock::DOCK_LABEL {
        return agent.pane.session.clone();
    }
    let id = agent
        .pane
        .workspace_id
        .rsplit(':')
        .next()
        .unwrap_or_default();
    herdrx::workspace_id_label(id, &agent.pane.session)
}

fn same_workspace(a: &Agent, b: &Agent) -> bool {
    tower_state::group_key(a) == tower_state::group_key(b)
}

/// The same branch: one workspace, and one tab as the tree draws it — a
/// worker under a coordinator counts in the coordinator's tab.
fn same_tab(window: &impl Fn(&Agent) -> i64, a: &Agent, b: &Agent) -> bool {
    same_workspace(a, b) && window(a) == window(b)
}

/// The branch drawn before one agent. Rows arrive tab-contiguous with the
/// supervising pane first, so this pass needs no second tree model.
fn tree_gutter(agents: &[Agent], index: usize, window: &impl Fn(&Agent) -> i64) -> String {
    let mut tab_start = index;
    while tab_start > 0 && same_tab(window, &agents[tab_start - 1], &agents[index]) {
        tab_start -= 1;
    }
    let mut tab_end = index;
    while tab_end + 1 < agents.len() && same_tab(window, &agents[tab_end + 1], &agents[index]) {
        tab_end += 1;
    }
    let last_tab =
        tab_end + 1 == agents.len() || !same_workspace(&agents[tab_end + 1], &agents[index]);

    if index == tab_start {
        return match (last_tab, tab_end > tab_start) {
            (false, false) => "├──── ",
            (false, true) => "├─┬─  ",
            (true, false) => "└──── ",
            (true, true) => "└─┬─  ",
        }
        .to_string();
    }

    let trunk = if last_tab { "  " } else { "│ " };
    let branch = if index == tab_end {
        "└── "
    } else {
        "├── "
    };
    format!("{trunk}{branch}")
}

/// The list as lines, with the rule each line belongs under.
pub struct RowLines {
    pub lines: Vec<Line<'static>>,
    /// Where the cursor landed, counted in *rows* rather than agents.
    pub cursor_row: usize,
    /// For each line, the index of the workspace rule that governs it. A rule
    /// governs itself.
    ///
    /// Kept so a viewport that begins below a rule can repeat it: without the
    /// mapping the visible rows would be indented under nothing, and since the
    /// SESSION column is gone there would be nothing left on them saying which
    /// workspace they are in.
    pub heading_of: Vec<usize>,
    /// For each line, the agent it draws, or `None` for a rule. What a mouse
    /// click on that line selects.
    pub agent_of: Vec<Option<usize>>,
    /// For each line, the saved machine's agent it draws, as an index into
    /// [`App::remote_agents`] — what a click there selects.
    pub remote_of: Vec<Option<usize>>,
    /// For each line, the machine whose band it is — what a click there
    /// folds or opens.
    pub band_of: Vec<Option<String>>,
    /// The band just clicked, as its line and the list row it was clicked
    /// on: the list is held there instead of around the cursor, so a fold
    /// neither moves the selection nor scrolls the band away. See
    /// [`App::band_anchor`].
    pub anchor: Option<(usize, usize)>,
}

/// Every row, under a rule per workspace, and where the cursor landed.
///
/// The cursor index counts *rows*, not agents, because a rule occupies one.
/// Returning it from the same pass that inserts the rules is what keeps the two
/// from disagreeing.
///
/// The grouping is read off the app's order rather than computed here: a rule is
/// drawn wherever the workspace changes. That is only correct because the app
/// sorts by workspace first, and it is deliberately not defended against a
/// different order — a second grouping pass here could disagree with the one
/// the cursor moves through, and then the selection would land on a row the
/// reader is not looking at.
pub fn rows(app: &App, width: usize) -> RowLines {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut heading_of: Vec<usize> = Vec::new();
    let mut agent_of: Vec<Option<usize>> = Vec::new();
    let mut remote_of: Vec<Option<usize>> = Vec::new();
    let mut band_of: Vec<Option<String>> = Vec::new();
    let mut cursor_row = 0usize;
    let mut head = 0usize;
    let remote_cursor = app.remote_mode().then(|| app.remote_cursor());
    // With a saved machine on screen every machine is a root of the tree —
    // `@local` first — and its workspaces hang off it. Without one there is
    // only this machine, and the list is drawn as it always was.
    let rooted = has_remote_rows(app);
    let last_group = app.agents().last().map(tower_state::group_key);
    let local = crate::app::LOCAL_MACHINE;
    let folded = app.is_folded(local);
    if rooted && (!app.agents().is_empty() || folded && app.local_matched() > 0) {
        lines.push(machine_root(&local_band(app), band_name_width(app), width));
        heading_of.push(0);
        agent_of.push(None);
        remote_of.push(None);
        band_of.push(Some(local.to_string()));
    }
    for (i, a) in app.agents().iter().enumerate() {
        let in_last = Some(tower_state::group_key(a)) == last_group;
        if i == 0 || tower_state::group_key(&app.agents()[i - 1]) != tower_state::group_key(a) {
            head = lines.len();
            let label = group_label(app, i);
            lines.push(if rooted {
                branch(
                    if in_last { "└─" } else { "├─" },
                    workspace_divider(&label, width.saturating_sub(2)),
                    width,
                )
            } else {
                workspace_divider(&label, width)
            });
            heading_of.push(head);
            agent_of.push(None);
            remote_of.push(None);
            band_of.push(None);
        }
        let window = |a: &Agent| a.pane.window;
        let line = row_with_gutter(app, a, tree_gutter(app.agents(), i, &window), width);
        let line = if rooted {
            branch(if in_last { "  " } else { "│ " }, line, width)
        } else {
            line
        };
        if i == app.cursor() {
            if !app.remote_mode() {
                cursor_row = lines.len();
            }
            // With the usage board in the preview's place, nothing below is
            // this row's: a bar would read as "the board is about this one".
            // The window still follows the cursor, so `j`/`k` bring it back
            // where it was.
            // On a saved machine's row the bar is there, not here.
            if app.usage_view() || app.remote_mode() {
                lines.push(line);
            } else {
                lines.push(selection_bar(&line, width));
            }
        } else {
            lines.push(line);
        }
        heading_of.push(head);
        agent_of.push(Some(i));
        remote_of.push(None);
        band_of.push(None);
    }
    for (label, machine_lines) in remote_sections(app, width) {
        let head = lines.len();
        for (k, (line, flat)) in machine_lines.into_iter().enumerate() {
            band_of.push(label.clone().filter(|_| k == 0));
            if flat.is_some() && flat == remote_cursor {
                cursor_row = lines.len();
                lines.push(selection_bar(&line, width));
            } else {
                lines.push(line);
            }
            heading_of.push(head);
            agent_of.push(None);
            remote_of.push(flat);
        }
    }
    let anchor = app.band_anchor().and_then(|(label, at)| {
        band_of
            .iter()
            .position(|b| b.as_deref() == Some(label))
            .map(|line| (line, at))
    });
    RowLines {
        lines,
        cursor_row,
        heading_of,
        agent_of,
        remote_of,
        band_of,
        anchor,
    }
}

/// Whether anything from a saved machine is drawn under the list. The local
/// rules say `local ·` only then: with nothing else on screen, every row is
/// local and the word would be noise.
fn has_remote_rows(app: &App) -> bool {
    !app.remote_shown().is_empty()
}

/// The state cell for herdr's own word, padded like [`state_label`] so the
/// columns line up with the local rows.
fn remote_state(status: &str) -> (&'static str, AgentState) {
    match status {
        "working" => ("● working", AgentState::Working),
        "blocked" => ("◆ wait   ", AgentState::Waiting),
        "done" => ("✔ done   ", AgentState::DoneUnread),
        "idle" => ("· idle   ", AgentState::Idle),
        _ => ("? unknown", AgentState::Idle),
    }
}

/// `↻ 30m · 3 · 71h`, the local loop badge, from a `loop list` line of the
/// machine's own herdr-tower (`idle=30m  sent=3  expires=71h  paused: …`).
/// A line it cannot read is shown as it came.
fn remote_loop_badge(state: &str) -> (String, bool) {
    let field = |key: &str| {
        state
            .split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .map(str::to_string)
    };
    let paused = state.contains("paused");
    match (field("idle="), field("sent="), field("expires=")) {
        (Some(idle), _, _) if paused => (format!("↻ {idle} · paused"), true),
        (Some(idle), Some(sent), Some(left)) => (format!("↻ {idle} · {sent} · {left}"), false),
        _ => (format!("↻ {state}"), paused),
    }
}

/// One remote agent in the local rows' columns. There is no task, no `since`
/// and no ctx: that machine's herdr-tower knows those, and this one only reads
/// herdr there.
fn remote_row(a: &crate::RemoteAgent, gutter: &str, width: usize, stale: bool) -> Line<'static> {
    let (label, state) = remote_state(&a.status);
    let mut spans = vec![Span::styled(gutter.to_string(), theme::header())];
    spans.extend(state_spans(label, state));
    spans.push(Span::raw(" "));
    spans.extend(kind_cell(&a.agent));
    spans.extend([
        Span::raw(" "),
        cell(&a.pane_id, W_PANE, W_PANE_TEXT),
        Span::raw(" "),
    ]);
    if sid_column(width) {
        let sid: String = a.session_id.chars().take(W_SID).collect();
        spans.extend([cell(&sid, W_SID, W_SID_TEXT), Span::raw(" ")]);
    }
    // `tab›name`, as the local rows put it.
    // A tab named after its only pane says it once, as herdr's navigator does.
    let title = match (a.tab.is_empty(), a.title.is_empty()) {
        (false, false) if a.tab == a.title => a.title.clone(),
        (false, false) => format!("{}›{}", a.tab, a.title),
        (false, true) => a.tab.clone(),
        _ => a.title.clone(),
    };
    let title = truncate_width(&title, W_TITLE_TEXT, "…");
    spans.push(Span::styled(
        pad_right(&title, W_TITLE),
        theme::pane_title(),
    ));
    spans.extend([Span::raw(" "), cell("", W_TASK, W_TASK_TEXT)]);
    spans.push(Span::raw(format!(" {}", a.cwd)));
    if let Some(state) = &a.loop_state {
        let (badge, paused) = remote_loop_badge(state);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(badge, theme::loop_badge(paused)));
    }
    // `⏰ 19:00`, as a local row wears it: the soonest, and how many more.
    if let Some(first) = a.schedules.first() {
        let more = a.schedules.len() - 1;
        let text = if more > 0 {
            format!("⏰ {} +{more}", first.next)
        } else {
            format!("⏰ {}", first.next)
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(text, theme::model()));
    }
    // The last read of a machine that stopped answering: all of it dim, so
    // nobody takes it for the state of things now.
    if stale {
        let dim = Style::default().fg(theme::DEAD);
        spans = spans
            .into_iter()
            .map(|s| Span::styled(s.content, dim))
            .collect();
    }
    Line::from(spans)
}

/// Each machine herdr has saved as a root of the tree, `@gpu`, its
/// workspaces hanging off it and their agents in the local rows' columns. The
/// first machine with agents says how to reach them (`R`). No row is an agent
/// of the list, so the cursor never lands on one and a click selects nothing.
/// A machine that did not answer is a root saying so; one with no agents is
/// its band alone, for its load. Panes that look like agents but are not
/// listed as any hang under their workspace too, dim and never selected.
///
/// A machine's band is a section of its own and comes with the machine's
/// label, for the click on it; a workspace's section has none. A folded
/// machine is its band alone, saying how many rows it holds.
#[allow(clippy::type_complexity)]
fn remote_sections(
    app: &App,
    width: usize,
) -> Vec<(Option<String>, Vec<(Line<'static>, Option<usize>)>)> {
    let mut flat = 0usize;
    let mut sections = Vec::new();
    for (m, agents) in app.remote_shown() {
        let stale = m.stale_since.is_some();
        let name_width = band_name_width(app);
        let unseen = app.remote_unseen(m);
        let note = match m.stale_since {
            Some(at) => format!(
                "{} · last read {} ago",
                m.error.as_deref().unwrap_or("not reachable"),
                since(at, app.now()).trim()
            ),
            None => m
                .error
                .clone()
                .or_else(|| m.tower_trouble.clone())
                .unwrap_or_default(),
        };
        if agents.is_empty() && unseen.is_empty() {
            // Nothing under it to fold, so not a band to click: folding it
            // now would only surprise later, once it has rows.
            sections.push((
                None,
                vec![(
                    machine_root(&remote_band(m, 0, None, note), name_width, width),
                    None,
                )],
            ));
            continue;
        }
        let folded = app.is_folded(&m.label);
        sections.push((
            Some(m.label.clone()),
            vec![(
                machine_root(
                    &remote_band(m, agents.len(), Some(folded), note),
                    name_width,
                    width,
                ),
                None,
            )],
        ));
        if folded {
            continue;
        }
        // Each workspace's agents, then its unseen panes; a workspace with
        // only unseen panes comes after the agents' workspaces.
        let mut groups: Vec<(&str, Vec<&crate::RemoteAgent>, Vec<&crate::UnseenPane>)> = Vec::new();
        for a in agents {
            match groups.last_mut() {
                Some((ws, members, _)) if *ws == a.workspace => members.push(a),
                _ => groups.push((&a.workspace, vec![a], Vec::new())),
            }
        }
        for p in unseen {
            match groups.iter_mut().find(|(ws, _, _)| *ws == p.workspace) {
                Some((_, _, panes)) => panes.push(p),
                None => groups.push((&p.workspace, Vec::new(), vec![p])),
            }
        }
        let n_groups = groups.len();
        for (g, (ws, members, panes)) in groups.into_iter().enumerate() {
            let last = g + 1 == n_groups;
            let mut lines = vec![(
                branch(
                    if last { "└─" } else { "├─" },
                    workspace_divider(ws, width.saturating_sub(2)),
                    width,
                ),
                None,
            )];
            let rows = members.len() + panes.len();
            let gutter = |i: usize| {
                if i + 1 == rows {
                    "└──── "
                } else {
                    "├──── "
                }
            };
            let trunk = if last { "  " } else { "│ " };
            let first_pane = members.len();
            for (i, a) in members.into_iter().enumerate() {
                let row = remote_row(a, gutter(i), width, stale);
                lines.push((branch(trunk, row, width), Some(flat)));
                flat += 1;
            }
            for (i, p) in panes.into_iter().enumerate() {
                let row = unseen_row(p, gutter(first_pane + i), width);
                lines.push((branch(trunk, row, width), None));
            }
            sections.push((None, lines));
        }
    }
    sections
}

/// A pane that looks like an agent but that its herdr does not list as one,
/// dim, in the remote rows' columns: `? unseen  w3:p1  Claude Code  not an
/// agent to herdr — under sudo/su?`. Never selected: nothing there answers.
fn unseen_row(p: &crate::UnseenPane, gutter: &str, width: usize) -> Line<'static> {
    let mut spans = vec![
        Span::raw(gutter.to_string()),
        Span::raw(pad_right("? unseen", W_STATE)),
        Span::raw(" "),
        Span::raw(" ".repeat(W_KIND)),
        Span::raw(" "),
        cell(&p.pane_id, W_PANE, W_PANE_TEXT),
        Span::raw(" "),
    ];
    if sid_column(width) {
        spans.push(Span::raw(" ".repeat(W_SID + 1)));
    }
    let title = match (p.tab.is_empty(), p.title.is_empty()) {
        (false, false) if p.tab == p.title => p.title.clone(),
        (false, false) => format!("{}›{}", p.tab, p.title),
        (false, true) => p.tab.clone(),
        _ => p.title.clone(),
    };
    spans.push(Span::raw(pad_right(
        &truncate_width(&title, W_TITLE_TEXT, "…"),
        W_TITLE,
    )));
    spans.push(Span::raw(if p.other_user {
        " not an agent to herdr — under sudo/su?"
    } else {
        " not an agent to herdr"
    }));
    let dim = Style::default().fg(theme::DEAD);
    Line::from(
        spans
            .into_iter()
            .map(|s| Span::styled(s.content, dim))
            .collect::<Vec<_>>(),
    )
}

/// The part of the rows the list area shows.
struct ListView {
    window: ListWindow,
    /// The rule repeated above the window, when the window begins below it.
    sticky: Option<usize>,
}

/// Fits `rows` into `list_h` lines.
///
/// A viewport that begins below a rule would indent its rows under nothing, and
/// with the SESSION column gone there is nothing else on them naming the
/// workspace. So the rule is repeated at the top, paid for with a row: the
/// window is picked again one line shorter, so the repeated rule cannot push the
/// last row off the list it was sized for.
///
/// Shared by the renderer and [`drawn_agents`], so a click lands on the row that is
/// drawn under it.
fn list_view(rows: &RowLines, list_h: usize) -> ListView {
    if let Some((band, at)) = rows.anchor {
        let len = rows.lines.len();
        let mut window = window_at(len, band, at, list_h);
        let mut sticky = None;
        if list_h > 1 && len > 0 && rows.heading_of[window.start] < window.start {
            // The repeated rule takes the row above: the band one row on.
            window = window_at(len, band, at.saturating_sub(1), list_h - 1);
            let h = rows.heading_of[window.start];
            if h < window.start {
                sticky = Some(h);
            }
        }
        return ListView { window, sticky };
    }
    let mut window = list_window(rows.lines.len(), rows.cursor_row, list_h);
    let mut sticky = None;
    if list_h > 1 && rows.heading_of[window.start] < window.start {
        window = list_window(rows.lines.len(), rows.cursor_row, list_h - 1);
        let h = rows.heading_of[window.start];
        if h < window.start {
            sticky = Some(h);
        }
    }
    ListView { window, sticky }
}

/// For each screen line, the agent drawn on it.
///
/// Reads the same layout [`screen_lines`] draws, at the size the app last drew
/// at, so a click cannot select a row other than the one under the pointer.
pub fn drawn_agents(app: &App) -> Vec<Option<usize>> {
    let (width, height) = app.size();
    compose(app, width.into(), height.into()).agents
}

/// The slice of rows that fits, and how many are hidden on each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListWindow {
    pub start: usize,
    pub end: usize,
    pub above: usize,
    pub below: usize,
}

/// The slice that draws line `band` on list row `at` — the row of the list
/// area, `↑` indicator included, it was clicked on — as near as the list's
/// ends allow, and on the screen in any case. Sized like [`list_window`], so
/// letting go of the band for the cursor again does not change the height.
fn window_at(len: usize, band: usize, at: usize, height: usize) -> ListWindow {
    if len <= height || height < 3 {
        return list_window(len, band, height);
    }
    let body = height - 2;
    // With rows hidden above, the first list row is the `↑` indicator.
    let start = (band + 1)
        .saturating_sub(at)
        .max((band + 1).saturating_sub(body))
        .min(band)
        .min(len - body);
    ListWindow {
        start,
        end: start + body,
        above: start,
        below: len - start - body,
    }
}

/// Picks the visible slice, keeping the cursor inside it.
///
/// Both indicator rows are reserved whenever anything is hidden, even when only
/// one of them is needed. Reclaiming the spare row would make the window's size
/// depend on its own position, and the size then oscillates by one as the cursor
/// crosses either end — visible as the list twitching on every `j`.
pub fn list_window(len: usize, cursor: usize, height: usize) -> ListWindow {
    if len == 0 || height == 0 {
        return ListWindow {
            start: 0,
            end: 0,
            above: 0,
            below: 0,
        };
    }
    if len <= height {
        return ListWindow {
            start: 0,
            end: len,
            above: 0,
            below: 0,
        };
    }
    // Below three rows there is no room to say anything is hidden, so the
    // indicators are dropped rather than allowed to push a row off the screen —
    // which is the failure the window exists to prevent.
    let (body, indicators) = if height >= 3 {
        (height - 2, true)
    } else {
        (height, false)
    };
    let start = cursor.saturating_sub(body / 2).min(len - body);
    let end = start + body;
    ListWindow {
        start,
        end,
        above: if indicators { start } else { 0 },
        below: if indicators { len - end } else { 0 },
    }
}

/// How the space between the header and the footer is divided.
///
/// The preview only appears when the whole list already fits with room to
/// spare: a tower that hides agents to show a screenshot of one of them has its
/// priorities backwards. No row goes to separators ([`PREVIEW_RULES`]): the
/// preview's heading band marks where the list ends.
pub fn split_body(body_h: usize, rows_len: usize, want_preview: bool) -> (usize, usize) {
    split_sized(body_h, rows_len, want_preview, None)
}

/// Rows the list keeps when the person has sized the preview: the cursor's row
/// and one either side, so `j`/`k` still show where they are going.
pub const LIST_KEEPS_SIZED: usize = 3;

/// [`split_body`], with the preview's height chosen by the person — dragged
/// by its heading or set with `[`/`]` ([`App::preview_rows`]).
///
/// A chosen height is honoured even when the list does not fit: that is the
/// whole point of choosing one, and the list scrolls in what is left. It is
/// kept between a useful preview ([`PREVIEW_MIN`]) and a list that can still
/// be steered ([`LIST_KEEPS_SIZED`]), and is not capped at [`PREVIEW_MAX`] —
/// that cap is the automatic split's restraint, not the person's.
pub fn split_sized(
    body_h: usize,
    rows_len: usize,
    want_preview: bool,
    chosen: Option<usize>,
) -> (usize, usize) {
    if !want_preview {
        return (body_h, 0);
    }
    if let Some(chosen) = chosen {
        let most = body_h.saturating_sub(LIST_KEEPS_SIZED + PREVIEW_RULES);
        if most < PREVIEW_MIN {
            return (body_h, 0);
        }
        let preview = chosen.clamp(PREVIEW_MIN, most);
        return (body_h - preview - PREVIEW_RULES, preview);
    }
    let spare = body_h.saturating_sub(rows_len + PREVIEW_RULES);
    if spare < PREVIEW_MIN {
        return (body_h, 0);
    }
    let preview = spare.min(PREVIEW_MAX);
    (body_h - preview, preview)
}

/// [`split_body`] with the usage board in the preview's place, `want` rows
/// of it. The board gets what it needs as long as the list keeps a few rows
/// — enough for the cursor and its neighbours; the list scrolls in the rest.
pub fn usage_split(body_h: usize, rows_len: usize, want: usize) -> (usize, usize) {
    const LIST_KEEPS: usize = 4;
    let list = rows_len.clamp(1, LIST_KEEPS);
    let board = want.min(body_h.saturating_sub(list + PREVIEW_RULES));
    if board < PREVIEW_MIN {
        return (body_h, 0);
    }
    // A list long enough to scroll fills every row it is given.
    (body_h - board - PREVIEW_RULES, board)
}

/// The rule over the board, in the preview's style: what it is, and how to
/// go back.
fn usage_heading(width: usize) -> Line<'static> {
    let left = "══ usage ";
    let right = " u preview ══";
    let fill = width.saturating_sub(text_width(left) + text_width(right));
    Line::from(vec![
        Span::styled("══ ", theme::header()),
        Span::styled("usage", theme::preview()),
        Span::styled(format!(" {}", "═".repeat(fill)), theme::header()),
        Span::raw(" "),
        Span::styled("u", theme::help_key()),
        Span::styled(" preview", theme::help_txt()),
        Span::styled(" ══", theme::header()),
    ])
}

/// The pane the preview should capture next, and how many rows of it.
///
/// Asked of the same arithmetic the renderer uses, so the capture never fetches
/// rows that will not be drawn — the request and the layout cannot drift.
pub fn preview_request(app: &App) -> Option<(String, u16)> {
    let (width, height) = app.size();
    compose(app, width.into(), height.into()).preview
}

/// [`preview_request`], from a layout already worked out: `body_h` rows between
/// the header and the footer, `rows_len` of them wanted by the list.
fn preview_for(app: &App, body_h: usize, rows_len: usize) -> Option<(String, u16)> {
    let target = app.preview_target()?;
    if app.preview_folded() {
        return None;
    }
    let (_, preview) = split_sized(body_h, rows_len, true, app.preview_rows());
    if preview < PREVIEW_MIN {
        return None;
    }
    Some((target, (preview - 1) as u16))
}

// ── footer ───────────────────────────────────────────────────────────────

/// The key guide. One function for every screen, so a key never means one thing
/// in the list and something else in the usage view.
pub fn help_line(items: &[(&str, &str)]) -> Line<'static> {
    let (lines, _) = help_layout(items, None, usize::MAX);
    lines.into_iter().next().unwrap_or_default()
}

/// One key the guide draws: which of its lines it is on, the columns it covers
/// there, the key it presses, and which entry of the guide it belongs to.
type GuideKey = (usize, Range<usize>, KeyCode, usize);

/// The key guide, wrapped to `width`, and which columns of which of its lines
/// press which key.
///
/// Built together so a click cannot land on a different key than the one drawn
/// there: the columns are counted off the very spans that are pushed. In `j/k`
/// each letter is its own key; the words after a key press the first one.
///
/// `pressed` is the entry the mouse is holding down, drawn reversed — key,
/// words and the gap between them, so it reads as one button.
fn help_layout(
    items: &[(&str, &str)],
    pressed: Option<usize>,
    width: usize,
) -> (Vec<Line<'static>>, Vec<GuideKey>) {
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut keys = Vec::new();
    let mut col = 0;
    for (i, (key, what)) in items.iter().enumerate() {
        // An entry that would run past the edge starts the next line instead,
        // so a narrow screen still has every key on it — cut off, the last
        // ones could be neither read nor clicked.
        let entry = text_width(key) + 1 + text_width(what);
        if col > 0 && col + text_width(" · ") + entry > width {
            lines.push(Vec::new());
            col = 0;
        }
        let row = lines.len() - 1;
        let spans = lines.last_mut().expect("there is always a line");
        if col > 0 {
            spans.push(Span::styled(" · ", theme::help()));
            col += text_width(" · ");
        }
        let mut at = col;
        let mut first = None;
        // `/` separates the keys of one entry, so the filter's own `/` is
        // the one entry that is not split on it.
        let names: Vec<&str> = if *key == "/" {
            vec!["/"]
        } else {
            key.split('/').collect()
        };
        for (j, name) in names.into_iter().enumerate() {
            if j > 0 {
                at += 1;
            }
            for (range, code) in group_keys(name, at) {
                first.get_or_insert(code);
                keys.push((row, range, code, i));
            }
            at += text_width(name);
        }
        col += text_width(key);
        let words = col..col + 1 + text_width(what);
        if let Some(code) = first {
            keys.push((row, words.clone(), code, i));
        }
        col = words.end;
        let held = |style: Style| {
            if pressed == Some(i) {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            }
        };
        spans.push(Span::styled((*key).to_string(), held(theme::help_key())));
        spans.push(Span::styled(" ", held(Style::default())));
        spans.push(Span::styled((*what).to_string(), held(theme::help_txt())));
    }
    (lines.into_iter().map(Line::from).collect(), keys)
}

/// The keys one `/`-separated part of a guide entry draws, starting at column
/// `at`: a named key (`enter`, `esc`, `q`) is one; otherwise every character is
/// its own, so `↑↓/jk` presses up, down, j and k where each is drawn.
fn group_keys(name: &str, at: usize) -> Vec<(Range<usize>, KeyCode)> {
    if let Some(code) = key_code(name) {
        return vec![(at..at + text_width(name), code)];
    }
    let mut out = Vec::new();
    let mut col = at;
    for c in name.chars() {
        let s = c.to_string();
        let w = text_width(&s);
        if let Some(code) = key_code(&s) {
            out.push((col..col + w, code));
        }
        col += w;
    }
    out
}

/// The key a name in the guide stands for.
fn key_code(name: &str) -> Option<KeyCode> {
    match name {
        "enter" => Some(KeyCode::Enter),
        "esc" => Some(KeyCode::Esc),
        "↑" => Some(KeyCode::Up),
        "↓" => Some(KeyCode::Down),
        "←" => Some(KeyCode::Left),
        "→" => Some(KeyCode::Right),
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(KeyCode::Char(c)),
                _ => None,
            }
        }
    }
}

/// The key guide for the screen on show.
fn help_items(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.screen() {
        Screen::Info(_) if !app.quits() => DOCK_INFO_HELP.to_vec(),
        Screen::Info(_) => INFO_HELP.to_vec(),
        Screen::List if app.remote_mode() => REMOTE_HELP.to_vec(),
        Screen::List if app.usage_view() => USAGE_HELP.to_vec(),
        // The dock's tower always has its usage board.
        Screen::List if !app.quits() => list_help(true, false),
        Screen::List => list_help(app.usage_frame().is_some(), true),
    }
}

/// One key in the guide as last drawn: a click on `row`, between columns
/// `from` and `to`, presses `key`. `entry` is its place in the guide, which is
/// what is drawn reversed while the button is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHit {
    pub row: usize,
    pub from: usize,
    pub to: usize,
    pub key: KeyCode,
    pub entry: usize,
}

/// Where each key of the guide is on screen, for [`App::on_mouse`].
///
/// Read off the same layout [`screen_lines`] draws — the footer pinned to the
/// bottom, the guide after the prompt line if there is one — and cut at the
/// width, so a key the line lost off its end cannot be pressed.
///
/// [`App::on_mouse`]: crate::App::on_mouse
pub fn drawn_keys(app: &App) -> Vec<KeyHit> {
    let (width, height) = app.size();
    compose(app, width.into(), height.into()).keys
}

const INFO_HELP: [(&str, &str); 3] = [("i/esc", "close"), ("enter", "jump"), ("q", "quit")];
/// [`INFO_HELP`] in the dock's tower, which `q` does not quit.
const DOCK_INFO_HELP: [(&str, &str); 2] = [("i/esc", "close"), ("enter", "jump")];
/// The board in the preview's place: its own keys first, then the list's
/// few that still apply.
const USAGE_HELP: [(&str, &str); 6] = [
    ("↑/↓", "choose"),
    ("←/→", "keep ±5%"),
    ("x", "guard off"),
    ("r", "refresh usage"),
    ("u/esc", "back to preview"),
    ("j/k", "move"),
];
/// The list's keys, and where each one is shown: `u` only where there is a
/// usage board, `q` only in a tower that quits (the dock's tower is started
/// again by the supervisor, so `q` there would do nothing). One table, so a
/// key added to the list cannot be left out of one of its guides.
const LIST_KEYS: [(&str, &str, Shown); 14] = [
    ("↑↓/jk", "move", Shown::Always),
    ("enter", "jump+read", Shown::Always),
    ("/", "filter", Shown::Always),
    ("i", "detail", Shown::Always),
    ("g", "git", Shown::Always),
    ("L", "idle loop", Shown::Always),
    ("T", "schedule", Shown::Always),
    ("u", "usage", Shown::WithUsage),
    ("[/]/=", "preview size", Shown::Always),
    ("W", "wake all", Shown::Always),
    ("C", "close all", Shown::Always),
    ("B", "broadcast", Shown::Always),
    ("r", "refresh", Shown::Always),
    ("q", "quit", Shown::WhereItQuits),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Always,
    WithUsage,
    WhereItQuits,
}

/// The list's guide: every key of [`LIST_KEYS`] that applies.
fn list_help(usage: bool, quits: bool) -> Vec<(&'static str, &'static str)> {
    LIST_KEYS
        .iter()
        .filter(|(_, _, shown)| match shown {
            Shown::Always => true,
            Shown::WithUsage => usage,
            Shown::WhereItQuits => quits,
        })
        .map(|(key, label, _)| (*key, *label))
        .collect()
}

/// The label in front of the broadcast editor. Short on purpose: every column
/// it takes is a column the message cannot use, and `B` in the key guide
/// already says what the line is for.
const BROADCAST_LABEL: &str = "⌨ broadcast: ";

/// The label in front of a message for an agent on a saved machine.
const REMOTE_LABEL: &str = "⌨ remote: ";

/// The guide in remote mode, where only these keys do anything.
const REMOTE_HELP: [(&str, &str); 7] = [
    ("↑↓/jk", "move"),
    ("L", "idle loop on/off"),
    ("T", "schedule"),
    ("m", "message"),
    ("x", "interrupt"),
    ("/", "filter"),
    ("esc", "back to local"),
];

fn loop_label(selected_minutes: u64) -> String {
    let presets = [5, 10, 30, 60]
        .into_iter()
        .map(|minutes| {
            if minutes == selected_minutes {
                format!("[{minutes}m]")
            } else {
                format!("{minutes}m")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("⌨ loop {presets}: ")
}

/// `⏰ once [daily] weekdays: ` — the repetition `tab` has picked, bracketed.
fn schedule_label(picked: tower_state::Repeat) -> String {
    let choices = tower_state::Repeat::ALL
        .into_iter()
        .map(|r| {
            if r == picked {
                format!("[{}]", r.label())
            } else {
                r.label().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("⏰ {choices}: ")
}

/// `⏰ 19:00` on a row with a scheduled message of its own — `⏰ now` while
/// one is waiting for it to come to rest.
fn schedule_badge(app: &App, a: &Agent) -> Option<Span<'static>> {
    let local = app.now().to_offset(app.utc_offset());
    let mine: Vec<&tower_state::Schedule> = app
        .schedules()
        .iter()
        .filter(|s| s.names(a.session_id.trim(), &a.id))
        .collect();
    let text = if mine.iter().any(|s| s.slot.is_some()) {
        "⏰ now".to_string()
    } else {
        let next = mine.iter().filter_map(|s| s.next(local)).min()?;
        format!("⏰ {}", tower_state::clock_label(next, local))
    };
    Some(Span::styled(text, theme::model()))
}

/// `⏰ 2 · next 19:00` in the header: every schedule counts, the ones for a
/// workspace or for every handoff folder too, which no single row shows.
fn schedules_indicator(app: &App) -> Option<Span<'static>> {
    let all = app.schedules();
    if all.is_empty() {
        return None;
    }
    let local = app.now().to_offset(app.utc_offset());
    let when = if all.iter().any(|s| s.slot.is_some()) {
        "sending now".to_string()
    } else {
        all.iter().filter_map(|s| s.next(local)).min().map_or_else(
            || "none due".into(),
            |at| format!("next {}", tower_state::clock_label(at, local)),
        )
    };
    Some(Span::styled(
        format!("⏰ {} · {when}", all.len()),
        theme::model(),
    ))
}

/// The prompt line, the help line, and the error line.
///
/// The prompt is drawn on **every** screen. If the detail card returned early,
/// a confirmation armed while the card was open would never appear — pressing
/// `y` afterwards would broadcast to every session with nothing on screen
/// having asked for it.
pub fn footer_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    footer(app, width).lines
}

/// The footer, with where its key guide starts in it and which columns of the
/// guide's lines press which key — everything [`compose`] needs from it, built
/// once.
struct Footer {
    lines: Vec<Line<'static>>,
    guide_at: usize,
    guide: Vec<GuideKey>,
}

fn footer(app: &App, width: usize) -> Footer {
    let mut out: Vec<Line<'static>> = prompt_line(app, width).into_iter().collect();
    let guide_at = out.len();
    let (help, guide) = help_layout(&help_items(app), app.pressed_key(), width);
    out.extend(help);

    // A message can be several lines, and the startup warning is: a heading, one
    // `mv` per folder it found, then what to do afterwards. One `Line` each —
    // a `\n` inside a single `Line` is drawn as a character rather than a break,
    // and the row count here is what reserves the space in `screen_lines`. Carried
    // as one line it collapsed into the heading, and the commands the warning
    // exists to hand over never appeared.
    if let Some(e) = app.err() {
        let style = theme::state(AgentState::Error);
        for (i, line) in e.lines().enumerate() {
            // Only the first is marked; the rest arrive with their own indent,
            // which sits under the mark.
            let text = if i == 0 {
                format!("⚠ {line}")
            } else {
                line.to_string()
            };
            out.push(Line::from(Span::styled(text, style)));
        }
    }
    Footer {
        lines: out,
        guide_at,
        guide,
    }
}

/// A confirmation's receipt is coloured by its mark, so "sent" and "not sent"
/// cannot be taken for each other — or for the guide below — at a glance.
fn notice_style(notice: &str) -> Style {
    match notice.chars().next() {
        Some('✔') => theme::state(AgentState::Working).add_modifier(Modifier::BOLD),
        Some('⚠') => theme::state(AgentState::Waiting),
        Some('✖') => theme::state(AgentState::Error),
        _ => theme::title(),
    }
}

/// The line above the key guide, when there is something to ask or say: the
/// message being typed, a confirmation waiting for `y`, or the last notice.
fn prompt_line(app: &App, width: usize) -> Option<Line<'static>> {
    if let Some(input) = app.input() {
        let shown = filter_count(app);
        let (label, hint) = match (app.loop_input_idle_for(), app.schedule_input_repeat()) {
            _ if app.remote_input_open() => (
                REMOTE_LABEL.into(),
                "  enter to send · esc to cancel".into(),
            ),
            _ if app.filtering() => (
                FILTER_LABEL.into(),
                format!("  {shown} · enter keep · esc clear"),
            ),
            (Some(idle_for), _) => (
                loop_label(idle_for.as_secs() / 60),
                "  tab interval · enter start · esc cancel".into(),
            ),
            (None, Some(repeat)) => (
                schedule_label(repeat),
                "  19:00 or +30m, then the message · tab repeat · enter · esc".into(),
            ),
            (None, None) => (
                BROADCAST_LABEL.into(),
                "  enter to send · esc to cancel".into(),
            ),
        };
        // The message is windowed to what is left after the label, so the caret
        // is on screen at any width. `clamp_line` cuts this line rather than
        // wrapping it, and the hint at the end is the part that may go.
        let room = width.saturating_sub(crate::text::width(&label));
        let (before, at, after) = input.window(room);
        // A hint is drawn in the guide's dim colour, caret reversed on its
        // first character, so it reads as a suggestion rather than as text
        // already typed. Taken with `→`, it is text and drawn as text.
        // Italic as well as dim: the key reminder after the field is dim grey
        // too, and a suggestion in the same colour ran straight into it.
        let text = if input.showing_hint() {
            theme::help().add_modifier(Modifier::ITALIC)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(label, theme::title()),
            Span::styled(before, text),
            // The caret is the character under it, reversed — so moving it does
            // not shift the rest of the message sideways.
            Span::styled(at, text.add_modifier(Modifier::REVERSED)),
            Span::styled(after, text),
            // Where the field ends: a bar in the separator colour, so the
            // reminder after it never reads as more of the message.
            Span::styled(" │ ", theme::tab_sep()),
        ];
        // What an empty `enter` said is guidance about this field, so it
        // takes the key reminder's place on the field's own line; the notice
        // line is the field while one is open.
        if app.notice().is_empty() {
            spans.extend(reminder_spans(hint.trim_start()));
        } else {
            spans.push(Span::styled(app.notice().to_string(), theme::title()));
        }
        Some(Line::from(spans))
    } else if let Some(p) = app.pending() {
        let warn = theme::state(AgentState::Waiting);
        // The keys come first, and that is the whole of the layout rule here.
        // `screen_lines` clamps a footer to the width rather than wrapping it,
        // and this terminal goes down to 60 columns — a prompt that puts its
        // answer at the end loses the answer, and an armed action then looks
        // like a key that did nothing. Anything after `[y/N]` is explanation and
        // may be cut.
        let text = match p {
            Pending::Broadcast(t) => {
                format!("⚠ [y/N] send {t:?} to every session")
            }
            Pending::Wake => {
                "⚠ [y/N] wake every session — an instruction goes to all of them".into()
            }
            Pending::Close => {
                "⚠ [y/N] close every session — an instruction goes to all of them".into()
            }
            Pending::RemoteInterrupt { label, id } => {
                format!("⚠ [y/N] press Escape in {id} on {label}")
            }
        };
        Some(Line::from(Span::styled(text, warn)))
    } else if !app.notice().is_empty() {
        Some(Line::from(Span::styled(
            app.notice().to_string(),
            notice_style(app.notice()),
        )))
    } else if !app.filter().is_empty() {
        // The filter stays said while it holds, so a short list is never
        // taken for every agent there is.
        let mut spans = vec![
            Span::styled(FILTER_LABEL, theme::title()),
            Span::raw(app.filter().to_string()),
            Span::styled(" │ ", theme::tab_sep()),
        ];
        spans.extend(reminder_spans(&format!(
            "{} · / edit · esc clear",
            filter_count(app)
        )));
        Some(Line::from(spans))
    } else {
        None
    }
}

/// A field's key reminder drawn like the key guide: the key bright and bold,
/// what it does in the guide's grey, entries apart by a dim ` · `.
fn reminder_spans(text: &str) -> Vec<Span<'static>> {
    const KEYS: [&str; 6] = ["tab", "enter", "esc", "/", "→", "u"];
    let mut out = Vec::new();
    for (i, entry) in text.split(" · ").enumerate() {
        if i > 0 {
            out.push(Span::styled(" · ", theme::help()));
        }
        match entry.split_once(' ') {
            Some((key, what)) if KEYS.contains(&key) => {
                out.push(Span::styled(key.to_string(), theme::help_key()));
                out.push(Span::styled(format!(" {what}"), theme::help_txt()));
            }
            None if KEYS.contains(&entry) => {
                out.push(Span::styled(entry.to_string(), theme::help_key()));
            }
            _ => out.push(Span::styled(entry.to_string(), theme::help_txt())),
        }
    }
    out
}

const FILTER_LABEL: &str = "filter / ";

/// `3 of 12`: how much of the list the filter left.
/// Saved machines' agents count too: a filter that leaves only a remote row
/// said `0 of 18` over the one row it was showing.
fn filter_count(app: &App) -> String {
    let remote_total: usize = app.remote().iter().map(|m| m.agents.len()).sum();
    format!(
        "{} of {}",
        app.agents().len() + app.remote_agents().len(),
        app.all_agents().len() + remote_total
    )
}

// ── the whole screen ─────────────────────────────────────────────────────

/// The preview block as drawn, and where its parts are in it.
struct PreviewBlock {
    lines: Vec<Line<'static>>,
    /// The columns of the session ID in the heading, if it shows one.
    sid: Option<Range<usize>>,
    /// The first line that is the pane's own text, after the heading and a
    /// loop's line.
    text_from: usize,
}

/// The preview's heading: a titled rule, and the columns of the session ID
/// in it, if it shows one.
///
/// It folds like a machine's band and says so before the name, but points
/// the way it moves: `▼` while the preview is open under it, `▲` while it is
/// folded to the foot, where it opens upward from.
fn preview_heading(app: &App, width: usize, folded: bool) -> (Line<'static>, Option<Range<usize>>) {
    // On a saved machine's row: `@gpu · [w2]diskann`, and that agent's ID.
    let remote = app.selected_remote();
    let session = match remote {
        Some((m, a)) => Some(format!("@{} · {}", m.label, a.workspace)),
        None => app
            .selected()
            .map(|agent| workspace_label(app, agent))
            .filter(|session| !session.is_empty()),
    };
    let (sid_full, sid_short) = match remote {
        Some((_, a)) => (
            Some(a.session_id.trim().to_string()),
            Some(a.session_id.chars().take(W_SID).collect::<String>()),
        ),
        None => (
            app.selected().map(|a| a.session_id.trim().to_string()),
            app.selected().map(|a| a.short_session_id().to_string()),
        ),
    };
    // Workspace groups use a light `──` rule. The preview is a different
    // region rather than another group, so a one-row double rule gives it a
    // clear boundary without spending another scarce terminal row.
    let glyph = if folded { " ▲ " } else { " ▼ " };
    let mut heading = vec![Span::styled(glyph, theme::header())];
    // The whole session ID — what `claude --resume` and `codex resume` take,
    // so it can be read off and used — unless the line is too narrow for it.
    // Then the row's short ID stands in, rather than a UUID cut in the middle.
    let tail_with = |sid: Option<&str>| {
        let mut tail = vec![Span::styled("preview", theme::preview())];
        if let Some(sid) = sid.filter(|sid| !sid.is_empty()) {
            tail.push(Span::styled(" · ", theme::header()));
            tail.push(Span::styled(sid.to_string(), theme::session_id()));
        }
        tail.push(Span::raw(" "));
        tail
    };
    // The workspace name is the one part that can be any length, so it gets
    // what is left once the rest — the ID and a stub of the rule — has its
    // room. Given all it asked for, a long name pushed the ID off the edge.
    const RULE_STUB: usize = 2;
    let fixed_with = |tail: &[Span<'static>]| -> usize {
        heading
            .iter()
            .chain(tail)
            .map(|s| text_width(&s.content))
            .sum()
    };
    let mut tail = tail_with(sid_full.as_deref());
    if fixed_with(&tail) + RULE_STUB > width {
        tail = tail_with(sid_short.as_deref());
    }
    let fixed = fixed_with(&tail);
    let room = width.saturating_sub(fixed + RULE_STUB + 1);
    if let Some(session) = session.filter(|_| room > text_width("…")) {
        heading.push(Span::styled(
            truncate_width(&session, room, "…"),
            theme::preview(),
        ));
        heading.push(Span::raw(" "));
    }
    heading.extend(tail);
    let used: usize = heading.iter().map(|s| text_width(&s.content)).sum();
    heading.push(Span::styled(
        fold_rule(used, width, if folded { '▲' } else { '▼' }),
        theme::header(),
    ));
    // Where the ID landed, for a click that copies it.
    let mut at = 0;
    let mut sid = None;
    for span in &heading {
        let w = text_width(&span.content);
        if span.style == theme::session_id() && !span.content.is_empty() {
            sid = Some(at..(at + w).min(width));
        }
        at += w;
    }
    (Line::from(heading), sid)
}

/// The rule that fills a preview heading from column `from` to `width`, with
/// the fold mark again in the middle of the screen and at its right end, so
/// the heading reads as something that folds wherever the eye lands on it.
///
/// Each mark sits in a space either side and keeps at least one `═` from its
/// neighbours. One that would land on the heading's text, or crowd the other,
/// is left out — on a narrow line the rule is plain, as before.
pub fn fold_rule(from: usize, width: usize, mark: char) -> String {
    const MARK: usize = 3;
    let fill = width.saturating_sub(from);
    let mut cells = vec!['═'; fill];
    let mut put = |at: usize| {
        cells[at] = ' ';
        cells[at + 1] = mark;
        cells[at + 2] = ' ';
    };
    // The right end: its last three cells, after at least one `═`.
    if fill < MARK + 1 {
        return cells.into_iter().collect();
    }
    put(fill - MARK);
    // The middle of the screen, not of the rule: the mark's glyph on
    // column `width / 2`.
    let mid = (width / 2).saturating_sub(1);
    if mid > from && mid + MARK < width - MARK {
        put(mid - from);
    }
    cells.into_iter().collect()
}

/// The preview folded to its band: the heading alone, on the preview's teal.
fn preview_band(app: &App, width: usize) -> Line<'static> {
    banded(preview_heading(app, width, true).0, width)
}

/// The preview block: a titled rule and the tail of the pane.
fn preview_block(app: &App, width: usize, height: usize) -> PreviewBlock {
    let Some(body) = app.live_preview() else {
        return PreviewBlock {
            lines: Vec::new(),
            sid: None,
            text_from: 0,
        };
    };
    let remote = app.selected_remote();
    let (heading, sid) = preview_heading(app, width, false);
    let mut out = vec![heading];
    let mut loop_paused = false;
    // A local agent's loop only: under a remote pane it would be another
    // agent's.
    let loop_message = app
        .selected()
        .filter(|_| remote.is_none())
        .and_then(|agent| app.loop_lease(&agent.id))
        .map(|lease| {
            loop_paused = !lease.paused_reason.is_empty();
            let state = if loop_paused {
                "↻ LOOP PAUSED"
            } else {
                "↻ LOOP"
            };
            let body = if lease.message.trim().is_empty() {
                DEFAULT_LOOP_MESSAGE
            } else {
                lease.message.trim()
            };
            let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("{state} {}m · {body}", lease.idle_for_secs / 60)
        });
    if let Some(message) = loop_message {
        out.push(Line::from(Span::styled(
            truncate_width(&message, width, "…"),
            theme::loop_badge(loop_paused),
        )));
    }
    let text_from = out.len();
    let rows = height.saturating_sub(out.len());
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.len().saturating_sub(rows);
    for ln in &lines[start..] {
        out.push(Line::from(Span::styled(
            truncate_width(ln, width, ""),
            theme::model(),
        )));
    }
    while out.len() < height {
        out.push(Line::default());
    }
    let lines = out
        .into_iter()
        .map(|mut line| {
            let used = crate::text::line_width(&line);
            for span in &mut line.spans {
                span.style = span.style.bg(theme::PREVIEW_BG);
            }
            if used < width {
                line.spans.push(Span::styled(
                    " ".repeat(width - used),
                    theme::preview_background(),
                ));
            }
            line
        })
        .collect();
    PreviewBlock {
        lines,
        sid,
        text_from,
    }
}

/// `line` with the columns `cols` drawn reversed — the part of a preview line
/// a mouse drag has selected. Spans are cut where the selection starts and
/// ends, by display width, so a wide character is kept whole on the side it
/// starts on.
fn highlight(line: Line<'static>, cols: Range<usize>) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut at = 0;
    for span in line.spans {
        let style = |reversed: bool| {
            if reversed {
                span.style.add_modifier(Modifier::REVERSED)
            } else {
                span.style
            }
        };
        let (mut run, mut run_inside) = (String::new(), false);
        // By glyph, as `text::columns` steps: a heart and its selector are one
        // piece two columns wide, and splitting them across spans would draw
        // them as two.
        for glyph in crate::text::glyphs(&span.content) {
            let inside = cols.contains(&at);
            if inside != run_inside && !run.is_empty() {
                out.push(Span::styled(std::mem::take(&mut run), style(run_inside)));
            }
            run_inside = inside;
            run.push_str(glyph);
            at += crate::text::glyph_width(glyph);
        }
        if !run.is_empty() {
            out.push(Span::styled(run, style(run_inside)));
        }
    }
    Line::from(out)
}

/// The whole screen, as values.
///
/// At most `height` lines, each at most `width` columns. The footer is pushed to
/// the bottom, which is where a key guide belongs on a screen that owns the
/// terminal, rather than floating directly under whatever content there is.
pub fn screen_lines(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    compose(app, width, height).lines
}

/// One frame, worked out once: the lines to draw, and what the driver needs to
/// know about them afterwards.
///
/// The list's rows, the header and the footer used to be built three or four
/// times a frame — to draw, to map clicks onto rows, to place the guide's keys,
/// and to size the preview request. Each question is now answered from the one
/// layout, which also means the answers cannot disagree with what is drawn.
#[derive(Debug, Default)]
pub struct Composed {
    /// The screen, as [`screen_lines`] returns it.
    pub lines: Vec<Line<'static>>,
    /// For each line, the agent drawn on it; see [`drawn_agents`].
    pub agents: Vec<Option<usize>>,
    /// For each line, the saved machine's agent drawn on it, as an index into
    /// [`App::remote_agents`].
    pub remotes: Vec<Option<usize>>,
    /// For each line, the machine whose band is drawn on it.
    pub bands: Vec<Option<String>>,
    /// Where the guide's keys are; see [`drawn_keys`].
    pub keys: Vec<KeyHit>,
    /// The `u usage` reminder in the header, when it fits on screen.
    pub header_usage: Option<(usize, Range<usize>)>,
    /// The row of the open field, when one is open: a click there focuses
    /// it, which drops its dim hint.
    pub prompt_row: Option<usize>,
    /// What the preview should capture next; see [`preview_request`].
    pub preview: Option<(String, u16)>,
    /// Where the preview's text and session ID are, for the mouse.
    pub preview_area: Option<PreviewArea>,
    /// Where the usage board is, when it is in the preview's place.
    pub usage_area: Option<UsageArea>,
    /// The row that resizes the preview when dragged, when there is one.
    pub split: Option<SplitHandle>,
    /// The screen row the list starts on, when the list is drawn: what a
    /// band click's row is counted from.
    pub list_top: Option<usize>,
}

/// The divider between the list and the preview: the preview's heading, or
/// the band it is folded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitHandle {
    /// The screen row it is on.
    pub row: usize,
    /// The preview's height as drawn, heading included; 1 when it is folded
    /// to its band.
    pub preview_h: usize,
    /// The rows between the header and the footer, list and preview together.
    pub body_h: usize,
    /// Whether the automatic split has room for the preview beside this list:
    /// what unfolding it opens to when no height was chosen.
    pub auto_opens: bool,
}

impl SplitHandle {
    /// The preview height a drag from this handle to screen row `to` asks
    /// for. Above the handle grows the preview; [`split_sized`] keeps it in
    /// bounds when drawn, and this keeps the stored number in bounds too, so
    /// a drag far past either end is not remembered as a height no screen has.
    pub fn dragged_to(&self, to: usize) -> usize {
        self.sized((self.preview_h + self.row).saturating_sub(to))
    }

    /// `wanted` rows of preview, kept in the bounds a drag is kept in.
    pub fn sized(&self, wanted: usize) -> usize {
        let most = self
            .body_h
            .saturating_sub(LIST_KEEPS_SIZED + PREVIEW_RULES)
            .max(PREVIEW_MIN);
        wanted.clamp(PREVIEW_MIN, most)
    }
}

/// The scrollbar beside the list's visible rows: for each, `Some(true)` on the
/// thumb, `Some(false)` on the track. Empty when the whole list is shown —
/// a bar that says "this is all of it" only takes a column.
///
/// The thumb is as long as the window is a share of the list, at least one
/// row, and sits where the window does: at the top for the first rows, at the
/// bottom for the last.
pub fn scrollbar(len: usize, w: ListWindow) -> Vec<Option<bool>> {
    let shown = w.end - w.start;
    if shown == 0 || shown >= len {
        return Vec::new();
    }
    let thumb = (shown * shown / len).clamp(1, shown);
    let travel = shown - thumb;
    let hidden = len - shown;
    let at = (travel * w.start + hidden / 2) / hidden;
    (0..shown)
        .map(|i| Some((at..at + thumb).contains(&i)))
        .collect()
}

/// `line` cut to at most `cols` columns, between glyphs, with no `…`.
fn cut_line(line: &Line<'static>, cols: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in line.iter() {
        let head = truncate_width(&span.content, cols - used, "");
        used += text_width(&head);
        let whole = head.len() == span.content.len();
        out.push(Span::styled(head, span.style));
        if !whole {
            break;
        }
    }
    Line::from(out)
}

/// `line`, fitted to one column short of `width`, with a scrollbar cell in
/// the last column.
fn with_scrollbar(line: &Line<'static>, width: usize, thumb: bool) -> Line<'static> {
    if width < 2 {
        return line.clone();
    }
    // Cut where the bar begins. A tail of nothing but padding — the selected
    // row's band, a group rule's `─` — is dropped quietly; `…` is for text
    // that was lost, and drawing it on every rule said otherwise.
    let cols = width - 1;
    let text = crate::text::line_text(line);
    let rest = &text[truncate_width(&text, cols, "").len()..];
    let mut out = if rest.chars().all(|c| c == ' ' || c == '─') {
        cut_line(line, cols)
    } else {
        clamp_line(line, cols)
    };
    let used = crate::text::line_width(&out);
    if used < width - 1 {
        // In the last span's style, so the selected row's band runs on to
        // the bar rather than stopping where its text does.
        let style = out.spans.last().map(|s| s.style).unwrap_or_default();
        out.spans
            .push(Span::styled(" ".repeat(width - 1 - used), style));
    }
    let (glyph, style) = if thumb {
        ("┃", theme::help_key())
    } else {
        ("│", theme::help())
    };
    out.spans.push(Span::styled(glyph, style));
    out
}

/// Where the usage board is on screen: the rows it takes, and which of its
/// lines the first of them shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageArea {
    pub rows: Range<usize>,
    pub first_line: usize,
}

/// Where the preview is on screen, for selecting its text and copying its ID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewArea {
    /// The screen rows holding the pane's text.
    pub rows: Range<usize>,
    /// Those rows as drawn, plain — what a selection over them copies. Kept
    /// here because the driver hands the frame's lines to the terminal before
    /// the app remembers the frame.
    pub text: Vec<String>,
    /// The session ID in the heading, if it shows one.
    pub sid: Option<SidHit>,
}

/// The session ID in the preview's heading: where it is, and the whole ID a
/// click copies — the whole one even when the heading had room only for the
/// short one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidHit {
    pub row: usize,
    pub cols: Range<usize>,
    pub id: String,
}

/// Lays out one frame at `width` × `height`. See [`Composed`].
pub fn compose(app: &App, width: usize, height: usize) -> Composed {
    let mut out = Composed {
        agents: vec![None; height],
        remotes: vec![None; height],
        bands: vec![None; height],
        ..Composed::default()
    };
    if width == 0 || height == 0 {
        return out;
    }
    let footer = footer(app, width);
    // The list's rows and head are needed on the list screen to draw it, and
    // on every screen to size the preview request, which is asked regardless.
    let rows = rows(app, width);
    // The summary lines (models, plan usage) sit on the preview's dark teal as
    // one band, so they end where the table's column names begin without a
    // line of their own between them.
    let header = header_lines(app);
    if app.screen() == &Screen::List && !app.usage_view() {
        if let Some(line) = header
            .last()
            .filter(|_| app.usage_frame().is_some_and(|f| !f.summary.is_empty()))
        {
            let end = crate::text::line_width(line);
            if end <= width && header.len() <= height {
                out.header_usage = Some((header.len() - 1, end - "u usage".len()..end));
            }
        }
    }
    let mut head: Vec<Line<'static>> = header.into_iter().map(|l| banded(l, width)).collect();
    head.push(column_header(width));
    let body_h = height.saturating_sub(head.len() + footer.lines.len());
    // The board in the preview's place: nothing to capture meanwhile.
    out.preview = if app.usage_view() {
        None
    } else {
        preview_for(app, body_h, rows.lines.len())
    };

    let mut body: Vec<Line<'static>> = Vec::new();
    match app.screen() {
        Screen::Info(card) => {
            body.push(Line::from(Span::styled(
                "herdr-tower — detail",
                theme::title(),
            )));
            body.push(Line::default());
            for ln in card.lines() {
                body.push(Line::from(Span::raw(ln.to_string())));
            }
        }
        Screen::List => {
            let usage = app.usage_frame().filter(|_| app.usage_view());
            // Something to preview at all. The automatic split waits for its
            // capture before giving it rows; a height the person chose opens
            // at once — the capture is only asked for once the rows exist.
            let has_target = usage.is_none() && app.preview_target().is_some();
            let want_preview = match app.preview_rows() {
                Some(_) => has_target,
                None => has_target && app.live_preview().is_some(),
            };
            let (list_h, preview_h) = match usage {
                Some(frame) => usage_split(body_h, rows.lines.len(), frame.board.len() + 1),
                None => split_sized(body_h, rows.lines.len(), want_preview, app.preview_rows()),
            };
            // The preview folds to a band at the foot, as a machine folds
            // under its own: when the person folded it, and when the list is
            // too long for the automatic split to give it rows — so a long
            // list still says there is a preview and where to open it.
            let auto_opens = split_sized(body_h, rows.lines.len(), true, None).1 > 0;
            let band = usage.is_none()
                && has_target
                && body_h > LIST_KEEPS_SIZED
                && (app.preview_folded() || (app.preview_rows().is_none() && !auto_opens));
            let (list_h, preview_h) = if band {
                (body_h - 1, 1)
            } else {
                (list_h, preview_h)
            };
            let head_len = head.len();
            body.extend(head);
            if rows.lines.is_empty() {
                body.push(Line::from(Span::styled(
                    "  no claude/codex/gemini/opencode running in herdr",
                    theme::help(),
                )));
            } else {
                out.list_top = Some(head_len);
                let ListView { window: w, sticky } = list_view(&rows, list_h);
                let first = head_len + usize::from(w.above > 0) + usize::from(sticky.is_some());
                for (y, i) in (first..).zip(w.start..w.end) {
                    if let Some(slot) = out.agents.get_mut(y) {
                        *slot = rows.agent_of[i];
                    }
                    if let Some(slot) = out.remotes.get_mut(y) {
                        *slot = rows.remote_of[i];
                    }
                    if let Some(slot) = out.bands.get_mut(y) {
                        slot.clone_from(&rows.band_of[i]);
                    }
                }
                // A band held at the top while its rows scroll past folds
                // them too.
                if let Some(h) = sticky {
                    let y = head_len + usize::from(w.above > 0);
                    if let Some(slot) = out.bands.get_mut(y) {
                        slot.clone_from(&rows.band_of[h]);
                    }
                }
                if w.above > 0 {
                    body.push(Line::from(Span::styled(
                        format!("  ↑ {} more", w.above),
                        theme::help(),
                    )));
                }
                if let Some(h) = sticky {
                    body.push(rows.lines[h].clone());
                }
                let bar = scrollbar(rows.lines.len(), w);
                body.extend(
                    rows.lines[w.start..w.end]
                        .iter()
                        .zip(bar.iter().copied().chain(std::iter::repeat(None)))
                        .map(|(line, cell)| match cell {
                            Some(thumb) => with_scrollbar(line, width, thumb),
                            None => line.clone(),
                        }),
                );
                if w.below > 0 {
                    body.push(Line::from(Span::styled(
                        format!("  ↓ {} more", w.below),
                        theme::help(),
                    )));
                }
            }
            if band {
                // At the foot, where the open preview's heading would be.
                while body.len() < head_len + list_h {
                    body.push(Line::default());
                }
                out.split = Some(SplitHandle {
                    row: body.len(),
                    preview_h,
                    body_h,
                    auto_opens,
                });
                body.push(preview_band(app, width));
            } else if let (Some(frame), true) = (usage, preview_h >= PREVIEW_MIN) {
                let top = body.len();
                body.push(usage_heading(width));
                let shown = preview_h - 1;
                out.usage_area = Some(UsageArea {
                    rows: top + 1..top + 1 + shown.min(frame.board.len()),
                    first_line: 0,
                });
                for line in frame.board.iter().take(shown) {
                    body.push(crate::usage::ansi_line(line));
                }
            } else if preview_h >= PREVIEW_MIN {
                // A chosen height is measured from the bottom, so the heading
                // stays where it was dragged to even when the list is shorter
                // than the rows it was left.
                if app.preview_rows().is_some() {
                    while body.len() < head_len + list_h {
                        body.push(Line::default());
                    }
                }
                let top = body.len();
                let block = preview_block(app, width, preview_h);
                if !block.lines.is_empty() {
                    // The heading band is the divider: pressed and dragged, it
                    // moves the boundary between the list and the preview.
                    out.split = Some(SplitHandle {
                        row: top,
                        preview_h,
                        body_h,
                        auto_opens,
                    });
                    let id = app
                        .selected()
                        .map(|a| a.session_id.trim().to_string())
                        .unwrap_or_default();
                    out.preview_area = Some(PreviewArea {
                        rows: top + block.text_from..top + block.lines.len(),
                        text: Vec::new(),
                        sid: block.sid.filter(|_| !id.is_empty()).map(|cols| SidHit {
                            row: top,
                            cols,
                            id,
                        }),
                    });
                }
                body.extend(block.lines);
            }
        }
    }

    // Trim the body before padding, so an over-long card cannot push the key
    // guide off the bottom of the screen.
    let room = height.saturating_sub(footer.lines.len());
    let guide_row = room + footer.guide_at;
    // The prompt line is the footer's first when there is one, and while a
    // field is open it is always the field.
    out.prompt_row =
        (app.input().is_some() && footer.guide_at > 0 && room < height).then_some(room);
    // Where each guide line's own characters stop. A line wider than the
    // screen — one entry longer than the whole width — is cut with `…` in its
    // last column, and a click there must not press the key it covers.
    let visible = |line: usize| {
        let drawn = crate::text::line_width(&footer.lines[footer.guide_at + line]);
        if drawn > width {
            width.saturating_sub(1)
        } else {
            width
        }
    };
    out.keys = footer
        .guide
        .into_iter()
        .filter(|(line, cols, _, _)| guide_row + line < height && cols.start < visible(*line))
        .map(|(line, cols, key, entry)| KeyHit {
            row: guide_row + line,
            from: cols.start,
            to: cols.end.min(visible(line)),
            key,
            entry,
        })
        .collect();
    body.truncate(room);
    if let Some(area) = &mut out.usage_area {
        area.rows.end = area.rows.end.min(room);
    }
    if let Some(area) = &mut out.preview_area {
        area.rows.end = area.rows.end.min(room);
        if area.sid.as_ref().is_some_and(|s| s.row >= room) {
            area.sid = None;
        }
        area.text = area
            .rows
            .clone()
            .map(|row| crate::text::line_text(&body[row]))
            .collect();
        if let Some(sel) = app.selection() {
            for row in area.rows.clone() {
                if let Some(cols) = sel.columns(row, width) {
                    body[row] = highlight(std::mem::take(&mut body[row]), cols);
                }
            }
        }
    }
    while body.len() < room {
        body.push(Line::default());
    }
    body.extend(footer.lines);
    body.truncate(height);
    out.lines = body.iter().map(|l| clamp_line(l, width)).collect();
    out
}

/// Draws the tower.
///
/// Everything above decides what the screen says; this puts it in the buffer.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let composed = compose(app, area.width as usize, area.height as usize);
    render_lines(frame, area, composed.lines);
}

/// Draws lines [`compose`] has already laid out, taking them rather than
/// copying every span of the screen once more.
pub fn render_lines(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::{remote_state, short_name, state_label};
    use time::OffsetDateTime;
    use tower_state::{Agent, AgentState};

    fn fresh(state: AgentState) -> Agent {
        let now = OffsetDateTime::now_utc();
        let mut a = Agent::new("x", "claude", now);
        a.state = state;
        a.updated_at = now;
        a
    }

    /// A remote row and a local row in the same state read the same: the
    /// cell for herdr's word is the local label for the state it maps to.
    #[test]
    fn a_remote_state_cell_is_the_local_label_for_that_state() {
        let pairs = [
            ("working", AgentState::Working),
            ("blocked", AgentState::Waiting),
            ("done", AgentState::DoneUnread),
            ("idle", AgentState::Idle),
        ];
        for (word, state) in pairs {
            let (cell, mapped) = remote_state(word);
            assert_eq!(mapped, state, "{word}");
            assert_eq!(
                cell,
                state_label(&fresh(state), OffsetDateTime::now_utc()),
                "{word}"
            );
        }
    }

    /// Words herdr may add later, and nothing at all, are shown as unknown
    /// in the same width — never mistaken for one the tower knows.
    #[test]
    fn an_unknown_remote_state_is_unknown_and_keeps_the_width() {
        for word in ["", "Working", "WORKING", " working", "paused", "작업중"] {
            let (cell, _) = remote_state(word);
            assert_eq!(cell, "? unknown", "{word:?}");
            assert_eq!(cell.chars().count(), 9, "{word:?}");
        }
    }

    /// The header counts in the row's own word for the states both name:
    /// `working 2` sits over `● working` rows, `wait 1` over `◆ wait`.
    #[test]
    fn the_header_and_the_rows_say_the_same_word() {
        let now = OffsetDateTime::now_utc();
        for state in [AgentState::Working, AgentState::Waiting, AgentState::Idle] {
            let word = short_name(state);
            let row = state_label(&fresh(state), now);
            assert_eq!(
                row[row.find(' ').unwrap() + 1..].trim_end(),
                word,
                "{state:?}"
            );
        }
        let mut stale = fresh(AgentState::Working);
        stale.updated_at = now - time::Duration::days(1);
        assert_eq!(state_label(&stale, now), "? working");
    }

    /// The three list guides are exactly the tables they replaced — the
    /// dock's (no `q`), a tower with a usage board, and one without — keys
    /// and order unchanged.
    #[test]
    fn the_list_guides_are_the_tables_they_replaced() {
        let common = [
            ("↑↓/jk", "move"),
            ("enter", "jump+read"),
            ("/", "filter"),
            ("i", "detail"),
            ("g", "git"),
            ("L", "idle loop"),
            ("T", "schedule"),
        ];
        let tail = [
            ("[/]/=", "preview size"),
            ("W", "wake all"),
            ("C", "close all"),
            ("B", "broadcast"),
            ("r", "refresh"),
        ];
        let with = |u: bool, q: bool| {
            let mut v = common.to_vec();
            if u {
                v.push(("u", "usage"));
            }
            v.extend(tail);
            if q {
                v.push(("q", "quit"));
            }
            v
        };
        assert_eq!(list_help(true, false), with(true, false), "dock");
        assert_eq!(list_help(true, true), with(true, true), "with usage");
        assert_eq!(list_help(false, true), with(false, true), "without usage");
        assert_eq!(list_help(false, true).len(), 13);
        assert_eq!(list_help(true, true).len(), LIST_KEYS.len());
    }

    use super::*;

    /// `pad_right` never cuts, so a kind name wider than its cell pushes every
    /// column after it one step right of the header, on that row alone.
    #[test]
    fn every_supervised_kind_fits_the_agent_column() {
        for kind in tower_state::SUPERVISED_KINDS {
            assert!(text_width(kind) <= W_KIND, "{kind} is wider than W_KIND");
        }
    }

    /// Every kind's cell is the same width, so the columns after it stay
    /// under their headings, and the name starts one cell in, after the bar.
    #[test]
    fn the_kind_cell_is_a_bar_and_a_name_of_one_width() {
        for kind in tower_state::SUPERVISED_KINDS
            .iter()
            .copied()
            .chain(["bash"])
        {
            let [bar, name] = kind_cell(kind);
            assert_eq!(text_width(&bar.content), 1, "{kind}");
            assert_eq!(text_width(&name.content), W_KIND, "{kind}");
            assert!(name.content.starts_with(kind), "{kind}");
        }
        for kind in tower_state::SUPERVISED_KINDS {
            let [bar, name] = kind_cell(kind);
            let v = theme::provider(kind).unwrap();
            assert_eq!(bar.style.fg, Some(v.bar), "{kind}");
            assert_eq!(name.style.fg, Some(v.name), "{kind}");
            assert_eq!(bar.style.bg, None, "no fill: {kind}");
        }
    }

    /// xorshift64*: the same sequence on every machine, so a failure names a
    /// case that replays exactly.
    struct Rng(tower_testrng::Rng);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(tower_testrng::Rng::new(seed))
        }

        fn below(&mut self, n: usize) -> usize {
            self.0.below(n)
        }

        /// Up to `max` pieces, narrow and wide glyphs and spaces mixed —
        /// and characters a zero-width one widens (`❤` + U+FE0F) or combines
        /// with (`e` + U+0301), which a cut measured per character overflowed.
        fn text(&mut self, max: usize) -> String {
            const PIECES: &[&str] = &[
                "a",
                "Z",
                "fix",
                "-",
                " ",
                "·",
                "…",
                "세",
                "션",
                "界",
                "🗼",
                "é",
                "pr-2752",
                "❤\u{fe0f}",
                "e\u{301}",
            ];
            (0..self.below(max + 1))
                .map(|_| PIECES[self.below(PIECES.len())])
                .collect()
        }
    }

    fn with_tab(tab: &str, title: &str) -> Agent {
        let mut a = Agent::new("claude-t", "claude", OffsetDateTime::UNIX_EPOCH);
        a.pane.tab = tab.into();
        a.title = title.into();
        a
    }

    fn text_of(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The ID prefix never reaches the TITLE cell — the PANE column says
    /// where the row is — while a bracket that is not an ID is somebody's own.
    #[test]
    fn the_title_cell_leaves_the_id_prefix_off() {
        let bare = text_of(&title_cell(&with_tab("pr-2752", "fix it")));
        let tab = text_of(&title_cell(&with_tab("[w2:t3] pr-2752", "fix it")));
        assert_eq!(tab, bare);
        let mut named = with_tab("pr-2752", "");
        named.label = "[w2:p1] hero-dark-codex".into();
        let mut plain = with_tab("pr-2752", "");
        plain.label = "hero-dark-codex".into();
        assert_eq!(text_of(&title_cell(&named)), text_of(&title_cell(&plain)));
        assert!(text_of(&title_cell(&with_tab("[wip] x", "t"))).starts_with("[wip] x"));
    }

    /// A pane whose name is only its ID has no name: the live terminal title
    /// shows, exactly as for a pane with no name at all.
    #[test]
    fn a_name_that_is_only_an_id_falls_back_to_the_title() {
        let mut only_id = with_tab("pr-2752", "Claude Code");
        only_id.label = "[w2:p1]".into();
        let unnamed = with_tab("pr-2752", "Claude Code");
        assert_eq!(
            text_of(&title_cell(&only_id)),
            text_of(&title_cell(&unnamed))
        );
        assert!(text_of(&title_cell(&only_id)).contains("Claude Code"));
    }

    /// Whatever the tab and the title — empty, blank, wide glyphs, far too
    /// long — the TITLE cell is exactly its column wide, so the TASK column
    /// after it never moves from one row to the next.
    #[test]
    fn the_title_cell_is_always_exactly_its_column_wide() {
        let mut rng = Rng::new(0x7AB5_EED5);
        for case in 0..3000 {
            let (tab, title) = (rng.text(16), rng.text(16));
            let cell = title_cell(&with_tab(&tab, &title));
            let drawn = text_of(&cell);
            let at = format!("case {case}: tab {tab:?} title {title:?} drew {drawn:?}");
            assert_eq!(text_width(&drawn), W_TITLE, "{at}");
            assert!(drawn.ends_with("  "), "two columns clear of TASK: {at}");
            let both = !tab.trim().is_empty() && !title.trim().is_empty();
            assert_eq!(
                cell.iter().any(|s| s.content == TAB_SEP),
                both,
                "the separator only between a tab and a title: {at}"
            );
            if both {
                // The title always keeps its columns, less the one a wide
                // glyph at the cut can cost.
                let shown = text_width(&cell[2].content);
                let keep = text_width(title.trim()).min(TITLE_KEEP - 1);
                assert!(shown >= keep, "{at}");
                // And a title no wider than TITLE_KEEP is never cut at all,
                // beside any tab, wide glyphs included (#34).
                if text_width(title.trim()) <= TITLE_KEEP {
                    assert_eq!(cell[2].content, title.trim(), "{at}");
                }
            }
        }
    }

    /// The rule, stated case by case for plain ASCII and checked at every
    /// pair of lengths up to past the cell: both whole when they fit — however
    /// long the tab, beside a title short enough (#34); else the title is cut
    /// first; and a tab too long to leave the title [`TITLE_KEEP`] columns is
    /// cut to leave exactly that — or just the title's width, when that is
    /// less.
    #[test]
    fn the_title_is_cut_before_the_tab_at_every_length() {
        // From the constants, so the rule follows them if they change.
        let cell = W_TITLE_TEXT;
        let sep = text_width(TAB_SEP);
        let tab_room = cell - sep - TITLE_KEEP;
        let cut = |s: &str, max: usize| {
            if s.len() <= max {
                s.to_string()
            } else {
                format!("{}…", &s[..max - 1])
            }
        };
        for t in 0..=cell + 4 {
            for n in 0..=cell + 4 {
                let tab = "t".repeat(t);
                let title = "n".repeat(n);
                let want = if t == 0 {
                    if n == 0 {
                        NOT_KNOWN.to_string()
                    } else {
                        cut(&title, cell)
                    }
                } else if n == 0 {
                    cut(&tab, cell)
                } else if t + sep + n <= cell {
                    format!("{tab}{TAB_SEP}{title}")
                } else if t <= tab_room {
                    format!("{tab}{TAB_SEP}{}", cut(&title, cell - sep - t))
                } else {
                    // The tab leaves the title what it uses, up to TITLE_KEEP.
                    format!(
                        "{}{TAB_SEP}{}",
                        cut(&tab, cell - sep - n.min(TITLE_KEEP)),
                        cut(&title, TITLE_KEEP)
                    )
                };
                let drawn = text_of(&title_cell(&with_tab(&tab, &title)));
                assert_eq!(drawn.trim_end(), want, "tab {t} title {n}");
            }
        }
    }

    /// Blank is not a tab: a tab of spaces leaves the title alone in the
    /// cell, and a title of spaces leaves the tab alone, with no dangling
    /// separator either way.
    #[test]
    fn a_blank_tab_or_title_is_left_out_of_the_title_cell() {
        let drawn = |tab: &str, title: &str| text_of(&title_cell(&with_tab(tab, title)));
        assert_eq!(drawn("   ", "fix login").trim_end(), "fix login");
        assert_eq!(drawn("pr-2752", "   ").trim_end(), "pr-2752");
        assert_eq!(drawn("  pr-2752 ", " fix ").trim_end(), "pr-2752›fix");
        // A tab of wide glyphs is cut on a glyph boundary, never through one.
        let wide = drawn(&"세".repeat(15), "fix login");
        assert_eq!(wide.trim_end(), format!("{}…›fix log…", "세".repeat(9)));
    }

    /// A line of several styled spans, narrow and wide glyphs mixed.
    fn styled_line(rng: &mut Rng) -> Line<'static> {
        let styles = [
            Style::default(),
            theme::model().bg(theme::PREVIEW_BG),
            theme::preview_background(),
        ];
        let spans: Vec<Span<'static>> = (0..1 + rng.below(4))
            .map(|_| Span::styled(rng.text(6), styles[rng.below(styles.len())]))
            .collect();
        Line::from(spans)
    }

    /// The style each character of `line` is drawn in, with the column its
    /// glyph starts on: a selector or combining mark shares the column of the
    /// character it belongs to, and `❤` + U+FE0F moves the next glyph two
    /// columns on, as the screen draws it.
    fn cells(line: &Line<'static>) -> Vec<(usize, char, Style)> {
        let mut at = 0;
        let mut out = Vec::new();
        for span in &line.spans {
            for glyph in crate::text::glyphs(&span.content) {
                out.extend(glyph.chars().map(|c| (at, c, span.style)));
                at += crate::text::glyph_width(glyph);
            }
        }
        out
    }

    /// A selection only reverses what it covers: the text and the width are
    /// what they were, every character keeps its own style, and exactly the
    /// characters starting inside the columns — the ones a copy takes, by
    /// [`crate::text::columns`] — are reversed, over any spans and any range,
    /// empty, past the end, or starting halfway through a wide glyph.
    #[test]
    fn a_highlight_reverses_exactly_the_characters_a_copy_takes() {
        let mut rng = Rng::new(0x5E1E_C7ED);
        for case in 0..3000 {
            let line = styled_line(&mut rng);
            let text = line_text_of(&line);
            let (a, b) = (rng.below(40), rng.below(40));
            let cols = a..b;
            let lit = highlight(line.clone(), cols.clone());
            let at = format!("case {case}: {text:?} over {cols:?}");
            assert_eq!(line_text_of(&lit), text, "{at}");
            assert_eq!(
                crate::text::line_width(&lit),
                crate::text::line_width(&line),
                "{at}"
            );
            let before = cells(&line);
            let after = cells(&lit);
            assert_eq!(before.len(), after.len(), "{at}");
            let mut reversed = String::new();
            for ((col, c, style), (col2, c2, style2)) in before.into_iter().zip(after) {
                assert_eq!((col, c), (col2, c2), "{at}");
                let want = if cols.contains(&col) {
                    reversed.push(c);
                    style.add_modifier(Modifier::REVERSED)
                } else {
                    style
                };
                assert_eq!(style2, want, "{at}: {c:?} at column {col}");
            }
            assert_eq!(reversed, crate::text::columns(&text, cols.clone()), "{at}");
        }
    }

    fn line_text_of(line: &Line<'static>) -> String {
        crate::text::line_text(line)
    }

    /// The board never takes more than the body,
    /// even when the list is long enough to scroll and fills its share; and
    /// the list keeps the rows it was promised.
    #[test]
    fn the_usage_board_fits_the_body() {
        for body_h in 0..60 {
            for rows in 0..30 {
                for want in 0..40 {
                    let (list_h, board) = usage_split(body_h, rows, want);
                    if board == 0 {
                        assert_eq!(list_h, body_h);
                        continue;
                    }
                    assert!(board >= PREVIEW_MIN);
                    assert!(board <= want);
                    assert_eq!(
                        list_h + PREVIEW_RULES + board,
                        body_h,
                        "{body_h} {rows} {want}"
                    );
                    assert!(list_h >= rows.clamp(1, 4), "{body_h} {rows} {want}");
                }
            }
        }
    }
}
