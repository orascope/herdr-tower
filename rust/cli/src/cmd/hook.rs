// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Hook and transition notification command handlers.

use time::OffsetDateTime;

use super::common::*;
use super::Result;

/// `herdr-tower hook <agent> [--event <event>] [args…]`
///
/// **This never reports failure.** It runs as a child of the agent's own CLI,
/// where a non-zero exit is the agent's problem: claude and gemini read the exit
/// code, and a hook that fails loudly can stop the turn it was only meant to
/// observe. So every error goes to stderr and the command still succeeds.
pub fn run_hook(args: &[String]) -> Result<()> {
    let Some(agent) = args.first() else {
        return Ok(());
    };
    let rest = &args[1..];
    let event = flag_value(rest, "--event").unwrap_or_default();

    let store = match store() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("herdr-tower hook: {e}");
            return Ok(());
        }
    };
    let cfg = outbound_config();
    let now = OffsetDateTime::now_utc();

    let transition = match agent.as_str() {
        "claude" => {
            tower_hook::claude::run(&store, &event, std::io::stdin(), tower_hook::os_env, now)
        }
        "codex" => {
            // Two different hook systems reach this one command. With `--event`
            // it is codex's own hooks.json, which sends JSON on stdin and
            // reports WORKING and WAITING as they happen. Without it, it is the
            // notify hook, which passes JSON in argv and only ever says a turn
            // finished — enough on its own to leave a working codex showing DONE
            // for the whole time it worked.
            if event.is_empty() {
                tower_hook::codex::notify(&store, rest, tower_hook::os_env, now)
            } else {
                tower_hook::codex::run(&store, &event, std::io::stdin(), tower_hook::os_env, now)
            }
        }
        "gemini" => {
            let r =
                tower_hook::gemini::run(&store, &event, std::io::stdin(), tower_hook::os_env, now);
            // agy's hook contract: a JSON object on stdout, empty meaning "do
            // not intervene". Printed whatever happened above — a hook that says
            // nothing is read as a hook that made a decision. A closed stdout is
            // ignored: the hook's own work is done, and it must not fail the agent.
            let _ = tower_cli::outln!("{{}}");
            r
        }
        // Sent by the plugin `init` installs, which has already reduced
        // opencode's events to the handful that move a row.
        "opencode" => {
            tower_hook::opencode::run(&store, &event, std::io::stdin(), tower_hook::os_env, now)
        }
        _ => Ok(None),
    };

    let transition = match transition {
        Ok(t) => t,
        Err(e) => {
            eprintln!("herdr-tower hook: {e}");
            None
        }
    };

    let moved = transition.as_ref().is_some_and(|t| t.moved());
    if let Some(t) = &transition {
        // Handed to another process, not delivered here. This runs inside the
        // agent's own turn, and delivery is two things the hook cannot bound:
        // `osascript`/`notify-send`, which have no timeout at all, and a webhook
        // POST whose own five-second budget is five seconds of an agent waiting.
        // A notification is best-effort by nature; nothing here reads its result.
        spawn_detached(&[
            "notify-transition",
            "--agent",
            &t.agent.id,
            "--from",
            &tower_cli::infocmd::state_word(t.prev),
            "--to",
            &tower_cli::infocmd::state_word(t.to()),
        ]);
    }

    // Worth doing and not worth waiting for.
    if moved && cfg.discord_webhook_url.is_some() {
        spawn_detached(&["card", "--event"]);
    }
    Ok(())
}

/// `herdr-tower notify-transition --agent <id> --from <state> --to <state>`
///
/// The delivery half of a hook's notification, in a process of its own. Not in
/// `help`: nothing types this, the hook spawns it — see the call site for why it
/// must not run inline.
///
/// The record is re-read here rather than passed along, so the title and body
/// come from the same place the tower's do. The states are passed because they
/// are what the *transition* was: by the time this runs, the record may have
/// moved on, and a notification describing the newer state would be a lie about
/// which event fired.
pub fn run_notify(args: &[String]) -> Result<()> {
    let fs = tower_cli::helpcmd::FlagSet::new("herdr-tower notify-transition")
        .string("agent", "", "record id")
        .string("from", "", "previous state")
        .string("to", "", "new state");
    let p = fs.parse(args)?;
    if !p.args().is_empty() {
        return Err("notify-transition takes no positional arguments".into());
    }
    let (Some(prev), Some(to)) = (parse_state(p.str("from")), parse_state(p.str("to"))) else {
        return Err("--from and --to must be state names".into());
    };
    let a = store()?.load(p.str("agent"))?;
    tower_notify::notify(&outbound_config(), &notify_sender(), &a, prev, to);
    Ok(())
}
