// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Hooks from opencode, through the plugin `herdr-tower init` installs.
//!
//! opencode has no hook configuration of its own; it has plugins, which see its
//! event bus. The plugin (`plugins/opencode/herdr-tower.js`) turns the few
//! events that matter into `herdr-tower hook opencode --event <name>` with a
//! small JSON object on stdin, already reduced to the fields below — the
//! translation from opencode's event shapes lives in the plugin, next to them.
//! It also drops every event of a subagent's session, whose turns end long
//! before the one a person is waiting on.

use std::io::Read;

use serde::Deserialize;
use time::OffsetDateTime;
use tower_state::{valid_session_id, AgentState, Store};

use crate::claude::task_line;
use crate::{commit, commit_new_turn, hook_pane, payload, record, touch, Transition};

/// What the plugin sends. Every field is optional: each event fills only its
/// own.
#[derive(Debug, Default, Deserialize)]
struct Payload {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
    /// `user-prompt-submit`: the text a person sent.
    #[serde(default)]
    prompt: String,
    /// `notification`: what the session is asking.
    #[serde(default)]
    message: String,
    /// The model answering, as opencode names it.
    #[serde(default)]
    model: String,
    /// `usage`: the last request's prompt size, and the model's window.
    #[serde(default)]
    context_tokens: u64,
    #[serde(default)]
    context_window: u64,
}

/// The state each event implies, or `None` for one we do not know.
///
/// | event                | opencode event                       | state       |
/// |----------------------|--------------------------------------|-------------|
/// | `user-prompt-submit` | `chat.message`                       | WORKING     |
/// | `notification`       | `permission.asked`, `question.asked` | WAITING     |
/// | `resume`             | `permission.replied`, `question.*`   | WORKING     |
/// | `stop`               | `session.idle`                       | DONE_UNREAD |
/// | `usage`              | assistant `message.updated`, done    | (unchanged) |
/// | `error`              | `session.error`, not an abort        | ERR         |
fn state_for(event: &str) -> Option<AgentState> {
    match event {
        "user-prompt-submit" | "resume" | "usage" => Some(AgentState::Working),
        "notification" => Some(AgentState::Waiting),
        "stop" => Some(AgentState::DoneUnread),
        "error" => Some(AgentState::Error),
        _ => None,
    }
}

/// Handles one `herdr-tower hook opencode --event <event>`.
pub fn run(
    store: &Store,
    event: &str,
    stdin: impl Read,
    env: impl Fn(&str) -> Option<String>,
    now: OffsetDateTime,
) -> std::io::Result<Option<Transition>> {
    let backend = crate::hook_backend(&env);
    let Some(pane) = hook_pane(env) else {
        return Ok(None);
    };
    let p: Payload = payload(stdin);
    let Some(to) = state_for(event) else {
        return Ok(None);
    };
    // Held from the load through the save — see `hold`.
    let _lock = crate::hold(store);
    let Some(mut a) = record(store, "opencode", &backend, &pane, &p.session_id, now) else {
        return Ok(None);
    };

    if valid_session_id(&p.session_id) {
        a.session_id = p.session_id;
    }
    if !p.cwd.is_empty() {
        a.cwd = p.cwd;
    }
    if !p.model.is_empty() {
        a.model = p.model;
    }

    match event {
        "user-prompt-submit" => {
            if let Some(t) = task_line(&p.prompt) {
                a.task = t;
            }
            // A new instruction answers whatever the last one asked.
            a.ask.clear();
            commit_new_turn(store, a, now)
        }
        // The reason goes where the question would, so the row says why.
        "notification" | "error" => {
            a.ask = p.message;
            commit(store, a, to, now)
        }
        // opencode goes idle after an error too, and the idle that follows is
        // the same failed turn ending — not a result to read. The error stays
        // until the next prompt.
        "stop" if a.state == AgentState::Error => touch(store, a, now),
        // An answer only moves a session that was waiting for one. A reply
        // that lands after the turn already ended must not pull a finished,
        // unread session back into WORKING.
        "resume" if a.state != AgentState::Waiting => touch(store, a, now),
        "resume" => {
            a.ask.clear();
            commit(store, a, to, now)
        }
        // Mid-turn bookkeeping, not a transition: a step finished and said how
        // big the context has grown.
        "usage" => {
            if p.context_tokens > 0 {
                a.ctx_tokens = p.context_tokens;
            }
            if p.context_window > 0 {
                a.ctx_window = p.context_window;
            }
            touch(store, a, now)
        }
        _ => {
            a.ask.clear();
            commit(store, a, to, now)
        }
    }
}
