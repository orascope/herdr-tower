// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Reviewing a worker's diff by writing in it.
//!
//! The interaction is deliberately a file and not a TUI: the diff goes to disk,
//! the user opens it in whatever editor they already have, types comments under
//! the lines they mean, and `wt send` collects them. Nothing to learn, and the
//! review survives closing the terminal.

use std::fs;
use std::path::{Path, PathBuf};

use herdrx::Herdr;
use tower_state::{Agent, AgentState, Store};

use crate::git;
use crate::meta::{load_meta, save_meta};
use crate::{Error, Result};

/// The prefix that turns a line in the review file into a comment.
///
/// It starts with `#` so that a review file is still a readable diff — every
/// tool that reads diffs already ignores `#` lines — and the `>` distinguishes a
/// comment meant for the agent from the header's own `#` lines.
pub const COMMENT_MARKER: &str = "#> ";

/// Where a task's review file lives.
///
/// Inside the state directory, never in the repository: a review file in the
/// worktree would show up in the very diff it is reviewing.
pub fn review_path(state_dir: &Path, task: &str) -> PathBuf {
    // Encoded the same way the record is. Unencoded, a task called `feat/login`
    // resolves under a `worktrees/feat/` directory that nothing creates, so `wt
    // new feat/login` succeeds and `wt review feat/login` then fails with "no
    // such file or directory".
    state_dir
        .join("worktrees")
        .join(format!("{}.review.diff", crate::meta::encode_task(task)))
}

/// Writes the task's diff against base, with instructions on top.
///
/// Refuses when there is nothing to review, because an empty review file invites
/// somebody to write comments about code that is not there.
pub fn write_review_file(state_dir: &Path, task: &str) -> Result<PathBuf> {
    let m = load_meta(state_dir, task)?;
    let diff = git::diff(&m.path, &m.base)?;
    if diff.trim().is_empty() {
        return Err(Error::NoChanges);
    }
    let path = review_path(state_dir, task);
    let header = format!(
        r##"# herdr-tower review file — task {task} (against {base})
#
# Write "#> your comment" under a line you want changed, and save.
# Then, in a terminal:  herdr-tower wt send {task}
# Any other line starting with "#" is ignored.

"##,
        task = task,
        base = m.base
    );
    fs::write(&path, format!("{header}{diff}\n"))?;
    Ok(path)
}

/// One comment: what the user wrote, and the diff line above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The last real diff line before the comment — blank lines and other `#`
    /// lines skipped. This is the whole reason a comment can be terse: it says
    /// "pull this line out into a constant" and the context says which line.
    pub context: String,
    pub text: String,
}

/// Collects the `#> ` comments from a review file, each with its context.
pub fn extract_comments(path: &Path) -> Result<Vec<Comment>> {
    let body = fs::read_to_string(path).map_err(|_| Error::NoReviewFile)?;
    let mut out = Vec::new();
    let mut last_code = String::new();
    for line in body.split('\n') {
        if let Some(rest) = line.strip_prefix(COMMENT_MARKER) {
            let text = rest.trim();
            if !text.is_empty() {
                out.push(Comment {
                    context: last_code.clone(),
                    text: text.to_string(),
                });
            }
            continue;
        }
        // Note the order: the marker is checked first, so a `#> ` line never
        // reaches this. Everything else beginning with `#` is header or diff
        // furniture, and a blank line is no context at all.
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        last_code = line.to_string();
    }
    Ok(out)
}

/// Assembles the comments into one instruction for the agent.
///
/// One line, on purpose. It goes into an agent's input, and a multi-line send is
/// several submissions to a TUI that treats Enter as "go" — the first fragment
/// would be dispatched as a complete instruction and the rest would arrive as
/// interruptions.
///
/// There is no task parameter: the paragraph it builds does not mention the
/// task, and a signature that advertises a task-specific message it does not
/// produce is worse than a short one.
pub fn build_instruction(comments: &[Comment]) -> String {
    let mut b = format!(
        "{} review comments on the diff. Please act on each one.",
        comments.len()
    );
    for (i, c) in comments.iter().enumerate() {
        b.push_str(&format!(" ({})", i + 1));
        if !c.context.is_empty() {
            b.push_str(&format!(" [near {}]", c.context.trim()));
        }
        // Trailing periods are trimmed and one is added back, so a comment
        // written with punctuation and one written without read the same.
        b.push_str(&format!(" {}.", c.text.trim_end_matches('.')));
    }
    b
}

