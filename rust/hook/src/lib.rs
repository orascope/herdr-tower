// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! CLI hook events, folded into the agent records.
//!
//! A hook runs inside the agent's own process, on its critical path: the agent
//! starts it, waits for it, and must not notice it. Everything in this crate
//! follows from that one fact — parsing is forgiving, an unreadable payload
//! still records the one thing that matters (which pane fired), an event we do
//! not recognize is ignored rather than rejected, and no path here may panic.
//! Losing one state update is a far cheaper failure than interrupting the
//! agent that fired the hook.
//!
//! What a hook contributes that the backend cannot: herdr reports the agent's
//! lifecycle state, but never *what* it is working on ([`tower_state::Agent::task`])
//! or *what it is asking* ([`tower_state::Agent::ask`]). Only the CLI knows those,
//! and only a hook carries them out.

use std::io::Read;

use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use tower_state::{valid_session_id, Agent, AgentState, Store};

pub mod claude;
pub mod codex;
pub mod gemini;
mod guard;
pub mod opencode;

pub use guard::{hook_backend, hook_pane};

/// What a hook did to a record, handed back to whoever wants to announce it.
///
/// Not a package-level callback installed before dispatching: a global mutable
/// would need a lock on the hook's critical path and would make tests
/// order-dependent, so the outcome is returned instead: the hook writes the
/// record, the caller decides whether it is worth interrupting a person for.
///
/// `None` in place of a `Transition` means "nothing to announce" — a rejected
/// pane, an unknown event, or one of the deliberately quiet paths where the
/// record was saved but its state did not move.
#[derive(Debug, Clone)]
pub struct Transition {
    /// The record as it was written.
    pub agent: Agent,
    /// The state it held before this hook.
    pub prev: AgentState,
}

impl Transition {
    /// The state the record moved to.
    pub fn to(&self) -> AgentState {
        self.agent.state
    }

    /// Whether the state actually changed. A heartbeat is not a transition,
    /// and announcing one would train the user to ignore all of them.
    pub fn moved(&self) -> bool {
        self.prev != self.agent.state
    }
}

/// The real process environment, shaped the way the hooks want it: a variable
/// that is unset and one set to the empty string are the same thing.
///
/// Every entry point takes the lookup as a parameter rather than reading the
/// ambient environment, because a guard that reads it directly can only be
/// tested on a machine that happens to be configured the right way — and this
/// guard is precisely the part that must be right everywhere.
pub fn os_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Parses a hook payload, falling back to an empty one.
///
/// Every failure mode collapses to the same answer on purpose: a body that
/// cannot be read, is empty, or is not the JSON we expect still leaves the
/// hook with the fact the record is keyed on — the pane it fired from.
///
/// Note this is strict in one respect: a payload with a field of the wrong
/// type yields *no* fields here, rather than keeping the ones decoded before
/// the error. All-or-nothing is the safer half of that trade — a half-decoded
/// payload can write a `cwd` from one session onto a record while silently
/// dropping its `session_id`.
pub(crate) fn payload<T: Default + DeserializeOwned>(mut stdin: impl Read) -> T {
    let mut buf = Vec::new();
    if stdin.read_to_end(&mut buf).is_err() || buf.is_empty() {
        return T::default();
    }
    serde_json::from_slice(&buf).unwrap_or_default()
}

