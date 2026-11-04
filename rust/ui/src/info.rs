// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What the wiring says about an agent: the detail card behind `i`.
//!
//! Plain text, built from values the caller already has, so the same card is
//! printed by `herdr-tower info` without a terminal being involved. The TUI and
//! the command line share this one renderer, so the two cannot drift apart.
//!
//! The coordinate line names herdr and shows only what herdr actually reports.
//! It carries no window number and no pid: a window number would be a base36
//! counter derived from the tab ID — a number the user never sees in herdr —
//! and `pane.list` returns no pid, so either would have to be filled in.
//! Printing a fabricated window and a zero pid as if they were measurements is
//! worse than printing neither.

use std::fmt::Write as _;

use time::OffsetDateTime;
use tower_state::{Agent, AgentState};
use tower_usage::CtxInfo;
use tower_wiring::Info;

use crate::text::{age, shorten_home, span};

/// What the card needs. Assembled by the caller, which is the only place that
/// knows all of it.
pub struct InfoData<'a> {
    pub agent: &'a Agent,
    pub wiring: &'a Info,
    pub ctx: Option<&'a CtxInfo>,
    /// The worktree branch for this folder, when there is one.
    pub branch: Option<&'a str>,
    /// The OS's word for a boot-time unit — macOS starts bots with LaunchAgents,
    /// Linux with systemd user units, and naming the wrong one sends the reader
    /// to a directory that does not exist on their machine.
    ///
    /// Data rather than a `cfg!` here. This crate renders; which machine it is
    /// rendering for is the caller's to know. [`unit_word`] is the one place
    /// that decides it, and the `cfg!` this replaced could not be checked for
    /// the platform the test run is not on — which is how a golden captured on
    /// macOS came to fail on Linux and nowhere else.
    pub unit: &'a str,
}

/// What this machine calls a unit that starts something at login.
pub fn unit_word() -> &'static str {
    unit_word_for(std::env::consts::OS)
}

/// [`unit_word`] for a named platform, so both spellings are assertable from one
/// test run — and so a card can be rendered for a machine other than this one,
/// which is what reproducing a golden captured elsewhere takes.
pub fn unit_word_for(os: &str) -> &'static str {
    if os == "linux" {
        "systemd unit"
    } else {
        "LaunchAgent"
    }
}

fn state_word(s: AgentState) -> &'static str {
    match s {
        AgentState::Working => "WORKING",
        AgentState::Waiting => "WAITING",
        AgentState::DoneUnread => "DONE_UNREAD",
        AgentState::Idle => "IDLE",
        AgentState::Error => "ERROR",
        AgentState::Dead => "DEAD",
    }
}

/// Renders the card.
pub fn render_info(d: &InfoData<'_>, now: OffsetDateTime) -> String {
    let a = d.agent;
    let mut out = String::new();
    let name = if a.pane.session.is_empty() {
        &a.id
    } else {
        &a.pane.session
    };
    let _ = writeln!(out, "{name}  {}", crate::view::state_label(a, now));
    // The name somebody gave the pane with `/rename`. The row leads with the
    // terminal title, which the CLI rewrites every turn, so a label is covered
    // there whenever there is a title — and this card is where it is recovered.
    // Without this line it was recoverable nowhere at all: the card rendered
    // the question and the task and never the name.
    if !a.label.is_empty() {
        let _ = writeln!(out, "  name       {}", a.label);
    }
    let _ = writeln!(out, "  folder     {}", shorten_home(&a.cwd));

    let mut engine = a.kind.clone();
    if let Some(ctx) = d.ctx {
        if !ctx.model.is_empty() {
            engine.push_str(" · ");
            engine.push_str(&ctx.model);
        }
        if let Some(used) = ctx.used_pct {
            let _ = write!(engine, " · ctx {}", used.label());
        }
    }
    let _ = writeln!(out, "  engine     {engine}");
    if let Some(br) = d.branch.filter(|b| !b.is_empty()) {
        let _ = writeln!(out, "  branch     ⎇ {br} (worktree)");
    }
    let _ = writeln!(out, "  herdr      {} {}", a.pane.session, a.pane.pane_id);

    if let Some(bot) = &d.wiring.bot {
        let _ = writeln!(out, "  folder-bot {} (engine {})", bot.name, bot.engine);
    }

    if d.wiring.launch_units.is_empty() {
        let _ = writeln!(out, "  launch     manual (no {})", d.unit);
    } else {
        let _ = writeln!(
            out,
            "  launch     {} {}",
            d.unit,
            d.wiring.launch_units.join(", ")
        );
    }

    if a.short_session_id().is_empty() {
        let _ = writeln!(out, "  session    none yet (filled in when a hook fires)");
    } else {
        let _ = writeln!(
            out,
            "  session    {} → if it dies: herdr-tower resume {}",
            a.short_session_id(),
            a.id
        );
    }
    if !a.ask.is_empty() {
        let _ = writeln!(out, "  asking     {}", a.ask);
    }
    if !a.task.is_empty() {
        let _ = writeln!(out, "  task       {}", a.task);
    }
    let _ = writeln!(
        out,
        "  state      {1} for {0} (updated {2})",
        span(a.state_since, now),
        state_word(a.state),
        age(a.updated_at, now)
    );
    out
}
