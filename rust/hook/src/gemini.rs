// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Hooks from both Gemini-family CLIs.

use std::io::Read;

use serde::Deserialize;
use time::OffsetDateTime;
use tower_state::{valid_session_id, AgentState, Store};

use crate::{commit, commit_new_turn, hook_pane, payload, record, Transition};

/// The fields we use out of either Gemini CLI's stdin JSON.
///
/// Two CLIs, one struct: agy (the Antigravity CLI) speaks protojson camelCase
/// while the stock Gemini CLI speaks snake_case. The keys do not collide, so
/// parsing both leniently into one shape costs nothing and keeps the two paths
/// from drifting.
#[derive(Debug, Default, Deserialize)]
struct Payload {
    // stock Gemini CLI (settings.json hooks)
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
    // Antigravity CLI (hooks.json)
    #[serde(default, rename = "conversationId")]
    conversation_id: String,
    #[serde(default, rename = "workspacePaths")]
    workspace_paths: Vec<String>,
    #[serde(default, rename = "modelName")]
    model_name: String,
}

/// The state each event implies, or `None` for one we do not know.
///
/// | agy              | stock Gemini CLI | state       |
/// |------------------|------------------|-------------|
/// | `pre-invocation` | `before-agent`   | WORKING     |
/// | `post-tool-use`  | `after-tool`     | WORKING     |
/// | `stop`           | `after-agent`    | DONE_UNREAD |
/// | —                | `session-start`  | IDLE        |
/// | —                | `notification`   | WAITING     |
fn state_for(event: &str) -> Option<AgentState> {
    match event {
        "pre-invocation" | "post-tool-use" | "before-agent" | "after-tool" => {
            Some(AgentState::Working)
        }
        // A session that started has not been asked for anything yet — the same
        // rule that keeps autostarted bots out of WORKING for Claude Code.
        "session-start" => Some(AgentState::Idle),
        "notification" => Some(AgentState::Waiting),
        "stop" | "after-agent" => Some(AgentState::DoneUnread),
        _ => None,
    }
}

/// Handles one `herdr-tower hook gemini --event <event>`.
///
/// The hook output contract (`{}` on stdout, meaning "no intervention") belongs
/// to the caller; this only touches the record.
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
    let session = if valid_session_id(&p.conversation_id) {
        p.conversation_id.as_str()
    } else {
        p.session_id.as_str()
    };
    let Some(mut a) = record(store, "gemini", &backend, &pane, session, now) else {
        return Ok(None);
    };

    if valid_session_id(&p.conversation_id) {
        a.session_id = p.conversation_id;
    } else if valid_session_id(&p.session_id) {
        a.session_id = p.session_id;
    }
    match p.workspace_paths.first() {
        Some(w) if !w.is_empty() => a.cwd = w.clone(),
        _ if !p.cwd.is_empty() => a.cwd = p.cwd,
        _ => {}
    }
    if !p.model_name.is_empty() {
        // An agy session has no file on disk to read the model out of, so this
        // hook is the only place the tower can ever learn it.
        a.model = p.model_name;
    }
    // The events that start an agent *run* — `post-tool-use` and `after-tool` are
    // the per-tool pair inside one, and neither vendor sends anything named like
    // a prompt. This is the table in `state_for` above, which is the wiring
    // `geminiinit` and `initcmd` actually install.
    //
    // The two names are two products, never both wired to one pane:
    // `before-agent` is the stock Gemini CLI's `settings.json`, `pre-invocation`
    // is Antigravity's `hooks.json`. So they are counted by what each one means
    // rather than by one rule over both.
    //
    // `before-agent` is the opening half of a pair — `after-agent` is what ends
    // the turn — so it counts wherever it arrives, a record left at WORKING by a
    // missed `after-agent` included. Refusing that left a prompt queued behind an
    // unobserved turn uncounted, and the batch watch waiting on the count timed
    // out on work that had finished.
    //
    // `pre-invocation` names an invocation, not a turn, and whether one turn can
    // hold several has not been measured against a live session. Counting each
    // one is the worse mistake — it would let that same watch read the *running*
    // turn's completion as the answer to the instruction queued behind it — so
    // this one still counts only from a record that is not already working, and
    // costs a timeout in the case above instead.
    let starts_a_turn = match event {
        "before-agent" => true,
        "pre-invocation" => a.state != AgentState::Working,
        _ => false,
    };
    if starts_a_turn {
        return commit_new_turn(store, a, now);
    }
    commit(store, a, to, now)
}
