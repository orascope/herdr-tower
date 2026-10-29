// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower usage` — each agent CLI's plan and how much of its five-hour
//! and seven-day windows is used, with when each resets.
//!
//! The readings come from `tower_usage` (codex from its rollouts, claude from
//! its OAuth usage endpoint). This module turns them into the board: one
//! block per vendor, a bar per window, coloured by how full it is. The
//! fetching and the watch loop live in `cmd::usage`, so everything here runs
//! in a test without the network or a terminal.

use serde_json::{json, Value};
use time::{OffsetDateTime, UtcOffset};
use tower_usage::{Limits, Window};

use tower_ui::meter::EMPTY_CELL;
/// Where a window's bar turns amber and red: the tower's meter, shared with
/// the header's machine load.
pub use tower_ui::meter::{ORANGE_FROM, RED_FROM};
/// Cells in a bar.
pub const BAR_WIDTH: usize = 20;
/// The rule drawn between the AIs' blocks: as wide as the longest line a
/// bar makes (`  7d  ` + bar + ` 100%  resets 09-26 Sat 17:35 (in 1d22h)`).
pub const RULE_WIDTH: usize = 67;

/// How full a window is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Green,
    Orange,
    Red,
}

pub fn level(used_pct: f64) -> Level {
    if used_pct >= RED_FROM {
        Level::Red
    } else if used_pct >= ORANGE_FROM {
        Level::Orange
    } else {
        Level::Green
    }
}

/// The tower's own palette (`tower_ui::theme`): green, the amber it uses for
/// "waiting", red.
fn ansi(level: Level) -> &'static str {
    match level {
        Level::Green => "\x1b[1;38;2;67;181;129m",
        Level::Orange => "\x1b[1;38;2;250;166;26m",
        Level::Red => "\x1b[1;38;2;240;71;71m",
    }
}
const DIM: &str = "\x1b[38;2;138;138;138m";
/// The guard's floor, on the bars and on the sliders, and the reserve it keeps.
const YELLOW: &str = "\x1b[1;38;2;255;215;0m";
/// The calendar's panel: the tower's dark slate teal, the same as its
/// preview panel, so every colour drawn on it keeps its contrast.
const CALENDAR_BG: &str = "\x1b[48;2;31;48;58m";
/// The slider being changed, as an effort picker marks its choice.
const GREEN: &str = "\x1b[1;38;2;67;181;129m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// An AI's name with its bar before it (`▌claude`), padded to `width` cells —
/// the same mark the tower's AGENT column gives it, so one vendor looks like
/// one vendor on both screens. The bar is there without colour too, so the
/// plain board is the coloured one with its escapes taken out; a name with
/// no brand gets a blank in the bar's cell and lines up with the others.
fn vendor_label(name: &str, width: usize, color: bool) -> String {
    let pad = " ".repeat(width.saturating_sub(1 + tower_text::width(name)));
    let brand = tower_ui::vendor_rgb(name);
    let bar = if brand.is_some() {
        tower_ui::VENDOR_BAR
    } else {
        " "
    };
    match brand {
        Some([bar_rgb, name_rgb]) if color => format!(
            "{}{bar}{RESET}{BOLD}{}{name}{RESET}{pad}",
            fg(bar_rgb),
            fg(name_rgb)
        ),
        _ if color => format!("{bar}{BOLD}{name}{RESET}{pad}"),
        _ => format!("{bar}{name}{pad}"),
    }
}

/// A truecolour foreground escape.
fn fg((r, g, b): (u8, u8, u8)) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// `width` cells, the used share filled. Anything above zero shows at least
/// one cell, and anything below 100 leaves at least one empty, so 1% and 99%
/// never look like 0% and 100%.
pub fn bar(used_pct: f64, width: usize) -> String {
    let filled = tower_ui::meter::filled(used_pct, width);
    "█".repeat(filled) + &"░".repeat(width - filled)
}

/// [`bar`] with the guard's limit marked (`╎`) between cells. The marker
/// must not replace a used or unused cell: at a 5% floor, that last unused
/// cell is the only visual difference between 5% and 0% left.
pub fn bar_marked(used_pct: f64, width: usize, limit: Option<u8>) -> String {
    let mut cells: Vec<char> = bar(used_pct, width).chars().collect();
    if let Some(l) = limit {
        let i = ((f64::from(l) / 100.0) * width as f64).round() as usize;
        cells.insert(i.min(width), '╎');
    }
    cells.into_iter().collect()
}

/// A [`bar_marked`] bar drawn as btop draws a meter: every used cell a `■`
/// in the colour of its place on the bar — green at the start, amber half
/// way, red at the end — so how far it reached reads from the colour of its
/// tip; unused cells the same `■` in dark grey; the floor's `╎` in yellow.
fn meter(marked: &str) -> String {
    let width = marked.chars().filter(|c| *c != '╎').count().max(1);
    let mut out = String::new();
    let mut i = 0;
    for c in marked.chars() {
        match c {
            '█' => out.push_str(&format!("{}■", fg(tower_ui::meter::cell(i, width, true)))),
            '╎' => out.push_str(&format!("{YELLOW}╎")),
            _ => out.push_str(&format!("{}■", fg(EMPTY_CELL))),
        }
        if c != '╎' {
            i += 1;
        }
    }
    out.push_str(RESET);
    out
}

/// The screen column (1-based) where every bar's first cell is drawn: bar
/// lines, and the tab's sliders, start `"  5h  "`.
pub const BAR_COL: u16 = 7;
/// The cell a floor's handle sits in: where "this much left" falls on a bar
/// of `width` cells, at the same boundary [`bar_marked`] marks.
fn handle_cell(floor: u8, width: usize) -> usize {
    let used = 100 - u32::from(floor);
    ((used as f64 / 100.0 * width as f64).round() as usize).min(width)
}

/// A slider as text, like an effort picker: a thin track `─` with the
/// handle `▼` at the floor; all track when off.
pub fn slider(floor: Option<u8>, width: usize) -> String {
    let Some(f) = floor else {
        return "─".repeat(width + 1);
    };
    let h = handle_cell(f, width);
    (0..=width)
        .map(|c| if c == h { '▼' } else { '─' })
        .collect()
}

/// The scale drawn once above the sliders, in the bars' columns: the ends'
/// meaning, then the share used at every quarter.
pub fn scale(width: usize) -> (String, String) {
    let mut ends: Vec<char> = " ".repeat(width).chars().collect();
    for (i, c) in "spend".chars().enumerate() {
        ends[i] = c;
    }
    for (i, c) in "keep".chars().enumerate() {
        ends[width - 4 + i] = c;
    }
    let mut ticks: Vec<char> = " ".repeat(width + 1).chars().collect();
    for (pct, text) in [(0, "0"), (25, "25"), (50, "50"), (75, "75"), (100, "100")] {
        let at = (pct * width / 100).min(width + 1 - text.len());
        for (i, c) in text.chars().enumerate() {
            ticks[at + i] = c;
        }
    }
    (
        ends.into_iter().collect(),
        ticks.into_iter().collect::<String>() + "% used",
    )
}

