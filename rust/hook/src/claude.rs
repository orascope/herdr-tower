// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Claude Code hooks.

use std::io::Read;

use serde::Deserialize;
use time::OffsetDateTime;
use tower_state::{valid_session_id, AgentState, Store};

use crate::{commit, commit_new_turn, hook_pane, payload, record, touch, Transition};

/// The Notification text Claude Code sends when a prompt has sat untouched for
/// 60 seconds. It is an idle echo, not an approval request, and reading it as
/// one is the difference between "this agent needs you" and "this agent is
/// waiting for nothing in particular".
const IDLE_ECHO: &str = "waiting for your input";

/// The same notification, named instead of spelled.
///
/// Claude Code 2.1.266 sends it as `{message: "Claude is waiting for your input",
/// notification_type: "idle_prompt"}`, and the hook input carries the type
/// through verbatim. Either signal is enough, deliberately: the type survives a
/// rewording or a translation of the sentence, and the sentence survives a CLI
/// that does not send the type or renames it. Neither is load-bearing alone, and
/// that is the point — the alternative is a hook whose recovery of a missed Stop
/// depends on one string in somebody else's release notes.
const IDLE_TYPE: &str = "idle_prompt";

/// The fields we use out of the Claude Code hook payload on stdin.
///
/// Everything else the CLI sends is ignored by design: it adds fields, and a
/// hook that fails when it sees one it does not know would take the agent down
/// with it.
#[derive(Debug, Default, Deserialize)]
struct Payload {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
    /// The Notification event's text.
    #[serde(default)]
    message: String,
    /// The Notification event's kind — `idle_prompt` and others.
    #[serde(default)]
    notification_type: String,
    /// The SessionStart event's reason — `startup`, `resume`, `clear`,
    /// `compact`.
    #[serde(default)]
    source: String,
    /// The UserPromptSubmit event's prompt text, which is what fills the TASK
    /// column.
    ///
    /// The name is the CLI's own: Claude Code 2.1.266 builds the hook input as
    /// `{hook_event_name: "UserPromptSubmit", prompt, source, session_title}` and
    /// declares `prompt` a required string in the schema it validates against.
    ///
    /// `default` rather than required here even so. A field that is renamed in a
    /// later CLI should cost the TASK column — which was blank before this existed
    /// — and not the state update the same payload carries.
    #[serde(default)]
    prompt: String,
}

/// The first line of a prompt worth showing, bounded, or `None` when there is
/// none.
///
/// Characters rather than bytes, so a Korean prompt is cut between characters and
/// not through one.
///
/// **What this declines, and why it declines so little.** A line that is one tag
/// and nothing else — `<task-notification>`, `</instructions>`,
/// `<environment_context source="harness">` — is a wrapper, not something a
/// person typed, so it is skipped and the scan moves on. A line with any content
/// is never skipped. When the whole prompt is wrappers, there is nothing to show
/// and the previous task keeps the row.
///
/// It used to do more: skip a tagged block whole when its interior held no
/// prose, so `<environment_context>\n<cwd>/repo</cwd>\n</environment_context>`
/// gave up its metadata and the instruction after it won. That is gone, and the
/// reason is worth recording because it is not a retreat from a hard problem but
/// from an impossible one.
///
/// ```text
/// <cwd>/repo</cwd>                      ← machine metadata
/// <step>refactor authentication</step>  ← somebody's instruction
/// ```
///
/// Structurally these are the same line: an element wrapping text. Every rule
/// that suppressed the first also suppressed the second, and five successive
/// attempts each found a shape the previous one got wrong — nested wrappers,
/// trailing text, attributes, inline elements — while the cost of guessing wrong
/// is asymmetric. Showing metadata is a bad row: visible, obviously wrong, and
/// replaced by the next real prompt. Hiding an instruction is silent, and the
/// row confidently reports the *previous* task while the agent works on
/// something else.
///
/// So the rule only ever declines what carries no content at all, and the line
/// it settles on is unwrapped for display when it is a single element —
/// `<step>refactor authentication</step>` reads as `refactor authentication`.
/// Telling a machine wrapper from a person's by *name* would need a list, which
/// is exact for the one marker seen in the wild and says nothing about the next
/// one; this needs no list, and its worst case is a legible bad row.
///
/// The display has its own half of this, [`tower_state::is_machine_tag`] — declining
/// to *write* a wake-up cannot help a record that already has one on disk.
pub(crate) fn task_line(prompt: &str) -> Option<String> {
    for raw in prompt.lines() {
        let line = raw.trim();
        if line.is_empty() || tower_state::is_machine_tag(line) || tower_state::is_task_id(line) {
            continue;
        }
        // Display only: the line was going to be shown either way, and the tags are
        // noise in a column 40 characters wide on the card.
        let shown = tower_state::element_text(line)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or(line);
        if tower_state::is_task_id(shown) {
            continue;
        }
        return Some(shown.chars().take(200).collect());
    }
    None
}

