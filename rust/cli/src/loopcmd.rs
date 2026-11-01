// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Opt-in continuation of one agent after it has been settled for a while.
//!
//! The durable lease and the send reservation are deliberately separate from
//! the supervisor process.  A crashed process may miss one send, but it cannot
//! duplicate one after restart; for an autonomous prompt that is the safe side
//! of the trade.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use herdrx::{Herdr, Metadata, Pane};
use time::OffsetDateTime;
use tower_state::{
    Agent, AgentState, DaemonBackend, DaemonSnapshot, DaemonStore, LockTimes, LoopLease, LoopStore,
    Store, DEFAULT_LOOP_MESSAGE, HOOK_PATIENCE,
};

pub const CONTROL_SOURCE: &str = "herdr-tower-loop-agent";
pub const CONTROL_TOKEN: &str = "loop_control";
pub const DISPLAY_SOURCE: &str = "herdr-tower-loop";
pub const DISPLAY_TOKEN: &str = "loop";
pub const MAX_IDLE_ATTEMPTS: u8 = 2;

/// The pauses that are a *description of what was observed*.
///
/// Each says "the agent is in this state right now", so seeing the agent in
/// any other state is itself the answer — no turn counter required. That
/// distinction is what the counter is for: an exhausted-attempts pause is not
/// an observation about the agent and must survive until one moves.
pub const PAUSED_DEAD: &str = "agent is dead";
pub const PAUSED_WAITING: &str = "agent is waiting for input";
pub const PAUSED_ERROR: &str = "agent is in error";
pub const PAUSED_MISSING: &str = "agent is missing";
pub const PAUSED_SESSION_UNKNOWN: &str = "agent session identity is unavailable";
/// 🚨 Not a state description. This one is the agent's own statement, written
/// by `herdr-tower loop blocked`: it says a person is needed, not that herdr
/// currently sees a blocking UI. The pane token it came from can vanish on its
/// own — a herdr metadata reset or restart drops it — and clearing the pause
/// because this snapshot lacks the token would send another autonomous
/// instruction to an agent that explicitly asked for a human. It waits for a
/// real turn, or for someone to re-arm the lease.
pub const PAUSED_BLOCKED: &str = "agent requested user input";

/// Whether a pause merely describes a state the agent was in.
fn describes_a_state(reason: &str) -> bool {
    matches!(
        reason,
        PAUSED_WAITING | PAUSED_ERROR | PAUSED_MISSING | PAUSED_SESSION_UNKNOWN
    )
}

const SUPERVISOR_LOCK: &str = "loop-supervisor.lock";
const FALLBACK_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    pub agent_id: String,
    pub backend: String,
    pub pane_id: String,
    pub message: String,
    /// When the lease that planned this send was armed. A lease re-armed for
    /// the same agent while the send was under way is a different lease, and
    /// the send's failure is not its failure (#128).
    pub lease_created_at: OffsetDateTime,
}

/// Pauses the lease a failed send was planned for, and only that one.
///
/// The send ran after the planning lock was released and across a round trip
/// to another server; `loop start` or the tower may have armed a new lease for
/// the same agent in between. Matched by agent alone, the failure landed on
/// that new lease, and `delivery failed` is a sticky reason: the loop the
/// person had just armed sat paused until the agent's turn moved (#128).
pub fn pause_failed_delivery(leases: &mut [LoopLease], d: &Dispatch, reason: &str) -> bool {
    match leases
        .iter_mut()
        .find(|l| l.agent_id == d.agent_id && l.created_at == d.lease_created_at)
    {
        Some(l) => {
            l.paused_reason = reason.to_string();
            true
        }
        None => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopNotice {
    pub session: String,
    pub backend: String,
    pub pane_id: String,
    pub reason: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub dispatches: Vec<Dispatch>,
    pub stopped: Vec<StopNotice>,
    /// Panes whose `loop blocked` token has been taken into the lease and
    /// should now be cleared. See the blocked branch in [`plan_inner`].
    pub consumed_blocks: Vec<PaneTarget>,
}

/// One pane, by backend and id, with the turn its block was observed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTarget {
    pub backend: String,
    pub pane_id: String,
    /// The agent's turn count when the block was read. Every report carries
    /// the same token value, so this is the only thing that tells the one
    /// being consumed from a later one — see [`consume_blocks`].
    pub work_seq: u64,
}

/// Reserves every due send in `leases` before handing it to the caller.
pub fn plan(
    leases: &mut Vec<LoopLease>,
    agents: &[Agent],
    panes: &[Pane],
    now: OffsetDateTime,
) -> Plan {
    let panes = panes
        .iter()
        .map(|pane| (tower_mux::target_key("", &pane.pane_id), pane.clone()))
        .collect();
    plan_with_backends(leases, agents, &panes, now)
}

/// Multi-backend form used by the resident supervisor. `panes` is keyed by
/// [`tower_mux::target_key`].
pub fn plan_with_backends(
    leases: &mut Vec<LoopLease>,
    agents: &[Agent],
    panes: &HashMap<String, Pane>,
    now: OffsetDateTime,
) -> Plan {
    plan_inner(leases, agents, panes, None, &HashSet::new(), now)
}

/// `hold` names agents the supervisor wrote to a moment ago — a scheduled
/// message the records do not show being worked on yet — whose loop message
/// waits for a later pass rather than landing on top of it.
fn plan_connected_backends(
    leases: &mut Vec<LoopLease>,
    agents: &[Agent],
    panes: &HashMap<String, Pane>,
    connected: &HashSet<String>,
    hold: &HashSet<String>,
    now: OffsetDateTime,
) -> Plan {
    plan_inner(leases, agents, panes, Some(connected), hold, now)
}

fn plan_inner(
    leases: &mut Vec<LoopLease>,
    agents: &[Agent],
    panes: &HashMap<String, Pane>,
    connected: Option<&HashSet<String>>,
    hold: &HashSet<String>,
    now: OffsetDateTime,
) -> Plan {
    let agents: HashMap<&str, &Agent> = agents.iter().map(|a| (a.id.as_str(), a)).collect();
    let mut out = Plan::default();

    leases.retain_mut(|lease| {
        let stop = |reason: &str, out: &mut Plan, lease: &LoopLease| {
            out.stopped.push(StopNotice {
                session: lease.session.clone(),
                backend: lease.backend.clone(),
                pane_id: lease.pane_id.clone(),
                reason: reason.into(),
            });
        };

        if now >= lease.expires_at {
            stop("maximum lifetime expired", &mut out, lease);
            return false;
        }
        if lease.max_turns.is_some_and(|n| lease.sent_count >= n) {
            stop("transmission limit reached", &mut out, lease);
            return false;
        }
        if connected.is_some_and(|backends| !backends.contains(&lease.backend)) {
            return true;
        }
        // Every pause below is written from this pass's observation and left
        // for the next pass to reconsider. A reason that is never re-derived
        // is a reason that never lifts: an agent that asked one question, was
        // answered, and went back to work would stay paused for the lease's
        // whole week.
        let Some(agent) = agents.get(lease.agent_id.as_str()).copied() else {
            pause_for_state(lease, PAUSED_MISSING);
            return true;
        };
        if !lease.agent_session_id.is_empty() && agent.session_id.is_empty() {
            pause_for_state(lease, PAUSED_SESSION_UNKNOWN);
            return true;
        }
        if !lease.agent_session_id.is_empty() && lease.agent_session_id != agent.session_id {
            stop("agent session changed", &mut out, lease);
            return false;
        }
        // An agent seen dead and now alive again is one of two things, and the
        // session decides which. The same conversation resumed in the same
        // pane (`claude --resume <id>`) is this lease's agent: herdr reports
        // no agent for the seconds the process is down, the scan marks the
        // record dead, and the next hook revives it under the same session.
        // Holding that death against it paused a live loop for the rest of
        // its week. Anything else — including an arrival a blank lease has no
        // session to compare with — is not provably this agent, and a lease
        // that can never resume into it has nothing left to do.
        if lease.saw_dead && agent.state != AgentState::Dead && !agent.session_id.is_empty() {
            if lease.agent_session_id.is_empty() {
                stop("agent replaced after it died", &mut out, lease);
                return false;
            }
            lease.saw_dead = false;
            if lease.paused_reason == PAUSED_DEAD {
                lease.paused_reason.clear();
            }
        }
        if lease.agent_session_id.is_empty() && !agent.session_id.is_empty() {
            lease.agent_session_id = agent.session_id.clone();
        }
        lease.backend = agent.pane.backend.clone();
        lease.pane_id = agent.pane.pane_id.clone();
        lease.session = agent.pane.session.clone();

        let target = tower_mux::target_key(&agent.pane.backend, &agent.pane.pane_id);
        match panes
            .get(&target)
            .and_then(|p| p.tokens.get(CONTROL_TOKEN))
            .map(String::as_str)
        {
            Some("done") => {
                stop("agent reported completion", &mut out, lease);
                return false;
            }
            // 🚨 The token is taken *into* the lease and then cleared, rather
            // than read on every pass. It has no TTL and nothing else clears
            // it — an ordinary human answer does not — so a standing token
            // read afresh each poll would pause the lease forever, while a
            // token that is merely ignored after the first read cannot tell a
            // new block from the one already answered. Consuming it moves the
            // fact into `paused_reason` and `seen_work_seq`, where a later
            // turn can answer it, and leaves the pane able to say it again.
            Some("blocked") => {
                // The baseline is recorded when the block is new, when there
                // is none, or when the turn has moved since the one on record.
                //
                // That last case is the ABA one, and it is why the comparison
                // is against the *turn* rather than against "have we already
                // seen a block". Every report carries the same token value, so
                // if clearing one fails, a person can answer it and the agent
                // can report a second block before the next poll. Keeping the
                // first baseline would then let the turn that led to the
                // *second* block read as the answer to it, resuming a loop
                // nobody unblocked. A token still standing on a later turn is
                // a new report.
                //
                // 🚨 Not unconditional, either: re-recording on a turn that
                // has not moved would keep pushing the baseline forward while
                // the same block stands, and the answer, when it came, could
                // never register as movement.
                if lease.paused_reason != PAUSED_BLOCKED || lease.seen_work_seq != agent.work_seq {
                    lease.seen_work_seq = agent.work_seq;
                    lease.paused_reason = PAUSED_BLOCKED.into();
                }
                out.consumed_blocks.push(PaneTarget {
                    backend: agent.pane.backend.clone(),
                    pane_id: agent.pane.pane_id.clone(),
                    work_seq: agent.work_seq,
                });
                return true;
            }
            _ => {}
        }

        // A turn a background-task notification began is not the agent
        // advancing: while it runs, or asks, or fails, the lease is left
        // exactly as it was — clock, attempts, retry and pause — and nothing is
        // sent into it. After the tokens above, so `loop done` and `loop
        // blocked` still land. A dead agent keeps its own handling below.
        if agent.background_turn
            && matches!(
                agent.state,
                AgentState::Working | AgentState::Waiting | AgentState::Error
            )
        {
            return true;
        }

        match agent.state {
            AgentState::Working => {
                // Work is the clearest answer to most pauses there are: the
                // question was answered, the error passed, the delivery that
                // failed was retried.
                lift_pause(lease, agent, Seen::Working);
                lease.idle_since = None;
                lease.attempts_in_idle = 0;
                lease.retry_at = None;
            }
            // The unsettled states end the settled generation as work does. A
            // question dismissed without a turn returns to `Idle` with the
            // counter unchanged, and the clock from before it would send the
            // moment the pane settles again — seconds after a person touched it.
            AgentState::Waiting => {
                pause_for_state(lease, PAUSED_WAITING);
                lease.idle_since = None;
            }
            AgentState::Error => {
                pause_for_state(lease, PAUSED_ERROR);
                lease.idle_since = None;
            }
            AgentState::Dead => {
                // Recorded on the lease, not just in the reason: the reason is
                // overwritten by `agent is missing` as soon as the record is
                // pruned, and the record goes after a day while this lease can
                // last a week.
                lease.saw_dead = true;
                lease.paused_reason = PAUSED_DEAD.into();
                lease.idle_since = None;
            }
            AgentState::Idle | AgentState::DoneUnread => {
                // A lease armed on a pane that was already idle waits the full
                // configured interval; a later settled generation starts when
                // the agent settled — `DoneUnread`, the end of its turn.
                //
                // A new generation also lifts a pause. Settling again is the
                // agent having moved since whatever stopped the loop — it
                // answered, finished a turn, recovered — so the attempts that
                // found no work belong to the generation that is over, not to
                // this one.
                // Asked before the generation test, not inside it: movement is
                // a turn counter, and a turn that began and ended between two
                // passes of a two-second loop can leave `state_since` where it
                // was. Whether the pause lifts must not depend on catching the
                // agent mid-turn.
                //
                // Not after a background turn: that is neither an answer to a
                // pause nor a new generation, and `seen_work_seq` staying put
                // lets the next real turn still read as movement.
                //
                // Unless an ordinary turn slipped in before it: both can end
                // between two looks, leaving only the background mark.
                let missed_ordinary =
                    agent.background_turn && agent.ordinary_seq > lease.seen_work_seq;
                let turn_moved = (!agent.background_turn || missed_ordinary)
                    && lift_pause(lease, agent, Seen::Settled);
                // 🚨 A turn confirmed by the counter starts a fresh generation
                // even when the timestamp did not move — which is exactly the
                // between-polls case the counter exists for. Without this an
                // exhausted lease clears its pause above and then writes it
                // straight back here, because the old retry deadline is due
                // and the attempt count is still at the cap: the real turn
                // would never resume the loop.
                //
                // 🚨 And only a turn or a return from an unsettled state does:
                // not a newer `state_since` on its own. `DoneUnread` → `Idle`
                // is the row being read — by a person, or by the auto-read a
                // few minutes on — and restarting the clock there made every
                // interval start late, sometimes skipping the send outright
                // (a 37-minute quiet spell counted as 27). It also handed an
                // unanswered send a fresh pair of attempts on every read.
                if turn_moved || (lease.idle_since.is_none() && !agent.background_turn) {
                    lease.idle_since = Some(agent.state_since.max(lease.created_at));
                    lease.attempts_in_idle = 0;
                    lease.retry_at = None;
                } else if lease.idle_since.is_none() {
                    // Only the clock is filled after a background turn; the
                    // attempts and their retry stay those of the generation.
                    lease.idle_since = Some(agent.state_since.max(lease.created_at));
                }
                // The same generation keeps its pause: an exhausted lease that
                // cleared itself here would send two more prompts into the
                // same silence on every pass.
                if !lease.paused_reason.is_empty() || hold.contains(&lease.agent_id) {
                    return true;
                }
                let due = if lease.attempts_in_idle == 0 {
                    lease.idle_since.map(|at| at + lease.idle_for())
                } else {
                    lease.retry_at
                };
                if due.is_some_and(|at| now >= at) {
                    if lease.attempts_in_idle >= MAX_IDLE_ATTEMPTS {
                        lease.paused_reason = "no work detected after two delivery attempts".into();
                    } else {
                        lease.attempts_in_idle += 1;
                        lease.sent_count += 1;
                        lease.retry_at = Some(now + lease.idle_for());
                        out.dispatches.push(Dispatch {
                            agent_id: lease.agent_id.clone(),
                            backend: lease.backend.clone(),
                            pane_id: lease.pane_id.clone(),
                            message: message(lease, now),
                            lease_created_at: lease.created_at,
                        });
                    }
                }
            }
        }
        true
    });
    out
}

pub fn message(lease: &LoopLease, now: OffsetDateTime) -> String {
    let left = lease.remaining_hours(now);
    let limit = lease
        .max_turns
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unlimited".into());
    let body = if lease.message.trim().is_empty() {
        DEFAULT_LOOP_MESSAGE
    } else {
        lease.message.trim()
    };
    let mut out = format!(
        "[Herdr auto-continue · send {}/{} · {}h left]\n\n{}",
        lease.sent_count, limit, left, body
    );
    if !lease.instruction.trim().is_empty() {
        out.push_str("\n\nAdditional instructions:\n");
        out.push_str(lease.instruction.trim());
    }
    out
}

pub fn resolve<'a>(agents: &'a [Agent], selector: &str) -> Result<&'a Agent, String> {
    let matches: Vec<&Agent> = agents
        .iter()
        .filter(|a| {
            a.id == selector
                || a.pane.pane_id == selector
                || a.pane.session == selector
                || (!a.label.is_empty() && a.label == selector)
        })
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no live agent matches {selector:?}")),
        [agent] => Ok(agent),
        many => Err(format!(
            "{selector:?} matches multiple agents: {}",
            many.iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn clear_control(h: &Herdr, pane_id: &str) {
    let _ = h.report_metadata(&control_metadata(pane_id, ""));
}

pub fn report_control(h: &Herdr, pane_id: &str, value: &str) -> Result<(), String> {
    h.report_metadata(&control_metadata(pane_id, value))
        .map_err(|error| error.to_string())
}

fn control_metadata(pane_id: &str, value: &str) -> Metadata {
    Metadata {
        pane_id: pane_id.into(),
        source: Some(CONTROL_SOURCE.into()),
        tokens: [(CONTROL_TOKEN.into(), value.into())].into(),
        ..Metadata::default()
    }
}

pub fn spawn_supervisor(exe: &Path) -> std::io::Result<()> {
    let status = std::process::Command::new(exe)
        .args(["daemon", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "resident supervisor start exited with {status}"
        )))
    }
}