/// Sends the review comments to the task's agent.
///
/// The agent is found by working directory rather than by any stored pane
/// coordinate: coordinates move when tabs are rearranged, but the worktree path
/// is what makes the agent *that* task's agent. DEAD records are skipped so a
/// coordinate a dead worker used cannot receive somebody's review.
///
/// The delivery goes through [`tower_mux::send_text`], which asks what lives in the
/// pane first. That matters here more than anywhere: a review lands on a pane
/// that is running an agent, and `agent.prompt` refuses to write into an
/// approval dialog, where typing would answer a permission question with a code
/// review.
pub fn send_comments(state_dir: &Path, task: &str, store: &Store, h: &Herdr) -> Result<usize> {
    let comments = extract_comments(&review_path(state_dir, task))?;
    if comments.is_empty() {
        return Err(Error::NoComments);
    }
    send_instruction(state_dir, task, store, h, &build_instruction(&comments))?;
    Ok(comments.len())
}

/// Sends any instruction to the task's agent.
///
/// The addressing rule is [`send_comments`]'s and matters for the same reason:
/// the agent is found by worktree path, not by a stored coordinate, because
/// coordinates move when tabs are rearranged and a moved one would deliver this
/// task's instruction into somebody else's pane. Delivery goes through
/// [`tower_mux::send_text`], which asks what lives in the pane first — typing
/// into an approval dialog would answer a permission question with a bug
/// report.
pub fn send_instruction(
    state_dir: &Path,
    task: &str,
    store: &Store,
    h: &Herdr,
    text: &str,
) -> Result<Delivered> {
    send_dispatch(state_dir, task, store, h, text, Invalidate::Verdict)
}

/// Whether a dispatch clears the drive verdict stored on the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidate {
    /// An outside dispatch — `wt send`, a review. Whatever a drive concluded
    /// is no longer about the work being done, so it goes.
    Verdict,
    /// A drive's own retry. It has just written `Running` for exactly the
    /// window this dispatch opens, and clearing that would put the task back
    /// to showing nothing if the drive were interrupted — which is what that
    /// record exists to prevent.
    Nothing,
}

/// [`send_instruction`], with control over the stored verdict.
pub fn send_dispatch(
    state_dir: &Path,
    task: &str,
    store: &Store,
    h: &Herdr,
    text: &str,
    invalidate: Invalidate,
) -> Result<Delivered> {
    deliver(state_dir, task, store, h, text, invalidate, None).map_err(|f| f.error)
}

/// The stricter checks a dispatch board send adds on top of
/// [`send_dispatch`]'s.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BoardGuard<'a> {
    /// The board's repository; the task's meta must name the same one.
    pub repo: &'a str,
    /// Only a worker on this herdr backend (empty = the default socket).
    pub backend: &'a str,
}

/// A delivery that did not complete, and whether the prompt had gone out.
#[derive(Debug)]
pub(crate) struct Failed {
    pub error: Error,
    /// `agent.prompt` was called. Its error may then mean the server received
    /// it anyway; before it, nothing was sent.
    pub prompted: bool,
}

impl From<Error> for Failed {
    fn from(error: Error) -> Self {
        Failed {
            error,
            prompted: false,
        }
    }
}