/// The record for a pane, created if this is the first hook from it.
///
/// The ID comes from [`tower_scan::id_for_pane`] so a hook and the scanner agree on
/// which record they mean without either consulting the other. Session and
/// window are left empty: a hook knows its own coordinate and nothing else
/// about where it sits, and the next scan fills them in.
///
/// `session` is the session id this hook's payload carries, or empty. It
/// decides what a hook over a **dead** record means:
///
/// - a different session than the record's: a new agent in a recycled slot
///   (the scanner's rule, applied here because a hook can arrive first). The
///   record starts over — reviving it would carry the previous conversation's
///   task, turn count and `born_at` over, and a dispatch board would read this
///   agent's turns as progress on the previous one's item;
/// - the same session: the same agent, which one listing missed. Revived as is;
/// - no evidence either way (no id in the payload, or none on the record):
///   revived — starting over would wipe a running agent's record — but with a
///   new `born_at`, so nothing that compared turn counts against the old record
///   takes this one's for a continuation.
pub(crate) fn record(
    store: &Store,
    kind: &str,
    backend: &str,
    pane: &str,
    session: &str,
    now: OffsetDateTime,
) -> Option<Agent> {
    let id = tower_scan::id_for_backend_pane(kind, backend, pane);
    let fresh = |fresh_id: String| {
        let mut a = Agent::new(fresh_id, kind, now);
        a.pane.backend = backend.to_string();
        a.pane.pane_id = pane.to_string();
        a
    };
    let direct = store.load(&id).ok();
    let misplaced = direct
        .as_ref()
        .is_some_and(|a| !a.pane.pane_id.is_empty() && a.pane.pane_id != pane);
    let mut a = if misplaced {
        let old = direct.as_ref().expect("misplaced record exists");
        // A moved process keeps its old HERDR_PANE_ID in the environment. Its
        // session distinguishes its hook from a new occupant of that old ID.
        if valid_session_id(session) && old.session_id == session {
            old.clone()
        } else {
            let records = store.list().ok()?;
            let mut matches = records.iter().filter(|a| {
                a.kind == kind
                    && a.state != AgentState::Dead
                    && a.pane.backend == backend
                    && a.pane.pane_id == pane
                    && valid_session_id(session)
                    && (a.session_id.is_empty() || a.session_id == session)
            });
            match (matches.next(), matches.next()) {
                (Some(a), None) => a.clone(),
                (None, _) if valid_session_id(session) => {
                    let fresh_id = (1..)
                        .map(|n| format!("{id}~{n}"))
                        .find(|candidate| !records.iter().any(|a| a.id == *candidate))?;
                    fresh(fresh_id)
                }
                // Without a session ID the old and new occupants cannot be
                // told apart; losing one hook is safer than corrupting either.
                _ => return None,
            }
        }
    } else if let Some(a) = direct {
        a
    } else {
        // After a move the record keeps its original ID but is found by its
        // current pane coordinate. Require a unique match on this backend.
        let records = store.list().ok()?;
        let mut matches = records.into_iter().filter(|a| {
            a.kind == kind
                && a.state != AgentState::Dead
                && a.pane.backend == backend
                && a.pane.pane_id == pane
                && (!valid_session_id(session)
                    || a.session_id.is_empty()
                    || a.session_id == session)
        });
        match (matches.next(), matches.next()) {
            (Some(a), None) => a,
            (None, _) => fresh(id.clone()),
            _ => return None,
        }
    };
    if a.state != AgentState::Dead {
        return Some(a);
    }
    let known = valid_session_id(session) && !a.session_id.is_empty();
    if known && a.session_id != session {
        return Some(fresh(a.id.clone()));
    }
    if !known {
        a.born_at = Some(now);
    }
    Some(a)
}

/// Holds the store for one hook's read-modify-write, or gives up.
///
/// Taken before [`record`] loads and released after the save, because the gap
/// between those two is where a scan's stale snapshot overwrites what this hook
/// learned. `try_lock_briefly` rather than a blocking wait: this runs inside the
/// agent's own process, so it proceeds unlocked rather than ever stalling a turn.
pub(crate) fn hold(store: &Store) -> Option<tower_state::StoreLock> {
    store.try_lock_briefly()
}

/// Moves a record to the state the event implies and writes it.
///
/// [`Agent::mark_hook`] is not bookkeeping. It is what makes a hook outrank the
/// backend's own observation for the next [`tower_scan::HOOK_PRECEDENCE`]: the
/// backend can only see a state, while a hook fired on the transition itself
/// and carries the task and the question with it. Dropping the mark here would
/// not break a test — it would let the next scan quietly overwrite everything
/// the hook just learned.
pub(crate) fn commit(
    store: &Store,
    a: Agent,
    to: AgentState,
    now: OffsetDateTime,
) -> std::io::Result<Option<Transition>> {
    commit_as(store, a, to, now, false)
}