/// The floor a click or drag at screen column `col` asks for: the share left
/// at that boundary. Left of the bar is 100% reserve; its right endpoint
/// is zero, which turns the guard off. Each cell spans 5% on a 20-cell bar.
pub fn floor_at_column(col: u16, width: usize) -> Option<u8> {
    if width == 0 {
        return None;
    }
    let cell = usize::from(col.saturating_sub(BAR_COL)).min(width);
    let floor = (100.0 - cell as f64 / width as f64 * 100.0).round() as u8;
    (floor > 0).then_some(floor)
}

/// "3d4h", "1h18m", "12m", "<1m": how long until `at`.
pub fn until(now: OffsetDateTime, at: OffsetDateTime) -> String {
    let mins = (at - now).whole_minutes();
    if mins < 1 {
        return "<1m".into();
    }
    let (d, h, m) = (mins / 1440, mins % 1440 / 60, mins % 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

/// A moment in local time with its date and weekday: "09-26 Sat 17:35" —
/// ISO 8601's month-day order without the year, so it cannot be read as
/// day/month.
pub fn dated(at: OffsetDateTime, offset: UtcOffset) -> String {
    let l = at.to_offset(offset);
    format!(
        "{:02}-{:02} {} {:02}:{:02}",
        u8::from(l.month()),
        l.day(),
        &l.weekday().to_string()[..3],
        l.hour(),
        l.minute()
    )
}

/// When a window resets, in local time and always with its date:
/// "resets 09-26 Sat 17:35 (in 1d22h)"; "reset" once it has passed.
pub fn reset_label(now: OffsetDateTime, at: OffsetDateTime, offset: UtcOffset) -> String {
    if at <= now {
        return "reset — waiting for a new reading".into();
    }
    format!("resets {} (in {})", dated(at, offset), until(now, at))
}

/// One vendor's block.
#[derive(Debug, Clone, PartialEq)]
pub struct Vendor {
    pub name: &'static str,
    pub limits: Limits,
    /// Why there are no figures, or why they may be old.
    pub note: Option<String>,
}

/// Every block, in the order they are drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Board {
    pub vendors: Vec<Vendor>,
    pub at: OffsetDateTime,
    /// The usage guard's limits, drawn on the bars.
    pub guard: crate::usageguard::Guard,
}

/// A time of day today ("08:00"), with its date on any other day.
fn stamp(t: OffsetDateTime, now: OffsetDateTime, offset: UtcOffset) -> String {
    let l = t.to_offset(offset);
    if l.date() == now.to_offset(offset).date() {
        format!("{:02}:{:02}", l.hour(), l.minute())
    } else {
        dated(t, offset)
    }
}

/// The board as text; `color` adds the tower's colours (a terminal), without
/// it the same text reads in a log.
pub fn render(board: &Board, offset: UtcOffset, color: bool) -> String {
    render_editing(board, offset, color, None).text
}

/// Which slider the usage tab is changing: an index into
/// [`Rendered::sliders`].
pub type Editing = Option<usize>;

/// One slider on the tab's screen: its row (1-based), and whose floor it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliderRow {
    pub row: u16,
    pub vendor: &'static str,
    /// `"5h"` or `"7d"`.
    pub window: &'static str,
}

/// The board as drawn, with where each slider went.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub text: String,
    pub sliders: Vec<SliderRow>,
}

/// [`render`] for the usage tab: each AI's bars carry a slider right above
/// them, a yellow dotted line in the bars' columns (`BAR_COL`), and
/// `editing` is the one the keys move. The rows are answered so the mouse
/// handler can read a click back.
pub fn render_editing(board: &Board, offset: UtcOffset, color: bool, editing: Editing) -> Rendered {
    let now = board.at;
    let guard = board.guard;
    let holds = crate::usageguard::holds(board, &guard, now);
    let paint = |code: &str, text: &str| {
        if color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    };
    // A window's bar, the floor's mark where the floor is: at 25% left, at
    // 75% used. In colour it is drawn the way btop draws a meter.
    let painted_bar = |used_pct: f64, floor: Option<u8>| {
        let marked = bar_marked(used_pct, BAR_WIDTH, floor.map(|f| 100 - f));
        if color {
            meter(&marked)
        } else {
            marked
        }
    };
    let tab = editing.is_some();
    let mut sliders: Vec<SliderRow> = Vec::new();
    let mut out = String::new();
    out.push_str(&format!(
        "{}{}\n",
        paint(BOLD, "USAGE"),
        paint(DIM, &format!("  updated {}", stamp(now, now, offset)))
    ));
    out.push_str(&calendar(board, &holds, offset, color));
    if tab {
        out.push_str(&paint(
            DIM,
            "guard  drag ▼ or ←→ (1% steps, 0 = off) · ↑↓ choose · x off · r refresh\n",
        ));
        let (ends, ticks) = scale(BAR_WIDTH);
        out.push_str(&format!("      {}\n", paint(DIM, ends.trim_end())));
        out.push_str(&format!("      {}\n", paint(DIM, &ticks)));
    }
    for v in &board.vendors {
        // A long rule between the AIs, so each one's block reads as its own.
        out.push_str(&paint(DIM, &"─".repeat(RULE_WIDTH)));
        out.push('\n');
        let floors = guard.floors(v.name);
        let covered = crate::usageguard::vendor_of(v.name).is_some();
        let mut head = vendor_label(v.name, 8, color);
        head.push_str(&paint(BOLD, v.limits.plan.as_deref().unwrap_or("—")));
        if v.limits.reached {
            head.push_str(&paint(ansi(Level::Red), "  LIMIT REACHED"));
        }
        if let Some(t) = v.limits.as_of {
            // Figures older than a few minutes say how old they are.
            if (now - t).whole_minutes() >= 5 {
                head.push_str(&paint(DIM, &format!("  as of {}", stamp(t, now, offset))));
            }
        }
        if covered && !tab && (floors.five_hour.is_some() || floors.seven_day.is_some()) {
            let show = |f: Option<u8>| f.map_or("off".into(), |f| format!("{f}%"));
            head.push_str(&paint(
                YELLOW,
                &format!(
                    "  floor ◆5h {} · ●7d {}",
                    show(floors.five_hour),
                    show(floors.seven_day)
                ),
            ));
        }
        out.push_str(&head);
        out.push('\n');
        for (label, w) in [("5h", &v.limits.five_hour), ("7d", &v.limits.seven_day)] {
            let floor = floors.get(label);
            // The slider, right above the bar it sets the floor of.
            if tab && covered && w.is_some() {
                let index = sliders.len();
                sliders.push(SliderRow {
                    row: out.matches('\n').count() as u16 + 1,
                    vendor: crate::usageguard::vendor_of(v.name).unwrap_or(""),
                    window: label,
                });
                let chosen = editing == Some(index);
                let value = floor.map_or("off".into(), |f| format!("keep {f}%"));
                let caption = format!("{} {} · {value}", v.name, window_label(label));
                // Track, handle, and the reserve beyond it in its own colour —
                // the part of the window the tower will not spend.
                let text = slider(floor, BAR_WIDTH);
                let drawn = match text.split_once('▼') {
                    Some((spend, keep)) => format!(
                        "{}{}{}",
                        paint(DIM, spend),
                        paint(if chosen { GREEN } else { YELLOW }, "▼"),
                        paint(YELLOW, keep)
                    ),
                    None => paint(DIM, &text),
                };
                out.push_str(&format!(
                    "      {drawn}  {}\n",
                    if chosen {
                        paint(GREEN, &format!("▸ {caption}"))
                    } else {
                        paint(DIM, &format!("  {caption}"))
                    }
                ));
            }
            match w {
                // The window has reset since this reading: what it said is
                // no longer true, and what is true is not known yet.
                // The bar still shows the floor, on an empty window.
                Some(Window {
                    resets_at: Some(t), ..
                }) if *t <= now => {
                    out.push_str(&format!(
                        " {}  {}    {}\n",
                        window_label(label),
                        painted_bar(0.0, floor),
                        paint(
                            DIM,
                            &format!("—  reset at {}, no newer reading", stamp(*t, now, offset))
                        )
                    ));
                }
                Some(Window {
                    used_pct,
                    resets_at,
                }) => {
                    let lvl = level(*used_pct);
                    let reset = resets_at
                        .map(|t| reset_label(now, t, offset))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        " {}  {} {}  {}\n",
                        window_label(label),
                        painted_bar(*used_pct, floor),
                        paint(
                            ansi(lvl),
                            &format!("{:>3.0}% left", 100.0 - used_pct.clamp(0.0, 100.0))
                        ),
                        paint(DIM, &reset)
                    ));
                }
                None if v.limits.is_empty() => {}
                None => out.push_str(&paint(DIM, &format!(" {}  —\n", window_label(label)))),
            }
        }
        if let Some(h) = holds.iter().find(|h| h.vendor == v.name) {
            out.push_str(&paint(
                ansi(Level::Orange),
                &format!(
                    "  ⏸ tower sends held — {}, until {}\n",
                    h.why,
                    crate::usageguard::until_label(h.until, offset)
                ),
            ));
        }
        if let Some(note) = &v.note {
            out.push_str(&paint(DIM, &format!("  {note}\n")));
        }
    }
    Rendered { text: out, sliders }
}

