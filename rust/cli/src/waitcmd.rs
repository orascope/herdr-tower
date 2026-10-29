// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower wt wait` — blocking until a worker is finished with its turn.
//!
//! This exists so an orchestration script does not have to poll. It used to be
//! a person doing it: the skill told the operator to check every thirty
//! seconds, which is a loop with a human in it.
//!
//! Waiting is handed to herdr, which knows when its own agents settle, so the
//! caller asks once and the server answers once. There is no polling fallback:
//! no backend exists that cannot answer, so a loop re-reading the store from
//! disk would only be an unreachable branch.
//!
//! WAITING counts as finished. An agent sitting on an approval prompt is not
//! going anywhere without a person, so blocking on it would hang the script
//! forever; the state comes back as it is and the caller decides whether that
//! is a success.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use herdrx::Herdr;
use time::OffsetDateTime;
use tower_state::{Agent, AgentState, Store};

use crate::infocmd::state_word;
use crate::pad_right;

/// How long to wait when the caller does not say.
///
/// Generous, because this is a worker being left to work. Not infinite, because
/// a script that hangs forever gives its caller no way to learn why.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The states that end a wait.
///
/// Every one of them maps onto a herdr status, which matters: `tower_mux::
/// wait_for_agent` refuses a request whose states herdr has no word for rather
/// than silently widening it into herdr's default of "wait for anything". If
/// this list ever grows an unmappable state (ERROR, DEAD), that refusal is a
/// real error here and not a fallback.
const UNTIL: [AgentState; 3] = [
    AgentState::DoneUnread,
    AgentState::Waiting,
    AgentState::Idle,
];

/// The states that are evidence a turn actually ran.
///
/// IDLE is not among them, and that is the whole point. It is what a worker
/// looks like *after* a finished turn that somebody read, and equally what it
/// looks like between `wt new` and its first prompt — so accepting it as
/// completion made the documented sequence (`wt new` → `herdr pane run` → `wt
/// wait`) return success immediately whenever `wait` arrived before the first
/// hook, without ever contacting the backend. The orchestrator then collected a
/// report for a turn that had not started.
const OUTCOME: [AgentState; 2] = [AgentState::DoneUnread, AgentState::Waiting];

/// How long a dispatched turn is given to *begin* before the wait gives up on
/// it.
///
/// Generous against a cold CLI — `claude` booting and reaching its first hook is
/// seconds, not milliseconds — and short against the half-hour a turn itself
/// may take, because this window is only spent when something is wrong.
const START_GRACE: Duration = Duration::from_secs(60);

/// How one task's wait ended.
#[derive(Debug, Clone)]
pub struct WaitResult {
    pub task: String,
    /// The last state actually read. `None` when the worker was never found, so
    /// there was no state to read — an empty string here would print as a
    /// blank column and read as a rendering bug.
    pub state: Option<AgentState>,
    /// The refusal, already worded for a person.
    pub error: Option<String>,
}

impl WaitResult {
    /// Finished with nothing for a person to do.
    pub fn done(&self) -> bool {
        self.state == Some(AgentState::DoneUnread)
    }

    fn failed(task: &str, error: String) -> WaitResult {
        WaitResult {
            task: task.to_string(),
            state: None,
            error: Some(error),
        }
    }
}

/// Waits for each task's worker in turn, against one shared deadline.
///
/// One deadline for the whole call, not one per task: `--timeout 30m` on four
/// tasks means the command returns within thirty minutes, which is what the
/// caller can plan around. Per-task timeouts would make the worst case two
/// hours.
pub fn wait_tasks(
    store: &Store,
    h: &Herdr,
    state_dir: &Path,
    tasks: &[String],
    timeout: Duration,
    now: &dyn Fn() -> OffsetDateTime,
) -> Vec<WaitResult> {
    let timeout = if timeout.is_zero() {
        DEFAULT_WAIT_TIMEOUT
    } else {
        timeout
    };
    let deadline = now() + timeout;
    tasks
        .iter()
        .map(|t| wait_one(store, h, state_dir, t, deadline, now))
        .collect()
}

