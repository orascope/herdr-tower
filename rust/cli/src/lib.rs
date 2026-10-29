// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Rendering for the command line.
//!
//! Rendering only — synchronizing the store with the backend is the caller's
//! job, so these functions can be tested against fixed records and a fixed
//! clock.

// Command implementations and their helpers, one module each. Declared here
// rather than by whoever writes them, so two modules can land in parallel
// without both editing this file.
pub mod agentctl;
pub mod allcmd;
pub mod boardcmd;
pub mod boardnotice;
pub mod boardwatch;
pub mod cbreak;
pub mod codexinit;
pub mod codexsid;
pub mod configcmd;
pub mod daemoncmd;
pub mod decorate;
pub mod dockcmd;
pub mod dockkeep;
pub mod dockplugin;
pub mod doctorcmd;
pub mod geminiinit;
pub mod helpcmd;
pub mod housekeeping;
pub mod idlabels;
pub mod infocmd;
pub mod initcmd;
pub mod loopcmd;
pub mod mcp;
pub mod muxsel;
pub mod opencmd;
pub mod opencodeinit;
pub mod parallax;
pub mod pipe;
pub mod pluginsync;
pub mod reminder;
pub mod reportcmd;
pub mod restorecmd;
pub mod restorepick;
pub mod schedulecmd;
pub mod selfpath;
pub mod skills;
pub mod supervisorlog;
pub mod tree;
pub mod usagecmd;
pub mod usagefetch;
pub mod usageguard;
pub mod usagetab;
pub mod usagetui;
pub mod versioncmd;
pub mod waitcmd;
pub mod wtcmd;

use std::collections::HashMap;
use std::fmt::Write as _;

use tower_state::{Agent, AgentState, Store};
// Padding, cutting and the elapsed-time wording come from one place, so a pad
// and a truncation of the same string cannot disagree. `tower_text`'s module docs
// carry the measured two-table bug that made that worth enforcing.
use time::OffsetDateTime;
/// The status table's cell rule under the name the table has always used.
///
/// Note it is *not* `tower_text::truncate_width`, which takes an explicit tail and
/// is one column looser. Kept while callers still use this name; fold it into
/// `truncate_cell` once they have moved over.
pub use tower_text::truncate_cell as truncate_width;
pub use tower_text::{pad_right, shorten_home, since, truncate_cell};

/// The status label for a row.
///
/// WORKING that has gone quiet too long shows as `[working?]`: the state is what a
/// hook last said, and a lost hook looks exactly like work in progress. The
/// question mark is the honest rendering of "we were told this, a while ago".
pub fn state_label(a: &Agent, now: OffsetDateTime) -> &'static str {
    match a.state {
        AgentState::Working if a.stale(now) => "[working?]",
        AgentState::Working => "[working]",
        AgentState::Waiting => "[wait]",
        AgentState::DoneUnread => "[done]",
        AgentState::Error => "[err]",
        AgentState::Idle => "[idle]",
        AgentState::Dead => "[dead]",
    }
}

/// The store in one line, for herdr's tab bar: `🗼 2 wait · 1 done · 3 working`.
///
/// Only the states that ask for something, most urgent first, and a count of
/// zero is left out rather than shown — the line is read at a glance, and a
/// `0 wait` would be read as a wait. Live agents with nothing to say give
/// `🗼 idle`; no live agents give an empty line, which herdr shows as nothing.
pub fn status_line(agents: &[Agent]) -> String {
    status_line_with(agents, 0)
}

/// [`status_line`] with Parallax Browser's handoffs that wait on the person —
/// `🗼 2 wait · 1 parallax · 3 working`.
///
/// Right after WAIT because it is the same kind of thing — somebody stopped
/// and needs an answer — asked from another program. `parallax` is a count
/// the caller took from a *fresh* snapshot ([`tower_state::parallax_waiting`]):
/// this runs every five seconds from herdr's tab bar and reads files, never
/// Parallax. A handoff with no agent live still prints, since the line is
/// how the person learns that anything is waiting at all.
pub fn status_line_with(agents: &[Agent], parallax: usize) -> String {
    let live: Vec<_> = agents.iter().filter(|a| a.is_live()).collect();
    if live.is_empty() && parallax == 0 {
        return String::new();
    }
    let count = |state: AgentState| live.iter().filter(|a| a.state == state).count();
    let mut counts = vec![(count(AgentState::Waiting), "wait"), (parallax, "parallax")];
    counts.extend(
        [
            (AgentState::Error, "err"),
            (AgentState::DoneUnread, "done"),
            (AgentState::Working, "working"),
        ]
        .into_iter()
        .map(|(state, word)| (count(state), word)),
    );
    let parts: Vec<String> = counts
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| format!("{n} {word}"))
        .collect();
    if parts.is_empty() {
        "🗼 idle".to_string()
    } else {
        format!("🗼 {}", parts.join(" · "))
    }
}

