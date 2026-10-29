// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower` — the control tower command line.
//!
//! This file is the wiring, and only the wiring: it reaches the real
//! filesystem, the real process environment, the real herdr socket and the real
//! network, so that every crate under it can be tested without any of them.
//! Anything here worth a test belongs in a crate instead — which is why the
//! tests at the bottom cover only the few things that have nowhere else to live.

use std::process::ExitCode;

mod cmd;
use cmd::*;

fn main() -> ExitCode {
    // First, before any thread: on Windows `~` is `%USERPROFILE%`.
    tower_os::ensure_home();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Whoever read stdout has gone (`… | head -1`): an ordinary end,
            // said the way a shell says it, and nothing on stderr. Only when
            // stdout itself hung up — an EPIPE from a socket is a real failure.
            if tower_cli::pipe::is_broken_pipe(&e) && tower_cli::pipe::stdout_gone() {
                return ExitCode::from(tower_cli::pipe::EXIT_READER_GONE);
            }
            eprintln!("herdr-tower: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatches one invocation.
///
/// The backend is **not** reached here, and that is load-bearing rather than
/// tidiness. Selecting it at the top would make every command pay the probe
/// that comes with it (`ping`, `pane.list`, `workspace.list`) — including
/// `hook`, which runs inside the agent's own process and must never make it
/// wait. A blocked or missing server would be charged straight to the agent's
/// turn. Each command that needs the backend reaches for it itself.
fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None => run_tui(),
        Some("hook") => run_hook(&args[1..]),
        Some("status") => run_status(&args[1..]),
        Some("init") => run_init(&args[1..]),
        Some("card") => run_card(&args[1..]),
        Some("resume") => run_resume(&args[1..]),
        Some("restore") => run_restore(&args[1..]),
        Some("info") => run_info(&args[1..]),
        Some("open") => run_open(&args[1..]),
        Some("close") => run_close(&args[1..]),
        Some("interrupt") => run_interrupt(&args[1..]),
        Some("workspaces") => run_workspaces(&args[1..]),
        Some("dock") => run_dock(&args[1..]),
        Some("daemon") => run_daemon(&args[1..]),
        Some("doctor") => run_doctor(&args[1..]),
        Some("report") => run_report(&args[1..]),
        Some("loop") => run_loop(&args[1..]),
        Some("board") => run_board(&args[1..]),
        Some("tree") => run_tree(&args[1..]),
        Some("mcp") => run_mcp(&args[1..]),
        Some("config") => run_config(&args[1..]),
        Some("usage") => run_usage(&args[1..]),
        Some("host") => run_host(&args[1..]),
        Some("schedule") => run_schedule(&args[1..]),
        // Not in `help`, and deliberately: it exists so the hook can hand a
        // notification to another process instead of delivering it itself. See
        // `run_notify`.
        Some("notify-transition") => run_notify(&args[1..]),
        Some(c @ ("wake-all" | "close-all" | "broadcast")) => run_all(c, &args[1..]),
        Some("wt") => run_wt(&args[1..]),
        Some("version") | Some("--version") | Some("-v") => {
            tower_cli::outln!(
                "{}",
                tower_cli::versioncmd::format_version(
                    &tower_cli::versioncmd::VersionInfo::stamped()
                )
            )?;
            Ok(())
        }
        Some("help") | Some("-h") | Some("--help") => {
            tower_cli::out!("{}", tower_cli::helpcmd::help_text())?;
            Ok(())
        }
        Some(other) => {
            Err(format!("unknown command: {other}\n'herdr-tower help' lists them all").into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The refusal is all a mistyped command gets, so it has to carry the word
    /// back and say where the list is. It is also the only arm of the
    /// dispatcher a unit test may take: every other one reaches the store, the
    /// herdr socket, the terminal or the network, and `None` opens the TUI.
    ///
    /// Nothing is trimmed or lower-cased on the way in — `Status` is not
    /// `status`, and a stray flag before the command is not a command — because
    /// guessing at a near miss would run something the user did not ask for.
    #[test]
    fn an_unknown_command_is_refused_and_says_where_the_list_is() {
        for word in [
            "nope", "", "Status", "status ", " status", "-x", "--json", "카드",
        ] {
            let e = run(&[word.to_string()]).expect_err("this is not a command");
            assert_eq!(
                e.to_string(),
                format!("unknown command: {word}\n'herdr-tower help' lists them all"),
            );
        }
        // Only the first word decides, and only the first word is quoted back.
        let args = ["nope", "--json", "extra"].map(String::from);
        assert_eq!(
            run(&args).expect_err("still not a command").to_string(),
            "unknown command: nope\n'herdr-tower help' lists them all"
        );
    }

    /// Property: whatever the word is, it reaches the user unchanged — not
    /// truncated, not escaped, not cut at the first space. A command line is
    /// whatever a shell, a LaunchAgent plist or another agent handed over, and a
    /// message naming a different word than the one typed sends the reader
    /// hunting for a typo that is not there.
    ///
    /// Every generated word starts with `zz`, which no command does: a word that
    /// collided with a real one would *run* it.
    #[test]
    fn an_unrecognised_word_comes_back_verbatim_however_it_is_spelled() {
        let mut seed: u64 = 20_260_922;
        let mut next = |bound: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % bound
        };
        // Spaces, quotes, a newline and non-ASCII: all of it has to pass
        // through the message as typed.
        let alphabet: Vec<char> = "ab-_= \t\n\"'{}§승↩🚨".chars().collect();
        for _ in 0..500 {
            let len = next(12);
            let mut word = String::from("zz");
            for _ in 0..len {
                word.push(alphabet[next(alphabet.len())]);
            }
            let e = run(&[word.clone()]).expect_err("no command starts with zz");
            assert_eq!(
                e.to_string(),
                format!("unknown command: {word}\n'herdr-tower help' lists them all"),
                "the word came back changed: {word:?}"
            );
        }
    }

    /// Cross validation between the two halves that have to agree and cannot see
    /// each other: the command list a user reads in `help_text()` (a hand-
    /// written constant in the library) and the commands this dispatcher
    /// actually routes. `expected/help.txt` pins the text against itself, so a
    /// command added here and forgotten there passes the golden — and a help
    /// line left behind by a removed command passes it too.
    ///
    /// The second reading is the dispatcher's own source: the quoted words in
    /// the arms of `run`, read as text, which is independent of what `run` does
    /// when it is called. Calling it is not an option — each of those arms is a
    /// command that would run.
    #[test]
    fn the_help_text_lists_exactly_the_commands_that_are_routed() {
        // The arms of `run`, from its signature to the `}` that closes it at
        // column 0 — so this module's own strings are out of scope.
        let source = include_str!("main.rs");
        let body = source
            .split_once("fn run(args: &[String]) -> Result<()> {")
            .expect("the dispatcher is still called `run`")
            .1
            .split_once("\n}\n")
            .expect("and it still ends at column 0")
            .0;
        // Naive scan: a command is a quoted word with no whitespace and no
        // braces, which leaves out the `{}` of `println!` and the refusal
        // message. A leading `-` marks an alias (`-v`, `--help`), which the help
        // text carries in parentheses rather than as a list entry.
        let mut routed: BTreeSet<&str> = BTreeSet::new();
        let mut rest = body;
        while let Some(open) = rest.find('"') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('"') else { break };
            let (literal, after) = (&rest[..close], &rest[close + 1..]);
            rest = after;
            if !literal.is_empty()
                && !literal.starts_with('-')
                && !literal.contains(|c: char| c.is_whitespace() || c == '{' || c == '}')
            {
                routed.insert(literal);
            }
        }
        assert!(routed.len() > 10, "the scan found only {routed:?}");

        // The `Commands:` block, read the same naive way: the first word of
        // every indented line under it.
        let help = tower_cli::helpcmd::help_text();
        let listed: BTreeSet<&str> = help
            .split_once("Commands:\n")
            .expect("the help text still has a command list")
            .1
            .lines()
            .take_while(|line| line.starts_with("  "))
            .filter_map(|line| line.split_whitespace().next())
            .collect();

        // Two commands are routed on purpose without being offered: `hook` runs
        // inside an agent's own process, and `notify-transition` exists only so
        // the hook can hand a notification to another process. Both are wiring
        // between our own processes, not something a user types.
        const UNLISTED: [&str; 2] = ["hook", "notify-transition"];
        for command in &routed {
            assert!(
                listed.contains(command) || UNLISTED.contains(command),
                "`{command}` is routed but `herdr-tower help` never mentions it"
            );
        }
        for command in &listed {
            assert!(
                routed.contains(command),
                "`herdr-tower help` offers `{command}` but nothing routes it"
            );
        }
    }
}