fn wait_one(
    store: &Store,
    h: &Herdr,
    state_dir: &Path,
    task: &str,
    deadline: OffsetDateTime,
    now: &dyn Fn() -> OffsetDateTime,
) -> WaitResult {
    let a = match agent_for_task(store, state_dir, task) {
        Ok(a) => a,
        Err(e) => return WaitResult::failed(task, e),
    };
    // Already finished: there is nothing to wait for, and asking the server
    // would block on a transition that has already happened. Only an *outcome*
    // counts — see [`OUTCOME`] for why IDLE does not.
    if OUTCOME.contains(&a.state) {
        return WaitResult {
            task: task.to_string(),
            state: Some(a.state),
            error: None,
        };
    }

    let left = deadline - now();
    if !left.is_positive() {
        return WaitResult {
            task: task.to_string(),
            state: Some(a.state),
            error: Some("timed out".to_string()),
        };
    }

    // Nothing says a turn is under way yet, so wait for one to begin before
    // waiting for it to end. herdr's own view is what is asked, not the store's:
    // its WORKING comes from watching the pane, so it does not depend on this
    // agent's hooks being installed.
    if a.state != AgentState::Working {
        let grace = left.unsigned_abs().min(START_GRACE);
        if let Err(e) =
            tower_mux::wait_for_agent(h, &a.pane.pane_id, &[AgentState::Working], Some(grace))
        {
            // Either the dispatch never landed, or the whole turn fitted inside
            // the grace window — a turn that short leaves its outcome in the
            // record, so the record is what decides. The underlying error rides
            // along because it reads correctly either way: a herdr timeout and a
            // dropped socket are both reasons the start could not be seen.
            return match agent_for_task(store, state_dir, task) {
                Ok(fresh) if OUTCOME.contains(&fresh.state) => WaitResult {
                    task: task.to_string(),
                    state: Some(fresh.state),
                    error: None,
                },
                Ok(fresh) => WaitResult {
                    task: task.to_string(),
                    state: Some(fresh.state),
                    error: Some(format!("could not confirm the work started: {e}")),
                },
                Err(re) => WaitResult {
                    task: task.to_string(),
                    state: Some(a.state),
                    error: Some(format!("could not confirm the work started: {e} / {re}")),
                },
            };
        }
    }
    // Spent again, not reused. The start wait above can have consumed up to
    // `START_GRACE` of it, and handing the pre-start figure to the completion
    // wait let one `--timeout 10s` run for almost twenty — the deadline is
    // documented as shared by the whole call, and an orchestrator plans around it.
    let left = deadline - now();
    if !left.is_positive() {
        return WaitResult {
            task: task.to_string(),
            state: Some(a.state),
            error: Some("timed out".to_string()),
        };
    }
    if let Err(e) = tower_mux::wait_for_agent(h, &a.pane.pane_id, &UNTIL, Some(left.unsigned_abs()))
    {
        return WaitResult {
            task: task.to_string(),
            state: Some(a.state),
            error: Some(e.to_string()),
        };
    }

    // The server said it settled, so reconcile the store once and read the state
    // back rather than reporting the state the wait started from.
    //
    // A failure here is reported, not swallowed. Discarding it and falling back to
    // the pre-wait state meant `wt wait` could print `WORKING` and still exit
    // zero — and the caller is an orchestrator, which reads that exit code as
    // "this worker finished" and moves on to collect a report that does not exist.
    // An unverified completion has to look different from a verified one.
    // `sync_backend` answers `Ok` when it cannot reach the server, on purpose —
    // an absent backend is a reboot, not an error. That makes its result useless
    // for the question being asked here, which is "was this completion actually
    // reconciled?". So the server is asked directly first: if it has gone since
    // `agent.wait` answered, nothing was verified and reporting the pre-wait state
    // with no error would tell an orchestrator the worker finished.
    if let Err(e) = h.ping() {
        return WaitResult {
            task: task.to_string(),
            state: Some(a.state),
            error: Some(format!(
                "could not confirm completion (the backend dropped): {e}"
            )),
        };
    }
    if let Err(e) = tower_scan::sync_backend(store, h, now()) {
        return WaitResult {
            task: task.to_string(),
            state: Some(a.state),
            error: Some(format!("could not confirm completion (store update): {e}")),
        };
    }
    match agent_for_task(store, state_dir, task) {
        Ok(fresh) => WaitResult {
            task: task.to_string(),
            state: Some(fresh.state),
            error: unreconciled(fresh.state),
        },
        Err(e) => WaitResult {
            task: task.to_string(),
            state: Some(a.state),
            error: Some(format!(
                "could not confirm completion (re-reading the record): {e}"
            )),
        },
    }
}

