// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Per-task git worktrees: one branch, one directory, one agent, one record.
//!
//! Two rules order everything here, and both are about what happens when a step
//! fails halfway:
//!
//!   - **Record before you build.** The meta file is written before the worktree
//!     exists, so evidence of what was attempted always precedes the thing
//!     attempted. Debris is then findable (`wt list`) instead of being a
//!     directory nobody remembers creating.
//!   - **Check before you delete.** [`clean`] refuses on a single uncommitted
//!     file or unmerged commit and removes nothing at all. `git worktree
//!     remove` is never given `--force` and `git branch` never `-D`, so git's
//!     own refusals stay in force underneath ours.
//!
//! The launch path is the part that has changed most. It used to create a
//! shell window, type the agent's command into it, then watch the screen for
//! "do you trust the files in this folder?" and press Enter. herdr starts the
//! agent itself through `agent.start` and reports a startup question as
//! *blocked* rather than leaving it to a screen scrape, so the launch goes over
//! the socket and the scraper is off that path — see `lifecycle::open_window`
//! for the incident that settled it.

use std::fmt;

mod board;
mod drive;
mod git;
mod lifecycle;
mod meta;
mod prompts;
mod review;
mod runtest;

pub use board::{send_board_item, send_board_item_to_pane, BoardDelivery};
pub use drive::{
    build_retry_instruction, decide, failure_log_path, record, record_for, record_for_if,
    summarize_failure, write_failure_log, Checkout, Drive, DriveState, Step, Worker,
    DEFAULT_ATTEMPTS, SUMMARY_LINES,
};
pub use git::{
    add_worktree, branch_exists, checked_out_branch, current_branch, delete_branch, diff, dirty,
    head, merge, remove_worktree, repo_root, unmerged,
};
pub use lifecycle::{
    agent_kind, agent_name, clean, command_for, gemini_dir, merge_guide, new_task, split_command,
    with_auto_approve, NewOptions, AGENTS,
};
pub use meta::{
    delete_meta, list_metas, live_branches, load_meta, reserve_meta, save_meta, Meta, Parent,
};
pub use prompts::{accept_startup_prompts, is_trust_prompt};
pub use review::{
    build_instruction, extract_comments, review_path, send_comments, send_dispatch,
    send_instruction, write_review_file, Comment, Invalidate, COMMENT_MARKER,
};
pub use runtest::run_test;

