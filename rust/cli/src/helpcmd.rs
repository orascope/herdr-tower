// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower help`, and the flag machinery every other command's help hangs
//! off.
//!
//! The body is a frozen artifact, not a description: `expected/help.txt` holds
//! the recorded bytes, and `tests/helpcmd_golden.rs` compares against them.
//!
//! The banner names herdr and nothing else. Naming a terminal emulator on the
//! first line of `help` told people the tool would not work in theirs. What it
//! actually requires is herdr, so that is what the line says.

/// A command-line flag parser, reduced to the part these commands need.
///
/// It lives under `helpcmd` because its visible product is help: `Usage of
/// restore:` and the indented list of defaults beneath it are frozen in
/// `expected/restore-help.txt` exactly like the help body above. Three
/// commands parse flags and all three print that block, so the bytes are
/// written once.
///
/// Declared with an explicit path because sibling files are the only place
/// this crate may grow — `lib.rs` is shared with parallel work and must not
/// be edited to add a helper module.
#[path = "helpcmd_flags.rs"]
pub mod flags;

pub use flags::{format_duration, parse_duration, FlagError, FlagSet, Kind, Parsed};

/// `herdr-tower help`, also `-h` / `--help`.
pub fn help_text() -> String {
    format!("herdr-tower —{HELP_BODY}")
}

/// Everything after the banner. The snapshot in `expected/help.txt` pins both
/// halves.
///
/// The command list has to stay in step with what `main` actually routes. The
/// routing lives in a binary the library cannot see, so the snapshot
/// comparison is what holds the list still — a command added to `main` and
/// forgotten here shows up as a help text that never mentions it.
const HELP_BODY: &str = r#" the multi-agent control tower for herdr

Usage: herdr-tower [command] [flags]
  With no arguments it opens the TUI (inside herdr).

Commands:
  status         list sessions and their state  [--json | --line]
  report         what each session did in a window: turns, work, time waiting on you, errors, drive verdicts  [--since 12h] [--json]
  info <name|id> one session in detail (model, ctx, hook wiring, …)
  dock           run the tower in its own herdr workspace, opened with a click  [--focus | --install-startup]
  card           post the Discord dashboard card  [--out print only] [--event transition-triggered]
  init           hooks, the herdr key, skills — idempotent  [--dry-run]
  daemon         resident supervisor  (one process, multiple Herdr sockets)
  doctor         check that every hook, plugin and service still points at a real binary
  open <folder>  a new herdr tab in a folder with an agent started in it  [--agent claude] [--workspace label] [--prompt text] [--json]
  close <agent>  close an agent's pane — asks first; refuses a busy one unless --force  [--force] [--yes]
  interrupt <agent>  stop a working agent's turn (presses Escape)
  workspaces     herdr's workspaces, with how many agents each holds — the empty ones too  [--json]
  resume [id]    reopen a dead session's conversation in a new window (emergency recovery)
  restore        revive dead sessions in bulk — pick from a checklist  [--resume the conversation too] [--yes all] [--dry-run] [id ...]  (bots started at login are left out)
  loop <command> continue one opted-in session after 10m idle  (start·stop·list·done·blocked; 168h lifetime, unlimited sends)
  board <command>  what an advisor handed to which worker — a wt task or a pane — with each one's progress  (add·send·assign·next·prio·move·link·wake·done·drop·note·rm·prune·clear·show·open)
  tree [link|unlink]  who works under whom in the session list; link a pane under a coordinator (moved into its workspace when it is in another)
  schedule       send a message at a set time, once or every day — waits for a busy agent  (list·add·close·wake·rm; `schedule close 19:00`)
  wake-all       tell every session to carry on  [--yes] [--except name,..] [--watch] [--timeout 10m]
  close-all      tell every session to wrap up  (same flags as wake-all)
  broadcast <message>  send any message to every session
  wt <command>   worktree workers (new·list·diff·test·review·send·wait·merge·clean) — 'herdr-tower wt' for detail
  usage          plan limits: claude and codex, 5h and 7d windows used and when each resets  [--json] [--watch]
  host           this machine's processor counters, memory and system, which a tower on another machine reads over ssh
  config [get|set|unset|keys|path]  read or change config.json  (`config keys` lists every setting; secrets hidden unless --reveal)
  mcp <command>  a remote MCP server, to read and drive the tower from a phone's Claude app  (serve·pair·grants·revoke·on·off)
  version        version information (-v/--version)
  help           this help (-h/--help)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command `main` routes has to be findable in the help.
    ///
    /// A user who cannot see a command in `help` does not know it exists, so
    /// this list is the contract rather than a sample of it.
    #[test]
    fn help_lists_every_routed_command() {
        let out = help_text();
        for cmd in [
            "status",
            "report",
            "info",
            "dock",
            "card",
            "init",
            "doctor",
            "open",
            "close",
            "interrupt",
            "workspaces",
            "resume",
            "restore",
            "loop",
            "board",
            "schedule",
            "tree",
            "wake-all",
            "close-all",
            "broadcast",
            "wt",
            "host",
            "version",
            "help",
        ] {
            assert!(out.contains(cmd), "help never mentions {cmd:?}");
        }
    }

    /// Running with no arguments opens the TUI, which is not discoverable from
    /// anywhere else.
    #[test]
    fn help_says_that_bare_invocation_opens_the_tui() {
        assert!(help_text().contains("TUI"));
    }

    /// The flags people reach for daily are readable off `help` alone, without
    /// a second `--help` per command.
    #[test]
    fn help_names_the_flags_people_use_daily() {
        let out = help_text();
        for flag in ["--json", "--resume", "--dry-run", "--yes"] {
            assert!(out.contains(flag), "help never mentions {flag:?}");
        }
    }

    /// The banner names herdr, the one thing actually required.
    #[test]
    fn the_banner_names_what_is_actually_required() {
        let h = help_text();
        assert!(h.starts_with("herdr-tower — "), "got {:?}", &h[..40]);
        assert!(
            h.lines().next().unwrap().ends_with("for herdr"),
            "got {:?}",
            h.lines().next()
        );
        // Naming a terminal emulator here told people with another one that
        // the tool was not for them.
        assert!(
            !h.lines().next().unwrap().contains("iTerm2"),
            "the banner must not name a terminal emulator"
        );
    }
}