/// Why the state read back after a successful wait is not a completion, or
/// `None` when it is one.
///
/// herdr said the agent settled; the store can still disagree.
/// `apply_observations` keeps a recent hook's WORKING for two minutes rather than
/// letting the backend overwrite it, so a missed completion hook leaves the
/// record mid-turn — and reporting that as success hands an orchestrator a
/// completion nobody reconciled.
///
/// A function of its own because it is the only part of the post-wait path a test
/// can reach: everything around it is a socket call, and a unit test has no
/// server to answer one.
fn unreconciled(fresh: AgentState) -> Option<String> {
    if UNTIL.contains(&fresh) {
        return None;
    }
    Some(format!(
        "could not confirm completion (the state never settled: {})",
        state_word(fresh)
    ))
}

/// The live agent working in a task's worktree.
///
/// Matched by working directory, which is the only link there is: a worktree
/// has one folder and the agent in it reports that folder, so nothing else has
/// to be recorded to connect the two.
/// The live worker in a task's worktree.
///
/// Public because a drive needs the record itself, not just a wait: whether a
/// settled worker has already taken a turn is what tells an acknowledged
/// completion from a worktree that has never been worked in.
pub fn agent_for_task(store: &Store, state_dir: &Path, task: &str) -> Result<Agent, String> {
    let m = tower_wt::load_meta(state_dir, task).map_err(|e| format!("task {task}: {e}"))?;
    let agents = store.list().map_err(|e| e.to_string())?;
    agents
        .into_iter()
        .find(|a| a.state != AgentState::Dead && a.cwd == m.path)
        .ok_or_else(|| format!("no worker found for task {task} ({})", m.path))
}