/// [`send_dispatch`]'s body, with the board's extra checks when `board` is set.
pub(crate) fn deliver(
    state_dir: &Path,
    task: &str,
    store: &Store,
    h: &Herdr,
    text: &str,
    invalidate: Invalidate,
    board: Option<BoardGuard<'_>>,
) -> std::result::Result<Delivered, Failed> {
    let m = load_meta(state_dir, task)?;
    let agents = store.list().map_err(Error::from)?;
    let target = match board {
        None => target_agent(&agents, &m.path).ok_or_else(|| Error::NoAgentIn(m.path.clone()))?,
        Some(g) => {
            if m.repo != g.repo {
                return Err(Error::OtherRepo {
                    task: task.to_string(),
                    repo: m.repo.clone(),
                }
                .into());
            }
            board_target(&agents, &m.path, g.backend)?
        }
    };
    // 🚨 Only into an agent. `tower_mux::send_text` deliberately falls back to
    // `pane.run` when a pane holds no agent — which is right for a shell, and
    // catastrophic here: an agent that crashed back to its prompt between the
    // completion we saw and this call would receive a multi-line instruction
    // *and a captured test log* as shell input, and any line of that log which
    // happens to be valid shell would run in the worktree. The record being
    // non-dead is not enough; the pane has to still hold the agent now.
    let pane = h.pane(&target.pane.pane_id).map_err(Error::from)?;
    if !pane.has_agent() {
        return Err(Error::NoAgentIn(m.path.clone()).into());
    }
    // 🚨 And the *right* agent. herdr reuses pane coordinates, so a stale
    // record can name a pane where somebody else's agent is now working —
    // `has_agent` is true there too, and the instruction would be delivered
    // into an unrelated task. Worktree-based addressing is the guarantee this
    // function makes; the live pane has to still be in that worktree for it to
    // hold. A subdirectory counts: an agent may have cd'd within its own tree.
    let in_worktree = |dir: &str| dir == m.path || dir.starts_with(&format!("{}/", m.path));
    if !in_worktree(&pane.cwd) && !in_worktree(&pane.foreground_cwd) {
        return Err(Error::PaneMoved {
            pane: target.pane.pane_id.clone(),
            expected: m.path.clone(),
            found: pane.cwd.clone(),
        }
        .into());
    }
    // A board item is new work: it goes only to a worker that is settled now,
    // by herdr's live answer rather than the record, which can lag. This
    // narrows the race rather than closing it — herdr has no "prompt only if
    // idle" — and the board shows the remainder as "sent, not started".
    // The session the prompt will reach is the pane's, now; the record's can
    // lag a replacement the scanner has not seen yet.
    let live_session = pane.session_id().filter(|s| !s.is_empty());
    if board.is_some() {
        if let Some(status) = busy(pane.agent_status) {
            return Err(Error::WorkerBusy {
                task: task.to_string(),
                status,
            }
            .into());
        }
        // 🚨 A different conversation in the same pane and worktree: the record
        // describes an agent that is gone. Its turn count would be the wrong
        // baseline, and the board would track the item against the wrong
        // session — refuse until the scanner has caught up.
        if let Some(live) = live_session {
            if !target.session_id.is_empty() && live != target.session_id {
                return Err(Error::SessionChanged {
                    task: task.to_string(),
                }
                .into());
            }
        }
    }
    let (before, now, boundary_lost) =
        // A board send and an outside dispatch (`wt send`, a review) wait
        // for the worker to be seen starting: a prompt herdr accepts can still
        // be lost — a codex that updates itself on start exits with it. A
        // drive's own retry keeps returning on acceptance; its next lap's wait
        // is what tells it the worker moved.
        prompt_and_acknowledge(
            h,
            store,
            target,
            text,
            board.is_some() || invalidate == Invalidate::Verdict,
        )?;
    // New work supersedes whatever a drive concluded: a branch being revised is
    // not standing in the merge queue, and leaving `merge-ready` in `wt list`
    // would advertise it as if it were. A drive's own retry is the exception —
    // it wrote `Running` on purpose, just now.
    let mut verdict_kept = false;
    if invalidate == Invalidate::Verdict {
        // 🚨 Re-read first. `m` was loaded before the pane checks and the
        // delivery, and a drive or a standalone `wt test` can have written
        // `test_pass`, `test_at` or a verdict of its own in that interval —
        // saving this snapshot back would republish the values it superseded,
        // turning a test that just failed into the previous passing result.
        // Only the verdict is this call's to change.
        //
        // Also after delivery: failing to clear a stale verdict must not be
        // reported as failing to deliver.
        //
        // 🚨 Under the record lock, which is what makes the clearing final. A
        // drive that has just finished testing is at this moment deciding
        // whether to publish a verdict; without a shared boundary its write can
        // land after this one and put back the very verdict this dispatch
        // supersedes. `record_for_if` takes the same lock and re-reads the
        // worker inside it, so one of the two orderings always wins cleanly.
        let _guard = crate::meta::claim_lock(&crate::meta::dir(state_dir));
        match load_meta(state_dir, task) {
            Ok(mut fresh) => {
                if fresh.drive.take().is_some() {
                    verdict_kept = save_meta(state_dir, &fresh).is_err();
                }
            }
            Err(_) => verdict_kept = true,
        }
    }
    Ok(Delivered {
        boundary_lost,
        verdict_kept,
        agent_id: target.id.clone(),
        // herdr reports no session for some agents at all (codex), so the
        // record's is taken when the pane gives none. On a board send that is
        // safe only with a record generation: if the record turns out to be a
        // gone agent's, the scanner's new record has a different `born_at` and
        // the board shows the worker as unclear. A record from before `born_at`
        // gives no such guard, so its session is not borrowed.
        session_id: match (live_session, board, target.born_at) {
            (Some(live), _, _) => live.to_string(),
            (None, Some(_), None) => String::new(),
            (None, _, _) => target.session_id.clone(),
        },
        work_seq: before,
        record_born: target.born_at,
        at: now,
    })
}

