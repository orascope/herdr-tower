// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Closing an agent, interrupting one, and listing herdr's workspaces — the
//! other halves of `open`, shared by the command line (`close`, `interrupt`,
//! `workspaces`) and the remote MCP server (`close_agent`, `interrupt_agent`,
//! `list_workspaces`).
//!
//! The rules live here and the herdr calls are thin, so both callers refuse
//! the same things for the same reasons.

use herdrx::{Herdr, Workspace};
use serde_json::{json, Value};
use tower_state::{Agent, AgentState};

/// Why an agent must not be closed, or `None` when it may be.
///
/// `live` is herdr's word for what it is doing now: the store can be a pass
/// behind, and closing an agent the store still calls idle while it has just
/// started a turn throws that turn away. A busy agent — working, or waiting on
/// the user — is closed only with `force`.
pub fn close_refusal(
    a: &Agent,
    live: Option<AgentState>,
    self_pane: &str,
    force: bool,
) -> Option<String> {
    if !a.is_live() {
        return Some(format!("{} is DEAD — its pane is already gone", a.id));
    }
    if !self_pane.is_empty() && a.pane_id() == self_pane {
        return Some(format!(
            "{} is the session asking — it cannot close itself",
            a.id
        ));
    }
    let busy = match live.unwrap_or(a.state) {
        AgentState::Working => Some("working"),
        AgentState::Waiting => Some("waiting on the user"),
        _ => None,
    };
    match busy {
        Some(word) if !force => Some(format!(
            "{} is {word}; closing it would throw that away. Interrupt it first, or force it",
            a.id
        )),
        _ => None,
    }
}

/// Closes the agent's pane, and with it the agent.
pub fn close(h: &Herdr, a: &Agent) -> Result<(), String> {
    h.close_pane(a.pane_id())
        .map_err(|e| format!("herdr could not close {}: {e}", a.pane_id()))
}

/// Why an agent cannot be interrupted, or `None` when it can.
///
/// Only a working agent: Escape at an idle prompt clears whatever the user
/// had typed there, and at a dialog it answers the dialog — that is
/// `answer_prompt`'s job, with its screen check.
pub fn interrupt_refusal(a: &Agent, live: Option<AgentState>) -> Option<String> {
    if !a.is_live() {
        return Some(format!("{} is DEAD — its pane is gone", a.id));
    }
    match live {
        Some(AgentState::Working) => None,
        Some(AgentState::Waiting) => Some(format!(
            "{} is waiting on a dialog, not working — answer it instead",
            a.id
        )),
        Some(s) => Some(format!(
            "{} is not working (it is {}), so there is nothing to interrupt",
            a.id,
            crate::infocmd::state_word(s)
        )),
        None => Some(format!(
            "herdr cannot say what {} is doing, so nothing was pressed",
            a.id
        )),
    }
}

/// The key that interrupts a turn. Escape, for every supervised agent.
pub const INTERRUPT_KEY: &str = "esc";

/// Presses the interrupt key in the agent's pane.
pub fn interrupt(h: &Herdr, a: &Agent) -> Result<(), String> {
    h.send_keys(a.pane_id(), &[INTERRUPT_KEY])
        .map_err(|e| format!("the key could not be sent to {}: {e}", a.pane_id()))
}

/// What herdr says the agent in this pane is doing, in the tower's words.
pub fn live_state(h: &Herdr, a: &Agent) -> Option<AgentState> {
    let pane = h.pane(a.pane_id()).ok()?;
    if !pane.has_agent() {
        return None;
    }
    Some(match pane.agent_status {
        herdrx::AgentStatus::Idle => AgentState::Idle,
        herdrx::AgentStatus::Working => AgentState::Working,
        herdrx::AgentStatus::Blocked => AgentState::Waiting,
        herdrx::AgentStatus::Done => AgentState::DoneUnread,
        herdrx::AgentStatus::Unknown => return None,
    })
}