/// How many cells a window's bar gets in [`summary_line`].
pub const SUMMARY_BAR: usize = 5;

/// The board in one line, for the tower's header: each AI's plan and both
/// windows as short bars with what is left, and `⏸` where the guard holds
/// the tower's sends. The whole board is the tower's `u`.
pub fn summary_line(board: &Board, offset: UtcOffset, color: bool) -> String {
    let now = board.at;
    let holds = crate::usageguard::holds(board, &board.guard, now);
    let paint = |code: &str, text: &str| {
        if color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    };
    let mut parts: Vec<String> = Vec::new();
    for v in &board.vendors {
        let floors = board.guard.floors(v.name);
        let mut part = vendor_label(v.name, 1 + tower_text::width(v.name) + 1, color);
        if let Some(plan) = v.limits.plan.as_deref() {
            part.push_str(&paint(DIM, plan));
        }
        if v.limits.is_empty() {
            part.push_str(&paint(
                DIM,
                if v.name == "gemini" { "per-day" } else { "—" },
            ));
        }
        for (label, w) in [("5h", &v.limits.five_hour), ("7d", &v.limits.seven_day)] {
            if v.limits.is_empty() {
                break;
            }
            if !part.ends_with(' ') {
                part.push(' ');
            }
            part.push_str(&window_label(label));
            part.push(' ');
            match w {
                Some(Window {
                    resets_at: Some(t), ..
                }) if *t <= now => part.push_str(&paint(DIM, "reset")),
                Some(Window { used_pct, .. }) => {
                    let floor = floors.get(label).map(|f| 100 - f);
                    let marked = bar_marked(*used_pct, SUMMARY_BAR, floor);
                    part.push_str(&if color { meter(&marked) } else { marked });
                    part.push_str(&paint(
                        ansi(level(*used_pct)),
                        &format!(" {:.0}%", 100.0 - used_pct.clamp(0.0, 100.0)),
                    ));
                }
                None => part.push_str(&paint(DIM, "—")),
            }
        }
        if let Some(h) = holds.iter().find(|h| h.vendor == v.name) {
            part.push_str(&paint(
                ansi(Level::Red),
                &format!(
                    " ⏸ held → {}",
                    crate::usageguard::until_label(h.until, offset)
                ),
            ));
        } else if v.limits.reached {
            part.push_str(&paint(ansi(Level::Red), " LIMIT"));
        }
        parts.push(part);
    }
    parts.join("   ")
}

/// Writes one AI's floor for one window to the config
/// (`usage_guard_<ai>_<window>`), through `config set` and its lock.
pub fn save_floor(slider: &SliderRow, floor: Option<u8>) -> Result<(), String> {
    let path = tower_config::path()
        .ok_or("the config file has no path — neither HERDR_TOWER_CONFIG nor HOME is set")?;
    let key = format!("usage_guard_{}_{}", slider.vendor, slider.window);
    let value = floor.map_or("off".to_string(), |f| f.to_string());
    crate::configcmd::run(&mut std::io::sink(), &path, &["set".into(), key, value])
}

/// The board as JSON, for `--json` and for other programs.
pub fn to_json(board: &Board) -> Value {
    let ts = |t: &Option<OffsetDateTime>| {
        t.and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
    };
    let window = |w: &Option<Window>| {
        w.as_ref().map(|w| {
            json!({
                "used_pct": w.used_pct,
                "level": match level(w.used_pct) {
                    Level::Green => "green",
                    Level::Orange => "orange",
                    Level::Red => "red",
                },
                "resets_at": ts(&w.resets_at),
            })
        })
    };
    json!({
        "at": ts(&Some(board.at)),
        "vendors": board.vendors.iter().map(|v| json!({
            "name": v.name,
            "plan": v.limits.plan,
            "five_hour": window(&v.limits.five_hour),
            "seven_day": window(&v.limits.seven_day),
            "as_of": ts(&v.limits.as_of),
            "reached": v.limits.reached,
            "note": v.note,
        })).collect::<Vec<_>>(),
    })
}

/// A window's name with the mark the calendar draws for its reset: `◆5h`,
/// `●7d`. The same mark on the bar, the slider and the calendar ties them
/// together.
pub fn window_label(window: &str) -> String {
    let mark = if window == "5h" { '◆' } else { '●' };
    format!("{mark}{window}")
}

/// Days on the reset calendar: today and the week after it, so every seven-day
/// window's next reset is on it.
pub const CALENDAR_DAYS: usize = 8;
/// The width of a day's column (and of the name column before them).
const DAY_WIDTH: usize = 8;