/// Types `text` into `target`'s agent and records the dispatch boundary:
/// the part of a delivery that is the same whoever named the worker. Answers
/// the turn count and time read before the prompt, and whether the boundary
/// was lost.
fn prompt_and_acknowledge(
    h: &Herdr,
    store: &Store,
    target: &Agent,
    text: &str,
    board: bool,
) -> std::result::Result<(u64, time::OffsetDateTime, bool), Failed> {
    let now = time::OffsetDateTime::now_utc();
    let before = target.work_seq;
    // 🚨 `prompt`, not `tower_mux::send_text`. That helper asks the pane what
    // it holds *again* and falls back to `pane.run` when the answer is "no
    // agent" — so an agent that exits between the guard above and the send
    // would still get a multi-line instruction and a captured test log
    // executed as shell commands. Addressing the agent operation directly
    // makes that race end in herdr refusing, which is the outcome the guard
    // was for.
    // With `wait`, the prompt is confirmed only by a start seen after it: a
    // prompt herdr accepted can still be lost, and herdr answers
    // `agent_prompt_stalled` when no start comes within five seconds.
    let wait = herdrx::PromptOptions {
        until: vec![herdrx::AgentStatus::Working, herdrx::AgentStatus::Blocked],
        timeout: Some(BOARD_START_WAIT),
    };
    let opts = board.then_some(&wait);
    h.prompt(&target.pane.pane_id, text, opts)
        .map_err(|e| Failed {
            error: e.into(),
            prompted: true,
        })?;
    // 🚨 Everything that can refuse has now succeeded. Acknowledging before
    // this point would mark a worker's unread completion as read on a delivery
    // that never happened — and since the error path does not dispatch either,
    // that result would stay hidden for good.
    //
    // The completion this dispatch answers is spent. Left as `DoneUnread`, the
    // very next `wt wait` takes it as the *new* turn's outcome and returns
    // without asking the backend anything, so a caller that sends and then
    // waits gets an answer about work it just replaced.
    //
    // 🚨 Only *that* completion, though. A short turn can start and finish
    // between the line above and this one, and acknowledging the record then
    // would spend the answer that just arrived — leaving the next lap waiting
    // for a transition already past. The turn counter says which one is in
    // hand; a record that has moved on is left alone, because the new
    // completion is not this dispatch's to consume.
    //
    // The failure is propagated rather than swallowed: without the
    // acknowledgement the stale `DoneUnread` stands, `note_dispatch` is a
    // no-op against it, and the next lap accepts it as the answer to a turn
    // that has not run.
    // 🚨 One locked decision, not a read followed by a write: a short turn can
    // start and finish in between, and the acknowledgement would then land on
    // the completion that just arrived — the very answer the caller is about to
    // wait for. A record that has moved on is left alone.
    //
    // 🚨 Reported, not raised. Past the delivery above, a failure here is
    // bookkeeping rather than refusal: the caller has to know the work was
    // handed off even when the boundary was not recorded, because the next
    // completion may then read as the one already answered.
    // 🚨 `Ok(false)` is a lost boundary too. It means the agent had moved on
    // since `before` was read, so nothing was written: the previous turn's
    // `DoneUnread` still stands and `done_acked` was never cleared. Reading
    // only `is_err()` here told the drive the boundary was recorded, and the
    // next lap then accepted that stale completion as the answer to the turn
    // it had just asked for — the exact case this flag exists to stop.
    let boundary_lost = !boundary_recorded(store.acknowledge_dispatch(&target.id, before, now));
    Ok((before, now, boundary_lost))
}

