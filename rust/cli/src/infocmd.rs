// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower info <name|id>` — one session, in full.
//!
//! The card itself is [`tower_ui::render_info`], not a copy of it. The TUI and
//! the command line share one renderer so the two cannot disagree about what
//! is known; `tower_ui` cannot depend on `tower_cli`, so the shared renderer
//! lives there and this module is the command-line half — finding the agent the
//! user named, and the usage line for when they named nothing.
//!
//! `expected/info.txt` is what holds the card still: the recorded bytes for
//! five records out of `expected/fixture`, asserted in
//! `tests/infocmd_golden.rs`.

use time::OffsetDateTime;
use tower_state::{Agent, AgentState};

pub use tower_ui::InfoData;

/// The card, with the command line's own state label on its first line.
///
/// The body is [`tower_ui::render_info`] verbatim — every line of it, unchanged.
/// Only the header is rebuilt, because `tower_ui`'s version draws the header with
/// `tower_ui::view::state_label`, the TUI badge (`◆ WAIT `, icon plus a trailing
/// column pad), where the command line prints the table label (`[WAIT]`). The
/// difference is visible in `expected/info.txt` line 2 and is the reason this
/// wrapper exists rather than a plain re-export.
///
/// It is a header swap and not a second card on purpose: reimplementing the
/// twenty lines below the header is what would let the CLI and the TUI drift
/// about what is known, which is the thing a single shared renderer exists to
/// prevent. The right resting place is a label parameter on
/// `tower_ui::InfoData`; until `tower_ui` grows one, the swap is confined to the one
/// line that actually differs.
pub fn render_info(d: &InfoData<'_>, now: OffsetDateTime) -> String {
    let card = tower_ui::render_info(d, now);
    // Recomputed rather than parsed out of the header: a session name may
    // contain the same two-space separator the header uses.
    let a = d.agent;
    let name = if a.pane.session.is_empty() {
        &a.id
    } else {
        &a.pane.session
    };
    let body = card.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
    format!("{name}  {}\n{body}", crate::state_label(a, now))
}

/// What `herdr-tower info` says when told to describe nothing.
///
/// A constant rather than a literal in the dispatcher because it is recorded in
/// `expected/info.txt` along with the cards — the failure mode is part of the
/// command's output.
pub const INFO_USAGE: &str = "Usage: herdr-tower info <session name|id>";

/// An agent by session name, record ID or session ID, whichever the user typed.
///
/// All of them, because all of them are on screen: `status` shows the session
/// in the SESSION column, the ID is what `resume` takes, and the tower shows a
/// session ID in SID and in the preview heading. Asking somebody to know which
/// one this command wants would be asking them to read the source.
///
/// A session ID can name more than one record — a conversation revived in a
/// second pane, with the dead one still kept — and then the live one answers.
/// Eight characters can also name two *different* conversations, and then
/// nothing does: the caller says which ones, rather than showing whichever
/// sorted first as if it were the only one.
pub fn find_agent<'a>(agents: &'a [Agent], key: &str) -> Option<&'a Agent> {
    agents
        .iter()
        .find(|a| a.id == key || a.pane.session == key)
        .or_else(|| {
            let same = by_session(agents, key);
            let conversations: std::collections::HashSet<&str> =
                same.iter().map(|a| a.session_id.trim()).collect();
            if conversations.len() > 1 {
                return None;
            }
            same.iter().find(|a| a.is_live()).or(same.first()).copied()
        })
}

/// The records whose session ID is `key` — whole, or the eight characters the
/// tower shows in its SID column.
pub fn by_session<'a>(agents: &'a [Agent], key: &str) -> Vec<&'a Agent> {
    let key = key.trim();
    if key.is_empty() {
        return Vec::new();
    }
    agents
        .iter()
        .filter(|a| a.session_id.trim() == key || a.short_session_id() == key)
        .collect()
}

/// Whether a record has stopped, so that reviving its conversation cannot run
/// it twice: DEAD, or ERROR.
///
/// `resume` by session ID takes only these. A live agent answers to the same
/// session ID, and reviving it would put a second CLI on one conversation —
/// and delete the live agent's record once the second came up.
pub fn stopped(a: &Agent) -> bool {
    matches!(a.state, AgentState::Dead | AgentState::Error)
}

