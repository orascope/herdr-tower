// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Reconciles what the backend reports with the record store.
//!
//! The division of labour: this layer discovers panes, updates coordinates, and
//! handles disappearance. It does **not** invent semantic state — that comes
//! from the backend's own report or from a CLI hook, never from reading a
//! screen.

use std::collections::{HashMap, HashSet};

use herdrx::{AgentStatus, Pane};
use time::{Duration, OffsetDateTime};
use tower_state::{Agent, AgentState, PaneRef, Store};

/// How long a DEAD record is kept before it is cleaned out. Long enough that a
/// person who steps away still sees what died while they were gone.
pub const DEAD_RETENTION: Duration = Duration::hours(24);

/// How long a hook's word outranks the backend's observation.
///
/// Not forever: a hook that stops arriving — config lost, process gone — must
/// not freeze the record, so the backend takes over once this expires.
pub const HOOK_PRECEDENCE: Duration = Duration::minutes(2);

/// How long after a hook the backend's "it started working" is still taken to
/// be the tail of what that hook just reported, not a new turn.
///
/// The backend learns of a turn ending a moment after the CLI's own hook says
/// so — measured 2026-09-24 for opencode, whose plugin reports `session.idle`
/// about half a second before herdr stops calling the pane `working`. A poll in
/// that gap saw a record the hook had just finished, with the backend still
/// saying it worked, and counted a turn nobody started: the batch watch reads
/// that count, and the row sat in WORKING for the whole of
/// [`HOOK_PRECEDENCE`]. Short, so a prompt whose own start hook was lost is
/// still counted, only this much later.
pub const TURN_START_GRACE: Duration = Duration::seconds(10);

/// Folds the kind the backend reported into the kind the tower supervises.
///
/// There used to be guesswork here: reading a pane's process name, treating a
/// version-shaped name as Claude, walking the process tree past an npm wrapper,
/// even looking for "✳" in the title. A backend that recognizes its own agents
/// leaves nothing to infer — agy is the Antigravity CLI, supervised as gemini.
///
/// A kind listed here is one the tower keeps a row for, hook or no hook —
/// `opencode` has none, so the backend's status is all it gets.
pub fn kind(agent: Option<&str>) -> Option<&'static str> {
    tower_state::kind_from_herdr(agent?)
}

/// The deterministic record ID for a pane.
///
/// Hooks know only their own pane coordinate, so the ID has to be derivable
/// from it alone — that is what lets a hook and the scanner agree on which
/// record they are talking about without either consulting the other.
pub fn id_for_pane(kind: &str, pane_id: &str) -> String {
    format!("{kind}-{pane_id}")
}

/// The deterministic record ID when several Herdr servers share one store.
/// Default-server IDs intentionally keep their historical spelling.
pub fn id_for_backend_pane(kind: &str, backend: &str, pane_id: &str) -> String {
    if backend.is_empty() {
        id_for_pane(kind, pane_id)
    } else {
        format!("{kind}-{backend}-{pane_id}")
    }
}

/// What a backend knows beyond coordinates.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// `None` leaves the stored state alone.
    pub state: Option<AgentState>,
    /// Empty keeps whatever is stored.
    pub session_id: String,
}

/// Maps a herdr lifecycle status onto the tower's.
///
/// `Unknown` maps to nothing on purpose: herdr's docs are explicit that it does
/// not prove completion, so translating it would invent a fact.
pub fn state_from(status: AgentStatus) -> Option<AgentState> {
    match status {
        AgentStatus::Working => Some(AgentState::Working),
        AgentStatus::Blocked => Some(AgentState::Waiting),
        AgentStatus::Done => Some(AgentState::DoneUnread),
        AgentStatus::Idle => Some(AgentState::Idle),
        AgentStatus::Unknown => None,
    }
}

/// Reads what the backend observed about each agent pane.
pub fn observations(panes: &[Pane]) -> HashMap<String, Observation> {
    panes
        .iter()
        .filter(|p| p.has_agent())
        .map(|p| {
            (
                p.pane_id.clone(),
                Observation {
                    state: state_from(p.agent_status),
                    session_id: p.session_id().unwrap_or_default().to_string(),
                },
            )
        })
        .collect()
}