/// What the supervisor tells a person about, delivered by the caller.
pub struct Notices<S, W, G, P, B> {
    /// An idle loop stopped.
    pub stopped: S,
    /// A session has been in WAIT for this long without an answer.
    pub still_waiting: W,
    /// The usage guard held an AI, released it, or its floors changed: a
    /// title and a line, for the person.
    pub guard: G,
    /// One of Parallax Browser's agents asked for the person (see
    /// [`crate::parallax`]): a title and a line, once per handoff.
    pub parallax: P,
    /// A dispatch board item is ready for review (see
    /// [`crate::boardnotice`]): a title and a line, once per delivery.
    pub board: B,
}

/// Owns the state-store singleton lock for the lifetime of the resident service.
///
/// What it does is written to the event log in the state directory — see
/// [`crate::supervisorlog`] — including the error that ends it, which would
/// otherwise reach only a stderr nobody reads.
pub fn supervise(
    store: &Store,
    loops: &LoopStore,
    home: &Path,
    version: &str,
    executable: &Path,
    notices: Notices<
        impl Fn(&StopNotice),
        impl Fn(&Agent, std::time::Duration),
        impl Fn(&str, &str),
        impl Fn(&str, &str),
        impl Fn(&str, &str),
    >,
    locked: impl FnOnce(),
) -> std::io::Result<()> {
    let mut log = crate::supervisorlog::Log::open(store.dir());
    let result = (|| {
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(store.dir().join(SUPERVISOR_LOCK))?;
        lock.lock()?;
        // What serves beside the loop starts only now: a second supervisor
        // waiting here would otherwise fail to bind the first one's ports and
        // serve nothing for its whole life once it got the lock.
        locked();
        // Held for as long as the loop runs.
        let _lock = lock;
        supervise_logged(store, loops, home, version, executable, notices, &mut log)
    })();
    if let Err(e) = &result {
        log.line(&format!("supervisor stopped: {e}"));
    }
    result
}