/// Writes the results for a person.
///
/// WAITING gets its own sentence rather than being printed as a state name,
/// because it is the one outcome that needs an action and the reader is
/// scanning for exactly that.
///
/// The task column is padded to a display width rather than a character count;
/// the two differ only for a task named in a wide script, and nothing freezes
/// this output.
pub fn print_wait_results<W: Write>(w: &mut W, results: &[WaitResult]) -> std::io::Result<()> {
    for r in results {
        let task = pad_right(&r.task, 20);
        match (&r.error, r.state) {
            (Some(e), _) => writeln!(w, "  {task} failed: {e}")?,
            (None, Some(AgentState::Waiting)) => writeln!(
                w,
                "  {task} needs you — it cannot go on until somebody answers"
            )?,
            (None, Some(AgentState::DoneUnread)) => writeln!(w, "  {task} done")?,
            (None, Some(s)) => writeln!(w, "  {task} {}", state_word(s))?,
            (None, None) => writeln!(w, "  {task}")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_756_000_000).unwrap()
    }

    /// A socket that cannot exist, so a test that reaches the backend fails
    /// loudly instead of talking to the developer's own herdr server.
    fn unreachable_herdr() -> Herdr {
        Herdr::at("/nonexistent/tower_cli-waitcmd/herdr.sock")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        state_dir: std::path::PathBuf,
        store: Store,
        worktree: std::path::PathBuf,
    }

    /// A state directory with one task meta and, optionally, a worker record in
    /// its worktree. Every root is injected; nothing here reads `$HOME`.
    fn fixture(task: &str, state: Option<AgentState>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let worktree = dir.path().join("wt").join(task);
        std::fs::create_dir_all(&worktree).unwrap();
        let store = Store::new(&state_dir).unwrap();

        tower_wt::save_meta(
            &state_dir,
            &tower_wt::Meta {
                task: task.to_string(),
                repo: dir.path().join("repo").to_string_lossy().into_owned(),
                base: "main".into(),
                branch: format!("agent/{task}"),
                path: worktree.to_string_lossy().into_owned(),
                agent: "claude".into(),
                test_cmd: String::new(),
                test_pass: None,
                test_at: None,
                created_at: t0(),
                drive: None,
                parent: None,
            },
        )
        .unwrap();

        if let Some(s) = state {
            let mut a = Agent::new("claude-w2:p1", "claude", t0());
            a.state = s;
            a.pane.session = "dev".into();
            a.pane.pane_id = "w2:p1".into();
            a.cwd = worktree.to_string_lossy().into_owned();
            store.save(&a).unwrap();
        }

        Fixture {
            _dir: dir,
            state_dir,
            store,
            worktree,
        }
    }

    /// A worker that is already finished must not reach the backend at all —
    /// asking herdr to wait for a transition that has happened blocks until the
    /// timeout.
    ///
    /// `OUTCOME`, not `UNTIL`: IDLE is in the set that *ends* a wait but is not
    /// evidence that a turn ran, and the test below pins the difference.
    #[test]
    fn a_worker_that_has_already_finished_is_reported_without_asking_the_server() {
        for s in OUTCOME {
            let f = fixture("hero", Some(s));
            let got = wait_tasks(
                &f.store,
                &unreachable_herdr(),
                &f.state_dir,
                &["hero".to_string()],
                DEFAULT_WAIT_TIMEOUT,
                &t0,
            );
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].state, Some(s), "{s:?}");
            assert!(got[0].error.is_none(), "{:?}", got[0].error);
        }
    }

    /// An IDLE worker is not a finished one, so the wait has to reach the
    /// backend and ask whether a turn begins.
    ///
    /// This is the documented sequence's first moment: `wt new` leaves the record
    /// IDLE, `herdr pane run` dispatches, and `wt wait` can arrive before any
    /// hook has written WORKING. Treating IDLE as completion exited zero without
    /// contacting the server, and the orchestrator went to collect a report for a
    /// turn that had not started. With no reachable server the wait now fails
    /// loudly instead — which is the point: it tried.
    #[test]
    fn an_idle_worker_is_not_treated_as_a_finished_turn() {
        let f = fixture("hero", Some(AgentState::Idle));
        let got = wait_tasks(
            &f.store,
            &unreachable_herdr(),
            &f.state_dir,
            &["hero".to_string()],
            DEFAULT_WAIT_TIMEOUT,
            &t0,
        );
        let e = got[0].error.as_deref().unwrap_or_default();
        assert!(
            e.contains("could not confirm the work started"),
            "an idle record must be checked against the backend, got {e:?}"
        );
        assert_eq!(got[0].state, Some(AgentState::Idle));
        assert!(!got[0].done(), "an unverified start is not a completion");
    }

    /// A turn short enough to finish inside the start window leaves its outcome
    /// in the record, and that outcome is a success even though the start itself
    /// was never seen.
    ///
    /// The injected clock is the seam. `wait_one` reads the record, then calls
    /// `now()` for the remaining time, then waits for the start — so a clock that
    /// writes to the store on that call lands the hook exactly where a real one
    /// would, in the window between the two reads. Nothing else here can reach
    /// inside the wait.
    #[test]
    fn a_turn_that_finished_before_its_start_was_seen_is_still_a_success() {
        let f = fixture("hero", Some(AgentState::Idle));
        let calls = Cell::new(0);
        let clock = || {
            // Call 0 sets the deadline, in `wait_tasks`. Call 1 is inside
            // `wait_one`, after it has already read IDLE.
            if calls.get() >= 1 {
                let mut a = f.store.list().unwrap().pop().unwrap();
                a.state = AgentState::DoneUnread;
                f.store.save(&a).unwrap();
            }
            calls.set(calls.get() + 1);
            t0()
        };

        let got = wait_tasks(
            &f.store,
            &unreachable_herdr(),
            &f.state_dir,
            &["hero".to_string()],
            DEFAULT_WAIT_TIMEOUT,
            &clock,
        );
        assert_eq!(got[0].state, Some(AgentState::DoneUnread));
        assert!(got[0].error.is_none(), "{:?}", got[0].error);
        assert!(got[0].done());
    }

    /// The state read back after the server said "settled" has to be a settled
    /// state. A missed completion hook leaves WORKING in the record for two
    /// minutes, and `wt wait` exiting zero on that tells an orchestrator to
    /// collect a report that may not exist.
    #[test]
    fn a_state_that_never_settled_is_not_a_completion() {
        for s in UNTIL {
            assert!(unreconciled(s).is_none(), "{s:?}");
        }
        for s in [AgentState::Working, AgentState::Error, AgentState::Dead] {
            let why = unreconciled(s).unwrap_or_default();
            assert!(why.contains("could not confirm completion"), "{s:?}: {why}");
            assert!(
                why.contains(&state_word(s)),
                "{s:?}: the message has to name the state it found"
            );
        }
    }

    /// An unknown task names itself in the failure. The caller passed the name
    /// and a message about a missing file would not tell them which.
    #[test]
    fn an_unknown_task_names_itself_in_the_failure() {
        let f = fixture("hero", None);
        let got = wait_tasks(
            &f.store,
            &unreachable_herdr(),
            &f.state_dir,
            &["nope".to_string()],
            DEFAULT_WAIT_TIMEOUT,
            &t0,
        );
        let e = got[0].error.as_deref().unwrap_or_default();
        assert!(e.contains("nope"), "{e:?}");
        assert_eq!(got[0].state, None, "there was no state to read");
    }

    /// A task whose meta exists but whose worker is gone points at the folder
    /// it looked in — that is what a person needs to attach to by hand.
    #[test]
    fn a_task_with_no_live_worker_names_the_folder_it_searched() {
        let f = fixture("hero", None);
        let got = wait_tasks(
            &f.store,
            &unreachable_herdr(),
            &f.state_dir,
            &["hero".to_string()],
            DEFAULT_WAIT_TIMEOUT,
            &t0,
        );
        let e = got[0].error.as_deref().unwrap_or_default();
        assert!(
            e.contains(&f.worktree.to_string_lossy().to_string()),
            "{e:?}"
        );
    }

    /// A DEAD record does not count as the worker: the folder is right but
    /// nothing is running in it.
    #[test]
    fn a_dead_record_in_the_worktree_is_not_the_worker() {
        let f = fixture("hero", Some(AgentState::Dead));
        let got = wait_tasks(
            &f.store,
            &unreachable_herdr(),
            &f.state_dir,
            &["hero".to_string()],
            DEFAULT_WAIT_TIMEOUT,
            &t0,
        );
        assert!(got[0].error.is_some(), "{:?}", got[0]);
    }

    /// One deadline covers the whole call, so a second task cannot start a
    /// fresh timeout of its own.
    #[test]
    fn a_deadline_that_has_passed_is_reported_rather_than_waited_on() {
        let f = fixture("hero", Some(AgentState::Working));
        let calls = Cell::new(0u32);
        let clock = || {
            let n = calls.get();
            calls.set(n + 1);
            // The first read sets the deadline; every later one is past it.
            if n == 0 {
                t0()
            } else {
                t0() + Duration::from_secs(3600)
            }
        };
        let got = wait_tasks(
            &f.store,
            &unreachable_herdr(),
            &f.state_dir,
            &["hero".to_string()],
            Duration::from_secs(60),
            &clock,
        );
        assert_eq!(got[0].error.as_deref(), Some("timed out"));
        assert_eq!(got[0].state, Some(AgentState::Working));
    }

    /// Each outcome reads as a different sentence, because the reader is
    /// scanning for the one that needs them.
    #[test]
    fn each_outcome_reads_as_its_own_sentence() {
        let results = vec![
            WaitResult {
                task: "hero".into(),
                state: Some(AgentState::DoneUnread),
                error: None,
            },
            WaitResult {
                task: "wingman".into(),
                state: Some(AgentState::Waiting),
                error: None,
            },
            WaitResult {
                task: "scout".into(),
                state: Some(AgentState::Working),
                error: None,
            },
            WaitResult::failed("ghost", "task ghost: no metadata".into()),
        ];
        let mut buf = Vec::new();
        print_wait_results(&mut buf, &results).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("hero") && out.contains("done"));
        assert!(out.contains("needs you"), "{out}");
        assert!(out.contains("WORKING"), "{out}");
        assert!(out.contains("failed: task ghost"), "{out}");
        assert!(results[0].done() && !results[1].done());
    }
}