/// The same, for the event that means a person asked for something.
///
/// `new_turn` is what separates a prompt from a permission request being
/// answered — see [`tower_state::Agent::begin_turn`]. Only a `user-prompt-submit`
/// passes `true`, because only it knows.
pub(crate) fn commit_new_turn(
    store: &Store,
    a: Agent,
    now: OffsetDateTime,
) -> std::io::Result<Option<Transition>> {
    commit_as(store, a, AgentState::Working, now, true)
}

fn commit_as(
    store: &Store,
    mut a: Agent,
    to: AgentState,
    now: OffsetDateTime,
    new_turn: bool,
) -> std::io::Result<Option<Transition>> {
    let prev = a.state;
    scrub(&mut a);
    a.mark_hook(now);
    if new_turn {
        a.begin_turn(now);
    } else {
        a.transition(to, now);
    }
    store.save(&a)?;
    Ok(Some(Transition { agent: a, prev }))
}

/// Writes a record the hook deliberately left in the state it was already in.
///
/// The mark still lands: a hook that decided nothing changed has proved it is
/// alive and watching, which is exactly what hook precedence is asking about.
/// `state_since` is left alone so "5m in DONE" stays true.
pub(crate) fn touch(
    store: &Store,
    mut a: Agent,
    now: OffsetDateTime,
) -> std::io::Result<Option<Transition>> {
    scrub(&mut a);
    a.mark_hook(now);
    a.updated_at = now;
    store.save(&a)?;
    Ok(None)
}

/// Takes the terminal control characters out of the text a hook stores.
///
/// Here, in the two functions every hook saves through, rather than in each
/// parser: the task and the question come from four payload shapes, and the
/// opencode plugin forwards a tool's `metadata.command` verbatim, so an ESC or
/// OSC sequence in it would otherwise reach every surface that prints a row —
/// `herdr-tower status`, the notification body, the Discord card. The TUI
/// filters on its own; the plain surfaces do not.
fn scrub(a: &mut Agent) {
    for field in [&mut a.task, &mut a.ask] {
        if let std::borrow::Cow::Owned(clean) = tower_state::clean_text(field) {
            *field = clean;
        }
    }
}

#[cfg(test)]
mod pane_move_tests {
    use super::*;

    #[test]
    fn moved_records_and_reused_coordinates_route_by_session_for_every_kind() {
        for kind in ["claude", "codex", "gemini", "opencode"] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path()).unwrap();
            let now = OffsetDateTime::UNIX_EPOCH;
            let id = tower_scan::id_for_backend_pane(kind, "lab", "w2:p1");
            let mut moved = Agent::new(id.clone(), kind, now);
            moved.pane.backend = "lab".into();
            moved.pane.pane_id = "w7:p3".into();
            moved.session_id = "old-session".into();
            moved.task = "keep task".into();
            store.save(&moved).unwrap();

            let old_hook = record(&store, kind, "lab", "w2:p1", "old-session", now).unwrap();
            assert_eq!(old_hook.id, id, "{kind}");
            let new_coordinate = record(&store, kind, "lab", "w7:p3", "old-session", now).unwrap();
            assert_eq!(new_coordinate.id, id, "{kind}");

            assert!(
                record(&store, kind, "lab", "w2:p1", "", now).is_none(),
                "{kind}"
            );
            let newcomer = record(&store, kind, "lab", "w2:p1", "new-session", now).unwrap();
            assert_eq!(newcomer.id, format!("{id}~1"), "{kind}");
            assert_eq!(newcomer.pane.pane_id, "w2:p1", "{kind}");
            store.save(&newcomer).unwrap();
            assert_eq!(
                record(&store, kind, "lab", "w2:p1", "new-session", now)
                    .unwrap()
                    .id,
                newcomer.id,
                "{kind}"
            );
            assert_eq!(store.load(&id).unwrap().task, "keep task", "{kind}");
        }
    }
}