fn supervise_logged(
    store: &Store,
    loops: &LoopStore,
    home: &Path,
    version: &str,
    executable: &Path,
    notices: Notices<
        impl Fn(&StopNotice),
        impl Fn(&Agent, std::time::Duration),
        impl Fn(&str, &str),
        impl Fn(&str, &str),
        impl Fn(&str, &str),
    >,
    log: &mut crate::supervisorlog::Log,
) -> std::io::Result<()> {
    let Notices {
        stopped: notify,
        still_waiting: remind,
        guard: guard_notice,
        parallax: parallax_notice,
        board: board_notice,
    } = notices;
    let mut board_watch = crate::boardnotice::Watch::default();
    // Called with the supervisor lock held (see `supervise`).
    //
    // After the lock, not before: a second supervisor waiting on it is not
    // running anything, and a line saying it started would be a lie.
    //
    // The one before it may have left without a word: launchd stops it with a
    // signal on every install and at logout, and a crash says nothing either.
    // Nothing here catches signals, so the gap is noted here instead, where it
    // would otherwise read as a supervisor that never stopped.
    if log
        .last_event()
        .is_some_and(|last| !last.starts_with("supervisor stopped"))
    {
        log.line(
            "the previous supervisor ended without saying why — restarted, signalled or killed",
        );
    }
    log.line(&format!(
        "supervisor started: pid {} · {version}",
        std::process::id()
    ));
    let mut last_backends: HashMap<String, (bool, String)> = HashMap::new();
    let mut trouble = Trouble::default();
    let mut book = PassBook::default();
    // What `config.json` held that was not used, as last logged. Every pass
    // reads the file; only a change is worth a line.
    let mut config_problems: Vec<String> = Vec::new();
    // What only ever grew on disk — lock files, launchd's log, loop messages
    // nobody can arm again — swept at start and then hourly.
    if matches!(
        crate::housekeeping::trim_daemon_log(&store.dir().join("daemon.log")),
        Ok(true)
    ) {
        log.line("daemon.log was past 1 MiB: kept its last 64 KiB");
    }
    let lock_roots = crate::housekeeping::lock_roots(store.dir());
    // After an upgrade this is the first run of the new build: the plugin
    // copies an earlier one wrote are brought up to it (`pluginsync`).
    for line in crate::pluginsync::sync_owned(
        &crate::opencodeinit::config_dir(home, std::env::var_os("XDG_CONFIG_HOME").as_deref()),
        &crate::dockplugin::install_dir(home, std::env::var_os("XDG_DATA_HOME").as_deref()),
        executable,
    ) {
        log.line(&line);
    }
    // A pass that cannot read or write what it needs is logged and tried again
    // on the next one, not the end of the supervisor. Leaving by `?` handed a
    // passing hiccup — a full disk for a moment, a lease file caught mid-write
    // by hand — to launchd's restart throttle, and every idle loop went quiet
    // for as long as that took.
    /// 🚨 `break 'pass`, not `continue`. A pass that ends early is still a pass,
    /// and the one that ends early after a slow start is the one the slow-pass
    /// log is most useful for — a `continue` here skipped the observation
    /// entirely, so a herdr call that took ten seconds and then timed out was
    /// never counted or logged (#42). Leaving the labelled block instead means
    /// every exit passes the observation, and the wait this used to do itself is
    /// the one at the end of the loop.
    macro_rules! or_retry {
        ($label:lifetime, $result:expr, $what:literal) => {
            match $result {
                Ok(value) => value,
                Err(e) => {
                    trouble.failed(log, &format!("{}: {e}", $what));
                    break $label;
                }
            }
        };
    }

    let daemon = DaemonStore::new(store.dir());
    let mut scheduler = Scheduler::new(tower_state::ScheduleStore::new(store.dir())?);
    // Who was written to in the last few seconds, by a loop or a schedule:
    // the records do not show the new turn at once, and a second message
    // sent into that gap would land on top of the first.
    let mut recent: HashMap<String, std::time::Instant> = HashMap::new();
    let started_at = OffsetDateTime::now_utc();
    let mut roots_by_backend: HashMap<String, Vec<String>> = HashMap::new();
    let mut order_by_backend = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut labels_by_backend =
        std::collections::BTreeMap::<String, std::collections::BTreeMap<String, String>>::new();
    // The dock belongs to the default backend, which is the one `dock` itself
    // addresses; a second herdr server gets its own dock from its own `dock`.
    let default_socket = herdrx::default_socket_path();
    // The tower runs the same binary this supervisor restarts the dock with,
    // resolved the same way — an upgrade can have replaced the path this
    // process was started from.
    let tower_binary = crate::dockkeep::launcher(executable, Some(home))
        .unwrap_or_else(|| executable.to_path_buf());
    let mut dock = crate::dockkeep::Keeper::new(executable, Some(home.to_path_buf()));
    let mut usage_tab = crate::usagetab::Keeper::default();
    let mut id_labels = crate::idlabels::Reconciler::default();
    // The usage tab left from before the tower showed the board, closed on
    // the first pass that can see the dock — once per supervisor.
    let mut usage_tab_retired = false;
    // The usage board for the guard, refreshed off the pass: asking claude's
    // endpoint or codex's app server can take seconds, and the pass must not.
    let usage_board =
        crate::usageguard::Refresher::spawn(home.to_path_buf(), store.dir().to_path_buf());
    let mut guard_state = crate::usageguard::State::default();
    let mut reminders = crate::reminder::Reminders::default();
    // Parallax Browser's handoffs, polled off the pass like the usage board:
    // it is somebody else's server, and one that holds a request to its
    // timeout must not hold the idle loops with it.
    let parallax = crate::parallax::Refresher::spawn(home.to_path_buf());
    let parallax_store = tower_state::ParallaxStore::new(store.dir());
    let mut parallax_watch = crate::parallax::Watch::default();
    // The newest release on GitHub, for the tower's header: asked a few times
    // a day off the pass, like the two above.
    let releases = crate::releasecheck::Refresher::spawn(version.to_string());
    // codex session IDs its silent hooks never reported (`codexsid`). Idle
    // unless a live codex record has none, and throttled per backend then.
    let mut codex_ids = crate::codexsid::Filler::default();
    let codex_root = tower_usage::codex_sessions_root(home);
    loop {
        let pass_started = book.begin(store);
        // Read inside the pass, reported after it: a pass that ended before the
        // records were read has no count, which is not the same as none.
        let mut agents_read: Option<usize> = None;
        let mut dispatched_this_pass = false;
        // 🚨 Every exit from a pass leaves this block, so the observation below
        // it cannot be skipped (#42). Nothing in here may `continue`.
        'pass: {
            let now = OffsetDateTime::now_utc();
            let backends = or_retry!('pass,
                tower_mux::discover_backends(home),
                "finding the herdr servers"
            );
            let discovered: std::collections::HashSet<_> =
                backends.iter().map(|backend| backend.id.clone()).collect();
            roots_by_backend.retain(|backend, _| discovered.contains(backend));
            order_by_backend.retain(|backend, _| discovered.contains(backend));
            labels_by_backend.retain(|backend, _| discovered.contains(backend));
            let mut clients = HashMap::new();
            let mut refreshed_backends: HashSet<String> = HashSet::new();
            let mut panes = HashMap::new();
            let mut backend_snapshots = Vec::with_capacity(backends.len());
            let mut connected = false;
            // Read once per pass, and every pass: a changed setting takes
            // effect without restarting the supervisor, and one pass sees one
            // version of the file — six reads of it could straddle an edit.
            let (cfg, problems) = tower_config::load_checked();
            for line in config_log_lines(&mut config_problems, problems) {
                log.line(&line);
            }
            let keep_dock = cfg.dock_keep_enabled();
            // Off, no name is touched from this pass on.
            let id_labels_on = cfg.id_labels_enabled();
            let mut dock_workspaces = None;
            let mut dock_panes = None;
            let mut dock_client = None;

            for backend in backends {
                let h = backend.client();
                let mut status = DaemonBackend {
                    id: backend.id.clone(),
                    socket: backend.socket.to_string_lossy().into_owned(),
                    connected: false,
                    error: String::new(),
                    refreshed: false,
                };
                match h.ping() {
                    Ok(_) => {
                        status.connected = true;
                        connected = true;
                        let mut ready = true;
                        let mut snapshot_panes = None;
                        match tower_scan::sync_named_backend_layout_observed(
                            store,
                            &h,
                            &backend.id,
                            now,
                        ) {
                            Ok((layout, refreshed)) => {
                                // The backends whose records this pass really
                                // refreshed; a listing that succeeds later says
                                // nothing about the records.
                                if refreshed {
                                    refreshed_backends.insert(backend.id.clone());
                                    status.refreshed = true;
                                }
                                if let Some(layout) = layout {
                                    snapshot_panes = Some(layout.panes);
                                    labels_by_backend.insert(
                                        backend.id.clone(),
                                        layout
                                            .workspace_display_labels
                                            .into_iter()
                                            .map(|(workspace, label)| {
                                                (
                                                    tower_mux::target_key(&backend.id, &workspace),
                                                    label,
                                                )
                                            })
                                            .collect(),
                                    );
                                    order_by_backend.insert(
                                        backend.id.clone(),
                                        layout
                                            .workspace_order
                                            .into_iter()
                                            .map(|workspace| {
                                                tower_mux::target_key(&backend.id, &workspace)
                                            })
                                            .collect(),
                                    );
                                    roots_by_backend.insert(
                                        backend.id.clone(),
                                        layout
                                            .tab_roots
                                            .into_iter()
                                            .map(|pane| tower_mux::target_key(&backend.id, &pane))
                                            .collect(),
                                    );
                                }
                            }
                            Err(error) => {
                                status.error = format!("state synchronization failed: {error}");
                                ready = false;
                            }
                        }
                        match snapshot_panes.map(Ok).unwrap_or_else(|| h.list_panes()) {
                            Ok(list) => {
                                // Workspace/first-tab IDs and optional pane
                                // IDs, from the list just read. Throttled
                                // inside, best effort, and silent after the
                                // first failure of each kind.
                                for line in id_labels.tick(
                                    &h,
                                    &backend.id,
                                    &list,
                                    id_labels_on,
                                    std::time::Instant::now(),
                                ) {
                                    log.line(&line);
                                }
                                if keep_dock
                                    && default_socket.as_deref() == Some(backend.socket.as_path())
                                {
                                    dock_panes = Some(list.clone());
                                }
                                for pane in list {
                                    panes.insert(
                                        tower_mux::target_key(&backend.id, &pane.pane_id),
                                        pane,
                                    );
                                }
                            }
                            Err(error) => {
                                status.error = format!("pane listing failed: {error}");
                                ready = false;
                            }
                        }
                        // Only the dock's own backend is asked for workspaces, and
                        // only while the dock is being held open: a failure here
                        // leaves the dock alone rather than reporting the backend
                        // broken, because nothing else in the pass needs it.
                        if default_socket.as_deref() == Some(backend.socket.as_path()) && keep_dock
                        {
                            dock_workspaces = h.workspaces().ok();
                            dock_client = Some(h.clone());
                        }
                        if ready {
                            clients.insert(backend.id.clone(), h);
                        }
                    }
                    Err(error) => status.error = format!("herdr unavailable: {error}"),
                }
                backend_snapshots.push(status);
            }
            let mut tab_roots: Vec<_> = roots_by_backend.values().flatten().cloned().collect();
            tab_roots.sort();
            tab_roots.dedup();
            let error = backend_snapshots
                .iter()
                .filter(|backend| !backend.error.is_empty() && (backend.connected || !connected))
                .map(|backend| format!("{}: {}", backend.socket, backend.error))
                .collect::<Vec<_>>()
                .join("; ");
            for line in backend_changes(&mut last_backends, &backend_snapshots) {
                log.line(&line);
            }
            let snapshot = DaemonSnapshot {
                pid: std::process::id(),
                version: version.into(),
                started_at,
                updated_at: now,
                connected,
                error,
                tab_roots: tab_roots.clone(),
                workspace_order: order_by_backend.values().flatten().cloned().collect(),
                workspace_display_labels: labels_by_backend
                    .values()
                    .flat_map(|labels| labels.iter().map(|(id, label)| (id.clone(), label.clone())))
                    .collect(),
                parents: crate::tree::read_parents(
                    store.dir(),
                    &store.list().unwrap_or_default(),
                    now,
                ),
                backends: backend_snapshots,
                latest_release: releases.latest(),
            };
            // Not a reason to skip the pass: nothing below reads daemon.json, and
            // the dock keeper and the idle loops must go on without it.
            let mut stumbled = false;
            if let Err(e) = daemon.write(&snapshot) {
                trouble.failed(log, &format!("writing daemon.json: {e}"));
                stumbled = true;
            }

            // Asked per dock pane, of which there is normally one. The token a dead
            // tower leaves behind is not evidence, so the server's own view of the
            // process decides wherever it can: `None` (a failed call) falls back to
            // the token inside `dock_is_gone`.
            //
            // Answered once per pane per pass and kept, because two readers ask the
            // same question now — the dock's own liveness and the dock tab's name —
            // and `process_info` is a server call.
            let tower_seen: std::cell::RefCell<HashMap<String, Option<bool>>> =
                std::cell::RefCell::new(HashMap::new());
            let tower = |pane: &Pane| {
                if let Some(seen) = tower_seen.borrow().get(&pane.pane_id) {
                    return *seen;
                }
                let seen = dock_client.as_ref().and_then(|client: &Herdr| {
                    let info = client.process_info(&pane.pane_id).ok()?;
                    crate::dockcmd::tower_observed(&info, &tower_binary)
                });
                tower_seen.borrow_mut().insert(pane.pane_id.clone(), seen);
                seen
            };

            // `None` where this pass could not read the backend. The keeper must
            // not mistake that for a recovered dock — see `Keeper::tick`.
            let dock_gone: Option<bool> = match (&dock_workspaces, &dock_panes, &dock_client) {
                (Some(workspaces), Some(panes), Some(client)) => {
                    let gone = crate::dockkeep::dock_is_gone(workspaces, panes, tower);
                    // A dock that only *looks* gone because its workspace was
                    // renamed still has its tower in it. Give the name back rather
                    // than building a second one beside the first — and where
                    // neither move is safe, do nothing rather than duplicate.
                    if gone {
                        match crate::dockkeep::dock_to_reclaim(workspaces, panes, tower) {
                            Some(crate::dockkeep::Mislabeled::Reclaim(id)) => {
                                let _ = client.rename_workspace(&id, tower_ui::DOCK_LABEL);
                                // Renamed or not, this pass has said what it
                                // knows; the next one sees the result.
                                Some(false)
                            }
                            Some(crate::dockkeep::Mislabeled::Conflicted { stale, .. }) => {
                                // Free the label from the empty impostor. The next
                                // pass then sees an ordinary reclaim; doing
                                // nothing here would leave the live tower hidden
                                // under the wrong name for good.
                                let _ =
                                    client.rename_workspace(&stale, crate::dockkeep::STALE_LABEL);
                                Some(false)
                            }
                            None => Some(true),
                        }
                    } else {
                        Some(false)
                    }
                }
                // An unreadable backend says nothing about the dock, and neither
                // does a `dock_keep` that is off — the snapshot is not collected
                // then.
                _ => None,
            };

            // The dock tab's name, checked on its own rather than as a step of
            // reclaiming the workspace's — see `dockkeep::dock_tower_tabs` for the
            // failure that made this its own pass step (#40). Best effort: a failed
            // rename is taken again next pass, and the labels are asked for only
            // when there is a dock tab to ask about.
            if let (Some(workspaces), Some(panes), Some(client)) =
                (&dock_workspaces, &dock_panes, &dock_client)
            {
                let mine = crate::dockkeep::dock_tower_tabs(workspaces, panes, tower);
                if !mine.is_empty() {
                    if let Ok(tabs) = client.tabs() {
                        for tab_id in mine
                            .iter()
                            .filter(|tab_id| crate::dockcmd::tab_needs_dock_label(&tabs, tab_id))
                        {
                            let _ = client.rename_tab(tab_id, crate::dockcmd::TAB_LABEL);
                        }
                    }
                }
            }
            // The usage tab, beside a dock that is there with its tower in it.
            // Never while the dock is gone or unreadable: the dock's own
            // restart comes first, and it must find the workspace as it left it.
            if dock_gone == Some(false) && !cfg.usage_tab_enabled() && !usage_tab_retired {
                if let (Some(workspaces), Some(panes), Some(client)) =
                    (&dock_workspaces, &dock_panes, &dock_client)
                {
                    usage_tab_retired = true;
                    if let Some(line) = crate::usagetab::retire(client, workspaces, panes) {
                        log.line(&line);
                    }
                }
            }
            if dock_gone == Some(false) && cfg.usage_tab_enabled() {
                if let (Some(workspaces), Some(panes), Some(client)) =
                    (&dock_workspaces, &dock_panes, &dock_client)
                {
                    if let Some(line) = usage_tab.tick(
                        client,
                        workspaces,
                        panes,
                        &tower_binary,
                        std::time::Instant::now(),
                    ) {
                        log.line(&line);
                    }
                }
            }
            // Ticked on every pass, including the ones that start nothing. The
            // setting goes in rather than gating the call: a restart in flight has
            // to be stopped when `dock_keep` is turned off, and reaped in every
            // other case, neither of which can happen from outside the keeper.
            match dock.tick(keep_dock, dock_gone, std::time::Instant::now()) {
                Some(crate::dockkeep::Restart::Started) => {
                    log.line("dock: the tower is gone, starting it again");
                }
                Some(crate::dockkeep::Restart::Failed(e)) => {
                    log.line(&format!("dock: could not start it again: {e}"));
                }
                None => {}
            }

            // Before the herdr check below: Parallax is not a herdr client, and
            // a handoff waiting on the person is worth showing while herdr is
            // down too. Costs nothing on the passes between two polls.
            {
                let (lines, said) = parallax_watch.tick(&parallax, &parallax_store);
                for line in lines {
                    log.line(&line);
                }
                for (title, body) in &said {
                    parallax_notice(title, body);
                }
            }

            // No `fine` here: a disconnected pass stops before the records and
            // the leases, so a failure there has not been tried again, let alone
            // got over.
            if !snapshot.connected {
                break 'pass;
            }
            let agents = or_retry!('pass, store.list(), "reading the agent records");
            agents_read = Some(agents.len());
            // After the records, before anything that reads them for a
            // session: a row filled here shows its SID from the next pass on.
            for line in codex_ids.tick(
                store,
                &agents,
                &clients,
                &codex_root,
                now,
                std::time::Instant::now(),
            ) {
                log.line(&line);
            }
            // The usage guard: who the tower must not spend more on right now.
            let held: HashSet<String> = {
                let guard = crate::usageguard::Guard::from_config(&cfg);
                let holds = usage_board
                    .latest()
                    .map(|b| crate::usageguard::holds(&b, &guard, now))
                    .unwrap_or_default();
                let offset = tower_state::local_offset();
                // Types a message into an agent's pane, and says how it went.
                let mut tell = |agent: &Agent, message: &str, what: &str| match deliver(
                    &clients,
                    &agent.pane.backend,
                    &agent.pane.pane_id,
                    message,
                ) {
                    Delivered::Yes => {
                        recent.insert(agent.id.clone(), std::time::Instant::now());
                        log.line(&format!(
                            "usage guard: {what} {} ({})",
                            agent.id, agent.pane.pane_id
                        ));
                    }
                    Delivered::Maybe(e) => {
                        recent.insert(agent.id.clone(), std::time::Instant::now());
                        log.line(&format!(
                                "usage guard: {what} {} ({}) — the answer was lost, so it may have landed: {e}",
                                agent.id, agent.pane.pane_id
                            ));
                    }
                    Delivered::No(e) => log.line(&format!(
                        "usage guard: could not reach {} ({what}): {e}",
                        agent.id
                    )),
                };
                let mut said: Vec<(String, String)> = Vec::new();
                for event in guard_state.update(&holds, &agents, guard, std::time::Instant::now()) {
                    match event {
                        crate::usageguard::Event::Held(h) => {
                            let until = crate::usageguard::until_label(h.until, offset);
                            said.push((
                                format!("⏸ usage guard: {} held", h.vendor),
                                format!("{}, until {until}", h.why),
                            ));
                        }
                        crate::usageguard::Event::Released { vendor, why } => {
                            said.push((format!("▶ usage guard: {vendor} released"), why));
                        }
                        crate::usageguard::Event::FloorsChanged { vendor, change } => {
                            said.push((format!("🔋 usage guard: {vendor} floors"), change));
                        }
                        crate::usageguard::Event::WrapUp { agent, message } => {
                            tell(&agent, &message, "asked to wrap up:")
                        }
                        crate::usageguard::Event::Resume { agent, message } => {
                            tell(&agent, &message, "told it may go on:")
                        }
                    }
                }
                for (title, body) in &said {
                    log.line(&format!("{title} — {body}"));
                    guard_notice(title, body);
                }
                crate::usageguard::held_agents(&holds, &agents)
            };
            if let Some(after) = cfg.wait_reminder() {
                for a in reminders.due(&agents, after, now) {
                    log.line(&format!("still waiting: {} ({})", a.id, a.pane.pane_id));
                    // How long it has waited, not the setting: after a restart
                    // the first reminder can come hours in.
                    let waited = (now - a.state_since).try_into().unwrap_or(after);
                    remind(a, waited);
                }
            }
            recent.retain(|_, at| at.elapsed() < RECENT_SEND);
            let result = if or_retry!('pass, loops.list(), "reading the loop leases").is_empty() {
                Plan::default()
            } else {
                let connected = clients.keys().cloned().collect();
                let hold: HashSet<String> =
                    recent.keys().cloned().chain(held.iter().cloned()).collect();
                or_retry!('pass,
                    loops.update(|leases| {
                        Ok(plan_connected_backends(
                            leases, &agents, &panes, &connected, &hold, now,
                        ))
                    }),
                    "planning the loop messages"
                )
            };

            let mut dispatched = !result.dispatches.is_empty();
            for d in &result.dispatches {
                recent.insert(d.agent_id.clone(), std::time::Instant::now());
            }
            for d in result.dispatches {
                // An advisor with a dispatch board is told where it stands.
                let mut message = d.message.clone();
                if let Some(line) = agents.iter().find(|a| a.id == d.agent_id).and_then(|a| {
                    // Only the backends whose records this pass refreshed.
                    crate::boardcmd::loop_line(store.dir(), &a.cwd, &agents, now, &|b| {
                        refreshed_backends.contains(b)
                    })
                }) {
                    message.push_str("\n\n");
                    message.push_str(&line);
                }
                match deliver(&clients, &d.backend, &d.pane_id, &message) {
                    Delivered::Yes => {
                        log.line(&format!(
                            "idle loop: sent to {} ({})",
                            d.agent_id, d.pane_id
                        ));
                        store.note_dispatch(&d.agent_id, now)
                    }
                    // Not paused: if it did not land, the loop's own retry
                    // sends it again once the agent has sat idle long enough.
                    Delivered::Maybe(e) => {
                        log.line(&format!(
                            "idle loop: the answer from {} ({}) was lost, so it may have landed — counted as sent: {e}",
                            d.agent_id, d.pane_id
                        ));
                        store.note_dispatch(&d.agent_id, now)
                    }
                    Delivered::No(e) => {
                        log.line(&format!(
                            "idle loop: delivery to {} ({}) failed, paused: {e}",
                            d.agent_id, d.pane_id
                        ));
                        let reason = format!("delivery failed: {e}");
                        let _ = loops.update(|leases| {
                            pause_failed_delivery(leases, &d, &reason);
                            Ok(())
                        });
                    }
                }
            }
            consume_blocks(store, &clients, &result.consumed_blocks);
            // Board items whose worker's turn has ended, said once each, from
            // the records this pass refreshed. An advisor typed into just now
            // (a loop message this pass) or held by the usage guard is not
            // woken; one that is woken counts as typed into, for the scheduler.
            let hold: HashSet<String> =
                recent.keys().cloned().chain(held.iter().cloned()).collect();
            let woke = std::cell::RefCell::new(Vec::new());
            for line in board_watch.tick(
                store.dir(),
                &agents,
                &|b| refreshed_backends.contains(b),
                &|title, body| board_notice(title, body),
                &|advisor, message| {
                    let id = wake_advisor(&clients, &agents, &hold, advisor, message);
                    id.map(|id| woke.borrow_mut().push(id)).is_some()
                },
            ) {
                log.line(&line);
            }
            for id in woke.into_inner() {
                recent.insert(id, std::time::Instant::now());
                dispatched = true;
            }
            for stopped in &result.stopped {
                log.line(&format!(
                    "idle loop stopped: {} ({}): {}",
                    stopped.session, stopped.pane_id, stopped.reason
                ));
                if let Some(h) = clients.get(&stopped.backend) {
                    clear_display(h, &stopped.pane_id);
                }
                notify(stopped);
            }

            // Scheduled messages. Trouble with them is theirs: a schedule file
            // that cannot be read must not stop the loops' displays below.
            match scheduler.pass(
                log,
                store,
                loops,
                &clients,
                &agents,
                &mut recent,
                &held,
                now,
            ) {
                Ok(sent) => dispatched |= sent,
                Err(e) => {
                    trouble.failed(log, &e);
                    stumbled = true;
                }
            }

            let current = or_retry!('pass, loops.list(), "reading the loop leases");
            if !stumbled {
                trouble.fine(log);
            }
            for lease in &current {
                if let Some(h) = clients.get(&lease.backend) {
                    report_display(h, lease, now);
                }
            }
            dispatched_this_pass = dispatched;
        }
        // The one place every pass is counted, however it ended.
        book.end(log, store, pass_started, agents_read);
        if book.due_to_sweep(std::time::Instant::now()) {
            let swept =
                crate::housekeeping::sweep(store, loops, &lock_roots, std::time::SystemTime::now());
            if let Some(line) = swept.summary() {
                log.line(&line);
            }
        }
        // Reconcile once more immediately after a send. This catches an
        // explicit transmission cap without leaving a supervisor alive for a
        // heartbeat, and usually observes the new WORKING generation before
        // going back to sleep.
        if dispatched_this_pass {
            continue;
        }

        std::thread::sleep(FALLBACK_POLL);
    }
}

/// How long an agent just written to is left alone by the other sender.
const RECENT_SEND: Duration = Duration::from_secs(30);

/// The supervisor's side of scheduled messages: the offset a time of day is
/// read in, and which failed deliveries the log has heard about.
struct Scheduler {
    store: tower_state::ScheduleStore,
    /// Looked up now and then rather than every pass: where `time` will not
    /// read it, it comes from running `date`.
    offset: Option<(time::UtcOffset, std::time::Instant)>,
    /// Deliveries that failed, said once each rather than every pass the
    /// wait goes on; forgotten once nothing is due.
    failures: HashSet<(String, String)>,
}

impl Scheduler {
    fn new(store: tower_state::ScheduleStore) -> Self {
        Self {
            store,
            offset: None,
            failures: HashSet::new(),
        }
    }

    fn offset(&mut self) -> time::UtcOffset {
        match self.offset {
            Some((known, at)) if at.elapsed() < Duration::from_secs(300) => known,
            _ => {
                let fresh = tower_state::local_offset();
                self.offset = Some((fresh, std::time::Instant::now()));
                fresh
            }
        }
    }

    /// Sends what is due, against the local time — a schedule's `19:00` is
    /// the machine's — and never to an agent in `recent`. Answers whether
    /// anything went; an error is the schedule store's, for the log.
    #[allow(clippy::too_many_arguments)]
    fn pass(
        &mut self,
        log: &mut crate::supervisorlog::Log,
        store: &Store,
        loops: &LoopStore,
        clients: &HashMap<String, Herdr>,
        agents: &[Agent],
        recent: &mut HashMap<String, std::time::Instant>,
        held: &HashSet<String>,
        now: OffsetDateTime,
    ) -> Result<bool, String> {
        let any = self
            .store
            .list()
            .map_err(|e| format!("reading the schedules: {e}"))?;
        if any.is_empty() {
            self.failures.clear();
            return Ok(false);
        }
        let local = now.to_offset(self.offset());
        let connected: HashSet<String> = clients.keys().cloned().collect();
        // Held by the usage guard is not busy: the message waits for the
        // window to reset however long that takes, where a busy agent is
        // waited for only as long as the schedule's wait.
        let busy: HashSet<String> = recent.keys().cloned().collect();
        let handoff = |cwd: &str| tower_ui::has_session_handoff(cwd);
        let planned = self
            .store
            .update(|all| {
                Ok(crate::schedulecmd::plan(
                    all, agents, &connected, &handoff, &busy, held, local,
                ))
            })
            .map_err(|e| format!("planning the scheduled messages: {e}"))?;
        for note in &planned.notes {
            log.line(note);
        }
        if planned.sends.is_empty() {
            self.failures.clear();
        }
        let mut failed = Vec::new();
        for send in &planned.sends {
            let sent = deliver(clients, &send.backend, &send.pane_id, &send.message);
            match sent {
                // A delivery that may have landed is kept as sent: taking the
                // mark back would type it a second time on the next pass.
                Delivered::Yes | Delivered::Maybe(_) => {
                    recent.insert(send.agent_id.clone(), std::time::Instant::now());
                    log.line(&match &sent {
                        Delivered::Maybe(e) => format!(
                            "schedule {}: the answer from {} ({}) was lost, so it may have landed — counted as sent: {e}",
                            send.schedule, send.agent_id, send.pane_id
                        ),
                        _ => format!(
                            "schedule {}: sent to {} ({})",
                            send.schedule, send.agent_id, send.pane_id
                        ),
                    });
                    store.note_dispatch(&send.agent_id, now);
                    if tower_ui::is_close_message(&send.message) {
                        end_loop(log, loops, clients, send);
                    }
                }
                Delivered::No(e) => {
                    if self
                        .failures
                        .insert((send.schedule.clone(), send.agent_id.clone()))
                    {
                        log.line(&format!(
                            "schedule {}: delivery to {} ({}) failed, trying again while the wait lasts: {e}",
                            send.schedule, send.agent_id, send.pane_id
                        ));
                    }
                    failed.push(send);
                }
            }
        }
        if !failed.is_empty() {
            self.store
                .update(|all| {
                    for send in &failed {
                        crate::schedulecmd::unmark(all, &send.schedule, &send.keys);
                    }
                    Ok(())
                })
                .map_err(|e| {
                    format!("taking back scheduled messages that were not sent — they are not retried: {e}")
                })?;
        }
        Ok(planned.sends.len() > failed.len())
    }
}

/// How one of the supervisor's sends went.
#[derive(Debug)]
enum Delivered {
    Yes,
    /// The exchange failed after herdr was handed the text, so it may be in
    /// the pane — [`tower_mux::Error::may_have_landed`].
    Maybe(String),
    No(String),
}

/// Types `text` into a pane of `backend`, and says how it went.
fn deliver(
    clients: &HashMap<String, Herdr>,
    backend: &str,
    pane_id: &str,
    text: &str,
) -> Delivered {
    let Some(h) = clients.get(backend) else {
        return Delivered::No("backend is unavailable".to_string());
    };
    match tower_mux::send_text(h, pane_id, text) {
        Ok(()) => Delivered::Yes,
        Err(e) if e.may_have_landed() => Delivered::Maybe(e.to_string()),
        Err(e) => Delivered::No(e.to_string()),
    }
}

/// A scheduled close stops the agent's idle loop, as `close all` does:
/// leaving it armed would reopen the very work the close is ending.
fn end_loop(
    log: &mut crate::supervisorlog::Log,
    loops: &LoopStore,
    clients: &HashMap<String, Herdr>,
    send: &crate::schedulecmd::ScheduleSend,
) {
    match loops.disarm(&send.agent_id) {
        Ok(true) => {
            if let Some(h) = clients.get(&send.backend) {
                clear_display(h, &send.pane_id);
            }
            log.line(&format!(
                "idle loop stopped: {} ({}): the scheduled close ends its work",
                send.agent_id, send.pane_id
            ));
        }
        Ok(false) => {}
        Err(e) => log.line(&format!(
            "schedule {}: could not stop the idle loop on {} after the close: {e}",
            send.schedule, send.agent_id
        )),
    }
}

/// Whether the last pass failed, and how, so the log hears about trouble once
/// when it starts, again only if it changes, and once when it is over — not
/// once every two seconds while it lasts.
#[derive(Debug, Default)]
struct Trouble {
    last: Option<String>,
}

impl Trouble {
    fn failed(&mut self, log: &mut crate::supervisorlog::Log, what: &str) {
        if self.last.as_deref() != Some(what) {
            log.line(&format!("a pass failed, trying again every 2s: {what}"));
            self.last = Some(what.to_string());
        }
    }

    fn fine(&mut self, log: &mut crate::supervisorlog::Log) {
        if self.last.take().is_some() {
            log.line("passes are succeeding again");
        }
    }
}

/// How often the supervisor sweeps what would otherwise grow on disk.
const HOUSEKEEPING_EVERY: Duration = Duration::from_secs(3600);

/// A pass this long is worth a line in the log: it holds up the idle loops and
/// the schedules. What it cost hooks is the store lock's part, which the line
/// gives.
const SLOW_PASS: Duration = Duration::from_millis(500);

/// The bookkeeping every pass does either side of its work: the store-lock
/// count reset at the start and reported at the end, and the hourly sweep.
///
/// 🚨 Its own type because the loop it lives in needs a herdr server to run at
/// all, so none of this could be tested where it was written (#46) — and the
/// part that matters is *when* rather than *what*: the count has to start from
/// zero every pass or a slow pass inherits the last one's holds, and the sweep
/// has to be hourly rather than every two seconds. [`Self::due_to_sweep`] is
/// asked once per pass and answers on the clock it was given, so a test can move
/// that clock.
#[derive(Debug, Default)]
struct PassBook {
    slow: SlowPasses,
    swept_at: Option<std::time::Instant>,
}

impl PassBook {
    /// Starts a pass: the lock count goes back to zero, because what the last
    /// pass spent is not this one's.
    fn begin(&mut self, store: &Store) -> std::time::Instant {
        store.take_lock_times();
        std::time::Instant::now()
    }

    /// Ends it: the pass is counted, however it ended.
    fn end(
        &mut self,
        log: &mut crate::supervisorlog::Log,
        store: &Store,
        started: std::time::Instant,
        agents: Option<usize>,
    ) {
        self.slow
            .observe(log, started.elapsed(), store.take_lock_times(), agents);
    }

    /// Whether the hourly sweep is due, marking it done when it is.
    ///
    /// `now` is handed in so the hour can be moved in a test. The first pass
    /// sweeps — what only ever grew on disk is swept at start and then hourly.
    fn due_to_sweep(&mut self, now: std::time::Instant) -> bool {
        let due = self
            .swept_at
            .is_none_or(|at| now.duration_since(at) >= HOUSEKEEPING_EVERY);
        if due {
            self.swept_at = Some(now);
        }
        due
    }
}

/// Whether passes are slow, and whether they hold the store lock longer at a
/// stretch than a hook waits for it — each said once when it starts and once
/// when it is over, the same shape as [`Trouble`].
#[derive(Debug, Default)]
struct SlowPasses {
    slow: bool,
    long_hold: bool,
}

impl SlowPasses {
    /// `lock` is what the store lock cost the pass. It is in the slow line
    /// because a slow pass is not always the hooks' problem: the lock is never
    /// held across a call to herdr, so a pass that waited on a server starting
    /// up or shutting down shows little of either. Time spent waiting for it
    /// is someone else — the tower, a `wait` — holding it, and hooks queue
    /// behind that holder too.
    ///
    /// A long single hold has its own line, slow pass or not, said when it
    /// happens rather than when the stretch ends: a hook tries for the lock
    /// for [`HOOK_PATIENCE`] and then writes without it, and a scan can then
    /// put an older record back over what the hook wrote.
    fn observe(
        &mut self,
        log: &mut crate::supervisorlog::Log,
        took: Duration,
        lock: LockTimes,
        agents: Option<usize>,
    ) {
        // `None` is a pass that ended before it read the records — a herdr call
        // that failed after taking its time. Not "no records": saying zero there
        // would read as an empty store, and the difference is the whole reason
        // that pass is worth a line.
        let agents = match agents {
            Some(n) => format!("{n} agent records"),
            None => "no records read".to_string(),
        };
        if took >= SLOW_PASS {
            if !self.slow {
                log.line(&format!(
                    "passes are slow: {} ms with {agents} — {} ms holding the store lock (longest {} ms), {} ms waiting for it",
                    took.as_millis(),
                    lock.held.as_millis(),
                    lock.longest.as_millis(),
                    lock.waited.as_millis()
                ));
                self.slow = true;
            }
        } else if self.slow {
            log.line(&format!("passes are quick again: {} ms", took.as_millis()));
            self.slow = false;
        }
        if lock.longest >= HOOK_PATIENCE {
            if !self.long_hold {
                log.line(&format!(
                    "the store lock was held {} ms at a stretch; hooks give up on it after {} ms and write without it",
                    lock.longest.as_millis(),
                    HOOK_PATIENCE.as_millis()
                ));
                self.long_hold = true;
            }
        } else if self.long_hold {
            log.line(&format!(
                "store lock holds are short again: longest {} ms",
                lock.longest.as_millis()
            ));
            self.long_hold = false;
        }
    }
}

/// The lines worth logging about the backends this pass: each one whose
/// reachability or error changed since the last, and each that went away.
///
/// Changes only. The supervisor passes every two seconds, and a backend that
/// stays down must be one line when it goes and one when it returns — a line a
/// pass would bury everything else in the log within the hour. The first pass
/// has nothing to compare with, so it records where every backend stands.
/// Types a board's "ready for review" into its advisor, only while it is the
/// agent `board wake on` pinned and it is settled — never into a working or
/// asking agent, never into a pane that now holds someone else. Answers
/// whether it was delivered; the items stay due otherwise.
/// The advisor's record, when it may be woken now: still the agent that was
/// pinned, settled by its record, and neither typed into this pass nor held by
/// the usage guard (`hold`).
fn advisor_to_wake<'a>(
    agents: &'a [Agent],
    hold: &HashSet<String>,
    advisor: &tower_state::PaneWorker,
) -> Option<&'a Agent> {
    let record = agents.iter().find(|a| {
        a.pane.backend == advisor.backend
            && a.pane.pane_id == advisor.pane_id
            && a.state != AgentState::Dead
    })?;
    let reborn = advisor.born_at.is_some() && record.born_at != advisor.born_at;
    let other = !advisor.session_id.is_empty()
        && !record.session_id.is_empty()
        && record.session_id != advisor.session_id;
    let settled = matches!(record.state, AgentState::Idle | AgentState::DoneUnread);
    (!reborn && !other && settled && !hold.contains(&record.id)).then_some(record)
}