/// The reset calendar: a column per day from today, a row per AI, and in
/// each AI's row its next seven-day reset (`●`) and its next five-hour reset
/// (`◆`), with the time. Only resets the AIs reported: no dates are guessed.
/// Today's column is bold; the day a held AI is released is yellow.
pub fn calendar(
    board: &Board,
    holds: &[crate::usageguard::Hold],
    offset: UtcOffset,
    color: bool,
) -> String {
    let paint = |code: &str, text: &str| {
        if color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    };
    let today = board.at.to_offset(offset).date();
    let days: Vec<time::Date> = (0..CALENDAR_DAYS)
        .filter_map(|i| today.checked_add(time::Duration::days(i as i64)))
        .collect();
    // Each cell is its text painted, then plain padding: colour never moves
    // a column, and a line's trailing padding can be trimmed.
    let cell = |text: &str| format!("{text:<DAY_WIDTH$}");
    let painted = |code: &str, text: &str| {
        let pad = DAY_WIDTH.saturating_sub(text.chars().count());
        format!("{}{}", paint(code, text), " ".repeat(pad))
    };
    let mut weekdays = " ".repeat(DAY_WIDTH);
    let mut dates = " ".repeat(DAY_WIDTH);
    for (i, d) in days.iter().enumerate() {
        let code = if i == 0 { BOLD } else { DIM };
        weekdays.push_str(&painted(code, &d.weekday().to_string()[..3]));
        dates.push_str(&painted(
            code,
            &format!("{:02}-{:02}", u8::from(d.month()), d.day()),
        ));
    }
    let mut out = format!("{}\n{}\n", weekdays.trim_end(), dates.trim_end());
    for v in &board.vendors {
        let mut line = vendor_label(v.name, DAY_WIDTH, color);
        let Some(vendor) = crate::usageguard::vendor_of(v.name) else {
            line.push_str(&paint(DIM, "— no 5h or 7d resets"));
            out.push_str(&line);
            out.push('\n');
            continue;
        };
        let future = |w: &Option<Window>| {
            w.as_ref()
                .and_then(|w| w.resets_at)
                .filter(|t| *t > board.at)
        };
        let (five, seven) = (future(&v.limits.five_hour), future(&v.limits.seven_day));
        let release = holds
            .iter()
            .find(|h| h.vendor == vendor)
            .and_then(|h| h.until)
            .map(|t| t.to_offset(offset).date());
        let hm = |t: OffsetDateTime| {
            let l = t.to_offset(offset);
            format!("{:02}:{:02}", l.hour(), l.minute())
        };
        let mut cells = String::new();
        for d in &days {
            let on = |t: Option<OffsetDateTime>| t.filter(|t| t.to_offset(offset).date() == *d);
            let text = match (on(five), on(seven)) {
                (Some(_), Some(s)) => format!("◆●{}", hm(s)),
                (None, Some(s)) => format!("●{}", hm(s)),
                (Some(f), None) => format!("◆{}", hm(f)),
                (None, None) => String::new(),
            };
            cells.push_str(&if text.is_empty() {
                cell("")
            } else if release == Some(*d) {
                painted(YELLOW, &text)
            } else {
                painted(BOLD, &text)
            });
        }
        line.push_str(&cells);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if color {
        band(&out, DAY_WIDTH * (CALENDAR_DAYS + 1))
    } else {
        out
    }
}

/// The calendar's own background: every line of it filled to `width` on
/// [`CALENDAR_BG`], so the dates read as a panel apart from the bars below.
/// Each reset inside a line puts the background back, since a reset clears
/// it along with the colour.
fn band(text: &str, width: usize) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let visible = visible_width(line);
        out.push_str(CALENDAR_BG);
        out.push_str(&line.replace(RESET, &format!("{RESET}{CALENDAR_BG}")));
        out.push_str(&" ".repeat(width.saturating_sub(visible)));
        out.push_str(RESET);
        out.push('\n');
    }
    out
}

/// A line's width on screen: its characters, less the colour escapes.
fn visible_width(line: &str) -> usize {
    let mut n = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

/// A key the usage tab acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    /// Keep less in reserve: the floor goes down 1%.
    Left,
    /// Keep more in reserve: the floor goes up 1%.
    Right,
    /// Off, or back to the default when off.
    Toggle,
    /// Ask every source again now.
    Refresh,
    Quit,
    /// A left-button press or drag at a screen position (1-based).
    Mouse {
        row: u16,
        col: u16,
    },
    WheelUp,
    WheelDown,
}

/// The keys in what the terminal sent: arrows as `ESC [ A`…, vi keys,
/// `+`/`-`, `x`. The tab has no key to leave by: closing it is what ends it,
/// and the supervisor keeps it. Ctrl-C alone still ends the board, read as a
/// key so the terminal is put back first. Anything else is ignored.
pub fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let k = match bytes[i] {
            // SGR mouse: ESC [ < button ; col ; row (M | m).
            0x1b if bytes.get(i + 1) == Some(&b'[') && bytes.get(i + 2) == Some(&b'<') => {
                let rest = &bytes[i + 3..];
                let Some(end) = rest.iter().position(|b| *b == b'M' || *b == b'm') else {
                    break;
                };
                let fields: Vec<u16> = std::str::from_utf8(&rest[..end])
                    .unwrap_or("")
                    .split(';')
                    .filter_map(|f| f.parse().ok())
                    .collect();
                i += 3 + end;
                match (fields.as_slice(), rest[end]) {
                    // 0: left press; 32: motion with the left button held.
                    ([button, col, row], b'M') if *button == 0 || *button == 32 => {
                        Some(Key::Mouse {
                            row: *row,
                            col: *col,
                        })
                    }
                    ([64, _, _], b'M') => Some(Key::WheelUp),
                    ([65, _, _], b'M') => Some(Key::WheelDown),
                    _ => None,
                }
            }
            0x1b if bytes.get(i + 1) == Some(&b'[') => {
                let k = match bytes.get(i + 2) {
                    Some(b'A') => Some(Key::Up),
                    Some(b'B') => Some(Key::Down),
                    Some(b'C') => Some(Key::Right),
                    Some(b'D') => Some(Key::Left),
                    _ => None,
                };
                i += 2;
                k
            }
            0x03 => Some(Key::Quit),
            b'k' => Some(Key::Up),
            b'j' => Some(Key::Down),
            b'l' | b'+' | b'=' => Some(Key::Right),
            b'h' | b'-' => Some(Key::Left),
            b'x' => Some(Key::Toggle),
            b'r' => Some(Key::Refresh),
            _ => None,
        };
        out.extend(k);
        i += 1;
    }
    out
}

/// Whether a manual refresh may go now: `None` when it may, or the whole
/// seconds left to wait when the last one was less than `gap` ago.
pub fn refresh_wait(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    gap: std::time::Duration,
) -> Option<u64> {
    let since = now.duration_since(last?);
    (since < gap).then(|| (gap - since).as_secs().max(1))
}