/// Brings the store in line with the panes the backend reports.
///
/// Coordinates and environment always follow reality. Semantic state is not
/// touched here — a pane that reappears at a coordinate a DEAD record holds is
/// treated as a fresh observation and reset to IDLE, but nothing else moves.
///
/// `labels` names workspaces and tabs by their herdr IDs — `w2` for a
/// workspace, `w2:tY` for a tab. The two cannot collide: a tab ID always
/// carries the workspace and a `:`.
pub fn sync(
    store: &Store,
    panes: &[Pane],
    labels: &HashMap<String, String>,
    now: OffsetDateTime,
) -> std::io::Result<()> {
    sync_for_backend(store, panes, labels, "", now)
}

/// Reconciles only records owned by `backend`; records from other sockets are
/// never marked dead merely because this server cannot see them.
pub fn sync_for_backend(
    store: &Store,
    panes: &[Pane],
    labels: &HashMap<String, String>,
    backend: &str,
    now: OffsetDateTime,
) -> std::io::Result<()> {
    // Held across the whole pass, not per write. The window is between the load
    // below and the saves that follow it: a hook committing DONE_UNREAD with its
    // task and question in that gap would be overwritten by the stale snapshot
    // this function is holding. The scanner can afford to wait — a hook's cycle
    // is one small read and one write.
    let _lock = store.lock();
    let existing = store.list()?;
    let mut by_id: HashMap<String, Agent> = existing
        .into_iter()
        .filter(|a| a.pane.backend == backend)
        .map(|a| (a.id.clone(), a))
        .collect();
    // An ID can remain attached to a moved record even if the old coordinate
    // is subsequently reused. Never overwrite that file for the new occupant.
    let mut reserved_ids: HashSet<String> = by_id.keys().cloned().collect();
    let live_pane_ids: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();

    let mut alive: HashSet<String> = HashSet::new();
    // Which `kind|session|cwd` slots a *newly arrived* agent holds. A session
    // revived outside `restore` — a bridge unit at boot, or somebody reopening the
    // folder by hand — comes back under a new pane ID, so its old DEAD record does
    // not match by coordinate and would sit beside the live one for the whole
    // retention window. Two rows for one agent is worse than losing the dead row
    // early, which is why the slot is tracked at all.
    //
    // "Newly arrived" and not "live", because the slot is not a unique identity:
    // two agents of the same kind in one workspace and one folder share it. With
    // every live pane counted, closing one of that pair made the survivor look
    // like the closed one's replacement, and the dead row — the only place its
    // session ID lives, and so the only way to resume the conversation — was
    // deleted within seconds instead of waiting out the retention window. A pane
    // that has been sitting in the slot all along is not a replacement for
    // anything.
    // Keyed by slot, holding the conversation IDs that arrived in it. A slot is
    // not an identity — two agents of the same kind in one workspace and folder
    // share it — so the *conversation* is what decides whether a dead row has
    // been superseded. Starting an ordinary second Claude in a folder must not
    // erase the session ID of an unrelated one that died there.
    let mut taken_over: HashMap<String, HashSet<String>> = HashMap::new();
    for p in panes {
        let Some(kind) = kind(p.agent.as_deref()) else {
            continue;
        };
        let id = id_for_backend_pane(kind, backend, &p.pane_id);
        let direct = by_id.get(&id).is_some_and(|a| {
            (a.pane.pane_id.is_empty() || a.pane.pane_id == p.pane_id)
                && (a.state == AgentState::Dead
                    || a.pane.terminal_id.is_empty()
                    || (!p.terminal_id.is_empty() && a.pane.terminal_id == p.terminal_id))
        });
        let previous = direct.then(|| by_id.remove(&id)).flatten().or_else(|| {
            // `pane.move` changes a workspace-qualified pane ID while keeping
            // the terminal (and the process inside it) alive. Only transfer a
            // record if the terminal identifies exactly one live record of this
            // kind on this backend. A hook can also have written the new pane
            // before this scan, when its terminal ID was not known yet.
            let mut matches = by_id
                .iter()
                .filter(|(_, a)| {
                    a.kind == kind
                        && a.state != AgentState::Dead
                        && ((!p.terminal_id.is_empty() && a.pane.terminal_id == p.terminal_id)
                            || (a.pane.terminal_id.is_empty() && a.pane.pane_id == p.pane_id))
                        && (a.pane.pane_id == p.pane_id
                            || !live_pane_ids.contains(a.pane.pane_id.as_str()))
                })
                .map(|(id, _)| id.clone());
            let old_id = matches.next()?;
            matches
                .next()
                .is_none()
                .then(|| by_id.remove(&old_id))
                .flatten()
        });
        let new_id = if previous.is_none() && reserved_ids.contains(&id) {
            (1..)
                .map(|n| format!("{id}~{n}"))
                .find(|candidate| !reserved_ids.contains(candidate))
                .expect("a free agent record ID")
        } else {
            id.clone()
        };
        reserved_ids.insert(new_id.clone());
        alive.insert(
            previous
                .as_ref()
                .map_or(new_id.as_str(), |a| a.id.as_str())
                .to_string(),
        );
        // The label if the lookup has one, else what this record already said,
        // else the raw ID.
        //
        // The middle step is the one that matters. `workspace.list` can fail on
        // its own while `pane.list` succeeds, and an empty label map then rewrote
        // *every* record's session from its name to its workspace ID at once —
        // ugly in `status`, and worse afterwards: a record that died before the
        // next good refresh could not be resolved back to its workspace, because
        // `herdrx::names` accepts an ID only for a workspace that has no label. A
        // `restore` would have created a duplicate beside the real one.
        //
        // The cost is that a workspace whose label is *cleared* keeps its old name
        // here until some other change moves it. That is the rarer event, and it
        // costs one stale name rather than every name at once.
        let agent_session = labels
            .get(&p.workspace_id)
            .cloned()
            .or_else(|| {
                previous
                    .as_ref()
                    .map(|a| a.pane.session.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| p.workspace_id.clone());
        let agent_session = herdrx::strip_workspace_prefix(&agent_session).to_string();
        // The tab label takes the same three steps, and for the same reason
        // (#44): `tab.list` can fail while `pane.list` succeeds, and an empty
        // map then blanked the `tab›` part of *every* record's TITLE at once and
        // rewrote every file with it, which also defeats the write-only-what-
        // changed rule. The record's own last answer is the better guess, and its
        // cost is the same as above — a tab whose label is cleared keeps the old
        // one here until something else about the record moves.
        //
        // Two steps, not three: the last resort for a tab is the empty string,
        // so an empty previous answer needs no filtering — it *is* the last
        // resort. The session above filters because its last resort is the
        // workspace ID, which an empty name must not displace.
        //
        // 🚨 Read before `previous` is consumed below, not after.
        //
        // Stored without the `[w2:t3] ` prefix herdr-tower puts on tab names
        // (`idlabels`): the record's name for its tab is the name, and the ID is
        // already its own field — the TITLE column, the filter and every
        // reader of `tab` would otherwise show or match the coordinate twice.
        let agent_tab = labels
            .get(&p.tab_id)
            .map(|label| herdrx::strip_id_prefix(label).to_string())
            .or_else(|| previous.as_ref().map(|a| a.pane.tab.clone()))
            .unwrap_or_default();
        // A record that was not there, or was DEAD, means this is an agent that
        // has just arrived in the slot rather than one that was always in it.
        let arrived = previous
            .as_ref()
            .is_none_or(|a| a.state == AgentState::Dead);
        if arrived {
            taken_over
                .entry(slot(kind, &agent_session, &p.cwd))
                .or_default()
                .insert(p.session_id().unwrap_or_default().to_string());
        }

        let read = previous.clone();
        let mut agent = match previous {
            // The coordinate came back. herdr hands out base36 IDs and reuses
            // them, so this is a different agent in a recycled slot — and
            // everything the previous conversation left behind belongs to it: the
            // task, the question it was asking, its session ID, its model, the
            // acknowledgement of its last completion, and the hook mark that would
            // have let a two-minute-old hook outrank this agent's own state. A row
            // wearing the previous agent's task is not a smaller error than a
            // blank one, and for a harness with no task-populating hook it would
            // never be corrected. Only the identity survives, because the identity
            // is what the coordinate names.
            Some(a) if a.state == AgentState::Dead => Agent::new(new_id.clone(), kind, now),
            Some(a) => a,
            None => Agent::new(new_id.clone(), kind, now),
        };

        agent.pane = PaneRef {
            backend: backend.to_string(),
            // The label if we have one — "dbstore_go_v2" reads, "w2" does not.
            session: agent_session,
            window: herdrx::tab_index(&p.tab_id) as i64,
            pane_id: p.pane_id.clone(),
            terminal_id: p.terminal_id.clone(),
            tab: agent_tab,
            // The label is what a person reads and can change; this is what
            // groups the row, so a rename does not split a workspace whose
            // dead records still carry the old label.
            workspace_id: if backend.is_empty() {
                p.workspace_id.clone()
            } else {
                format!("{backend}:{}", p.workspace_id)
            },
        };
        agent.cwd = p.cwd.clone();
        // Both belong to the pane, not to the conversation, so they are taken
        // from the backend on every sync rather than carried across from the
        // previous record: renaming a pane has to show up without waiting for a
        // hook, and un-naming it has to clear.
        //
        // The pane's name without its `[w2:p1] ` prefix, for the same reason as
        // the tab's above — a pane named only `[w2:p1]` has no name, so its
        // title shows. A selector (`send hero-dark-codex`) and the TITLE cell want
        // the name, and the coordinate is the record's pane ID already.
        agent.label = p
            .label
            .as_deref()
            .map(herdrx::strip_id_prefix)
            .unwrap_or_default()
            .to_string();
        agent.title = p.title.clone();
        // Most passes change nothing about most panes: written only when the
        // record did, so the pass holds the lock for milliseconds, not for a
        // flush per agent. And only over the record it read: a hook that wrote
        // meanwhile wins, and the next pass starts from what it wrote.
        let read = read.filter(|r| r.id == agent.id);
        store.save_over(&agent, read.as_ref())?;
    }

    // Whatever the backend no longer reports has gone.
    for (id, mut agent) in by_id {
        if alive.contains(&id) {
            continue;
        }
        if agent.state == AgentState::Dead {
            if superseded(&taken_over, &agent) || now - agent.state_since > DEAD_RETENTION {
                store.delete(&id)?;
            }
            continue;
        }
        let read = agent.clone();
        agent.transition(AgentState::Dead, now);
        store.save_over(&agent, Some(&read))?;
    }
    Ok(())
}

/// Whether an agent that just arrived is this dead record's replacement.
///
/// The slot puts them in the same place; the conversation is what makes them the
/// same agent. A record whose session ID an arriving agent carries has plainly
/// come back — that is the case the slot check exists for, a session revived
/// outside `restore` under a new pane ID. A record with no session ID at all has
/// nothing left to resume, so keeping it only shows the same agent twice.
///
/// Anything else waits out the retention window. Starting a second Claude in a
/// folder where one died is an ordinary thing to do, and it used to delete that
/// conversation's only handle within seconds.
fn superseded(taken_over: &HashMap<String, HashSet<String>>, dead: &Agent) -> bool {
    let Some(arrivals) = taken_over.get(&slot(&dead.kind, &dead.pane.session, &dead.cwd)) else {
        return false;
    };
    if dead.session_id.is_empty() {
        return true;
    }
    arrivals.contains(&dead.session_id)
}

/// The logical place an agent occupies, independent of which pane it landed in.
///
/// Coordinates change when a session is restarted; this does not, which is what
/// makes it possible to recognize that a dead record has been superseded.
fn slot(kind: &str, session: &str, cwd: &str) -> String {
    format!("{kind}|{session}|{cwd}")
}

/// Applies the backend's semantic observations to the store.
///
/// This does not contradict the rule that the scanner never invents state. That
/// rule forbids *guessing from a screen*; what arrives here is the multiplexer
/// reporting on agents it recognizes. Where hooks cannot reach, it is the only
/// source there is — without it every agent would sit at IDLE forever.
///
/// A record a hook touched recently keeps its state: a hook knows more than the
/// state alone. Session IDs are filled in either way, since a hook cannot
/// supply one.
pub fn apply_observations(
    store: &Store,
    obs: &HashMap<String, Observation>,
    now: OffsetDateTime,
) -> std::io::Result<()> {
    apply_observations_for_backend(store, obs, "", now)
}

pub fn apply_observations_for_backend(
    store: &Store,
    obs: &HashMap<String, Observation>,
    backend: &str,
    now: OffsetDateTime,
) -> std::io::Result<()> {
    if obs.is_empty() {
        return Ok(());
    }
    let _lock = store.lock();
    for read in store.list()? {
        if read.state == AgentState::Dead || read.pane.backend != backend {
            continue;
        }
        let Some(o) = obs.get(&read.pane.pane_id) else {
            continue;
        };
        let mut agent = read.clone();

        let mut changed = false;
        if let Some(state) = o.state {
            // A completion the user already marked read must not come back. The
            // backend keeps reporting the same `done` for that pane and cannot
            // be told otherwise, so the record remembers instead; any other
            // observation is new activity and consumes the acknowledgement.
            if state != AgentState::DoneUnread && agent.done_acked {
                agent.done_acked = false;
                changed = true;
            }
            // A settled record that starts working has begun a turn; one coming
            // back from WAITING has had a permission request answered and is
            // still in the one it was in.
            let starting = state == AgentState::Working
                && matches!(
                    agent.state,
                    AgentState::Idle | AgentState::DoneUnread | AgentState::Error
                );
            let acked_done = state == AgentState::DoneUnread && agent.done_acked;
            // Hook precedence is about the *state*, and a turn beginning is the
            // one thing a hook that has already fired cannot be speaking about: a
            // Stop hook a minute ago said this agent finished, which is no answer
            // to the backend now seeing it work. Left under the freshness check,
            // a prompt whose own start hook was lost went uncounted for as long
            // as the previous turn's hook stayed fresh — and a batch watch
            // waiting on that count timed out on work that had finished.
            //
            // Except in the moment right after a hook: then the backend is
            // usually still describing the turn the hook just closed — see
            // [`TURN_START_GRACE`].
            let speaks_for_it = if starting {
                !agent.hook_fresh(now, TURN_START_GRACE)
            } else {
                !agent.hook_fresh(now, HOOK_PRECEDENCE)
            };
            if agent.state != state && !acked_done && speaks_for_it {
                // This is the backend's view of an agent whose hooks are absent —
                // and of one whose start hook is merely late, which is why the
                // count is left unclaimed for that hook to take over rather than
                // count again.
                if starting {
                    agent.infer_turn(now);
                } else {
                    agent.transition(state, now);
                }
                changed = true;
            }
        }
        // Held to the check the hooks use: herdr relays whatever ID the agent
        // reported, and `resume` types this field into a shell. An invalid one
        // is dropped, and the record keeps the ID it had.
        if tower_state::valid_session_id(&o.session_id) && agent.session_id != o.session_id {
            agent.session_id = o.session_id.clone();
            changed = true;
        }
        if changed {
            store.save_over(&agent, Some(&read))?;
        }
    }
    Ok(())
}

/// Coordinates first, then the semantic state — in one call, so the two halves
/// cannot drift apart. Forgetting the second one is invisible until somebody
/// notices every agent parked at IDLE.
pub fn sync_backend(
    store: &Store,
    herdr: &herdrx::Herdr,
    now: OffsetDateTime,
) -> std::io::Result<()> {
    // An unreachable backend is not an empty pane set, and treating it as one is
    // worse than it looks.
    //
    // The appeal is that `plan_restore` only considers DEAD records, so it seems
    // like a server that cannot answer should let everything fall to DEAD and
    // give `restore` something to work with. But a transition to DEAD resets
    // `state_since` and overwrites DONE_UNREAD, so one transient socket error
    // during any `status` call would erase every unread completion and every
    // agent's age — permanently, because a later revival stamps `state_since`
    // with the revival time.
    //
    // And it buys nothing. `restore` cannot spawn into a server that is not
    // there; `occupied_slots` already reports nothing occupied, and every planned
    // item would fail. The reboot case that *does* matter works without this:
    // herdr restores its own workspaces and tabs, then reports them as panes with
    // no agent in them, and `sync` marks the records DEAD because a shell pane is
    // not an agent. That is the path `restore` actually runs on.
    let Ok(panes) = herdr.list_panes() else {
        return Ok(());
    };
    let labels = labels_or_empty(herdr);
    sync(store, &panes, &labels, now)?;
    apply_observations(store, &observations(&panes), now)
}

/// [`sync_backend`] scoped to one member of a multi-backend supervisor.
pub fn sync_named_backend(
    store: &Store,
    herdr: &herdrx::Herdr,
    backend: &str,
    now: OffsetDateTime,
) -> std::io::Result<()> {
    sync_named_backend_observed(store, herdr, backend, now).map(|_| ())
}

/// [`sync_named_backend`], answering whether the backend was actually read:
/// `false` means it did not answer and the stored records were left as they
/// were. Asking herdr again afterwards is not the same thing — the listing
/// that refreshed the records is the one that has to have succeeded.
pub fn sync_named_backend_observed(
    store: &Store,
    herdr: &herdrx::Herdr,
    backend: &str,
    now: OffsetDateTime,
) -> std::io::Result<bool> {
    let Ok(panes) = herdr.list_panes() else {
        return Ok(false);
    };
    let labels = labels_or_empty(herdr);
    sync_for_backend(store, &panes, &labels, backend, now)?;
    apply_observations_for_backend(store, &observations(&panes), backend, now)?;
    Ok(true)
}

/// The UI's atomic backend refresh, including the actual root pane of each tab.
///
/// The ordinary CLI paths above only need agent records. The tree view also
/// needs layout, and deriving it from a pane ID is invalid because Herdr reuses
/// those coordinates. A session snapshot supplies panes, labels, and layouts
/// from one generation. Older or temporarily unreachable servers fall back to
/// the ordinary sync and leave the caller's last known roots untouched.
pub fn sync_backend_snapshot(
    store: &Store,
    herdr: &herdrx::Herdr,
    now: OffsetDateTime,
) -> std::io::Result<Option<HashSet<String>>> {
    let Ok(snapshot) = herdr.pane_snapshot() else {
        sync_backend(store, herdr, now)?;
        return Ok(None);
    };
    sync(store, &snapshot.panes, &snapshot_labels(&snapshot), now)?;
    apply_observations(store, &observations(&snapshot.panes), now)?;
    Ok(Some(snapshot.tab_roots))
}

/// Atomic refresh for one named backend in the resident supervisor.
pub fn sync_named_backend_snapshot(
    store: &Store,
    herdr: &herdrx::Herdr,
    backend: &str,
    now: OffsetDateTime,
) -> std::io::Result<Option<HashSet<String>>> {
    sync_named_backend_snapshot_observed(store, herdr, backend, now).map(|(roots, _)| roots)
}

/// [`sync_named_backend_snapshot`], also answering whether the records were
/// actually refreshed — by the snapshot, or by the fallback listing. A later,
/// separate call that succeeds says nothing about these records.
pub fn sync_named_backend_snapshot_observed(
    store: &Store,
    herdr: &herdrx::Herdr,
    backend: &str,
    now: OffsetDateTime,
) -> std::io::Result<(Option<HashSet<String>>, bool)> {
    sync_named_backend_layout_observed(store, herdr, backend, now)
        .map(|(snapshot, refreshed)| (snapshot.map(|s| s.tab_roots), refreshed))
}

/// The same refresh, retaining sidebar order and tab roots from its snapshot.
/// `None` means the fallback refreshed records without observing the layout.
pub fn sync_named_backend_layout_observed(
    store: &Store,
    herdr: &herdrx::Herdr,
    backend: &str,
    now: OffsetDateTime,
) -> std::io::Result<(Option<herdrx::PaneSnapshot>, bool)> {
    let Ok(snapshot) = herdr.pane_snapshot() else {
        let refreshed = sync_named_backend_observed(store, herdr, backend, now)?;
        return Ok((None, refreshed));
    };
    sync_for_backend(
        store,
        &snapshot.panes,
        &snapshot_labels(&snapshot),
        backend,
        now,
    )?;
    apply_observations_for_backend(store, &observations(&snapshot.panes), backend, now)?;
    Ok((Some(snapshot), true))
}

/// Workspace and tab labels, or none.
///
/// A failed lookup is not fatal: a workspace ID reads worse than its label but
/// beats showing nothing, and on the unreachable-server path there is nothing to
/// label anyway.
fn labels_or_empty(herdr: &herdrx::Herdr) -> HashMap<String, String> {
    let mut labels = herdr.workspace_labels().unwrap_or_default();
    labels.extend(herdr.tab_labels().unwrap_or_default());
    labels
}

/// A snapshot's workspace and tab labels, in the one map [`sync`] reads.
fn snapshot_labels(snapshot: &herdrx::PaneSnapshot) -> HashMap<String, String> {
    let mut labels = snapshot.workspace_labels.clone();
    labels.extend(snapshot.tab_labels.clone());
    labels
}