/// Delivers a board item to a pane worker — a long-lived agent named by its
/// pane, not a `wt` task. The same guards as a task send, less the worktree
/// and repository checks, plus one of its own: the pane must still hold the
/// agent it held when the item was assigned (record generation and session),
/// since herdr reuses pane ids.
pub(crate) fn deliver_to_pane(
    store: &Store,
    h: &Herdr,
    backend: &str,
    pin: &tower_state::PaneWorker,
    text: &str,
) -> std::result::Result<Delivered, Failed> {
    let pane_id = &pin.pane_id;
    if pin.backend != backend {
        return Err(Error::PaneElsewhere {
            pane: pane_id.clone(),
        }
        .into());
    }
    let agents = store.list().map_err(Error::from)?;
    let live: Vec<&Agent> = agents
        .iter()
        .filter(|a| {
            a.pane.backend == backend && a.pane.pane_id == *pane_id && a.state != AgentState::Dead
        })
        .collect();
    let target = match live.as_slice() {
        [] => return Err(Error::NoAgentInPane(pane_id.clone()).into()),
        [one] => *one,
        many => {
            return Err(Error::SeveralAgentsIn {
                worktree: pane_id.clone(),
                panes: many.iter().map(|a| a.id.clone()).collect(),
            }
            .into())
        }
    };
    // 🚨 The agent it was assigned to, not whoever holds the pane now.
    let reborn = pin.born_at.is_some() && target.born_at != pin.born_at;
    let other = !pin.session_id.is_empty()
        && !target.session_id.is_empty()
        && target.session_id != pin.session_id;
    if reborn || other {
        return Err(Error::PaneReassigned {
            pane: pane_id.clone(),
        }
        .into());
    }
    let pane = h.pane(pane_id).map_err(Error::from)?;
    if !pane.has_agent() {
        return Err(Error::NoAgentInPane(pane_id.clone()).into());
    }
    if let Some(status) = busy(pane.agent_status) {
        return Err(Error::WorkerBusy {
            task: pane_id.clone(),
            status,
        }
        .into());
    }
    let live_session = pane.session_id().filter(|s| !s.is_empty());
    if let Some(live) = live_session {
        if !target.session_id.is_empty() && live != target.session_id {
            return Err(Error::SessionChanged {
                task: pane_id.clone(),
            }
            .into());
        }
    }
    let (before, now, boundary_lost) = prompt_and_acknowledge(h, store, target, text, true)?;
    Ok(Delivered {
        boundary_lost,
        verdict_kept: false,
        agent_id: target.id.clone(),
        session_id: match (live_session, target.born_at) {
            (Some(live), _) => live.to_string(),
            (None, None) => String::new(),
            (None, Some(_)) => target.session_id.clone(),
        },
        work_seq: before,
        record_born: target.born_at,
        at: now,
    })
}

/// How long a board send waits for the worker to be seen starting. herdr's
/// own limit for a start is five seconds; this only has to outlast it.
const BOARD_START_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Why a worker is not taking new work now, by herdr's live status.
fn busy(status: herdrx::AgentStatus) -> Option<&'static str> {
    use herdrx::AgentStatus as S;
    match status {
        S::Idle | S::Done => None,
        S::Working => Some("working"),
        S::Blocked => Some("waiting for an answer"),
        S::Unknown => Some("in an unknown state"),
    }
}

/// The one live agent in `worktree` on `backend`.
///
/// Stricter than [`target_agent`]: pane ids are unique only within a backend,
/// so a worker elsewhere is not this board's, and two live candidates are
/// refused rather than resolved by list order — the board would otherwise
/// track progress in one and deliver into the other.
fn board_target<'a>(
    agents: &'a [Agent],
    worktree: &str,
    backend: &str,
) -> std::result::Result<&'a Agent, Error> {
    let live: Vec<&Agent> = agents
        .iter()
        .filter(|a| a.cwd == worktree && a.state != AgentState::Dead && a.pane.backend == backend)
        .collect();
    match live.as_slice() {
        [] => Err(Error::NoAgentIn(worktree.to_string())),
        [one] => Ok(one),
        many => Err(Error::SeveralAgentsIn {
            worktree: worktree.to_string(),
            panes: many.iter().map(|a| a.pane.pane_id.clone()).collect(),
        }),
    }
}