fn wake_advisor(
    clients: &HashMap<String, Herdr>,
    agents: &[Agent],
    hold: &HashSet<String>,
    advisor: &tower_state::PaneWorker,
    message: &str,
) -> Option<String> {
    let h = clients.get(&advisor.backend)?;
    let record = advisor_to_wake(agents, hold, advisor)?;
    let pane = h.pane(&advisor.pane_id).ok()?;
    if !pane.has_agent()
        || !matches!(
            pane.agent_status,
            herdrx::AgentStatus::Idle | herdrx::AgentStatus::Done
        )
    {
        return None;
    }
    h.prompt(&advisor.pane_id, message, None)
        .ok()
        .map(|_| record.id.clone())
}

fn backend_changes(
    last: &mut HashMap<String, (bool, String)>,
    now: &[DaemonBackend],
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for backend in now {
        seen.insert(backend.socket.clone());
        let state = (backend.connected, backend.error.clone());
        if last.get(&backend.socket) == Some(&state) {
            continue;
        }
        lines.push(if backend.error.is_empty() {
            format!("{}: connected", backend.socket)
        } else {
            format!("{}: {}", backend.socket, backend.error)
        });
        last.insert(backend.socket.clone(), state);
    }
    let mut gone: Vec<_> = last
        .keys()
        .filter(|socket| !seen.contains(*socket))
        .cloned()
        .collect();
    gone.sort();
    for socket in gone {
        last.remove(&socket);
        lines.push(format!("{socket}: no longer found"));
    }
    lines
}

/// Writes a pause that describes what was observed, without overwriting one
/// that outranks it.
///
/// 🚨 An explicit `loop blocked` must survive these. Its token can vanish on
/// its own — a herdr metadata reset drops it — and if a state observation then
/// replaced the reason, the protection would be gone with it: a same-turn
/// return to `Working` clears a state-derived pause, and the loop would resume
/// into an agent that asked for a person. Death is protected by
/// `LoopLease::saw_dead` instead, which no reason can overwrite.
fn pause_for_state(lease: &mut LoopLease, reason: &str) {
    if lease.paused_reason != PAUSED_BLOCKED {
        lease.paused_reason = reason.to_string();
    }
}

/// What this pass saw the agent doing, for [`lift_pause`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// Working. Progress itself — nothing else puts a record in this state.
    Working,
    /// Settled (`Idle` or `DoneUnread`), which a record reaches both by
    /// finishing a turn and by being read.
    Settled,
}

/// Lifts a pause when this agent has actually moved, and remembers where its
/// turn counter stood either way.
///
/// Three things have to be true, and each of them was a real way to send an
/// autonomous instruction somewhere it did not belong:
///
/// 1. **The agent moved.** What counts as evidence depends on what was seen.
///    `Working` *is* movement: no other path puts a record in that state, so a
///    turn is genuinely under way. A settled record is not, by itself —
///    `Store::mark_read` transitions a `DoneUnread` record to `Idle` and
///    stamps `state_since`, so merely opening a row would otherwise clear an
///    exhausted lease and send two more prompts into a silent pane, defeating
///    the attempt cap. There, the turn counter has to have moved.
///
///    🚨 The counter alone is not enough, which is why `Working` is its own
///    case. A `Waiting` → `Working` edge — a permission prompt answered — is
///    the *same* turn, and `Agent::begin_turn` deliberately does not count it.
///    Requiring a new count there would leave the answered agent paused for
///    the rest of its lease, which is the exact bug this whole line of work
///    began with.
/// 2. **The lease knows whose session it is.** herdr recycles pane
///    coordinates, and the scanner replaces a dead record with a fresh `Agent`
///    under the same deterministic ID (`tower_scan`), so a lease armed
///    without a session id could otherwise resume into a different agent that
///    happens to be sitting where the old one died.
/// 3. **This lease has never seen its agent dead.** Something else arriving
///    at a dead agent's coordinate is not it. That fact is kept on the lease
///    rather than read from the pause reason, which does not survive: the
///    record is pruned after a day (`tower_scan::DEAD_RETENTION`) and the
///    reason becomes `agent is missing` on the next pass, while the lease runs
///    for a week. Only the same session coming back clears it (`plan_inner`),
///    or a person re-arming the lease.
fn lift_pause(lease: &mut LoopLease, agent: &Agent, seen: Seen) -> bool {
    let baseline = lease.seen_work_seq;
    lease.seen_work_seq = agent.work_seq;
    let turn_moved = baseline != agent.work_seq;
    let moved = match seen {
        // 🚨 An explicit `loop blocked` is answered only by a turn that comes
        // *after* it. Not by the agent being seen working — the turn it was
        // blocked in can still be running, and `Working` would otherwise clear
        // the pause unconditionally — and not by the token going away, which
        // a herdr metadata reset does on its own.
        _ if lease.paused_reason == PAUSED_BLOCKED => turn_moved,
        Seen::Working => true,
        // Settling answers a pause that merely described a state: the agent is
        // demonstrably not in that state now. 🚨 This is not a convenience —
        // a permission-gated turn that is answered *and finishes* inside one
        // two-second supervisor interval is never observed `Working` at all,
        // and requiring a new turn there would strand the answered agent for
        // the rest of its lease.
        //
        // The rest — exhausted attempts, a failed delivery — are not
        // observations about the agent, so they wait for a turn. An unknown
        // baseline is recorded, never acted on: a lease from an earlier
        // release has no count, and comparing against zero would read an
        // agent's whole history as movement since the pause.
        Seen::Settled => describes_a_state(&lease.paused_reason) || turn_moved,
    };
    if moved && !lease.agent_session_id.is_empty() && !lease.saw_dead {
        lease.paused_reason.clear();
    }
    turn_moved
}

/// Clears the blocked tokens the leases have taken in.
///
/// 🚨 Only the report that was actually read. Every `loop blocked` writes the
/// same token value, so a clear issued after the snapshot can land on a
/// *second* block — one raised after the user answered the first, in the
/// moment between reading the pane and writing to it. The pane would then
/// show no token and an advanced turn, and the answer to the first block
/// would read as the answer to the second, which nobody gave.
///
/// The agent's turn count is re-read immediately before the write and the
/// clear is skipped if it has moved. That does not close the window — nothing
/// short of a conditional write on herdr's side would — but it narrows it from
/// a whole polling interval to the span of one call, and the skipped case is
/// handled correctly on the next pass, where the standing token on a later
/// turn is recorded as the new report it is.
///
/// Best effort otherwise: a failed clear leaves the token standing, and the
/// next pass takes it again without changing what it already recorded.
fn consume_blocks(store: &Store, clients: &HashMap<String, Herdr>, targets: &[PaneTarget]) {
    if targets.is_empty() {
        return;
    }
    let latest = store.list().unwrap_or_default();
    for target in targets {
        let Some(h) = clients.get(&target.backend) else {
            continue;
        };
        let moved_on = latest.iter().any(|agent| {
            agent.pane.backend == target.backend
                && agent.pane.pane_id == target.pane_id
                && agent.work_seq != target.work_seq
        });
        if moved_on {
            continue;
        }
        clear_control(h, &target.pane_id);
    }
}

fn report_display(h: &Herdr, lease: &LoopLease, now: OffsetDateTime) {
    let value = if lease.paused_reason.is_empty() {
        let hours = lease.remaining_hours(now);
        format!(
            "↻ {}m · {} · {}h",
            lease.idle_for_secs / 60,
            lease.sent_count,
            hours
        )
    } else {
        format!("↻ {}m · paused", lease.idle_for_secs / 60)
    };
    let _ = h.report_metadata(&Metadata {
        pane_id: lease.pane_id.clone(),
        source: Some(DISPLAY_SOURCE.into()),
        tokens: [(DISPLAY_TOKEN.into(), value)].into(),
        ttl: Some(Duration::from_secs(5 * 60)),
        ..Metadata::default()
    });
}