/// Whether a prompt is Claude Code telling the agent a background task
/// finished, rather than an instruction: its first non-blank line is exactly
/// `<task-notification>`.
pub(crate) fn is_background_notification(prompt: &str) -> bool {
    prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|l| l == "<task-notification>")
}

/// The state each event implies, or `None` for an event we do not know.
///
/// Unknown events are ignored rather than treated as an error so the CLI stays
/// free to add one without breaking the tower.
fn state_for(event: &str) -> Option<AgentState> {
    match event {
        "post-tool-use" | "user-prompt-submit" => Some(AgentState::Working),
        // A session that started is not a session that is working. Folder bots
        // autostarting after a reboot fire nothing but SessionStart, and
        // mapping it to WORKING filled the morning screen with busy agents
        // that had not been asked for anything. Work begins with a prompt.
        "session-start" => Some(AgentState::Idle),
        "notification" => Some(AgentState::Waiting),
        "stop" => Some(AgentState::DoneUnread),
        _ => None,
    }
}

/// Handles one `herdr-tower hook claude --event <event>`.
///
/// Returns what changed, or `None` when there is nothing to announce — the
/// pane was rejected, the event is unknown, or the record was touched without
/// its state moving.
pub fn run(
    store: &Store,
    event: &str,
    stdin: impl Read,
    env: impl Fn(&str) -> Option<String>,
    now: OffsetDateTime,
) -> std::io::Result<Option<Transition>> {
    // Outside herdr, or on a server whose coordinates are not ours to write.
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
    let Some(mut a) = record(store, "claude", &backend, &pane, &p.session_id, now) else {
        return Ok(None);
    };

    // The TASK column's only source. Nothing in the tree ever wrote this field, so
    // every row's TASK was permanently blank and a finished agent could not say
    // what it had done — the one thing the column exists for.
    //
    // A prompt is the right source because it is what the user asked for, in their
    // words. Kept to the first line and bounded: a prompt can be thousands of
    // characters, the display truncates anyway, and a record is read far more
    // often than it is written.
    if event == "user-prompt-submit" {
        a.background_turn = is_background_notification(&p.prompt);
        if let Some(t) = task_line(&p.prompt) {
            a.task = t;
            // A new instruction answers whatever the last one asked.
            a.ask.clear();
        }
    }

    if event == "notification" {
        let idle_echo = p.notification_type == IDLE_TYPE || p.message.contains(IDLE_ECHO);
        // An idle echo arriving while we believe the agent is WORKING says the
        // opposite: it is sitting at the prompt. That happens when the Stop
        // hook never came — a live background shell defers it, and an Esc
        // interrupt skips it altogether. The echo is the only signal that the
        // turn is over, so it recovers the missed stop as "needs you".
        if idle_echo && a.state == AgentState::Working {
            return commit(store, a, AgentState::Waiting, now);
        }
        // Otherwise an idle echo must not overwrite DONE, IDLE or WAIT with a
        // question nobody asked. The DONE guard is kept even for text we could
        // not match, so a future rewording of the echo degrades into silence
        // rather than into a false approval request on a finished turn.
        if idle_echo || a.state == AgentState::DoneUnread {
            return touch(store, a, now);
        }
    }
    // A SessionStart with source `compact` happens *mid-task* — the context was
    // compacted, not restarted. Taking WORKING down to IDLE there would lose
    // the running turn.
    if event == "session-start" && p.source == "compact" {
        if valid_session_id(&p.session_id) {
            a.session_id = p.session_id;
        }
        return touch(store, a, now);
    }

    // Only an ID that *looks* like one is kept. `resume` types this field into a
    // shell, and a payload is whatever reached the hook's stdin — so an invalid
    // one is dropped here, leaving whatever valid ID the record already had,
    // rather than stored for a later resume to refuse.
    if valid_session_id(&p.session_id) {
        a.session_id = p.session_id;
    }
    if !p.cwd.is_empty() {
        a.cwd = p.cwd;
    }
    // Ask is what the agent is asking *right now*, so only a Notification fills
    // it and every other event clears it — the question has been answered by
    // the time work resumes or the turn ends. Left standing, a finished row
    // wears "Claude needs your permission" forever (seen on screen 2026-09-04).
    //
    // Task is the last thing it was doing, which a hook never learns; it is
    // left exactly as the scanner wrote it.
    a.ask = if event == "notification" {
        p.message
    } else {
        String::new()
    };
    // A prompt is a turn of its own; everything else is the same turn moving.
    if event == "user-prompt-submit" {
        return commit_new_turn(store, a, now);
    }
    commit(store, a, to, now)
}