/// herdr's workspaces, each with how many live agents the tower sees in it —
/// including the ones with none, which `list_agents` cannot show. `backend`
/// is the server the workspaces came from: workspace ids are per server, and
/// another server's `w1` is not this one's.
pub fn workspaces_json(list: &[Workspace], agents: &[Agent], backend: &str) -> Value {
    let rows: Vec<Value> = list
        .iter()
        .map(|w| {
            let n = agents
                .iter()
                .filter(|a| a.is_live() && a.pane.backend == backend)
                .filter(|a| {
                    if a.pane.workspace_id.is_empty() {
                        a.pane.session == w.label
                    } else {
                        a.pane.workspace_id == w.workspace_id
                    }
                })
                .count();
            json!({
                "label": w.label,
                "id": w.workspace_id,
                "focused": w.focused,
                "tabs": w.tab_count,
                "panes": w.pane_count,
                "agents": n,
            })
        })
        .collect();
    json!({ "workspaces": rows })
}

/// `herdr-tower workspaces`, as a table.
pub fn workspaces_table(v: &Value) -> String {
    let rows = v["workspaces"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no workspaces\n".into();
    }
    let width = rows
        .iter()
        .map(|r| tower_text::width(r["label"].as_str().unwrap_or("")))
        .max()
        .unwrap_or(0)
        .max("WORKSPACE".len());
    let mut out = format!(
        "  {}  {:>4}  {:>5}  {:>6}\n",
        tower_text::pad_right("WORKSPACE", width),
        "TABS",
        "PANES",
        "AGENTS"
    );
    for r in rows {
        let mark = if r["focused"].as_bool() == Some(true) {
            "▸ "
        } else {
            "  "
        };
        out.push_str(&format!(
            "{mark}{}  {:>4}  {:>5}  {:>6}\n",
            tower_text::pad_right(r["label"].as_str().unwrap_or(""), width),
            // As numbers: a `Value` prints itself and ignores the width.
            r["tabs"].as_u64().unwrap_or(0),
            r["panes"].as_u64().unwrap_or(0),
            r["agents"].as_u64().unwrap_or(0)
        ));
    }
    out
}

/// One named agent and the flags around it: `<agent> [flags]` or
/// `[flags] <agent>`, and exactly one agent.
pub fn one_target(
    fs: &crate::helpcmd::FlagSet,
    args: &[String],
    usage: &str,
) -> Result<(String, crate::helpcmd::Parsed), String> {
    one_positional(fs, args, usage, "an agent", "name one agent")
}