pub fn clear_display(h: &Herdr, pane_id: &str) {
    let _ = h.report_metadata(&Metadata {
        pane_id: pane_id.into(),
        source: Some(DISPLAY_SOURCE.into()),
        tokens: [(DISPLAY_TOKEN.into(), String::new())].into(),
        ..Metadata::default()
    });
}

pub fn render_list(
    out: &mut impl Write,
    leases: &[LoopLease],
    now: OffsetDateTime,
) -> std::io::Result<()> {
    if leases.is_empty() {
        return writeln!(out, "no idle loops are configured.");
    }
    for l in leases {
        // A later observation can replace the pause's wording, but never the
        // durable evidence that this lease saw its agent die. Show the reason
        // that actually keeps it paused, even if a pane ID is reused.
        let status = if l.saw_dead {
            format!("paused: {PAUSED_DEAD}")
        } else if !l.paused_reason.is_empty() {
            format!("paused: {}", l.paused_reason)
        } else if now >= l.expires_at {
            "expired".into()
        } else {
            "active".into()
        };
        writeln!(
            out,
            "{}  {}  idle={}m  sent={}  expires={}h  {}",
            l.agent_id,
            l.session,
            l.idle_for_secs / 60,
            l.sent_count,
            l.remaining_hours(now),
            status
        )?;
    }
    Ok(())
}