/// Everything that can go wrong in this crate.
///
/// One enum rather than one per module, because these errors are all headed for
/// the same place — a line printed to a person running `herdr-tower wt` — and
/// every one of them is spelled as a finished sentence for that person.
/// The variants exist so a caller can branch on *which* refusal it was
/// ([`Error::CleanRefused`] is the one `wt clean` must not treat as a crash) and
/// so the sentence is written once.
#[derive(Debug)]
pub enum Error {
    /// A git invocation failed. `args` omits the `-C <dir>` prefix.
    Git {
        args: String,
        output: String,
    },
    /// `rev-list --count` answered with something that is not a number.
    ///
    /// The message names the command, not the parser that choked on its output,
    /// because the command is the part a person can act on.
    Count {
        output: String,
    },
    Io(std::io::Error),
    Json(serde_json::Error),
    Mux(tower_mux::Error),
    Herdr(herdrx::Error),
    /// `wt new` with no task name.
    NoTask,
    /// An agent kind this tool does not launch.
    UnknownAgent(String),
    /// `--repo` does not point inside a git repository.
    NotARepo(String),
    /// No `--base` given and HEAD cannot name a branch (detached HEAD).
    NoBase,
    /// The task's branch is already there.
    BranchExists(String),
    /// A meta record for that task is already on disk.
    TaskExists(String),
    /// herdr already runs an agent under the name this task would take.
    ///
    /// Its own variant because the remedy is different from every other refusal
    /// here: nothing is wrong with the task, the *name* it normalizes to is in
    /// use, and renaming the task is what gets past it.
    AgentNameTaken(String),
    /// No meta record for that task.
    NoMeta(String),
    /// The worktree and branch were created but the agent did not start.
    ///
    /// The meta rides along because the worktree is real and is deliberately
    /// left in place: the user can attach to it by hand, and `wt list` shows it.
    /// A `Result` cannot carry a value and an error together, so the record
    /// travels inside the failure instead of being dropped on the floor.
    WorkerStart {
        meta: Box<Meta>,
        source: Box<Error>,
    },
    /// Not running inside herdr, so there is nowhere to put a window.
    OutsideHerdr,
    /// `agent.start` refused. The pane ID rides along because the pane exists
    /// and is left open on purpose — somebody has to be able to attach and see
    /// what blocked.
    AgentStart {
        pane_id: String,
        source: herdrx::Error,
    },
    /// [`clean`] found work that would be lost. Nothing was deleted.
    CleanRefused {
        dirty: Vec<String>,
        unmerged: usize,
    },
    /// The merge itself failed — a conflict, most likely.
    MergeFailed(Box<Error>),
    /// Nothing changed against base, so there is no diff to review.
    NoChanges,
    /// `wt send` before `wt review`.
    NoReviewFile,
    /// The review file exists but carries no `#> ` lines.
    NoComments,
    /// `wt test` with no command, stored or given.
    NoTestCmd(String),
    /// No live agent has that worktree as its working directory.
    NoAgentIn(String),
    /// The pane the record names is live, but is somewhere else now.
    ///
    /// herdr reuses pane coordinates, so a stale record can point at an agent
    /// working in an unrelated directory. Worktree-based addressing is the
    /// guarantee this module makes; this is what it refusing looks like.
    PaneMoved {
        pane: String,
        expected: String,
        found: String,
    },
    /// A board send found the worker busy by herdr's live status.
    WorkerBusy {
        task: String,
        status: &'static str,
    },
    /// A board send found more than one live agent in the worktree.
    SeveralAgentsIn {
        worktree: String,
        panes: Vec<String>,
    },
    /// A board send found a different conversation in the worker's pane than
    /// the record describes.
    SessionChanged {
        task: String,
    },
    /// A board send to a pane found no live agent there.
    NoAgentInPane(String),
    /// A board send to a pane found another agent there than the one the
    /// item was assigned to.
    PaneReassigned {
        pane: String,
    },
    /// A board send to a pane on another herdr server than this one.
    PaneElsewhere {
        pane: String,
    },
    /// A board send named a task that belongs to another repository.
    OtherRepo {
        task: String,
        repo: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Git { args, output } => write!(f, "git {args}: {output}"),
            Error::Count { output } => {
                write!(
                    f,
                    "git rev-list --count returned something that is not a number: {output:?}"
                )
            }
            Error::Io(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "{e}"),
            Error::Mux(e) => write!(f, "{e}"),
            Error::Herdr(e) => write!(f, "{e}"),
            Error::NoTask => write!(f, "a task name is required"),
            Error::UnknownAgent(a) => write!(f, "unsupported agent: {a}"),
            Error::NotARepo(d) => write!(f, "not a git repository: {d}"),
            Error::NoBase => write!(
                f,
                "the base branch cannot be determined — name it with --base"
            ),
            Error::BranchExists(b) => {
                write!(f, "branch {b} already exists — pick another task name")
            }
            Error::TaskExists(t) => write!(f, "task {t:?} already exists"),
            Error::AgentNameTaken(n) => write!(
                f,
                "herdr already has an agent named {n:?} — rename the task"
            ),
            Error::NoMeta(t) => write!(f, "no metadata for task {t:?} — check with wt list"),
            Error::WorkerStart { source, .. } => {
                write!(
                    f,
                    "the worktree was made but the worker would not start: {source}"
                )
            }
            Error::OutsideHerdr => write!(f, "this is outside herdr — run it from inside herdr"),
            Error::AgentStart { pane_id, source } => {
                write!(f, "the agent would not start ({pane_id}): {source}")
            }
            Error::CleanRefused { dirty, unmerged } => {
                let mut parts = Vec::new();
                if !dirty.is_empty() {
                    parts.push(format!("{} uncommitted or untracked", dirty.len()));
                }
                if *unmerged > 0 {
                    parts.push(format!("{unmerged} unmerged commits"));
                }
                write!(
                    f,
                    "refusing to clean up: {} — commit and merge, then try again",
                    parts.join(", ")
                )
            }
            Error::MergeFailed(e) => write!(
                f,
                "merge failed — resolve it in the repository yourself: {e}"
            ),
            Error::NoChanges => write!(f, "nothing changed — there is no diff to review"),
            Error::NoReviewFile => write!(f, "there is no review file — run wt review first"),
            Error::NoComments => {
                write!(
                    f,
                    "there are no comments — add {:?} lines to the review file",
                    COMMENT_MARKER.trim()
                )
            }
            Error::NoTestCmd(task) => {
                write!(
                    f,
                    "no test command — give it one with wt test {task} --cmd '...'"
                )
            }
            Error::NoAgentIn(p) => write!(f, "no agent is running in worktree {p}"),
            Error::PaneMoved {
                pane,
                expected,
                found,
            } => write!(
                f,
                "pane {pane} now holds an agent in {found}, not {expected} — refusing to send there"
            ),
            Error::WorkerBusy { task, status } => write!(
                f,
                "{task} is {status} — send new work once it settles (a follow-up to its current item goes with `herdr agent prompt`)"
            ),
            Error::SeveralAgentsIn { worktree, panes } => write!(
                f,
                "more than one agent is running in {worktree} ({}) — refusing to guess",
                panes.join(", ")
            ),
            Error::SessionChanged { task } => write!(
                f,
                "{task}'s pane now holds a different conversation than its record — try again in a few seconds"
            ),
            Error::NoAgentInPane(p) => write!(f, "no agent is running in pane {p}"),
            Error::PaneReassigned { pane } => write!(
                f,
                "pane {pane} now holds a different agent than the item was assigned to — assign it again"
            ),
            Error::PaneElsewhere { pane } => write!(
                f,
                "pane {pane} was assigned on another herdr server — send from there"
            ),
            Error::OtherRepo { task, repo } => {
                write!(f, "task {task} belongs to {repo}, not this board's repository")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Mux(e) => Some(e),
            Error::Herdr(e) => Some(e),
            Error::AgentStart { source, .. } => Some(source),
            Error::WorkerStart { source, .. } => Some(source),
            Error::MergeFailed(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl From<tower_mux::Error> for Error {
    fn from(e: tower_mux::Error) -> Self {
        Error::Mux(e)
    }
}

impl From<herdrx::Error> for Error {
    fn from(e: herdrx::Error) -> Self {
        Error::Herdr(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