/// Whether an acknowledgement result means the dispatch boundary was written.
///
/// Only `Ok(true)` does. `Ok(false)` is the store saying the agent had moved on
/// since the turn count was read, so it wrote nothing — the old completion is
/// still standing — and an error wrote nothing either.
fn boundary_recorded(acknowledged: std::io::Result<bool>) -> bool {
    matches!(acknowledged, Ok(true))
}

/// What a delivered dispatch left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    /// The instruction arrived, but the record of it did not.
    ///
    /// The turn that follows may then be read as the answer to the previous
    /// one. A caller about to wait should say so rather than treat the run as
    /// clean.
    pub boundary_lost: bool,
    /// Delivered, but a stale `drive` verdict could not be cleared, so
    /// `wt list` may still call the task merge-ready.
    pub verdict_kept: bool,
    /// The worker's native session id, read before the prompt. Empty when the
    /// record had none yet.
    pub session_id: String,
    /// The worker's turns begun, read before the prompt.
    pub work_seq: u64,
    /// The agent record the delivery reached.
    pub agent_id: String,
    /// The worker record's `born_at`, which `work_seq` belongs to.
    pub record_born: Option<time::OffsetDateTime>,
    /// When the delivery began.
    pub at: time::OffsetDateTime,
}

/// The pane belonging to the agent working in `worktree`.
///
/// Split out from [`send_comments`] because this is the decision that can go
/// wrong quietly: everything else in that function fails loudly, while picking
/// the wrong pane delivers somebody's code review into an unrelated agent's
/// input and looks like success.
///
/// The first live record wins. That is deterministic rather than arbitrary
/// because [`tower_state::Store::list`] returns a fixed order, and two live agents
/// in one worktree is not a state this tool creates.
fn target_agent<'a>(agents: &'a [Agent], worktree: &str) -> Option<&'a Agent> {
    agents
        .iter()
        .find(|a| a.cwd == worktree && a.state != AgentState::Dead)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pane of whichever agent was picked, for the assertions below.
    fn pane_of(a: Option<&Agent>) -> Option<&str> {
        a.map(|a| a.pane.pane_id.as_str())
    }
    use crate::git::tests::fixture_repo;
    use crate::lifecycle::{new_task, NewOptions};
    use time::OffsetDateTime;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_789_000_496).unwrap()
    }

    /// The repository is handed back so it outlives the worktree pointing into
    /// it — dropping it first leaves git unable to resolve the worktree at all.
    fn a_task(state: &Path) -> (tempfile::TempDir, crate::Meta) {
        let repo = fixture_repo();
        let m = new_task(
            state,
            &NewOptions {
                task: "t",
                repo: repo.path().to_str().unwrap(),
                ..NewOptions::default()
            },
            now(),
        )
        .unwrap();
        (repo, m)
    }

    fn worker(pane: &str, cwd: &str, backend: &str, state: AgentState) -> Agent {
        let mut a = Agent::new(format!("codex-{pane}"), "codex", now());
        a.pane.pane_id = pane.into();
        a.pane.backend = backend.into();
        a.cwd = cwd.into();
        a.state = state;
        a
    }

    #[test]
    fn a_board_target_is_the_one_live_worker_on_this_backend() {
        let agents = [
            worker("w1:p1", "/r/wt/a", "", AgentState::Dead),
            worker("w1:p2", "/r/wt/a", "/other.sock", AgentState::Idle),
            worker("w1:p3", "/r/wt/a", "", AgentState::DoneUnread),
            worker("w1:p4", "/r/wt/b", "", AgentState::Idle),
        ];
        assert_eq!(
            board_target(&agents, "/r/wt/a", "").unwrap().pane.pane_id,
            "w1:p3"
        );
        assert_eq!(
            board_target(&agents, "/r/wt/a", "/other.sock")
                .unwrap()
                .pane
                .pane_id,
            "w1:p2"
        );
        assert!(matches!(
            board_target(&agents, "/r/wt/c", ""),
            Err(Error::NoAgentIn(_))
        ));
    }

    #[test]
    fn two_live_workers_in_one_worktree_are_refused_not_picked() {
        let agents = [
            worker("w1:p1", "/r/wt/a", "", AgentState::Idle),
            worker("w1:p2", "/r/wt/a", "", AgentState::Working),
        ];
        match board_target(&agents, "/r/wt/a", "") {
            Err(Error::SeveralAgentsIn { panes, .. }) => {
                assert_eq!(panes, vec!["w1:p1".to_string(), "w1:p2".to_string()])
            }
            other => panic!("{other:?}"),
        }
        // `wt send` keeps its old rule: the first live one.
        assert_eq!(pane_of(target_agent(&agents, "/r/wt/a")), Some("w1:p1"));
    }

    #[test]
    fn only_a_settled_worker_takes_a_board_item() {
        use herdrx::AgentStatus as S;
        assert_eq!(busy(S::Idle), None);
        assert_eq!(busy(S::Done), None);
        assert!(busy(S::Working).is_some());
        assert!(busy(S::Blocked).is_some());
        assert!(busy(S::Unknown).is_some(), "not knowing is not settled");
    }

    #[test]
    fn a_board_send_to_another_repositorys_task_is_refused_before_anything() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, m) = a_task(state.path());
        let store = Store::new(state.path().join("agents")).unwrap();
        let guard = BoardGuard {
            repo: "/not/this/repo",
            backend: "",
        };
        // No herdr is reachable here; the repository check has to come first.
        let h = herdrx::Herdr::at(state.path().join("no.sock"));
        let failed = deliver(
            state.path(),
            "t",
            &store,
            &h,
            "x",
            Invalidate::Verdict,
            Some(guard),
        )
        .unwrap_err();
        assert!(!failed.prompted);
        match failed.error {
            Error::OtherRepo { task, repo } => {
                assert_eq!(task, "t");
                assert_eq!(repo, m.repo);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_task_with_no_changes_gets_no_review_file() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, _m) = a_task(state.path());
        assert!(matches!(
            write_review_file(state.path(), "t"),
            Err(Error::NoChanges)
        ));
        assert!(!review_path(state.path(), "t").exists());
    }

    #[test]
    fn a_review_file_carries_the_diff_and_says_how_to_send_it_back() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, m) = a_task(state.path());
        fs::write(Path::new(&m.path).join("a.txt"), "changed content\n").unwrap();

        let path = write_review_file(state.path(), "t").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("herdr-tower wt send t"),
            "the file has to say what to do next:\n{content}"
        );
        assert!(content.contains("+changed content"), "{content}");
        assert!(
            content.starts_with("# herdr-tower review file — task t (against main)\n"),
            "{content}"
        );
        assert!(content.ends_with('\n'), "{content:?}");
    }

    #[test]
    fn comments_are_collected_with_the_line_they_sit_under() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, m) = a_task(state.path());
        fs::write(Path::new(&m.path).join("a.txt"), "changed content\n").unwrap();
        let path = write_review_file(state.path(), "t").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let edited = content.replacen(
            "+changed content",
            "+changed content\n#> pull this out into a constant\n#> and add a test",
            1,
        );
        fs::write(&path, edited).unwrap();

        let comments = extract_comments(&path).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].text, "pull this out into a constant");
        assert!(
            comments[0].context.contains("changed content"),
            "context: {:?}",
            comments[0].context
        );
        assert_eq!(
            comments[1].context, comments[0].context,
            "two comments under one line share it"
        );
    }

    /// The header's own `#` lines must never become context, or every comment
    /// near the top of the file would be anchored to the instructions.
    #[test]
    fn header_lines_and_blank_lines_are_never_context() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.review.diff");
        fs::write(
            &p,
            "# herdr-tower review file\n#\n\n#> comment with no context\n+real line\n\n#> comment with context\n",
        )
        .unwrap();
        let got = extract_comments(&p).unwrap();
        assert_eq!(
            got,
            vec![
                Comment {
                    context: String::new(),
                    text: "comment with no context".into()
                },
                Comment {
                    context: "+real line".into(),
                    text: "comment with context".into()
                },
            ]
        );
    }

    /// An empty marker line is somebody having second thoughts, not a comment.
    #[test]
    fn a_marker_with_nothing_after_it_is_not_a_comment() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.review.diff");
        fs::write(&p, "+code\n#> \n#>    \n#> real\n").unwrap();
        let got = extract_comments(&p).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "real");
    }

    #[test]
    fn sending_before_reviewing_says_so() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            extract_comments(&d.path().join("absent.review.diff")),
            Err(Error::NoReviewFile)
        ));
    }

    /// The instruction is one line because it is typed into an agent's input,
    /// where a newline is a submit.
    #[test]
    fn the_instruction_is_a_single_line_naming_every_comment() {
        let comments = vec![
            Comment {
                context: "+  const x = 1".into(),
                text: "pull this out into a constant".into(),
            },
            Comment {
                context: String::new(),
                text: "and add a test.".into(),
            },
        ];
        let inst = build_instruction(&comments);
        assert_eq!(
            inst,
            "2 review comments on the diff. Please act on each one. \
             (1) [near +  const x = 1] pull this out into a constant. (2) and add a test."
        );
        assert!(!inst.contains('\n'));
    }

    fn agent_at(id: &str, cwd: &str, pane: &str, state: AgentState) -> Agent {
        let mut a = Agent::new(id, "claude", now());
        a.cwd = cwd.into();
        a.pane = tower_state::PaneRef {
            backend: String::new(),
            session: "dev".into(),
            window: 1,
            pane_id: pane.into(),
            terminal_id: String::new(),
            tab: String::new(),
            workspace_id: "w1".into(),
        };
        a.state = state;
        a
    }

    /// The worktree path is what makes an agent *this* task's agent — not a
    /// stored coordinate, which moves whenever tabs are rearranged.
    #[test]
    fn the_review_goes_to_the_agent_working_in_that_worktree() {
        let agents = [
            agent_at("a", "/other", "w2:p1", AgentState::Working),
            agent_at("b", "/wt/t1", "w2:p2", AgentState::Working),
        ];
        assert_eq!(pane_of(target_agent(&agents, "/wt/t1")), Some("w2:p2"));
        assert_eq!(pane_of(target_agent(&agents, "/wt/absent")), None);
    }

    /// A dead worker's coordinate gets reused by whatever herdr puts there
    /// next, so sending to one is sending to a stranger.
    #[test]
    fn a_dead_record_never_receives_a_review() {
        let agents = [
            agent_at("dead", "/wt/t1", "w2:p1", AgentState::Dead),
            agent_at("live", "/wt/t1", "w2:p2", AgentState::Idle),
        ];
        assert_eq!(pane_of(target_agent(&agents, "/wt/t1")), Some("w2:p2"));

        let only_dead = [agent_at("dead", "/wt/t1", "w2:p1", AgentState::Dead)];
        assert_eq!(pane_of(target_agent(&only_dead, "/wt/t1")), None);
    }

    /// The review file sits beside the record, encoded the same way.
    ///
    /// Unencoded, `wt new feat/login` succeeds while `wt review feat/login`
    /// tries to write into a `worktrees/feat/` directory nothing creates.
    #[test]
    fn a_review_file_sits_beside_its_record() {
        let d = tempfile::tempdir().unwrap();
        let p = review_path(d.path(), "feat/login");
        assert_eq!(
            p,
            d.path().join("worktrees").join("feat%2Flogin.review.diff")
        );
        fs::create_dir_all(d.path().join("worktrees")).unwrap();
        assert!(
            fs::write(&p, "x").is_ok(),
            "the parent directory is the one the record already lives in"
        );
    }

    /// 🚨 A refused acknowledgement is a lost boundary. `Ok(false)` means the
    /// agent moved on and nothing was written — the previous turn's
    /// `DoneUnread` still stands — so reading only `is_err()` told the drive
    /// the boundary was kept, and its next lap accepted that stale completion
    /// as the answer to the turn it had just asked for.
    #[test]
    fn only_a_written_acknowledgement_keeps_the_boundary() {
        assert!(boundary_recorded(Ok(true)));
        assert!(!boundary_recorded(Ok(false)), "moved on: nothing written");
        assert!(!boundary_recorded(Err(std::io::Error::other("disk"))));
    }

    /// The premise the rule above rests on: when the turn count has moved, the
    /// store refuses and leaves the stale completion exactly as it was.
    #[test]
    fn a_moved_turn_count_leaves_the_old_completion_standing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path()).expect("store");
        let at = time::OffsetDateTime::UNIX_EPOCH;
        let mut a = Agent::new("hero", "claude", at);
        a.state = AgentState::DoneUnread;
        a.work_seq = 5;
        store.save(&a).expect("save");

        // The caller read 4; a turn began since.
        assert!(!store.acknowledge_dispatch("hero", 4, at).expect("ack"));
        let after = store.load("hero").expect("load");
        assert_eq!(
            after.state,
            AgentState::DoneUnread,
            "still the old completion"
        );
    }
}