/// One positional argument and the flags around it, before or after it.
/// `missing` names what is required ("an agent"); `one` is what a second
/// positional is told ("name one agent").
pub fn one_positional(
    fs: &crate::helpcmd::FlagSet,
    args: &[String],
    usage: &str,
    missing: &str,
    one: &str,
) -> Result<(String, crate::helpcmd::Parsed), String> {
    let (lifted, rest) = match args.split_first() {
        Some((first, tail)) if !first.starts_with('-') => (Some(first.clone()), tail),
        _ => (None, args),
    };
    let p = fs.parse(rest).map_err(|e| fs.diagnostic(&e))?;
    let mut positional: Vec<String> = lifted.into_iter().collect();
    positional.extend(p.args().iter().cloned());
    match positional.as_slice() {
        [] => Err(format!("{missing} is required\n{usage}")),
        [it] => Ok((it.clone(), p)),
        [_, extra, ..] => Err(format!("unknown argument: {extra:?} — {one}\n{usage}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn agent(id: &str, state: AgentState) -> Agent {
        let mut a = Agent::new(id, "claude", OffsetDateTime::UNIX_EPOCH);
        a.state = state;
        a.pane.pane_id = id.split_once('-').unwrap().1.to_string();
        a
    }

    #[test]
    fn a_busy_agent_is_closed_only_when_forced() {
        let idle = agent("claude-w1:p1", AgentState::Idle);
        assert_eq!(
            close_refusal(&idle, Some(AgentState::Idle), "", false),
            None
        );
        // herdr's word wins over the store's.
        let r = close_refusal(&idle, Some(AgentState::Working), "", false).unwrap();
        assert!(r.contains("working"), "{r}");
        assert_eq!(
            close_refusal(&idle, Some(AgentState::Working), "", true),
            None
        );
        let r = close_refusal(&idle, Some(AgentState::Waiting), "", false).unwrap();
        assert!(r.contains("waiting on the user"), "{r}");
        // No word from herdr: the store's state decides.
        let working = agent("claude-w1:p2", AgentState::Working);
        assert!(close_refusal(&working, None, "", false).is_some());
    }

    #[test]
    fn the_dead_and_the_caller_itself_are_never_closed() {
        let dead = agent("claude-w1:p1", AgentState::Dead);
        assert!(close_refusal(&dead, None, "", true)
            .unwrap()
            .contains("DEAD"));
        let me = agent("claude-w1:p9", AgentState::Idle);
        let r = close_refusal(&me, Some(AgentState::Idle), "w1:p9", true).unwrap();
        assert!(r.contains("cannot close itself"), "{r}");
        assert_eq!(
            close_refusal(&me, Some(AgentState::Idle), "w1:p1", false),
            None
        );
    }

    #[test]
    fn only_a_working_agent_is_interrupted() {
        let a = agent("claude-w1:p1", AgentState::Working);
        assert_eq!(interrupt_refusal(&a, Some(AgentState::Working)), None);
        for (live, word) in [
            (Some(AgentState::Idle), "not working"),
            (Some(AgentState::DoneUnread), "not working"),
            (Some(AgentState::Waiting), "answer it"),
            (None, "cannot say"),
        ] {
            let r = interrupt_refusal(&a, live).unwrap();
            assert!(r.contains(word), "{live:?}: {r}");
        }
        let dead = agent("claude-w1:p2", AgentState::Dead);
        assert!(interrupt_refusal(&dead, Some(AgentState::Working))
            .unwrap()
            .contains("DEAD"));
    }

    #[test]
    fn exactly_one_agent_is_named_before_or_after_the_flags() {
        let fs = crate::helpcmd::FlagSet::new("close").bool("force", false, "f");
        let w = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (a, p) = one_target(&fs, &w(&["api", "--force"]), "u").unwrap();
        assert_eq!((a.as_str(), p.bool("force")), ("api", true));
        let (a, p) = one_target(&fs, &w(&["--force", "api"]), "u").unwrap();
        assert_eq!((a.as_str(), p.bool("force")), ("api", true));
        assert!(one_target(&fs, &[], "u")
            .unwrap_err()
            .contains("an agent is required"));
        assert!(one_target(&fs, &w(&["api", "web"]), "u")
            .unwrap_err()
            .contains("\"web\""));
    }

    fn ws(id: &str, label: &str, focused: bool) -> Workspace {
        serde_json::from_value(json!({
            "workspace_id": id, "label": label, "focused": focused,
            "tab_count": 2, "pane_count": 3
        }))
        .unwrap()
    }

    #[test]
    fn workspaces_count_live_agents_and_keep_the_empty_ones() {
        let mut a1 = agent("claude-w1:p1", AgentState::Idle);
        a1.pane.workspace_id = "w1".into();
        let mut a2 = agent("codex-w1:p2", AgentState::Working);
        a2.pane.workspace_id = "w1".into();
        let mut gone = agent("claude-w1:p3", AgentState::Dead);
        gone.pane.workspace_id = "w1".into();
        // An old record with no workspace id falls back to the label.
        let mut old = agent("claude-w2:p1", AgentState::Idle);
        old.pane.session = "web".into();
        // Another herdr server's w1: not this server's workspace.
        let mut elsewhere = agent("claude-w1:p1@other", AgentState::Idle);
        elsewhere.pane.workspace_id = "w1".into();
        elsewhere.pane.backend = "other".into();
        let v = workspaces_json(
            &[
                ws("w1", "api", true),
                ws("w2", "web", false),
                ws("w3", "empty", false),
            ],
            &[a1, a2, gone, old, elsewhere],
            "",
        );
        let rows = v["workspaces"].as_array().unwrap();
        let counts: Vec<(&str, u64)> = rows
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["agents"].as_u64().unwrap()))
            .collect();
        assert_eq!(counts, [("api", 2), ("web", 1), ("empty", 0)]);
        let table = workspaces_table(&v);
        assert!(
            table.lines().nth(1).unwrap().starts_with("▸ api"),
            "{table}"
        );
        assert!(table.contains("empty"), "{table}");
        let widths: std::collections::HashSet<usize> =
            table.lines().map(tower_text::width).collect();
        assert_eq!(widths.len(), 1, "every row lines up:\n{table}");
        assert_eq!(
            workspaces_table(&json!({"workspaces": []})),
            "no workspaces\n"
        );
    }
}