/// What the supervisor's log says about `config.json` this pass: nothing
/// while its problems are the ones already logged, each of them when they
/// change, and one line when they clear. `logged` becomes `now`.
fn config_log_lines(logged: &mut Vec<String>, now: Vec<String>) -> Vec<String> {
    if *logged == now {
        return Vec::new();
    }
    let lines = if now.is_empty() {
        vec!["config.json: every setting is read again".to_string()]
    } else {
        now.clone()
    };
    *logged = now;
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_state::PaneRef;

    /// `config_log_lines` across a run of passes: each row is what the pass
    /// read and what it logs.
    #[test]
    fn config_problems_are_logged_once_per_change() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let cleared = "config.json: every setting is read again";
        let passes: &[(&[&str], &[&str])] = &[
            (&[], &[]),
            (&["a"], &["a"]),
            (&["a"], &[]),
            (&["a", "b"], &["a", "b"]),
            (&["b", "a"], &["b", "a"]),
            (&["b", "a"], &[]),
            (&[], &[cleared]),
            (&[], &[]),
            (&["c"], &["c"]),
        ];
        let mut logged = Vec::new();
        for (i, (read, logs)) in passes.iter().enumerate() {
            assert_eq!(config_log_lines(&mut logged, p(read)), p(logs), "pass {i}");
            assert_eq!(logged, p(read), "pass {i} remembers what it read");
        }
    }

    /// Whatever the sequence, a problem set is logged on the first pass that
    /// reads it and never on the passes that read it again.
    #[test]
    fn config_problems_are_never_logged_twice_in_a_row() {
        let sets: [Vec<String>; 3] = [Vec::new(), vec!["x".into()], vec!["x".into(), "y".into()]];
        let mut rng = tower_testrng::Rng::new(0x000c_0f19);
        let mut logged = Vec::new();
        let mut previous: Option<Vec<String>> = Some(Vec::new());
        for i in 0..500 {
            let now = sets[rng.below(sets.len())].clone();
            let lines = config_log_lines(&mut logged, now.clone());
            let changed = previous.as_ref() != Some(&now);
            assert_eq!(!lines.is_empty(), changed, "pass {i}");
            if changed && !now.is_empty() {
                assert_eq!(lines, now, "pass {i}");
            }
            previous = Some(now);
        }
    }

    #[test]
    fn list_shows_durable_death_over_a_later_observation() {
        let a = agent(AgentState::Idle, -100);
        let mut l = lease(&a);
        l.saw_dead = true;
        l.paused_reason = PAUSED_SESSION_UNKNOWN.into();
        let mut out = Vec::new();
        render_list(&mut out, &[l], at(10)).unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("paused: agent is dead"), "{shown}");
        assert!(!shown.contains(PAUSED_SESSION_UNKNOWN), "{shown}");
    }

    #[test]
    fn an_advisor_typed_into_or_held_this_pass_is_not_woken() {
        let mut rec = Agent::new("claude-w2:pBQ", "claude", at(0));
        rec.pane.pane_id = "w2:pBQ".into();
        rec.session_id = "S1".into();
        rec.state = AgentState::Idle;
        let pin = tower_state::PaneWorker {
            backend: String::new(),
            pane_id: "w2:pBQ".into(),
            agent_id: rec.id.clone(),
            session_id: "S1".into(),
            born_at: None,
        };
        let agents = [rec.clone()];
        let none = HashSet::new();
        let held: HashSet<String> = [rec.id.clone()].into();
        let other: HashSet<String> = ["claude-w9:p1".to_string()].into();
        assert!(
            advisor_to_wake(&agents, &none, &pin).is_some(),
            "idle, free"
        );
        assert!(
            advisor_to_wake(&agents, &other, &pin).is_some(),
            "another held"
        );
        assert!(
            advisor_to_wake(&agents, &held, &pin).is_none(),
            "held or typed into"
        );
        let mut busy = rec.clone();
        busy.state = AgentState::Working;
        assert!(advisor_to_wake(&[busy], &none, &pin).is_none(), "working");
        let mut replaced = rec.clone();
        replaced.session_id = "S2".into();
        assert!(
            advisor_to_wake(&[replaced], &none, &pin).is_none(),
            "another agent"
        );
        let mut dead = rec;
        dead.state = AgentState::Dead;
        assert!(advisor_to_wake(&[dead], &none, &pin).is_none(), "gone");
    }

    fn at(mins: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::minutes(mins)
    }

    fn agent(state: AgentState, since: i64) -> Agent {
        turned(state, since, 1)
    }

    /// The same agent, with its turn counter where the caller wants it.
    ///
    /// `work_seq` is what tells "the agent moved" from "somebody looked at the
    /// row": `Store::mark_read` stamps `state_since` without a turn having
    /// begun. Tests that mean movement have to raise this.
    fn turned(state: AgentState, since: i64, work_seq: u64) -> Agent {
        let mut a = Agent::new("codex-p1", "codex", at(0));
        a.state = state;
        a.state_since = at(since);
        a.work_seq = work_seq;
        a.session_id = "conversation-1".into();
        a.pane = PaneRef {
            backend: String::new(),
            session: "work".into(),
            pane_id: "p1".into(),
            ..PaneRef::default()
        };
        a
    }

    fn lease(a: &Agent) -> LoopLease {
        LoopLease::new(
            a,
            at(0),
            Duration::from_secs(600),
            Duration::from_secs(168 * 3600),
            None,
            String::new(),
        )
    }

    fn pane_with_control(value: &str) -> Pane {
        serde_json::from_value(serde_json::json!({
            "pane_id": "p1",
            "tokens": { "loop_control": value }
        }))
        .unwrap()
    }

    #[test]
    fn completion_commands_write_the_supervisors_control_token() {
        for value in ["done", "blocked"] {
            let metadata = control_metadata("p1", value);
            assert_eq!(metadata.pane_id, "p1");
            assert_eq!(metadata.source.as_deref(), Some(CONTROL_SOURCE));
            assert_eq!(
                metadata.tokens.get(CONTROL_TOKEN).map(String::as_str),
                Some(value)
            );
        }
    }

    /// #128: arm → plan → re-arm → the send fails. The failure belongs to
    /// the lease that planned it; the one armed since stays unpaused.
    #[test]
    fn a_failed_send_pauses_only_the_lease_that_planned_it() {
        let a = agent(AgentState::Idle, -100);
        let mut leases = vec![lease(&a)];
        let d = plan(&mut leases, std::slice::from_ref(&a), &[], at(10))
            .dispatches
            .remove(0);
        assert_eq!(d.lease_created_at, at(0));

        // Re-armed while the send was under way: same agent, a new lease.
        let mut rearmed = LoopLease::new(
            &a,
            at(11),
            Duration::from_secs(600),
            Duration::from_secs(168 * 3600),
            None,
            String::new(),
        );
        rearmed.paused_reason.clear();
        let mut now_on_disk = vec![rearmed];
        assert!(!pause_failed_delivery(
            &mut now_on_disk,
            &d,
            "delivery failed: x"
        ));
        assert_eq!(
            now_on_disk[0].paused_reason, "",
            "the new lease is not paused"
        );

        // Not re-armed: the lease that planned it takes the pause.
        assert!(pause_failed_delivery(&mut leases, &d, "delivery failed: x"));
        assert_eq!(leases[0].paused_reason, "delivery failed: x");
    }

    /// A turn begun by a background-task notification; the last ordinary
    /// turn was the arming one (1).
    fn background(state: AgentState, since: i64, work_seq: u64) -> Agent {
        let mut a = turned(state, since, work_seq);
        a.background_turn = true;
        a.ordinary_seq = 1;
        a
    }

    fn sends(leases: &mut Vec<LoopLease>, a: &Agent, now: i64) -> usize {
        plan(leases, std::slice::from_ref(a), &[], at(now))
            .dispatches
            .len()
    }

    #[test]
    fn background_turns_do_not_restart_the_idle_clock() {
        // Settled from minute 0; a watcher's notification wakes the agent at 5
        // and it settles again at 7. The loop still fires at 10, not at 17.
        let a = turned(AgentState::Idle, 0, 1);
        let mut leases = vec![lease(&a)];
        assert_eq!(sends(&mut leases, &a, 1), 0);
        assert_eq!(
            sends(&mut leases, &background(AgentState::Working, 5, 2), 5),
            0
        );
        assert_eq!(
            sends(&mut leases, &background(AgentState::DoneUnread, 7, 2), 9),
            0
        );
        assert_eq!(
            sends(&mut leases, &background(AgentState::DoneUnread, 7, 2), 10),
            1
        );
    }

    #[test]
    fn an_ordinary_turn_still_restarts_it() {
        let a = turned(AgentState::Idle, 0, 1);
        let mut leases = vec![lease(&a)];
        assert_eq!(sends(&mut leases, &a, 1), 0);
        assert_eq!(sends(&mut leases, &turned(AgentState::Working, 5, 2), 5), 0);
        assert_eq!(
            sends(&mut leases, &turned(AgentState::DoneUnread, 7, 2), 10),
            0
        );
        assert_eq!(
            sends(&mut leases, &turned(AgentState::DoneUnread, 7, 2), 17),
            1
        );
    }

    #[test]
    fn an_ordinary_turn_hidden_behind_a_background_one_still_restarts() {
        // Between two looks: a person's turn (2) and then a notification's (3)
        // both begin and end. Only the background mark is left on the record.
        let a = turned(AgentState::Idle, 0, 1);
        let mut leases = vec![lease(&a)];
        sends(&mut leases, &a, 1);
        let mut both = background(AgentState::DoneUnread, 7, 3);
        both.ordinary_seq = 2;
        assert_eq!(sends(&mut leases, &both, 10), 0, "the clock restarted at 7");
        assert_eq!(sends(&mut leases, &both, 17), 1);
    }

    #[test]
    fn a_real_turn_after_background_ones_restarts_the_clock() {
        let a = turned(AgentState::Idle, 0, 1);
        let mut leases = vec![lease(&a)];
        sends(&mut leases, &a, 1);
        sends(&mut leases, &background(AgentState::DoneUnread, 4, 2), 4);
        // A person's prompt at 8: counted from its settle at 9.
        sends(&mut leases, &turned(AgentState::Working, 8, 3), 8);
        assert_eq!(
            sends(&mut leases, &turned(AgentState::DoneUnread, 9, 3), 18),
            0
        );
        assert_eq!(
            sends(&mut leases, &turned(AgentState::DoneUnread, 9, 3), 19),
            1
        );
    }

    #[test]
    fn a_background_question_or_error_neither_pauses_nor_clears() {
        for state in [AgentState::Waiting, AgentState::Error] {
            let a = turned(AgentState::Idle, 0, 1);
            let mut leases = vec![lease(&a)];
            sends(&mut leases, &a, 1);
            assert_eq!(sends(&mut leases, &background(state, 5, 2), 5), 0);
            assert!(leases[0].paused_reason.is_empty(), "{state:?}");
            assert_eq!(leases[0].idle_since, Some(at(0)), "{state:?}");
            assert_eq!(
                sends(&mut leases, &background(AgentState::DoneUnread, 6, 2), 10),
                1,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_background_turn_does_not_lift_the_exhausted_pause() {
        let a = turned(AgentState::Idle, 0, 1);
        let mut leases = vec![lease(&a)];
        sends(&mut leases, &a, 10);
        sends(&mut leases, &a, 20);
        sends(&mut leases, &a, 30);
        assert!(
            !leases[0].paused_reason.is_empty(),
            "two sends into silence pause it"
        );
        let reason = leases[0].paused_reason.clone();
        sends(&mut leases, &background(AgentState::Working, 31, 2), 31);
        assert_eq!(
            sends(&mut leases, &background(AgentState::DoneUnread, 32, 2), 60),
            0
        );
        assert_eq!(leases[0].paused_reason, reason);
        // A real turn is the answer.
        sends(&mut leases, &turned(AgentState::Working, 61, 3), 61);
        assert!(leases[0].paused_reason.is_empty());
    }

    #[test]
    fn done_and_blocked_still_land_during_a_background_turn() {
        let a = background(AgentState::Working, 5, 2);
        let mut leases = vec![lease(&a)];
        let got = plan(
            &mut leases,
            std::slice::from_ref(&a),
            &[pane_with_control("blocked")],
            at(5),
        );
        assert_eq!(got.consumed_blocks.len(), 1);
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);
        let got = plan(&mut leases, &[a], &[pane_with_control("done")], at(6));
        assert_eq!(got.stopped.len(), 1);
        assert!(leases.is_empty());
    }

    #[test]
    fn a_lease_armed_during_a_background_turn_counts_from_its_settle() {
        let a = background(AgentState::Working, 0, 2);
        let mut leases = vec![lease(&a)];
        assert_eq!(sends(&mut leases, &a, 1), 0);
        assert_eq!(leases[0].idle_since, None);
        let settled = background(AgentState::DoneUnread, 3, 2);
        assert_eq!(sends(&mut leases, &settled, 12), 0);
        assert_eq!(sends(&mut leases, &settled, 13), 1);
    }

    #[test]
    fn an_already_idle_agent_waits_ten_minutes_from_arm() {
        let a = agent(AgentState::Idle, -100);
        let mut leases = vec![lease(&a)];
        assert!(plan(&mut leases, std::slice::from_ref(&a), &[], at(9))
            .dispatches
            .is_empty());
        assert_eq!(plan(&mut leases, &[a], &[], at(10)).dispatches.len(), 1);
    }

    #[test]
    fn a_named_backend_dispatch_keeps_its_socket_namespace() {
        let mut a = agent(AgentState::Idle, 0);
        a.id = "codex-b0123456789abcdef-p1".into();
        a.pane.backend = "b0123456789abcdef".into();
        let mut leases = vec![lease(&a)];
        let pane = pane_with_control("");
        let panes = HashMap::from([(
            tower_mux::target_key(&a.pane.backend, &a.pane.pane_id),
            pane,
        )]);

        let got = plan_with_backends(&mut leases, &[a], &panes, at(10));
        assert_eq!(got.dispatches.len(), 1);
        assert_eq!(got.dispatches[0].backend, "b0123456789abcdef");
        assert_eq!(got.dispatches[0].pane_id, "p1");
    }

    #[test]
    fn a_disconnected_backend_neither_dispatches_nor_pauses_its_lease() {
        let mut a = agent(AgentState::Idle, 0);
        a.id = "codex-b0123456789abcdef-p1".into();
        a.pane.backend = "b0123456789abcdef".into();
        let mut leases = vec![lease(&a)];

        let got = plan_connected_backends(
            &mut leases,
            &[a],
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
            at(20),
        );
        assert!(got.dispatches.is_empty());
        assert_eq!(leases[0].sent_count, 0);
        assert!(leases[0].paused_reason.is_empty());
    }

    /// An agent a scheduled message just went to is not handed the loop's
    /// on top of it: the loop waits, unpaused, and sends once it is let go.
    #[test]
    fn a_loop_message_waits_while_the_agent_was_just_written_to() {
        let a = agent(AgentState::DoneUnread, 0);
        let mut leases = vec![lease(&a)];
        let connected = HashSet::from([a.pane.backend.clone()]);
        let held = HashSet::from([a.id.clone()]);
        let agents = std::slice::from_ref(&a);
        let got = plan_connected_backends(
            &mut leases,
            agents,
            &HashMap::new(),
            &connected,
            &held,
            at(10),
        );
        assert!(got.dispatches.is_empty());
        assert_eq!(leases[0].sent_count, 0);
        assert!(leases[0].paused_reason.is_empty());
        let got = plan_connected_backends(
            &mut leases,
            agents,
            &HashMap::new(),
            &connected,
            &HashSet::new(),
            at(10),
        );
        assert_eq!(got.dispatches.len(), 1);
    }

    #[test]
    fn same_idle_epoch_gets_one_retry_then_pauses() {
        let a = agent(AgentState::DoneUnread, 0);
        let mut leases = vec![lease(&a)];
        assert_eq!(
            plan(&mut leases, std::slice::from_ref(&a), &[], at(10))
                .dispatches
                .len(),
            1
        );
        assert_eq!(
            plan(&mut leases, std::slice::from_ref(&a), &[], at(20))
                .dispatches
                .len(),
            1
        );
        assert!(plan(&mut leases, &[a], &[], at(30)).dispatches.is_empty());
        assert!(leases[0].paused_reason.contains("two"));
        assert_eq!(leases[0].sent_count, 2);
    }

    #[test]
    fn observed_work_opens_a_fresh_idle_epoch() {
        let mut a = agent(AgentState::Idle, 0);
        let mut leases = vec![lease(&a)];
        assert_eq!(
            plan(&mut leases, &[a.clone()], &[], at(10))
                .dispatches
                .len(),
            1
        );
        a.state = AgentState::Working;
        a.state_since = at(11);
        plan(&mut leases, &[a.clone()], &[], at(11));
        a.state = AgentState::DoneUnread;
        a.state_since = at(12);
        assert!(plan(&mut leases, &[a.clone()], &[], at(21))
            .dispatches
            .is_empty());
        assert_eq!(plan(&mut leases, &[a], &[], at(22)).dispatches.len(), 1);
        assert_eq!(leases[0].sent_count, 2);
    }

    #[test]
    fn working_at_the_due_time_skips_delivery_until_a_full_settled_interval() {
        let mut a = agent(AgentState::Working, 0);
        let mut leases = vec![lease(&a)];
        assert!(plan(&mut leases, &[a.clone()], &[], at(20))
            .dispatches
            .is_empty());
        assert_eq!(leases[0].idle_since, None);

        a.state = AgentState::Idle;
        a.state_since = at(21);
        assert!(plan(&mut leases, &[a.clone()], &[], at(30))
            .dispatches
            .is_empty());
        assert_eq!(plan(&mut leases, &[a], &[], at(31)).dispatches.len(), 1);
    }

    /// The row being read is not the agent settling again: the clock keeps
    /// the end of the turn. Seen live — finished at :07, auto-read at :17,
    /// woken at :44 — a 30-minute loop counted 27 and never sent.
    #[test]
    fn reading_the_row_does_not_restart_the_idle_clock() {
        let a = turned(AgentState::Working, 0, 4);
        let mut leases = vec![lease(&a)];
        plan(&mut leases, &[a], &[], at(1));

        let done = turned(AgentState::DoneUnread, 2, 5);
        plan(&mut leases, &[done], &[], at(3));
        let read = turned(AgentState::Idle, 5, 5);
        assert!(plan(&mut leases, std::slice::from_ref(&read), &[], at(11))
            .dispatches
            .is_empty());
        assert_eq!(leases[0].idle_since, Some(at(2)), "the end of the turn");
        assert_eq!(
            plan(&mut leases, &[read], &[], at(12)).dispatches.len(),
            1,
            "ten minutes after the turn, not after the read"
        );
    }

    /// Nor does a read hand an unanswered send a fresh pair of attempts.
    #[test]
    fn reading_the_row_after_a_send_keeps_the_attempt_count() {
        let a = turned(AgentState::DoneUnread, 0, 4);
        let mut leases = vec![lease(&a)];
        assert_eq!(plan(&mut leases, &[a], &[], at(10)).dispatches.len(), 1);

        let read = turned(AgentState::Idle, 15, 4);
        assert_eq!(
            plan(&mut leases, std::slice::from_ref(&read), &[], at(20))
                .dispatches
                .len(),
            1
        );
        assert!(plan(&mut leases, &[read], &[], at(30))
            .dispatches
            .is_empty());
        assert_eq!(leases[0].sent_count, 2);
        assert!(leases[0].paused_reason.contains("two"));
    }

    /// Settling back from a question or an error without a turn
    /// starts the interval over — the old clock would fire at once, right
    /// after a person dismissed the prompt.
    #[test]
    fn settling_back_from_an_unsettled_state_restarts_the_clock() {
        for state in [AgentState::Waiting, AgentState::Error] {
            let a = turned(AgentState::Idle, 0, 4);
            let mut leases = vec![lease(&a)];
            plan(&mut leases, &[a], &[], at(1));
            plan(&mut leases, &[turned(state, 8, 4)], &[], at(8));

            let back = turned(AgentState::Idle, 9, 4);
            assert!(
                plan(&mut leases, std::slice::from_ref(&back), &[], at(10))
                    .dispatches
                    .is_empty(),
                "{state:?}: not at the old due time"
            );
            assert_eq!(leases[0].idle_since, Some(at(9)), "{state:?}");
            assert_eq!(
                plan(&mut leases, &[back], &[], at(19)).dispatches.len(),
                1,
                "{state:?}: a full interval after settling back"
            );
        }
    }

    #[test]
    fn unsafe_states_pause_without_sending() {
        for state in [AgentState::Waiting, AgentState::Error, AgentState::Dead] {
            let a = agent(state, 0);
            let mut leases = vec![lease(&a)];
            assert!(plan(&mut leases, &[a], &[], at(20)).dispatches.is_empty());
            assert!(!leases[0].paused_reason.is_empty());
        }
    }

    #[test]
    fn changed_native_session_disarms_the_lease() {
        let mut a = agent(AgentState::Idle, 0);
        let mut leases = vec![lease(&a)];
        a.session_id = "conversation-2".into();
        let got = plan(&mut leases, &[a], &[], at(1));
        assert!(leases.is_empty());
        assert_eq!(got.stopped[0].reason, "agent session changed");
    }

    #[test]
    fn agent_done_disarms_and_blocked_pauses() {
        let a = agent(AgentState::DoneUnread, 0);

        let mut done = vec![lease(&a)];
        let got = plan(
            &mut done,
            std::slice::from_ref(&a),
            &[pane_with_control("done")],
            at(1),
        );
        assert!(done.is_empty());
        assert_eq!(got.stopped[0].reason, "agent reported completion");

        let mut blocked = vec![lease(&a)];
        let got = plan(&mut blocked, &[a], &[pane_with_control("blocked")], at(20));
        assert!(got.dispatches.is_empty());
        assert!(blocked[0].paused_reason.contains("user input"));
    }

    /// The pause that started this: an agent asks one question, is answered,
    /// and goes back to work. herdr reports `blocked` for as long as the
    /// question is on screen and nothing afterwards, so a pause that is never
    /// re-derived outlives the question by the rest of the lease — a week.
    #[test]
    fn work_after_a_question_lifts_the_pause() {
        let waiting = agent(AgentState::Waiting, 0);
        let mut leases = vec![lease(&waiting)];
        assert!(plan(&mut leases, &[waiting], &[], at(20))
            .dispatches
            .is_empty());
        assert_eq!(leases[0].paused_reason, "agent is waiting for input");

        // 🚨 The turn counter does NOT move here, and must not be required to.
        // Answering a permission prompt is a `Waiting` → `Working` edge inside
        // the *same* turn, which `Agent::begin_turn` deliberately does not
        // count. An earlier version of this test raised the count by hand and
        // so tested nothing: production never does that, and requiring it left
        // the answered agent paused for the rest of its lease.
        let working = turned(AgentState::Working, 21, 1);
        plan(&mut leases, &[working], &[], at(22));
        assert!(
            leases[0].paused_reason.is_empty(),
            "an answered agent is not still waiting for input"
        );

        // And the loop resumes: settled again, it sends at the next due time.
        // Still the same turn count — the pause is already gone.
        let settled = turned(AgentState::Idle, 30, 1);
        assert!(plan(&mut leases, &[settled], &[], at(31))
            .dispatches
            .is_empty());
        let settled = turned(AgentState::Idle, 30, 1);
        assert_eq!(
            plan(&mut leases, &[settled], &[], at(41)).dispatches.len(),
            1,
            "the loop is running again"
        );
    }

    /// A finished turn is a resumable state, not a stopped loop — that is what
    /// the loop exists to follow. Only the agent's own `done` control token
    /// disarms the lease.
    #[test]
    fn a_finished_turn_lifts_the_pause_too() {
        for state in [AgentState::Idle, AgentState::DoneUnread] {
            let waiting = agent(AgentState::Waiting, 0);
            let mut leases = vec![lease(&waiting)];
            plan(&mut leases, &[waiting], &[], at(20));
            assert!(!leases[0].paused_reason.is_empty());

            // A settled record is where a turn counter is needed: this state
            // is also what `Store::mark_read` produces from a glance.
            let settled = turned(state, 21, 2);
            plan(&mut leases, &[settled], &[], at(22));
            assert!(
                leases[0].paused_reason.is_empty(),
                "{state:?} with a new turn behind it is movement"
            );
        }
    }

    /// The exhausted-attempts pause is not an observation about the agent, so
    /// it must not clear itself: two more prompts into the same silence on
    /// every pass is exactly what the attempt cap exists to prevent.
    #[test]
    fn an_exhausted_generation_keeps_its_pause_until_the_agent_moves() {
        let a = agent(AgentState::Idle, 0);
        let mut leases = vec![lease(&a)];
        assert_eq!(
            plan(&mut leases, std::slice::from_ref(&a), &[], at(10))
                .dispatches
                .len(),
            1
        );
        assert_eq!(
            plan(&mut leases, std::slice::from_ref(&a), &[], at(20))
                .dispatches
                .len(),
            1
        );
        assert!(plan(&mut leases, std::slice::from_ref(&a), &[], at(30))
            .dispatches
            .is_empty());
        assert!(leases[0].paused_reason.contains("two"));

        // Same generation, later pass: still paused, still silent.
        assert!(plan(&mut leases, std::slice::from_ref(&a), &[], at(90))
            .dispatches
            .is_empty());
        assert!(leases[0].paused_reason.contains("two"));

        // 🚨 A fresh `state_since` alone is not movement. `Store::mark_read`
        // stamps one when somebody merely opens the row, and clearing the
        // pause on that would send two more prompts into the same silence.
        let looked_at = turned(AgentState::Idle, 100, 1);
        assert!(plan(&mut leases, &[looked_at], &[], at(101))
            .dispatches
            .is_empty());
        assert!(
            leases[0].paused_reason.contains("two"),
            "reading a row is not the agent moving"
        );

        // A turn actually begun is.
        let moved = turned(AgentState::Idle, 100, 2);
        assert!(plan(&mut leases, &[moved], &[], at(102))
            .dispatches
            .is_empty());
        assert!(leases[0].paused_reason.is_empty());
        let moved = turned(AgentState::Idle, 100, 2);
        assert_eq!(
            plan(&mut leases, &[moved], &[], at(112)).dispatches.len(),
            1,
            "the fresh generation gets its own attempts"
        );
    }

    /// herdr recycles pane coordinates, and `tower_scan` replaces a dead
    /// record with a fresh `Agent` under the same deterministic ID. A lease
    /// armed without a session id has no way to tell that arrival from its own
    /// agent, so it must never resume into it — sending one worker's
    /// autonomous instruction into a stranger's pane. It disarms instead: a
    /// lease that can never resume has nothing left to do.
    #[test]
    fn a_dead_lease_does_not_resume_into_a_replacement_agent() {
        // A lease whose session id is known is already safe — any later
        // mismatch disarms it. What is left is the lease that was still blank
        // when the agent died: `tower_scan` replaces a dead record with a
        // fresh `Agent`, so the blank lease would otherwise adopt *the
        // replacement's* id when it arrives and every later comparison agree.
        let mut dead = turned(AgentState::Dead, 0, 7);
        dead.session_id = String::new();
        let mut leases = vec![lease(&dead)];
        assert!(leases[0].agent_session_id.is_empty(), "armed while blank");
        plan(&mut leases, &[dead], &[], at(5));
        assert_eq!(leases[0].paused_reason, PAUSED_DEAD);

        // The fresh record before herdr reports whose it is: nothing to
        // compare, so the pause holds and nothing is sent.
        let mut fresh = turned(AgentState::Idle, 30, 0);
        fresh.session_id = String::new();
        assert!(plan(&mut leases, &[fresh], &[], at(60))
            .dispatches
            .is_empty());
        assert_eq!(
            leases[0].paused_reason, PAUSED_DEAD,
            "the dead pause waits for an identity, not for the next occupant"
        );

        // Its session arrives; the blank lease cannot claim it.
        let mut replacement = turned(AgentState::Idle, 61, 1);
        replacement.session_id = "conversation-2".into();
        let got = plan(&mut leases, &[replacement], &[], at(62));
        assert!(got.dispatches.is_empty());
        assert!(leases.is_empty(), "disarmed, not paused for a week");
        assert_eq!(got.stopped[0].reason, "agent replaced after it died");
    }

    /// 🚨 The dead record is pruned after a day
    /// (`tower_scan::DEAD_RETENTION`), and this lease lasts a week. Once the
    /// record is gone the reason becomes `agent is missing`, so a guard that
    /// read death out of the reason string would expire with the record — and
    /// the replacement agent at the recycled coordinate would then be resumed
    /// into.
    #[test]
    fn death_outlives_the_record_that_proved_it() {
        let mut dead = turned(AgentState::Dead, 0, 7);
        dead.session_id = String::new();
        let mut leases = vec![lease(&dead)];
        plan(&mut leases, &[dead], &[], at(5));
        assert_eq!(leases[0].paused_reason, PAUSED_DEAD);
        assert!(
            leases[0].saw_dead,
            "the lease remembers, not just the reason"
        );

        // A day later the record is gone: no agent by that id at all.
        plan(&mut leases, &[], &[], at(1500));
        assert_eq!(
            leases[0].paused_reason, "agent is missing",
            "the reason is overwritten — which is why it cannot be the guard"
        );
        assert!(leases[0].saw_dead);

        // Someone else takes the coordinate and works.
        let mut newcomer = turned(AgentState::Working, 1501, 1);
        newcomer.session_id = "conversation-9".into();
        let got = plan(&mut leases, &[newcomer], &[], at(1502));
        assert!(got.dispatches.is_empty());
        assert!(
            leases.is_empty(),
            "this lease never resumes into a stranger"
        );
    }

    /// The death this lease saw was the process restarting: `claude --resume
    /// <id>` in the same pane. herdr reports no agent for the seconds between
    /// the exit and the relaunch, the scan marks the record dead, and the next
    /// hook revives it under the same session. It is the same conversation,
    /// and the loop goes on — it used to stay paused for the rest of its week.
    #[test]
    fn the_same_session_resumed_after_a_death_lifts_the_pause() {
        let a = turned(AgentState::Idle, 0, 7);
        let mut leases = vec![lease(&a)];
        assert_eq!(leases[0].agent_session_id, "conversation-1");

        let dead = turned(AgentState::Dead, 10, 7);
        plan(&mut leases, &[dead], &[], at(10));
        assert_eq!(leases[0].paused_reason, PAUSED_DEAD);
        assert!(leases[0].saw_dead);

        // Revived under the same session and settled again.
        let back = turned(AgentState::Idle, 11, 8);
        assert!(plan(&mut leases, std::slice::from_ref(&back), &[], at(11))
            .dispatches
            .is_empty());
        assert!(!leases[0].saw_dead, "the death was this agent's restart");
        assert!(leases[0].paused_reason.is_empty());

        // The idle clock starts from the revival, not from before the death.
        assert!(plan(&mut leases, std::slice::from_ref(&back), &[], at(20))
            .dispatches
            .is_empty());
        let got = plan(&mut leases, &[back], &[], at(21));
        assert_eq!(got.dispatches.len(), 1, "the loop resumes");
    }

    /// The same revival, caught mid-turn: the first pass after the relaunch
    /// sees the resumed agent already working.
    #[test]
    fn a_resumed_session_seen_working_lifts_the_pause() {
        let a = turned(AgentState::Idle, 0, 7);
        let mut leases = vec![lease(&a)];
        plan(&mut leases, &[turned(AgentState::Dead, 10, 7)], &[], at(10));
        assert!(leases[0].saw_dead);

        plan(
            &mut leases,
            &[turned(AgentState::Working, 11, 8)],
            &[],
            at(11),
        );
        assert!(!leases[0].saw_dead);
        assert!(leases[0].paused_reason.is_empty());
    }

    /// A revival whose session is not known yet proves nothing either way:
    /// the death holds until the record says whose it is.
    #[test]
    fn a_revival_without_a_session_keeps_the_death() {
        let a = turned(AgentState::Idle, 0, 7);
        let mut leases = vec![lease(&a)];
        plan(&mut leases, &[turned(AgentState::Dead, 10, 7)], &[], at(10));

        let mut anonymous = turned(AgentState::Idle, 11, 0);
        anonymous.session_id = String::new();
        assert!(plan(&mut leases, &[anonymous], &[], at(40))
            .dispatches
            .is_empty());
        assert!(leases[0].saw_dead);
        assert!(!leases[0].paused_reason.is_empty());
    }

    /// A different conversation after the death disarms, as it does without
    /// one: the session is the identity, the death does not change that.
    #[test]
    fn a_different_session_after_a_death_disarms() {
        let a = turned(AgentState::Idle, 0, 7);
        let mut leases = vec![lease(&a)];
        plan(&mut leases, &[turned(AgentState::Dead, 10, 7)], &[], at(10));

        let mut other = turned(AgentState::Idle, 11, 1);
        other.session_id = "conversation-2".into();
        let got = plan(&mut leases, &[other], &[], at(11));
        assert!(leases.is_empty());
        assert_eq!(got.stopped[0].reason, "agent session changed");
    }

    /// The same guard from the other side: a lease that knows whose session it
    /// is never resumes into a different one. (`changed_native_session_disarms
    /// _the_lease` covers the disarm; this covers the pause staying put while
    /// the identity is unknown.)
    #[test]
    fn a_pause_does_not_lift_while_the_session_is_unknown() {
        let mut waiting = turned(AgentState::Waiting, 0, 3);
        waiting.session_id = String::new();
        let mut leases = vec![lease(&waiting)];
        assert!(leases[0].agent_session_id.is_empty());
        plan(&mut leases, &[waiting], &[], at(20));
        assert_eq!(leases[0].paused_reason, "agent is waiting for input");

        // Still nameless, and now working. Work would normally lift a pause,
        // but nothing here identifies this as the agent the lease was armed
        // on — the coordinate is not an identity.
        let mut anonymous = turned(AgentState::Working, 21, 4);
        anonymous.session_id = String::new();
        plan(&mut leases, &[anonymous], &[], at(22));
        assert_eq!(
            leases[0].paused_reason, "agent is waiting for input",
            "a nameless agent cannot vouch for itself"
        );

        // Once herdr reports an identity, the lease adopts it and the next
        // turn lifts the pause as usual.
        let mut named = turned(AgentState::Working, 23, 5);
        named.session_id = "conversation-9".into();
        plan(&mut leases, &[named], &[], at(24));
        assert!(leases[0].paused_reason.is_empty());
        assert_eq!(leases[0].agent_session_id, "conversation-9");
    }

    /// Opening a finished row is not the agent working. `Store::mark_read`
    /// transitions `DoneUnread` to `Idle` and stamps `state_since`, so a
    /// timestamp-only rule would let a glance clear an exhausted pause.
    #[test]
    fn reading_a_finished_row_does_not_resume_an_exhausted_lease() {
        let a = turned(AgentState::DoneUnread, 0, 4);
        let mut leases = vec![lease(&a)];
        leases[0].paused_reason = "no work detected after two delivery attempts".into();
        leases[0].attempts_in_idle = MAX_IDLE_ATTEMPTS;

        // What `mark_read` leaves behind: Idle, a newer stamp, same turn count.
        let read = turned(AgentState::Idle, 30, 4);
        assert!(plan(&mut leases, &[read], &[], at(60))
            .dispatches
            .is_empty());
        assert!(leases[0].paused_reason.contains("two"));
    }

    /// An answered permission prompt resumes the loop however the supervisor
    /// happens to catch it.
    ///
    /// 🚨 Including the case where it catches nothing in between. The turn
    /// resumes and finishes inside one two-second interval, so `Working` is
    /// never sampled and — answering being part of the same turn —
    /// `work_seq` does not move either. The pause lifts because the agent is
    /// demonstrably no longer waiting, which is the whole of what that pause
    /// claimed.
    #[test]
    fn an_answered_question_resumes_however_it_is_observed() {
        for (label, seen_state, work_seq) in [
            ("caught working", AgentState::Working, 4),
            ("settled between polls, same turn", AgentState::Idle, 4),
            ("settled with a new turn behind it", AgentState::Idle, 5),
            ("finished the turn", AgentState::DoneUnread, 4),
        ] {
            let waiting = turned(AgentState::Waiting, 0, 4);
            let mut leases = vec![lease(&waiting)];
            plan(&mut leases, &[waiting], &[], at(20));
            assert_eq!(leases[0].paused_reason, PAUSED_WAITING);

            let after = turned(seen_state, 21, work_seq);
            plan(&mut leases, &[after], &[], at(22));
            assert!(
                leases[0].paused_reason.is_empty(),
                "{label}: the agent is not waiting any more"
            );
        }
    }

    /// The counter is what an *exhausted* pause waits for, and that is the
    /// pause a glance can forge: `Store::mark_read` turns `DoneUnread` into
    /// `Idle` with a fresh stamp and no turn behind it.
    #[test]
    fn only_a_turn_resumes_an_exhausted_lease() {
        for (label, work_seq, resumes) in
            [("row merely read", 4, false), ("a new turn begun", 5, true)]
        {
            let a = turned(AgentState::DoneUnread, 0, 4);
            let mut leases = vec![lease(&a)];
            leases[0].paused_reason = "no work detected after two delivery attempts".into();
            leases[0].attempts_in_idle = MAX_IDLE_ATTEMPTS;

            let after = turned(AgentState::Idle, 21, work_seq);
            plan(&mut leases, &[after], &[], at(22));
            assert_eq!(
                leases[0].paused_reason.is_empty(),
                resumes,
                "{label}: expected resumes={resumes}"
            );
        }
    }

    /// `herdr-tower loop blocked` is the agent's own statement that a person
    /// is needed. The pane token it wrote can vanish by itself — a herdr
    /// metadata reset or restart drops it — and the pause must not go with it.
    #[test]
    fn an_explicit_blocked_pause_survives_losing_its_token() {
        let a = turned(AgentState::Idle, 0, 5);
        let mut leases = vec![lease(&a)];
        let a = turned(AgentState::Idle, 0, 5);
        plan(&mut leases, &[a], &[pane_with_control("blocked")], at(20));
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);

        // Token gone, agent settled, no new turn.
        let a = turned(AgentState::Idle, 30, 5);
        assert!(plan(&mut leases, &[a], &[], at(60)).dispatches.is_empty());
        assert_eq!(
            leases[0].paused_reason, PAUSED_BLOCKED,
            "a missing token is not the agent saying it is unblocked"
        );

        // 🚨 Nor is being seen working: the turn it blocked *in* can still be
        // running, and that is not an answer to the question it asked.
        let a = turned(AgentState::Working, 61, 5);
        plan(&mut leases, &[a], &[], at(62));
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);

        // A turn that came after the block is.
        let a = turned(AgentState::Idle, 70, 6);
        plan(&mut leases, &[a], &[], at(71));
        assert!(leases[0].paused_reason.is_empty());
    }

    /// The token can first be seen *after* its turn advanced, because the
    /// supervisor samples every two seconds. That turn is not an answer to the
    /// block, so the counter has to be snapshotted where the token is read.
    #[test]
    fn a_block_first_seen_after_its_turn_still_waits_for_the_next_one() {
        let a = turned(AgentState::Idle, 0, 5);
        let mut leases = vec![lease(&a)];

        // Between arming and this pass the agent took a turn and blocked.
        let a = turned(AgentState::Idle, 10, 6);
        plan(&mut leases, &[a], &[pane_with_control("blocked")], at(11));
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);
        assert_eq!(leases[0].seen_work_seq, 6, "the block's own turn");

        // Metadata reset drops the token; the same turn is still the latest.
        let a = turned(AgentState::Idle, 10, 6);
        assert!(plan(&mut leases, &[a], &[], at(60)).dispatches.is_empty());
        assert_eq!(
            leases[0].paused_reason, PAUSED_BLOCKED,
            "the turn it blocked in cannot answer for itself"
        );
    }

    /// The token has no TTL: it stands until the lease is armed again, so the
    /// blocked branch runs on every poll. The baseline must be taken on the
    /// way in and then left alone — otherwise it keeps advancing to the latest
    /// turn and the answer the user eventually gives never registers.
    /// The token is taken into the lease and asked to be cleared, so the fact
    /// lives where a later turn can answer it — and the pane is free to say it
    /// again.
    #[test]
    fn a_block_is_consumed_into_the_lease() {
        let a = turned(AgentState::Idle, 0, 5);
        let mut leases = vec![lease(&a)];

        let a = turned(AgentState::Idle, 10, 5);
        let got = plan(&mut leases, &[a], &[pane_with_control("blocked")], at(11));
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);
        assert_eq!(leases[0].seen_work_seq, 5, "the block's own turn");
        assert_eq!(
            got.consumed_blocks,
            vec![PaneTarget {
                backend: String::new(),
                pane_id: "p1".into(),
                work_seq: 5,
            }],
            "and the supervisor is asked to clear it"
        );

        // Clearing can fail; the next pass takes it again. The same turn is
        // the same block, so nothing moves.
        let a = turned(AgentState::Idle, 20, 5);
        plan(&mut leases, &[a], &[pane_with_control("blocked")], at(21));
        assert_eq!(leases[0].seen_work_seq, 5, "not moved by a re-read");

        // 🚨 But a token still standing on a *later* turn is a second report:
        // the first was answered and the agent blocked again, all before this
        // poll. Keeping the old baseline would let the turn that led to this
        // block read as the answer to it.
        let a = turned(AgentState::Idle, 25, 6);
        plan(&mut leases, &[a], &[pane_with_control("blocked")], at(26));
        assert_eq!(leases[0].seen_work_seq, 6, "the second block's turn");
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);

        // And that second block waits for its own answer.
        let a = turned(AgentState::Idle, 30, 6);
        plan(&mut leases, &[a], &[], at(31));
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);

        // Cleared, and a turn after it: answered.
        let a = turned(AgentState::Idle, 40, 7);
        plan(&mut leases, &[a], &[], at(41));
        assert!(leases[0].paused_reason.is_empty());
    }

    /// The token can vanish while the agent is in a state that writes its own
    /// pause. If that reason replaced the explicit block, the protection would
    /// go with it — and a same-turn return to `Working` clears a state-derived
    /// pause.
    #[test]
    fn a_state_observation_does_not_overwrite_an_explicit_block() {
        for state in [AgentState::Waiting, AgentState::Error] {
            let a = turned(AgentState::Idle, 0, 5);
            let mut leases = vec![lease(&a)];
            let a = turned(AgentState::Idle, 0, 5);
            plan(&mut leases, &[a], &[pane_with_control("blocked")], at(10));
            assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);

            // Token gone, agent now in a state with a pause of its own.
            let a = turned(state, 20, 5);
            plan(&mut leases, &[a], &[], at(21));
            assert_eq!(
                leases[0].paused_reason, PAUSED_BLOCKED,
                "{state:?} must not replace the agent's own statement"
            );

            // Same turn, back to working: a state-derived pause would clear
            // here, the block does not.
            let a = turned(AgentState::Working, 22, 5);
            plan(&mut leases, &[a], &[], at(23));
            assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);
        }

        // The record disappearing does not replace it either.
        let a = turned(AgentState::Idle, 0, 5);
        let mut leases = vec![lease(&a)];
        let a = turned(AgentState::Idle, 0, 5);
        plan(&mut leases, &[a], &[pane_with_control("blocked")], at(10));
        plan(&mut leases, &[], &[], at(30));
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);
    }

    /// 🚨 A turn confirmed by the counter starts a fresh attempt generation
    /// even when `state_since` did not move — the between-polls case the
    /// counter exists for. Otherwise the pause is cleared and written straight
    /// back in the same pass, and the real turn never resumes the loop.
    #[test]
    fn a_counted_turn_starts_a_fresh_attempt_generation() {
        let a = turned(AgentState::Idle, 0, 4);
        let mut leases = vec![lease(&a)];
        leases[0].paused_reason = "no work detected after two delivery attempts".into();
        leases[0].attempts_in_idle = MAX_IDLE_ATTEMPTS;
        leases[0].idle_since = Some(at(0));
        leases[0].retry_at = Some(at(10));

        // Same timestamp, higher turn count: the turn happened between polls.
        // The pause lifts, the generation starts over, and — the interval
        // having long since passed — this same pass sends again.
        let a = turned(AgentState::Idle, 0, 5);
        assert_eq!(
            plan(&mut leases, &[a], &[], at(60)).dispatches.len(),
            1,
            "the loop runs again on the fresh generation"
        );
        assert!(leases[0].paused_reason.is_empty(), "and it stays cleared");
        assert_eq!(leases[0].attempts_in_idle, 1, "one of the new two");
    }

    /// A pane that is still showing the question stays paused, however many
    /// passes go by — the control token is the agent's own word.
    #[test]
    fn a_standing_question_keeps_the_pause() {
        let a = agent(AgentState::Idle, 0);
        let mut leases = vec![lease(&a)];
        for minute in [20, 40, 60] {
            let a = agent(AgentState::Idle, 0);
            let got = plan(
                &mut leases,
                &[a],
                &[pane_with_control("blocked")],
                at(minute),
            );
            assert!(got.dispatches.is_empty());
            assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);
        }
    }

    /// Once the token has been consumed, a turn begun after the block answers
    /// it and the loop runs again.
    #[test]
    fn a_turn_after_the_block_resumes_the_loop() {
        let a = turned(AgentState::Idle, 0, 5);
        let mut leases = vec![lease(&a)];
        let a = turned(AgentState::Idle, 10, 5);
        plan(&mut leases, &[a], &[pane_with_control("blocked")], at(11));
        assert_eq!(leases[0].paused_reason, PAUSED_BLOCKED);

        // The user answers; the agent begins a turn.
        let a = turned(AgentState::Working, 20, 6);
        plan(&mut leases, &[a], &[], at(21));
        assert!(
            leases[0].paused_reason.is_empty(),
            "a turn begun after the block is the answer to it"
        );

        let a = turned(AgentState::Idle, 30, 6);
        assert!(plan(&mut leases, &[a], &[], at(31)).dispatches.is_empty());
        let a = turned(AgentState::Idle, 30, 6);
        assert_eq!(plan(&mut leases, &[a], &[], at(41)).dispatches.len(), 1);
    }

    #[test]
    fn a_finite_transmission_cap_stops_after_the_reserved_send() {
        let a = agent(AgentState::Idle, 0);
        let mut one = lease(&a);
        one.max_turns = Some(1);
        let mut leases = vec![one];
        assert_eq!(
            plan(&mut leases, std::slice::from_ref(&a), &[], at(10))
                .dispatches
                .len(),
            1
        );
        let stopped = plan(&mut leases, &[a], &[], at(10));
        assert!(leases.is_empty());
        assert_eq!(stopped.stopped[0].reason, "transmission limit reached");
    }

    #[test]
    fn the_prompt_preserves_scope_and_carries_the_optional_instruction() {
        let a = agent(AgentState::Idle, 0);
        let mut l = lease(&a);
        l.sent_count = 7;
        l.instruction = "issue #123 안에서만 작업".into();
        let got = message(&l, at(1));
        assert!(
            got.starts_with("[Herdr auto-continue · send 7/unlimited · "),
            "{got}"
        );
        assert!(got.contains("\n\nAdditional instructions:\n"), "{got}");
        assert!(got.contains("Proceed with the next approved task and its tests"));
        assert!(got.contains("herdr-tower loop done"));
        assert!(got.contains("herdr-tower loop blocked"));
        assert!(got.contains("issue #123 안에서만 작업"));
    }

    /// What the tower writes itself is English; what the person wrote goes
    /// out as they wrote it, in whatever language.
    #[test]
    fn only_the_person_s_own_words_may_be_in_another_language() {
        let hangul = |s: &str| s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c));
        let a = agent(AgentState::Idle, 0);
        let mut l = lease(&a);
        l.instruction = "extra".into();
        assert!(
            !hangul(&message(&l, at(1))),
            "the header, default body and label"
        );
        for preset in [tower_ui::WAKE_MESSAGE, tower_ui::CLOSE_MESSAGE] {
            assert!(!hangul(preset), "{preset}");
        }
        assert!(!hangul(tower_state::DEFAULT_LOOP_MESSAGE));
        l.message = "한글 지시".into();
        assert!(message(&l, at(1)).contains("한글 지시"));
    }

    #[test]
    fn a_lease_message_replaces_the_default_body_without_losing_the_header() {
        let a = agent(AgentState::Idle, 0);
        let mut l = lease(&a);
        l.sent_count = 2;
        l.message = "이 세션의 테스트만 이어서 실행하세요.".into();
        let got = message(&l, at(1));
        assert!(got.contains("send 2/unlimited"), "{got}");
        assert!(got.contains("이 세션의 테스트만 이어서 실행하세요."));
        assert!(!got.contains("Proceed with the next approved task"));
    }

    fn backend(socket: &str, connected: bool, error: &str) -> DaemonBackend {
        DaemonBackend {
            id: String::new(),
            socket: socket.into(),
            connected,
            error: error.into(),
            refreshed: false,
        }
    }

    /// The first pass says where every backend stands; after that, only a
    /// change is a line — a backend that stays down is not one per pass.
    #[test]
    fn the_log_hears_about_a_backend_only_when_it_changes() {
        let mut last = HashMap::new();
        let up = [backend("/tmp/a.sock", true, "")];
        assert_eq!(backend_changes(&mut last, &up), ["/tmp/a.sock: connected"]);
        assert!(backend_changes(&mut last, &up).is_empty(), "unchanged");

        let down = [backend("/tmp/a.sock", false, "herdr unavailable: refused")];
        assert_eq!(
            backend_changes(&mut last, &down),
            ["/tmp/a.sock: herdr unavailable: refused"]
        );
        assert!(backend_changes(&mut last, &down).is_empty(), "still down");
        assert_eq!(backend_changes(&mut last, &up), ["/tmp/a.sock: connected"]);
    }

    /// A different error on a backend that is still down is news too: it is
    /// the reason that changed.
    #[test]
    fn a_new_reason_for_the_same_trouble_is_logged() {
        let mut last = HashMap::new();
        let _ = backend_changes(&mut last, &[backend("/s", true, "pane listing failed: x")]);
        assert_eq!(
            backend_changes(&mut last, &[backend("/s", true, "pane listing failed: y")]),
            ["/s: pane listing failed: y"]
        );
    }

    #[test]
    fn a_backend_that_disappears_is_logged_once() {
        let mut last = HashMap::new();
        let _ = backend_changes(
            &mut last,
            &[backend("/a", true, ""), backend("/b", true, "")],
        );
        let only_a = [backend("/a", true, "")];
        assert_eq!(backend_changes(&mut last, &only_a), ["/b: no longer found"]);
        assert!(backend_changes(&mut last, &only_a).is_empty());
    }

    fn log_text(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join(crate::supervisorlog::LOG_FILE)).unwrap_or_default()
    }

    /// Trouble is logged when it starts, again only if it changes, and once
    /// when it is over — not once a pass while it lasts.
    #[test]
    fn a_failing_pass_is_logged_once_until_it_changes_or_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = crate::supervisorlog::Log::open(dir.path());
        let mut trouble = Trouble::default();
        trouble.failed(&mut log, "writing daemon.json: disk full");
        trouble.failed(&mut log, "writing daemon.json: disk full");
        trouble.failed(&mut log, "reading the loop leases: bad json");
        trouble.fine(&mut log);
        trouble.fine(&mut log);
        let text = log_text(dir.path());
        let events: Vec<_> = text.lines().map(|l| l.split_once(' ').unwrap().1).collect();
        assert_eq!(
            events,
            [
                "a pass failed, trying again every 2s: writing daemon.json: disk full",
                "a pass failed, trying again every 2s: reading the loop leases: bad json",
                "passes are succeeding again",
            ]
        );
    }

    /// What a pass cost at the store lock, in milliseconds.
    fn lock(held: u64, longest: u64, waited: u64) -> LockTimes {
        LockTimes {
            held: Duration::from_millis(held),
            longest: Duration::from_millis(longest),
            waited: Duration::from_millis(waited),
        }
    }

    fn events(dir: &std::path::Path) -> Vec<String> {
        log_text(dir)
            .lines()
            .map(|l| l.split_once(' ').unwrap().1.to_string())
            .collect()
    }

    /// 🚨 A pass starts its store-lock count from zero. Carrying the last pass's
    /// holds over would report a pass as slow for time it did not spend, and the
    /// supervisor's slow-pass line is the only record of where a pass went.
    #[test]
    fn a_pass_starts_its_lock_count_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path()).unwrap();
        let mut book = PassBook::default();

        // Something held the lock before this pass began.
        let held = store.lock().expect("the lock file has to be creatable");
        std::thread::sleep(Duration::from_millis(20));
        drop(held);

        let started = book.begin(&store);
        assert_eq!(
            store.take_lock_times(),
            tower_state::LockTimes::default(),
            "begin() left a count behind"
        );
        assert!(started.elapsed() < SLOW_PASS, "the pass has just started");
    }

    /// The sweep is hourly, not every pass — and the first pass sweeps, because
    /// what only ever grows on disk is swept at start and then on the hour.
    ///
    /// The clock is handed in, which is the whole point of the seam: the hour
    /// cannot be waited for in a test, and before this it was `Instant::now()`
    /// inside a loop that needs a herdr server to run at all (#46).
    #[test]
    fn the_sweep_is_hourly_and_the_first_pass_sweeps() {
        let mut book = PassBook::default();
        let start = std::time::Instant::now();
        assert!(book.due_to_sweep(start), "the first pass sweeps");
        assert!(!book.due_to_sweep(start), "and not again on the same pass");
        assert!(
            !book.due_to_sweep(start + HOUSEKEEPING_EVERY - Duration::from_secs(1)),
            "a minute short of the hour is not due"
        );
        assert!(book.due_to_sweep(start + HOUSEKEEPING_EVERY), "the hour is");
        assert!(
            !book.due_to_sweep(start + HOUSEKEEPING_EVERY + Duration::from_secs(1)),
            "and the clock restarts from the sweep that just happened"
        );
        assert!(book.due_to_sweep(start + HOUSEKEEPING_EVERY * 2));
    }

    /// A pass that ended before it read the records says so, rather than
    /// claiming an empty store.
    ///
    /// 🚨 This is the pass the log exists for (#42): a herdr call that took its
    /// time and then failed. Reporting `0 agent records` for it would read as a
    /// store with nothing in it, which is a different fault with a different fix.
    #[test]
    fn a_pass_that_read_no_records_is_not_a_pass_over_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = crate::supervisorlog::Log::open(dir.path());
        SlowPasses::default().observe(&mut log, SLOW_PASS, lock(0, 0, 0), None);
        let said = events(dir.path());
        assert!(
            said[0].contains("no records read"),
            "an early pass must not claim a count: {said:?}"
        );
        assert!(!said[0].contains("0 agent"), "{said:?}");

        // A pass that did read an empty store still says the count it read.
        let empty = tempfile::tempdir().unwrap();
        let mut log = crate::supervisorlog::Log::open(empty.path());
        SlowPasses::default().observe(&mut log, SLOW_PASS, lock(0, 0, 0), Some(0));
        assert!(
            events(empty.path())[0].contains("0 agent records"),
            "{:?}",
            events(empty.path())
        );
    }

    /// 🚨 One exit, checked in the source. The pass body is a labelled block and
    /// the observation sits after it, so every way out of a pass is counted —
    /// which is what #42 was: an `or_retry!` failure and a disconnected snapshot
    /// both `continue`d past the observation, so the slow pass that then failed
    /// was never logged.
    ///
    /// The plain mistake cannot come back: `continue` with no label inside a
    /// labelled block is `error[E0695]`, which the compiler refuses — measured by
    /// putting each `continue` back. What it does not refuse is a `continue` that
    /// names an enclosing loop, and that would skip the observation just as
    /// quietly. No unit test can see it either until `supervise_logged` has a
    /// seam (#46), so this reads the source.
    #[test]
    fn every_exit_from_a_pass_goes_through_the_observation() {
        let src = include_str!("loopcmd.rs");
        let (_, after_macro) = src
            .split_once("macro_rules! or_retry {")
            .expect("the macro");
        let (macro_body, _) = after_macro.split_once("\n    }").expect("the macro ends");
        assert!(
            macro_body.contains("break $label;") && !macro_body.contains("continue"),
            "or_retry! must leave the pass by its label: {macro_body}"
        );

        let (_, body) = src.split_once("'pass: {").expect("the pass block");
        let (body, tail) = body
            .split_once("        // The one place every pass is counted")
            .expect("the observation follows the block");
        assert!(
            !body.contains("continue"),
            "a `continue` inside the pass body skips the observation:\n{}",
            body.lines()
                .filter(|l| l.contains("continue"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            tail.starts_with(", however it ended.\n        book.end("),
            "the observation must be the first thing after the block: {}",
            &tail[..tail.len().min(120)]
        );
    }

    /// A slow pass is one line when passes turn slow and one when they recover,
    /// not a line every two seconds while it lasts.
    #[test]
    fn slow_passes_are_logged_once_until_they_recover() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = crate::supervisorlog::Log::open(dir.path());
        let mut slow = SlowPasses::default();
        let ms = Duration::from_millis;
        slow.observe(&mut log, ms(40), lock(30, 5, 0), Some(9));
        slow.observe(&mut log, ms(700), lock(640, 90, 0), Some(100));
        slow.observe(&mut log, ms(900), lock(850, 95, 0), Some(100));
        slow.observe(&mut log, ms(30), lock(20, 4, 0), Some(100));
        slow.observe(&mut log, ms(20), lock(10, 3, 0), Some(100));
        assert_eq!(
            events(dir.path()),
            [
                "passes are slow: 700 ms with 100 agent records — 640 ms holding the store lock (longest 90 ms), 0 ms waiting for it",
                "passes are quick again: 30 ms",
            ]
        );
    }

    /// The three ways a pass is slow read apart: on herdr's socket — a server
    /// starting up, say — it shows little of the lock; waiting behind another
    /// holder, it shows the wait, which hooks sat through too.
    #[test]
    fn a_slow_pass_says_whether_it_held_or_waited_for_the_store_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = crate::supervisorlog::Log::open(dir.path());
        let mut slow = SlowPasses::default();
        let ms = Duration::from_millis;
        slow.observe(&mut log, ms(820), lock(3, 2, 0), Some(4));
        slow.observe(&mut log, ms(30), lock(3, 2, 0), Some(4));
        slow.observe(&mut log, ms(650), lock(4, 4, 600), Some(90));
        assert_eq!(
            events(dir.path()),
            [
                "passes are slow: 820 ms with 4 agent records — 3 ms holding the store lock (longest 2 ms), 0 ms waiting for it",
                "passes are quick again: 30 ms",
                "passes are slow: 650 ms with 90 agent records — 4 ms holding the store lock (longest 4 ms), 600 ms waiting for it",
            ]
        );
    }

    /// A hold longer than a hook waits is said when it happens, whether or not
    /// the pass is slow — and not for many short holds that only add up to as
    /// much, since a hook waits for one hold at a time. It is said once, even
    /// in the middle of a slow stretch that began for another reason.
    #[test]
    fn a_hold_longer_than_a_hook_waits_is_its_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = crate::supervisorlog::Log::open(dir.path());
        let mut slow = SlowPasses::default();
        let ms = Duration::from_millis;
        slow.observe(&mut log, ms(420), lock(400, 60, 0), Some(40));
        slow.observe(&mut log, ms(450), lock(400, 400, 0), Some(40));
        slow.observe(&mut log, ms(820), lock(600, 590, 0), Some(40));
        slow.observe(&mut log, ms(40), lock(20, 20, 0), Some(40));
        assert_eq!(
            events(dir.path()),
            [
                "the store lock was held 400 ms at a stretch; hooks give up on it after 150 ms and write without it",
                "passes are slow: 820 ms with 40 agent records — 600 ms holding the store lock (longest 590 ms), 0 ms waiting for it",
                "passes are quick again: 40 ms",
                "store lock holds are short again: longest 20 ms",
            ]
        );
    }

    /// Property: the change detection is idempotent — a snapshot seen twice
    /// is news once — and after any sequence of snapshots, what it remembers
    /// is exactly the last one.
    #[test]
    fn backend_changes_are_news_once_and_remember_the_last_snapshot() {
        let states = [
            (true, ""),
            (false, "herdr unavailable: refused"),
            (true, "pane listing failed: x"),
        ];
        let mut last = HashMap::new();
        let mut seed: u64 = 7;
        for _ in 0..200 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let snapshot: Vec<DaemonBackend> = ["/a", "/b", "/c"]
                .iter()
                .enumerate()
                .filter(|(i, _)| (seed >> (i * 3)) & 3 != 0)
                .map(|(i, socket)| {
                    let (connected, error) = states[((seed >> (10 + i * 2)) % 3) as usize];
                    backend(socket, connected, error)
                })
                .collect();
            let _ = backend_changes(&mut last, &snapshot);
            assert!(
                backend_changes(&mut last, &snapshot).is_empty(),
                "seen twice, news once"
            );
            let remembered: HashMap<String, (bool, String)> = snapshot
                .iter()
                .map(|b| (b.socket.clone(), (b.connected, b.error.clone())))
                .collect();
            assert_eq!(last, remembered);
        }
    }

    /// No client for the backend sends nothing, so it is certainly not
    /// delivered.
    #[test]
    fn a_missing_backend_is_not_delivered() {
        let got = deliver(&HashMap::new(), "b0000000000000000", "w2:p1", "hi");
        assert!(
            matches!(got, Delivered::No(ref e) if e == "backend is unavailable"),
            "{got:?}"
        );
    }

    /// The supervisor's sends against a fake herdr, one way each can go: a
    /// reply, a refusal, a prompt whose reply misses the deadline after the
    /// text went out, and a lookup that misses it before anything did.
    #[cfg(unix)]
    #[test]
    fn deliver_sorts_each_outcome() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        const AGENT: &str = r#"{"id":"$ID","result":{"pane":{"pane_id":"w2:p1","workspace_id":"w2","agent":"claude","agent_status":"idle"}}}"#;
        const OK: &str = r#"{"id":"$ID","result":{"type":"ok"}}"#;
        const NO: &str = r#"{"id":"$ID","error":{"code":"agent_busy","message":"busy"}}"#;
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

        // `prompt` answers the prompt; `late` is the method answered too late.
        let fake = |prompt: &'static str, late: &'static str| {
            let dir = std::env::temp_dir().join(format!(
                "ld{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let socket = dir.join("h.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { return };
                    let mut line = String::new();
                    if BufReader::new(conn.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap_or(0)
                        == 0
                    {
                        continue;
                    }
                    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap_or_default();
                    if method == late {
                        std::thread::sleep(Duration::from_millis(600));
                    }
                    let body = if method == "pane.get" { AGENT } else { prompt };
                    let body = body.replace("$ID", req["id"].as_str().unwrap_or(""));
                    let _ = conn.write_all(format!("{body}\n").as_bytes());
                }
            });
            let mut h = Herdr::at(&socket);
            h.client.timeout = Some(Duration::from_millis(200));
            (dir, HashMap::from([(String::new(), h)]))
        };

        let sent = |prompt, late| {
            let (dir, clients) = fake(prompt, late);
            let got = deliver(&clients, "", "w2:p1", "hi");
            let _ = std::fs::remove_dir_all(dir);
            got
        };
        assert!(matches!(sent(OK, ""), Delivered::Yes));
        assert!(matches!(sent(NO, ""), Delivered::No(_)));
        assert!(matches!(sent(OK, "agent.prompt"), Delivered::Maybe(_)));
        assert!(matches!(sent(OK, "pane.get"), Delivered::No(_)));
    }
}
