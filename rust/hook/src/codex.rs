// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! codex hooks, over the two paths codex offers.

use std::io::Read;

use serde::Deserialize;
use time::OffsetDateTime;
use tower_state::{valid_session_id, Agent, AgentState, Store};

use crate::{commit, commit_new_turn, hook_backend, hook_pane, payload, record, touch, Transition};

/// The fields we use out of the JSON `codex notify` passes as its last argv.
#[derive(Debug, Default, Deserialize)]
struct NotifyPayload {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    cwd: String,
}

/// The record a codex hook belongs to: `(backend, pane)`, or `None` to record
/// nothing.
///
/// The environment's pane is not taken on trust for codex. codex 0.157 runs
/// its sessions' hooks through a shared app-server daemon, and that daemon
/// keeps the environment of whatever started it — once, the tower's own dock
/// pane, when the usage board asked `codex app-server` for the plan limits.
/// Every codex session's hooks then arrived with `HERDR_PANE_ID` naming that
/// long-closed pane: they were written into a DEAD record, its session ID
/// flipping between the live sessions, while the live rows heard nothing
/// (#45). So, in order:
///
/// 1. a live codex record that already holds this session's ID is the one;
/// 2. the environment's pane, when its record is live and in the payload's
///    folder (a fresh pane with no record yet counts, unless step 3 finds the
///    session elsewhere);
/// 3. the one live codex record in the payload's folder that has no session
///    ID yet — two or more cannot be told apart, and nothing is written;
/// 4. never a DEAD record.
fn route(
    store: &Store,
    backend: &str,
    env_pane: &str,
    session_id: &str,
    cwd: &str,
) -> Option<(String, String)> {
    let live_codex = |a: &Agent| a.kind == "codex" && a.state != AgentState::Dead;
    let all = || store.list().unwrap_or_default();
    let mut agents: Option<Vec<Agent>> = None;
    if valid_session_id(session_id) {
        let list = agents.get_or_insert_with(all);
        if let Some(a) = list
            .iter()
            .find(|a| live_codex(a) && a.session_id == session_id)
        {
            return Some((a.pane.backend.clone(), a.pane.pane_id.clone()));
        }
    }
    let env_id = tower_scan::id_for_backend_pane("codex", backend, env_pane);
    let env_record = store.load(&env_id).ok();
    // Live and in the same folder. The session may differ — `/new`, a restart
    // or a resume in the same pane starts one — but a hook from another folder
    // is another pane's, reaching here through a daemon started elsewhere.
    let env_fits =
        |a: &Agent| live_codex(a) && (cwd.is_empty() || a.cwd.is_empty() || a.cwd == cwd);
    if env_record.as_ref().is_some_and(env_fits) {
        return Some((backend.to_string(), env_pane.to_string()));
    }
    if !cwd.is_empty() {
        let list = agents.get_or_insert_with(all);
        let mut here = list
            .iter()
            .filter(|a| live_codex(a) && a.session_id.is_empty() && a.cwd == cwd);
        if let (Some(one), None) = (here.next(), here.next()) {
            return Some((one.pane.backend.clone(), one.pane.pane_id.clone()));
        }
    }
    // No record yet for the environment's pane: a codex just started there,
    // and its first hook makes the row. A record that exists but is DEAD, or
    // holds another session, is never written.
    env_record
        .is_none()
        .then(|| (backend.to_string(), env_pane.to_string()))
}

/// Handles a call from the `notify` program configured in codex's `config.toml`.
///
/// codex hands the event as a single JSON argument rather than on stdin. The
/// notify program is codex's own child, so it inherits the environment the
/// guard reads.
///
/// This path alone is not enough to supervise codex: it only ever reports a
/// finished turn, so a codex that was working showed as DONE the whole time it
/// worked (seen on screen 2026-09-03). [`run`] fills in the rest.
pub fn notify(
    store: &Store,
    args: &[String],
    env: impl Fn(&str) -> Option<String>,
    now: OffsetDateTime,
) -> std::io::Result<Option<Transition>> {
    let backend = hook_backend(&env);
    let Some(pane) = hook_pane(env) else {
        return Ok(None);
    };
    let p: NotifyPayload = args
        .last()
        .map(|a| payload(a.as_bytes()))
        .unwrap_or_default();
    // Anything else codex grows later is ignored rather than guessed at.
    if p.r#type != "agent-turn-complete" {
        return Ok(None);
    }
    // Held from the load through the save — see `hold`.
    let _lock = crate::hold(store);
    let Some((backend, pane)) = route(store, &backend, &pane, "", &p.cwd) else {
        return Ok(None);
    };
    let Some(mut a) = record(store, "codex", &backend, &pane, "", now) else {
        return Ok(None);
    };
    if !p.cwd.is_empty() {
        a.cwd = p.cwd;
    }
    commit(store, a, AgentState::DoneUnread, now)
}

/// The fields we use out of the codex hooks (`~/.codex/hooks.json`) stdin JSON.
///
/// The names match Claude Code's — `session_id`, `cwd`, `hook_event_name`.
#[derive(Debug, Default, Deserialize)]
struct Payload {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
    /// SessionStart's `startup` | `resume` | `clear` | `compact` | `fork`
    /// (codex 0.156.0, read from the binary 2026-09-24).
    #[serde(default)]
    source: String,
}

/// The state each event implies, or `None` for one we do not know.
fn state_for(event: &str) -> Option<AgentState> {
    match event {
        "user-prompt-submit" | "post-tool-use" => Some(AgentState::Working),
        // Same rule as Claude Code: a session that started has not been asked
        // for anything yet.
        "session-start" => Some(AgentState::Idle),
        "permission-request" => Some(AgentState::Waiting),
        "stop" => Some(AgentState::DoneUnread),
        _ => None,
    }
}

/// Handles one `herdr-tower hook codex --event <event>` — the codex hooks path.
///
/// Where [`notify`] reports only the end of a turn, these report WORKING and
/// WAITING as they happen. The hook is codex's child, and leaving stdout empty
/// keeps it from intervening in codex's own behaviour.
pub fn run(
    store: &Store,
    event: &str,
    stdin: impl Read,
    env: impl Fn(&str) -> Option<String>,
    now: OffsetDateTime,
) -> std::io::Result<Option<Transition>> {
    let backend = hook_backend(&env);
    let Some(pane) = hook_pane(env) else {
        return Ok(None);
    };
    let p: Payload = payload(stdin);
    let Some(to) = state_for(event) else {
        return Ok(None);
    };
    // Held from the load through the save — see `hold`.
    let _lock = crate::hold(store);
    let Some((backend, pane)) = route(store, &backend, &pane, &p.session_id, &p.cwd) else {
        return Ok(None);
    };
    let Some(mut a) = record(store, "codex", &backend, &pane, &p.session_id, now) else {
        return Ok(None);
    };
    // The rule claude has had all along: a SessionStart from a compaction
    // happens mid-turn — the context was compacted, not restarted — and
    // taking WORKING down to IDLE there dropped the running turn from the
    // row. codex sends one too; its sources include `compact`.
    if event == "session-start" && p.source == "compact" {
        if valid_session_id(&p.session_id) {
            a.session_id = p.session_id;
        }
        return touch(store, a, now);
    }
    if valid_session_id(&p.session_id) {
        a.session_id = p.session_id;
    }
    if !p.cwd.is_empty() {
        a.cwd = p.cwd;
    }
    // A prompt is a turn of its own; everything else is the same turn moving.
    if event == "user-prompt-submit" {
        return commit_new_turn(store, a, now);
    }
    commit(store, a, to, now)
}