/// The status table's closing section: Parallax Browser's handoffs that wait
/// on the person, or nothing at all when none do — a machine without
/// Parallax sees the table it always saw.
///
/// `waiting` is [`tower_state::parallax_waiting`]'s answer, most urgent first.
/// Parallax's text is another program's, so every cell is cleaned of controls
/// and cut to width like the table's own.
pub fn parallax_section(
    out: &mut impl std::io::Write,
    waiting: &[&tower_state::ParallaxHandoff],
) -> std::io::Result<()> {
    if waiting.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    writeln!(out, "PARALLAX — handoffs waiting on you")?;
    let rows: Vec<[String; 3]> = waiting
        .iter()
        .map(|h| {
            [
                truncate_width(&tower_state::clean_text(&h.workspace), 24),
                h.status_label().to_string(),
                // A redacted handoff shows nothing here: Parallax did not
                // entitle the tower to what it says.
                truncate_width(&tower_state::clean_text(&h.title), TASK_MAX_WIDTH),
            ]
        })
        .collect();
    let w0 = rows
        .iter()
        .map(|r| tower_text::width(&r[0]))
        .max()
        .unwrap_or(0);
    let w1 = rows
        .iter()
        .map(|r| tower_text::width(&r[1]))
        .max()
        .unwrap_or(0);
    for [workspace, status, title] in rows {
        let line = format!(
            "  {}{}{title}",
            pad_right(&workspace, w0 + 2),
            pad_right(&status, w1 + 2)
        );
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}

const TASK_MAX_WIDTH: usize = 40;

/// Renders the store as a table, or as JSON when asked.
///
/// `wired` maps a workspace name to its Discord bot mark, decided per session
/// rather than per folder — a working session in a bot's folder is not the bot.
pub fn status(
    out: &mut impl std::io::Write,
    store: &Store,
    json_out: bool,
    now: OffsetDateTime,
    wired: &HashMap<String, String>,
) -> std::io::Result<()> {
    let agents = store.list()?;

    if json_out {
        let body = serde_json::to_string_pretty(&agents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        return writeln!(out, "{body}");
    }
    if agents.is_empty() {
        return writeln!(
            out,
            "no agents — check that claude/codex/gemini/opencode are running in herdr."
        );
    }

    let mut rows: Vec<Vec<String>> = vec![["STATE", "AGENT", "SESSION", "TASK", "DIR", "SINCE"]
        .iter()
        .map(|s| s.to_string())
        .collect()];
    for a in &agents {
        let mut session = a.pane.session.clone();
        if let Some(mark) = wired.get(&session).filter(|m| !m.is_empty()) {
            session.push(' ');
            session.push_str(mark);
        }
        rows.push(vec![
            state_label(a, now).to_string(),
            a.kind.clone(),
            session,
            // `question_first`: this table is read in a terminal and over ssh,
            // with no detail view behind it and no key to press, so a pending
            // question takes the cell when there is one.
            // Cleaned before the cut: a tab becomes a space, which has a width
            // the budget must count.
            truncate_width(&tower_state::clean_text(a.question_first()), TASK_MAX_WIDTH),
            shorten_home(&a.cwd),
            since(a.state_since, now),
        ]);
    }
    // Printed straight to a terminal, often over ssh, so no cell may carry a
    // control character: an ESC or OSC sequence in a question could retitle the
    // window, write the clipboard (OSC 52) or move the cursor to fake a row.
    // The hooks already clean the task and the question as they store them;
    // this is for every other cell, and for records written before they did.
    // Before measuring, too — the width table counts a control as zero columns,
    // and a cell that shrinks after padding would misalign the table.
    for cell in rows.iter_mut().flatten() {
        if let std::borrow::Cow::Owned(clean) = tower_state::clean_text(cell) {
            *cell = clean;
        }
    }

    let mut widths = vec![0usize; rows[0].len()];
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(tower_text::width(c));
        }
    }
    for r in &rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i == r.len() - 1 {
                line.push_str(c);
                break;
            }
            let _ = write!(line, "{}", pad_right(c, widths[i] + 2));
        }
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}