/// The record `resume` should revive for `key`: the record ID it lists, or a
/// session ID — as long as exactly one record `resumable` accepts has it.
///
/// Several is an error rather than a guess: reviving the wrong one starts a
/// conversation nobody asked for in a window of its own.
pub fn find_resumable<'a>(
    agents: &'a [Agent],
    key: &str,
    resumable: impl Fn(&Agent) -> bool,
) -> Result<&'a Agent, String> {
    if let Some(a) = agents.iter().find(|a| a.id == key) {
        return Ok(a);
    }
    let found: Vec<_> = by_session(agents, key)
        .into_iter()
        .filter(|a| resumable(a))
        .collect();
    match found.as_slice() {
        [a] => Ok(a),
        [] => Err(format!(
            "no record {key:?} — `herdr-tower resume` lists what can be resumed"
        )),
        several => Err(format!(
            "{key:?} names {} records ({}) — resume one by its id",
            several.len(),
            several
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The state's own word, as the record on disk spells it.
///
/// Read back through the record's serialization rather than a second table here,
/// so the word a command prints and the word in the file cannot drift apart.
/// `tower_ui` has a private copy for the card; this one exists because `restore` and
/// `wt wait` need the same word and neither draws a card.
pub fn state_word(s: AgentState) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// What the OS calls a unit that starts something at login.
///
/// `restore` quotes it when it declines to revive a session a startup unit owns,
/// so a Linux user is not sent looking for a `LaunchAgents` directory that does
/// not exist on their machine. The card says it too, which is why the word lives
/// in `tower_ui` — one home for both readers — and this is the name the rest of this
/// crate knows it by.
pub use tower_ui::{unit_word, unit_word_for};

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn agent(id: &str, session: &str) -> Agent {
        let mut a = Agent::new(id, "claude", OffsetDateTime::UNIX_EPOCH);
        a.pane.session = session.into();
        a
    }

    #[test]
    fn an_agent_answers_to_its_session_name_and_to_its_record_id() {
        let agents = vec![agent("claude-6", "zzukumi-bot")];
        assert!(find_agent(&agents, "zzukumi-bot").is_some());
        assert!(find_agent(&agents, "claude-6").is_some());
        assert!(find_agent(&agents, "nosuch").is_none());
    }

    fn with_session(id: &str, sid: &str, state: AgentState) -> Agent {
        let mut a = agent(id, "work");
        a.session_id = sid.into();
        a.state = state;
        a
    }

    /// The ID the tower shows — in SID, or whole in the preview heading — can
    /// be copied straight into `info`.
    #[test]
    fn an_agent_answers_to_its_session_id_whole_or_as_shown_in_sid() {
        let agents = vec![
            with_session(
                "claude-1",
                "c376295e-95bf-45d2-9cd0-e26b0e837218",
                AgentState::Idle,
            ),
            with_session(
                "codex-1",
                "01a0b8e0-9917-7af1-b920-ab518ce5d454",
                AgentState::Idle,
            ),
        ];
        let id = |key: &str| find_agent(&agents, key).map(|a| a.id.as_str());
        assert_eq!(id("c376295e-95bf-45d2-9cd0-e26b0e837218"), Some("claude-1"));
        assert_eq!(id("c376295e"), Some("claude-1"));
        // codex's SID is the tail of a UUIDv7.
        assert_eq!(id("8ce5d454"), Some("codex-1"));
        assert_eq!(id("01a0b8e0"), None, "not what the tower shows for it");
        assert_eq!(id(""), None);
    }

    /// A conversation revived in a second pane leaves the dead record behind
    /// for a while; `info` answers with the live one.
    #[test]
    fn a_shared_session_id_answers_with_the_live_record() {
        let sid = "c376295e-95bf-45d2-9cd0-e26b0e837218";
        let agents = vec![
            with_session("claude-dead", sid, AgentState::Dead),
            with_session("claude-live", sid, AgentState::Working),
        ];
        assert_eq!(find_agent(&agents, "c376295e").unwrap().id, "claude-live");
    }

    /// Eight characters naming two different conversations name neither.
    #[test]
    fn a_short_id_shared_by_two_conversations_answers_with_neither() {
        let agents = vec![
            with_session("claude-1", "shortid1-aaaa", AgentState::Idle),
            with_session("claude-2", "shortid1-bbbb", AgentState::Idle),
        ];
        assert!(find_agent(&agents, "shortid1").is_none());
        assert_eq!(find_agent(&agents, "shortid1-aaaa").unwrap().id, "claude-1");
    }

    /// 🚨 A live agent's session ID must not revive it: that ran a second CLI
    /// on the same conversation, then deleted the live agent's record.
    #[test]
    fn resume_by_session_id_never_takes_a_live_agent() {
        let sid = "c376295e-95bf-45d2-9cd0-e26b0e837218";
        let live = vec![with_session("claude-1", sid, AgentState::Working)];
        assert!(find_resumable(&live, "c376295e", stopped).is_err());
        let errored = vec![with_session("claude-1", sid, AgentState::Error)];
        assert_eq!(
            find_resumable(&errored, "c376295e", stopped).unwrap().id,
            "claude-1"
        );
    }

    #[test]
    fn resume_takes_a_session_id_only_when_one_record_can_be_revived() {
        let sid = "c376295e-95bf-45d2-9cd0-e26b0e837218";
        let dead = |id: &str| with_session(id, sid, AgentState::Dead);
        let resumable = stopped;

        let one = vec![
            dead("claude-1"),
            with_session("claude-2", sid, AgentState::Working),
        ];
        assert_eq!(
            find_resumable(&one, "c376295e", resumable).unwrap().id,
            "claude-1"
        );
        assert_eq!(
            find_resumable(&one, "claude-2", resumable).unwrap().id,
            "claude-2"
        );

        let two = vec![dead("claude-1"), dead("claude-2")];
        let err = find_resumable(&two, "c376295e", resumable).unwrap_err();
        assert!(err.contains("claude-1, claude-2"), "{err}");

        assert!(find_resumable(&two, "nosuch", resumable).is_err());
    }

    /// The word on the card is the word in the record file, so a person can grep
    /// for what they saw.
    #[test]
    fn the_state_word_is_the_one_stored_on_disk() {
        assert_eq!(state_word(AgentState::DoneUnread), "DONE_UNREAD");
        assert_eq!(state_word(AgentState::Working), "WORKING");
        assert_eq!(state_word(AgentState::Dead), "DEAD");
    }

    #[test]
    fn the_startup_unit_is_named_the_way_the_platform_names_it() {
        assert_eq!(unit_word_for("linux"), "systemd unit");
        assert_eq!(unit_word_for("macos"), "LaunchAgent");
    }

    /// Property: across random populations, `resume` by session id never
    /// takes a live record, and anything `by_session` answers carries the key
    /// — whole, or as its short id.
    #[test]
    fn resume_by_session_never_takes_a_live_record_in_any_population() {
        let states = [
            AgentState::Dead,
            AgentState::Error,
            AgentState::Working,
            AgentState::Idle,
            AgentState::Waiting,
        ];
        let sids = [
            "c376295e-95bf-45d2-9cd0-e26b0e837218",
            "01a0b8e0-9917-7af1-b920-ab518ce5d454",
            "conversation-loop",
        ];
        let mut seed: u64 = 11;
        for _ in 0..300 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let agents: Vec<Agent> = (0..1 + (seed % 5) as usize)
                .map(|i| {
                    let pick = (seed >> (8 + i * 5)) as usize;
                    with_session(
                        &format!("claude-{i}"),
                        sids[pick % 3],
                        states[(pick / 3) % 5],
                    )
                })
                .collect();
            for key in [
                "c376295e",
                "8ce5d454",
                "conversa",
                "c376295e-95bf-45d2-9cd0-e26b0e837218",
            ] {
                for a in by_session(&agents, key) {
                    assert!(a.session_id == key || a.short_session_id() == key);
                }
                if let Ok(a) = find_resumable(&agents, key, stopped) {
                    assert!(stopped(a), "{} is {:?}", a.id, a.state);
                }
            }
        }
    }
}