/// A floor after a key: 1% steps between 1 and 100; zero is off, and
/// from off the next step up is 1. `Toggle` turns it off, or back to
/// `default` when it is off.
pub fn step_floor(floor: Option<u8>, key: Key, default: u8) -> Option<u8> {
    match (key, floor) {
        (Key::Right, None) => Some(1),
        (Key::Right, Some(f)) => Some(f.saturating_add(1).min(100)),
        (Key::Left, Some(f)) if f <= 1 => None,
        (Key::Left, Some(f)) => Some(f - 1),
        (Key::Left, None) => None,
        (Key::Toggle, None) => Some(default),
        (Key::Toggle, Some(_)) => None,
        (_, f) => f,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_ui::meter::gradient;

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }
    const KST: UtcOffset = match UtcOffset::from_hms(9, 0, 0) {
        Ok(o) => o,
        Err(_) => panic!(),
    };

    #[test]
    fn the_colour_changes_at_fifty_and_eighty() {
        let cases = [
            (0.0, Level::Green),
            (49.9, Level::Green),
            (50.0, Level::Orange),
            (79.9, Level::Orange),
            (80.0, Level::Red),
            (100.0, Level::Red),
        ];
        for (pct, want) in cases {
            assert_eq!(level(pct), want, "{pct}");
        }
    }

    /// Property: the bar is always `width` cells, fills monotonically, and
    /// only 0% is empty and only 100% is full.
    #[test]
    fn a_bar_is_always_its_width_and_fills_monotonically() {
        let mut last = 0;
        for tenth in 0..=1000 {
            let pct = f64::from(tenth) / 10.0;
            let b = bar(pct, BAR_WIDTH);
            assert_eq!(b.chars().count(), BAR_WIDTH, "{pct}");
            let filled = b.chars().filter(|c| *c == '█').count();
            assert!(filled >= last, "{pct}: {filled} < {last}");
            last = filled;
            assert_eq!(filled == 0, pct == 0.0, "{pct}");
            assert_eq!(filled == BAR_WIDTH, pct == 100.0, "{pct}");
        }
        assert_eq!(bar(-5.0, 4), "░░░░");
        assert_eq!(bar(500.0, 4), "████");
    }

    #[test]
    fn time_left_is_short_and_exact() {
        let now = at(0);
        let cases = [
            (30, "<1m"),
            (60, "1m"),
            (59 * 60, "59m"),
            (78 * 60, "1h18m"),
            (24 * 3600 - 60, "23h59m"),
            (24 * 3600, "1d0h"),
            (3 * 86400 + 4 * 3600 + 59 * 60, "3d4h"),
            (-100, "<1m"),
        ];
        for (secs, want) in cases {
            assert_eq!(until(now, at(secs)), want, "{secs}");
        }
    }

    #[test]
    fn a_reset_always_names_its_date_and_weekday() {
        // 2026-09-24 10:00 KST, a Thursday.
        let now = at(1_790_211_600);
        assert_eq!(
            reset_label(now, now + time::Duration::minutes(78), KST),
            "resets 09-24 Thu 11:18 (in 1h18m)"
        );
        assert_eq!(
            reset_label(now, now + time::Duration::hours(24 * 3 + 2), KST),
            "resets 09-27 Sun 12:00 (in 3d2h)"
        );
        assert!(reset_label(now, now, KST).starts_with("reset —"));
    }

    #[test]
    fn the_meter_runs_green_to_amber_to_red_along_the_bar() {
        assert_eq!(gradient(0.0), (67, 181, 129));
        assert_eq!(gradient(ORANGE_FROM), (250, 166, 26));
        assert_eq!(gradient(RED_FROM), (240, 71, 71));
        assert_eq!(gradient(100.0), (240, 71, 71));
        // Half way to amber is half way in each channel.
        assert_eq!(gradient(25.0), (159, 174, 78));
        // A cell's colour is its place on the bar, not how full the bar is:
        // the first cells of a full bar are as green as a nearly empty one's.
        let full = meter(&bar(100.0, 20));
        let low = meter(&bar(10.0, 20));
        assert!(
            full.starts_with(&format!("{}■", fg(gradient(2.5)))),
            "{full}"
        );
        assert!(low.starts_with(&format!("{}■", fg(gradient(2.5)))), "{low}");
        assert!(
            full.ends_with(&format!("{}■{RESET}", fg(gradient(97.5)))),
            "{full}"
        );
        assert!(
            low.ends_with(&format!("{}■{RESET}", fg(EMPTY_CELL))),
            "{low}"
        );
        assert_eq!(
            strip_ansi(&meter(&bar_marked(40.0, 20, Some(75))))
                .chars()
                .count(),
            21
        );
    }

    #[test]
    fn a_window_that_has_reset_since_its_reading_shows_no_share() {
        let mut b = board();
        let now = b.at;
        b.vendors[1].limits.seven_day = Some(Window {
            used_pct: 94.0,
            resets_at: Some(now - time::Duration::hours(1)),
        });
        let text = render(&b, KST, false);
        assert!(
            text.contains(" ●7d  ░░░░░░░░░░░░░░░░░░░░    —  reset at 09:00, no newer reading\n"),
            "{text}"
        );
        assert!(!text.contains("94%"), "{text}");
        assert!(!text.contains("  6% left"), "{text}");
        // The floor stays marked, in an empty window's colour.
        b.guard.codex.seven_day = Some(60);
        let text = render(&b, KST, false);
        assert!(
            text.contains(" ●7d  ░░░░░░░░╎░░░░░░░░░░░░    —  reset at 09:00, no newer reading\n"),
            "{text}"
        );
        // In colour: an empty meter, every cell grey, the mark yellow.
        let coloured = render(&b, KST, true);
        let empty = format!("{}■", fg(EMPTY_CELL));
        assert!(
            coloured.contains(&format!("{}{YELLOW}╎{}", empty.repeat(8), empty.repeat(12))),
            "{coloured}"
        );
    }

    fn board() -> Board {
        let now = at(1_790_211_600);
        Board {
            at: now,
            guard: crate::usageguard::Guard::default(),
            vendors: vec![
                Vendor {
                    name: "claude",
                    limits: Limits {
                        plan: Some("Max 5x".into()),
                        five_hour: Some(Window {
                            used_pct: 14.0,
                            resets_at: Some(now + time::Duration::minutes(78)),
                        }),
                        seven_day: Some(Window {
                            used_pct: 85.0,
                            resets_at: Some(now + time::Duration::hours(74)),
                        }),
                        as_of: None,
                        reached: false,
                    },
                    note: None,
                },
                Vendor {
                    name: "codex",
                    limits: Limits {
                        plan: Some("Pro".into()),
                        five_hour: None,
                        seven_day: Some(Window {
                            used_pct: 55.0,
                            resets_at: None,
                        }),
                        as_of: Some(now - time::Duration::hours(2)),
                        reached: false,
                    },
                    note: None,
                },
                Vendor {
                    name: "gemini",
                    limits: Limits::default(),
                    note: Some("no 5h or 7d windows".into()),
                },
            ],
        }
    }

    #[test]
    fn the_summary_is_every_ai_in_one_line_with_what_is_left() {
        let line = summary_line(&board(), KST, false);
        assert!(!line.contains('\n'));
        for want in [
            "▌claude Max 5x ◆5h ",
            " 86% ●7d ",
            " 15%   ▌codex Pro ◆5h — ●7d ",
            " 45%   ▌gemini per-day",
        ] {
            assert!(line.contains(want), "{want:?} in {line:?}");
        }
        assert!(!line.contains("held"), "no guard, nothing held");
        // Coloured, it reads the same once the escapes are out.
        let coloured = summary_line(&board(), KST, true);
        let mut plain = String::new();
        let mut rest = coloured.as_str();
        while let Some(at) = rest.find('\x1b') {
            plain.push_str(&rest[..at]);
            rest = &rest[at..];
            rest = &rest[rest.find('m').map_or(rest.len(), |m| m + 1)..];
        }
        plain.push_str(rest);
        assert_eq!(plain.replace('■', "█"), line.replace('░', "█"));
    }

    #[test]
    fn the_summary_says_whom_the_guard_holds() {
        let mut b = board();
        b.guard = crate::usageguard::Guard::from_config(&tower_config::Config {
            usage_guard_7d: Some("30".into()),
            ..Default::default()
        });
        let line = summary_line(&b, KST, false);
        assert!(line.contains("▌claude Max 5x"), "{line}");
        assert!(
            line.contains("⏸ held →"),
            "claude has 15% of 7d left, under 30%: {line}"
        );
        assert!(
            line.contains("●7d ███╎█ 15%") || line.contains('╎'),
            "the floor is on the bar: {line}"
        );
    }

    #[test]
    fn the_board_reads_the_same_with_and_without_colour() {
        let plain = render(&board(), KST, false);
        assert_eq!(
            plain,
            format!(
                "USAGE  updated 10:00\n\
                 {cal}\
                 {rule}\n\
                 ▌claude Max 5x\n\
                 \x20◆5h  ███░░░░░░░░░░░░░░░░░  86% left  resets 09-24 Thu 11:18 (in 1h18m)\n\
                 \x20●7d  █████████████████░░░  15% left  resets 09-27 Sun 12:00 (in 3d2h)\n\
                 {rule}\n\
                 ▌codex  Pro  as of 08:00\n\
                 \x20◆5h  —\n\
                 \x20●7d  ███████████░░░░░░░░░  45% left  \n\
                 {rule}\n\
                 ▌gemini —\n\
                 \x20 no 5h or 7d windows\n",
                rule = "─".repeat(RULE_WIDTH),
                cal = calendar(&board(), &[], KST, false)
            )
        );
        let coloured = render(&board(), KST, true);
        assert!(coloured.contains(ansi(Level::Red)), "85% is red");
        assert!(coloured.contains(ansi(Level::Orange)), "55% is orange");
        assert!(coloured.contains(ansi(Level::Green)), "14% is green");
        // The same text: colour only adds escapes and draws the bars' cells
        // as a meter's `■`, and the calendar's band pads its lines to its width.
        let trimmed = |t: &str| t.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
        assert_eq!(
            trimmed(&strip_ansi(&coloured)),
            trimmed(&plain.replace(['█', '░'], "■"))
        );
    }

    /// Each AI wears the tower's AGENT-column mark: the bar in its brand
    /// colour, the name in the lifted one, padded so the column after it
    /// does not move. An AI with no brand keeps the bar's cell blank.
    #[test]
    fn an_ai_wears_the_towers_bar_on_the_board() {
        for name in ["claude", "codex", "gemini"] {
            let [bar, lifted] = tower_ui::vendor_rgb(name).unwrap();
            let coloured = vendor_label(name, 8, true);
            assert!(
                coloured.starts_with(&format!("{}▌", fg(bar))),
                "{coloured:?}"
            );
            assert!(
                coloured.contains(&format!("{}{name}", fg(lifted))),
                "{coloured:?}"
            );
            let plain = vendor_label(name, 8, false);
            assert_eq!(plain, format!("▌{name:<7}"));
            assert_eq!(strip_ansi(&coloured), plain);
        }
        assert_eq!(vendor_label("other", 8, false), " other  ");
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for c in it.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn a_reached_limit_says_so_in_red() {
        let mut b = board();
        b.vendors[1].limits.reached = true;
        let plain = render(&b, KST, false);
        assert!(
            plain.contains("▌codex  Pro  LIMIT REACHED  as of 08:00\n"),
            "{plain}"
        );
        let coloured = render(&b, KST, true);
        assert!(coloured.contains(&format!("{}  LIMIT REACHED", ansi(Level::Red))));
        assert_eq!(to_json(&b)["vendors"][1]["reached"], true);
        assert_eq!(to_json(&b)["vendors"][0]["reached"], false);
    }

    #[test]
    fn an_exhausted_week_with_its_guard_off_is_not_shown_as_held() {
        let mut b = board();
        b.guard = crate::usageguard::Guard::uniform(Some(25), None);
        for v in &mut b.vendors[..2] {
            v.limits.seven_day.as_mut().unwrap().used_pct = 100.0;
            v.limits.reached = true;
        }
        let plain = render_editing(&b, KST, false, Some(0)).text;
        assert_eq!(plain.matches("LIMIT REACHED").count(), 2, "{plain}");
        assert_eq!(plain.matches("7d · off").count(), 2, "{plain}");
        assert!(!plain.contains("tower sends held"), "{plain}");
        let summary = summary_line(&b, KST, false);
        assert!(!summary.contains("held"), "{summary}");
    }

    #[test]
    fn each_ai_s_floor_is_marked_on_its_bars_and_a_held_one_says_so() {
        let mut b = board();
        b.guard = crate::usageguard::Guard {
            claude: crate::usageguard::Floors {
                five_hour: Some(50),
                seven_day: Some(20),
            },
            codex: crate::usageguard::Floors {
                five_hour: None,
                seven_day: Some(60),
            },
        };
        let text = render(&b, KST, false);
        assert!(
            text.contains("▌claude Max 5x  floor ◆5h 50% · ●7d 20%\n"),
            "{text}"
        );
        assert!(
            text.contains("▌codex  Pro  as of 08:00  floor ◆5h off · ●7d 60%\n"),
            "{text}"
        );
        // claude's 5h: 14% of 20 cells is 3; its 50% mark sits in cell 10.
        assert!(
            text.contains(" ◆5h  ███░░░░░░░╎░░░░░░░░░░  86% left"),
            "{text}"
        );
        // claude's 7d has 15% left ≤ 20%: held. codex's 7d has 45% left ≤ 60%: held too.
        assert!(
            text.contains("  ⏸ tower sends held — 7d 15% left ≤ 20%, until 09-27 Sun 12:00\n"),
            "{text}"
        );
        assert!(
            text.contains("  ⏸ tower sends held — 7d 45% left ≤ 60%, until the reset\n"),
            "{text}"
        );
        // gemini is not covered: no floor, never held.
        assert!(!text.contains("▌gemini —  floor"), "{text}");
        // The marker is yellow, between the meter's cells.
        let coloured = render(&b, KST, true);
        assert!(coloured.contains(&format!("{YELLOW}╎{}■", fg(EMPTY_CELL))));
    }

    #[test]
    fn the_tab_draws_a_slider_above_each_ai_s_bar() {
        let mut b = board();
        b.guard = crate::usageguard::Guard::uniform(Some(25), None);
        let drawn = render_editing(&b, KST, false, Some(1));
        let lines: Vec<&str> = drawn.text.lines().collect();
        assert!(
            lines.iter().any(|l| l.starts_with("guard  drag ▼")),
            "{}",
            drawn.text
        );
        // One slider per covered AI and shown window: claude 5h, claude 7d,
        // codex 7d (its 5h is not shown). gemini has none.
        let names: Vec<(&str, &str)> = drawn.sliders.iter().map(|s| (s.vendor, s.window)).collect();
        assert_eq!(names, [("claude", "5h"), ("claude", "7d"), ("codex", "7d")]);
        for s in &drawn.sliders {
            let slider_line = lines[usize::from(s.row) - 1];
            let bar_line = lines[usize::from(s.row)];
            assert!(
                slider_line.starts_with("      ─") || slider_line.starts_with("      ▼"),
                "{slider_line}"
            );
            assert!(
                bar_line.starts_with(&format!(" {}  ", window_label(s.window))),
                "right above its bar: {bar_line}"
            );
        }
        // The chosen one is marked, the others are not.
        assert!(lines[usize::from(drawn.sliders[1].row) - 1].ends_with("▸ claude ●7d · off"));
        assert!(lines[usize::from(drawn.sliders[0].row) - 1].ends_with("  claude ◆5h · keep 25%"));
        // Not the tab: no sliders at all.
        assert!(render_editing(&b, KST, false, None).sliders.is_empty());
    }

    /// The marker adds one cell and preserves every used and unused cell.
    #[test]
    fn a_marker_preserves_the_meter() {
        for limit in 0..=100 {
            for used in [0.0, 33.0, 99.0, 100.0] {
                let b = bar_marked(used, BAR_WIDTH, Some(limit));
                assert_eq!(b.chars().count(), BAR_WIDTH + 1);
                assert_eq!(b.matches('╎').count(), 1, "{limit} {used}");
                assert_eq!(b.replace('╎', ""), bar(used, BAR_WIDTH));
            }
        }
        assert_eq!(bar_marked(10.0, 10, None), bar(10.0, 10));
        assert_eq!(
            bar_marked(95.0, 20, Some(95)),
            format!("{}╎░", "█".repeat(19))
        );
        assert_eq!(
            bar_marked(100.0, 20, Some(95)),
            format!("{}╎█", "█".repeat(19))
        );
        assert_eq!(bar_marked(95.0, SUMMARY_BAR, Some(95)), "████░╎");
        assert_eq!(bar_marked(100.0, SUMMARY_BAR, Some(95)), "█████╎");
        assert_eq!(bar_marked(100.0, 0, Some(100)), "╎");
    }

    #[test]
    fn the_terminal_s_bytes_become_keys() {
        assert_eq!(
            parse_keys(b"\x1b[A\x1b[B\x1b[C\x1b[Dkjlh+-=xr\x03"),
            [
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Right,
                Key::Left,
                Key::Right,
                Key::Toggle,
                Key::Refresh,
                Key::Quit
            ]
        );
        // No key leaves the tab: not q, not a bare Esc.
        assert!(parse_keys(b"q").is_empty());
        assert!(parse_keys(b"\x1b").is_empty());
        assert!(parse_keys(b"zZ9 \n").is_empty());
    }

    #[test]
    fn a_refresh_waits_out_the_gap_since_the_last() {
        let gap = std::time::Duration::from_secs(30);
        let t0 = std::time::Instant::now();
        let at = |s: u64| t0 + std::time::Duration::from_secs(s);
        assert_eq!(refresh_wait(None, t0, gap), None, "the first goes");
        assert_eq!(refresh_wait(Some(t0), at(0), gap), Some(30));
        assert_eq!(refresh_wait(Some(t0), at(29), gap), Some(1));
        assert_eq!(refresh_wait(Some(t0), at(30), gap), None);
        assert_eq!(refresh_wait(Some(t0), at(300), gap), None);
    }

    /// Property: every reachable floor is off or a 1% step in 1..=100, and
    /// right then left comes back (except at the top).
    #[test]
    fn a_floor_moves_in_one_percent_steps_and_stays_in_range() {
        let mut floors = vec![None];
        floors.extend((1..=100).map(Some));
        for f in floors {
            for key in [Key::Left, Key::Right, Key::Toggle, Key::Up, Key::Quit] {
                let n = step_floor(f, key, 25);
                assert!(
                    n.is_none_or(|v| (1..=100).contains(&v)),
                    "{f:?} {key:?} → {n:?}"
                );
            }
            if f != Some(100) {
                assert_eq!(
                    step_floor(step_floor(f, Key::Right, 25), Key::Left, 25),
                    f,
                    "{f:?}"
                );
            }
        }
        assert_eq!(step_floor(Some(100), Key::Right, 25), Some(100));
        assert_eq!(step_floor(Some(5), Key::Left, 25), Some(4));
        assert_eq!(step_floor(Some(1), Key::Left, 25), None);
        assert_eq!(step_floor(None, Key::Right, 25), Some(1));
        assert_eq!(step_floor(None, Key::Toggle, 25), Some(25));
        assert_eq!(step_floor(Some(40), Key::Toggle, 25), None);
    }

    /// A slider's handle sits in the bars' column for its floor, and a
    /// click on that column gives the same floor back — for every floor.
    #[test]
    fn a_click_on_the_handle_gives_its_floor_back() {
        let mut b = board();
        for floor in (5..=95).step_by(5) {
            b.guard = crate::usageguard::Guard::uniform(Some(floor), None);
            let drawn = render_editing(&b, KST, false, Some(0));
            let s = &drawn.sliders[0];
            assert_eq!((s.vendor, s.window), ("claude", "5h"));
            let line = drawn.text.lines().nth(usize::from(s.row) - 1).unwrap();
            let cell = line.chars().position(|c| c == '▼').unwrap();
            let col = u16::try_from(cell).unwrap() + 1;
            assert_eq!(floor_at_column(col, BAR_WIDTH), Some(floor), "{line}");
            // The same cell as the mark on the bar below it.
            let bar = drawn.text.lines().nth(usize::from(s.row)).unwrap();
            assert_eq!(bar.chars().position(|c| c == '╎'), Some(cell), "{bar}");
        }
    }

    #[test]
    fn clicks_off_the_bar_hold_to_the_ends() {
        assert_eq!(
            floor_at_column(1, BAR_WIDTH),
            Some(100),
            "left of the bar: most reserve"
        );
        assert_eq!(floor_at_column(BAR_COL, BAR_WIDTH), Some(100));
        assert_eq!(floor_at_column(BAR_COL + 15, BAR_WIDTH), Some(25));
        assert_eq!(floor_at_column(200, BAR_WIDTH), None, "right of it: off");
        assert_eq!(floor_at_column(BAR_COL, 0), None);
        assert_eq!(slider(Some(25), 20), "───────────────▼─────");
        assert_eq!(slider(None, 4), "─────");
    }

    #[test]
    fn mouse_floors_match_an_integer_oracle_and_decrease_to_off() {
        for width in 1..=100usize {
            let mut previous = 100;
            for cell in 0..=width {
                // Round the remaining share using integer division rather
                // than the production floating-point coordinate mapping.
                let expected = (200 * (width - cell) + width) / (2 * width);
                let got = floor_at_column(BAR_COL + cell as u16, width);
                assert_eq!(got, (expected != 0).then_some(expected as u8));
                assert!(got.unwrap_or(0) <= previous);
                previous = got.unwrap_or(0);
            }
        }
        assert_eq!(floor_at_column(u16::MAX, BAR_WIDTH), None);
        for floor in (5..=100).step_by(5) {
            let handle = slider(Some(floor), BAR_WIDTH).find('▼').unwrap();
            // Every preceding track character is three bytes (─).
            let cell = handle / '─'.len_utf8();
            assert_eq!(
                floor_at_column(BAR_COL + cell as u16, BAR_WIDTH),
                Some(floor)
            );
        }
    }

    /// The scale sits in the bars' columns: 0 over the first cell, 50 over
    /// the middle one, and the ends say which way is which.
    #[test]
    fn the_scale_lines_up_with_the_bars() {
        let (ends, ticks) = scale(20);
        assert_eq!(ends, "spend           keep");
        assert_eq!(ticks, "0    25   50   75 100% used");
        assert_eq!(ticks.find("50"), Some(10));
        assert_eq!(ticks.find("75"), Some(15));
    }

    #[test]
    fn mouse_presses_and_drags_are_read_releases_are_not() {
        assert_eq!(
            parse_keys(b"\x1b[<0;22;3M\x1b[<32;20;3M\x1b[<0;20;3m\x1b[<2;5;4M"),
            [
                Key::Mouse { row: 3, col: 22 },
                Key::Mouse { row: 3, col: 20 }
            ]
        );
        // A mouse event and a key in one read.
        assert_eq!(
            parse_keys(b"\x1b[<0;9;4Mx"),
            [Key::Mouse { row: 4, col: 9 }, Key::Toggle]
        );
        // Cut short: nothing is made up.
        assert!(parse_keys(b"\x1b[<0;9").is_empty());
        assert_eq!(
            parse_keys(b"\x1b[<64;9;4M\x1b[<65;9;4M"),
            [Key::WheelUp, Key::WheelDown]
        );
    }

    /// Thursday 09-24 10:00: claude's 5h resets at 11:18 today and its 7d on
    /// Sunday; codex reported no reset time; gemini has none to report.
    #[test]
    fn the_calendar_puts_each_reset_on_its_day() {
        let text = calendar(&board(), &[], KST, false);
        assert_eq!(
            text,
            "        Thu     Fri     Sat     Sun     Mon     Tue     Wed     Thu\n\
             \x20       09-24   09-25   09-26   09-27   09-28   09-29   09-30   10-01\n\
             ▌claude ◆11:18                  ●12:00\n\
             ▌codex\n\
             ▌gemini — no 5h or 7d resets\n"
        );
    }

    #[test]
    fn both_resets_on_one_day_share_its_cell_and_past_ones_are_left_off() {
        let mut b = board();
        let now = b.at;
        // 7d later today, 5h already past: only the 7d one shows.
        b.vendors[0].limits.five_hour = Some(Window {
            used_pct: 10.0,
            resets_at: Some(now - time::Duration::minutes(5)),
        });
        b.vendors[0].limits.seven_day = Some(Window {
            used_pct: 10.0,
            resets_at: Some(now + time::Duration::hours(3)),
        });
        let text = calendar(&b, &[], KST, false);
        assert!(text.contains("▌claude ●13:00\n"), "{text}");
        // Both today.
        b.vendors[0].limits.five_hour = Some(Window {
            used_pct: 10.0,
            resets_at: Some(now + time::Duration::hours(1)),
        });
        let text = calendar(&b, &[], KST, false);
        assert!(text.contains("▌claude ◆●13:00\n"), "{text}");
        // Past the calendar's last day: not on it.
        b.vendors[0].limits.five_hour = None;
        b.vendors[0].limits.seven_day = Some(Window {
            used_pct: 10.0,
            resets_at: Some(now + time::Duration::days(9)),
        });
        assert!(calendar(&b, &[], KST, false).contains("claude\n"));
    }

    /// In colour, every calendar line is one full-width band on its own
    /// background, kept across the colour resets inside it.
    #[test]
    fn the_calendar_sits_on_a_band_of_its_own() {
        let text = calendar(&board(), &[], KST, true);
        let width = DAY_WIDTH * (CALENDAR_DAYS + 1);
        for line in text.lines() {
            assert!(line.starts_with(CALENDAR_BG), "{line:?}");
            assert!(line.ends_with(RESET), "{line:?}");
            assert_eq!(visible_width(line), width, "{line:?}");
            // No reset inside the line leaves the background off.
            let inner = &line[..line.len() - RESET.len()];
            for (at, _) in inner.match_indices(RESET) {
                assert!(
                    inner[at + RESET.len()..].starts_with(CALENDAR_BG),
                    "{line:?}"
                );
            }
        }
        // Without colour, no band and no padding.
        assert!(!calendar(&board(), &[], KST, false).contains('\x1b'));
    }

    /// The day a held AI is released stands out in yellow; other marks do not.
    #[test]
    fn a_held_ai_s_release_day_is_yellow() {
        let b = board();
        let hold = crate::usageguard::Hold {
            vendor: "claude",
            why: String::new(),
            until: b.vendors[0].limits.seven_day.as_ref().unwrap().resets_at,
        };
        let text = calendar(&b, &[hold], KST, true);
        assert!(text.contains(&format!("{YELLOW}●12:00{RESET}")), "{text:?}");
        assert!(text.contains(&format!("{BOLD}◆11:18{RESET}")), "{text:?}");
        // Colour moves no column: the same text, only padded to the band.
        let stripped: Vec<String> = strip_ansi(&text)
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        let plain: Vec<String> = calendar(&b, &[], KST, false)
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(stripped, plain);
    }

    #[test]
    fn the_json_carries_levels_and_times() {
        let j = to_json(&board());
        assert_eq!(j["vendors"][0]["five_hour"]["level"], "green");
        assert_eq!(j["vendors"][0]["seven_day"]["level"], "red");
        assert_eq!(j["vendors"][1]["five_hour"], Value::Null);
        assert_eq!(j["vendors"][1]["seven_day"]["level"], "orange");
        assert_eq!(j["vendors"][0]["plan"], "Max 5x");
        assert_eq!(j["vendors"][2]["note"], "no 5h or 7d windows");
        assert!(j["vendors"][0]["five_hour"]["resets_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-09-24T02:18"));
    }
}
