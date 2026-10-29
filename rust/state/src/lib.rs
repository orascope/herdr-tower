// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Agent records: the control tower's own source of truth.
//!
//! The field names and the RFC 3339 timestamps are pinned rather than
//! idiomatic: the on-disk shape is a format that existing stores were written
//! in, and every record already on disk has to keep reading.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub mod journal;
mod store;
pub use store::{group_key, GroupKey, LockTimes, Store, StoreLock, HOOK_PATIENCE, LOCK_FILE};
mod loop_store;
pub use loop_store::{
    LoopLease, LoopStore, DEFAULT_IDLE_FOR, DEFAULT_LOOP_MESSAGE, DEFAULT_LOOP_TTL,
};
mod schedule_store;
pub use schedule_store::{
    clock_label, local_offset, new_schedule_id, parse_date, parse_span, parse_when, ClockTime,
    Repeat, Schedule, ScheduleStore, ScheduleTarget, When, DEFAULT_SCHEDULE_WAIT_MINUTES,
};
mod daemon_store;
pub use daemon_store::{DaemonBackend, DaemonSnapshot, DaemonStore};
mod parallax_store;
pub use parallax_store::{
    parallax_waiting, sort_handoffs, Attention, NotifiedHandoff, ParallaxHandoff, ParallaxSnapshot,
    ParallaxStore, PARALLAX_FILE, PARALLAX_FRESH, PARALLAX_NOTIFIED_FILE,
};
mod board_store;
pub use board_store::{
    pid_alive, Applied, AssignTo, Board, BoardError, BoardItem, BoardStore, ItemStatus, Outcome,
    PaneClaim, PaneWorker, Receipt, Reservation, SendFlags, SendTo, Ticket, Typing,
    RESERVATION_TTL, TYPING_WINDOW,
};
mod board_view;
pub use board_view::{column, Column, Progress};
mod hint_store;
pub use hint_store::{FieldHints, HintStore, HINTS_FILE};
mod tree_store;
pub use tree_store::{TreeLink, TreeStore, TREE_FILE};
mod untrusted;
pub use untrusted::{clean_text, sh_quote, valid_session_id, MAX_SESSION_ID};

/// Shown in place of a workspace's name when herdr reports none.
///
/// A pane whose workspace is gone still has a record, and every surface that
/// groups by workspace — the tower's list, the Discord card — has to put those
/// rows under a heading rather than one with a hole in it. Named once here so
/// two surfaces cannot drift into two spellings of the same absence.
pub const NO_WORKSPACE: &str = "(no workspace)";

/// A WORKING record this old is suspected of a lost hook.
pub const STALE_AFTER: time::Duration = time::Duration::minutes(30);

/// How many characters [`Agent::short_session_id`] keeps.
pub const SHORT_SESSION_ID: usize = 8;

/// What an agent is doing, as the tower understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// A tool is running or a turn is in progress.
    #[serde(rename = "WORKING")]
    Working,
    /// Waiting on the user — input or approval.
    #[serde(rename = "WAITING")]
    Waiting,
    /// The turn ended and the user has not looked yet.
    #[serde(rename = "DONE_UNREAD")]
    DoneUnread,
    /// Seen, or freshly discovered.
    #[serde(rename = "IDLE")]
    Idle,
    /// An abnormal exit was detected.
    #[serde(rename = "ERROR")]
    Error,
    /// The pane or process is gone.
    #[serde(rename = "DEAD")]
    Dead,
}

impl AgentState {
    /// Sort order for the list: what needs a person first.
    ///
    /// Needs action (Waiting) > unseen completion > error > in progress > rest.
    pub fn priority(self) -> u8 {
        match self {
            AgentState::Waiting => 0,
            AgentState::DoneUnread => 1,
            AgentState::Error => 2,
            AgentState::Working => 3,
            AgentState::Idle => 4,
            AgentState::Dead => 5,
        }
    }
}

/// Where an agent lives, as the backend names it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaneRef {
    /// Empty for the default Herdr socket; otherwise the socket path that owns
    /// this coordinate. Pane IDs are unique only inside one Herdr backend.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub window: i64,
    /// The backend's coordinate. herdr uses `"w2:p4S"`.
    #[serde(default, rename = "pane_id")]
    pub pane_id: String,
    /// Stable across a pane move, even when its workspace-qualified pane ID
    /// changes. Empty for records written before this field existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_id: String,
    /// The label of the tab this pane is in, as herdr reports it — its
    /// number until somebody names it. Refreshed on every scan.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tab: String,
    /// The workspace this pane is in, as herdr identifies it (`"w2"`).
    ///
    /// `session` above is the workspace's *label*, which is what a person reads
    /// and what a person can change. Grouping on the label splits one workspace
    /// in two the moment somebody renames it while a dead record is still being
    /// retained: the scanner refreshes the label from the backend, and a dead
    /// pane is not in the backend's listing, so it keeps the old one until its
    /// retention expires. The id does not move, so the group does not split.
    ///
    /// Empty for a record written before this field existed, and the grouping
    /// falls back to the label there — which is the behaviour those records
    /// already had.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
}

/// One agent under supervision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    /// One of [`SUPERVISED_KINDS`]: `claude` | `codex` | `gemini` | `opencode`
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    /// The name a person gave this pane with `/rename`, if they gave one.
    ///
    /// Deliberate, so it outranks everything the tool inferred — see
    /// [`Agent::headline`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// The pane's terminal title, with the agent's own status glyph stripped.
    ///
    /// The agent CLIs keep this current by themselves — claude writes a summary
    /// of the turn, codex the task and its branch — so it is the best thing left
    /// when nothing else has been recorded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// What the agent is asking right now. Cleared once work resumes, so a
    /// stale question cannot sit on the row forever.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ask: String,
    pub state: AgentState,
    #[serde(default)]
    pub pane: PaneRef,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// For resume.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// The prompt size of the last request, as the agent reported it through
    /// its hook. Only for a kind whose CLI keeps no file the tower can read
    /// this from — opencode, whose plugin hands it over. Zero means unknown.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub ctx_tokens: u64,
    /// The window [`Agent::ctx_tokens`] is measured against, reported the same
    /// way. Zero means unknown.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub ctx_window: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pid: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// When a CLI hook last touched this record.
    ///
    /// The backend reports lifecycle state too, and the two can disagree. A
    /// hook knows more — it carries the task and the question, and it fires on
    /// the transition rather than inferring one — so a backend observation
    /// leaves the state alone while this is fresh.
    #[serde(with = "time::serde::rfc3339", default = "epoch")]
    pub last_hook: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub state_since: OffsetDateTime,
    /// Whether the backend's *current* completion has already been seen.
    ///
    /// Marking a row read only changes this record; herdr goes on reporting the
    /// same persistent `done` for that pane, and there is no API to acknowledge
    /// it (checked against `herdr agent --help`). So the next observation used to
    /// map that same `done` straight back to DONE_UNREAD — immediately when no
    /// hook was fresh, and after two minutes when one was. "Read until new work
    /// finishes" could not hold for longer than the hook-precedence window.
    ///
    /// The flag is consumed by any observation that is *not* done, by any
    /// transition to a state other than IDLE, and by the tower dispatching work
    /// into that pane — so a genuinely new completion arrives unread.
    ///
    /// One residual gap, and it cannot be closed from here: with **no hooks
    /// installed**, an agent that starts and finishes a whole turn between two
    /// backend polls is never observed in a non-done state, so its new
    /// completion reads as the acknowledged one and the row stays IDLE until the
    /// turn after. herdr reports a lifecycle state, not a per-turn token, so
    /// nothing distinguishes the two `done`s. A hook closes it (it writes the
    /// transition directly), and so does dispatching the work through the tower;
    /// what is left is somebody typing into a hookless pane faster than the poll
    /// interval.
    #[serde(default, skip_serializing_if = "is_false")]
    pub done_acked: bool,
    /// How many turns this agent has begun.
    ///
    /// A completion is not self-identifying: `DONE` looks the same whichever turn
    /// produced it, and a watcher that dispatched into an agent already working
    /// could not tell the running turn's completion from its own instruction's.
    /// The count survives that completion, which the state alone does not — see
    /// `tower_cli::allcmd`'s watch.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub work_seq: u64,
    /// When the turn now in progress was counted by inference rather than
    /// witnessed by a hook.
    ///
    /// The two counters see the same prompt. A start hook can arrive after the
    /// backend poll that already showed the agent working — the hook runs in the
    /// CLI's process and the poll is on its own clock — and then the scanner
    /// counts the start and the hook counts it again, one prompt worth two turns.
    /// That is not a cosmetic error: `tower_cli::allcmd` reads a jump in the count as
    /// *its own* instruction having started, so an unrelated turn counted twice
    /// lets that turn's completion be reported as the answer.
    ///
    /// So the scanner's count stays unclaimed, and the first hook to arrive takes
    /// it over instead of adding to it.
    ///
    /// An instant rather than a flag, because the claim has to expire. A claim
    /// left standing for the life of the turn could outlive a completion nobody
    /// observed — both the Stop hook and the backend's brief DONE missed — and
    /// then be taken by the *next* prompt, which is the one case where a missing
    /// count strands a watch until timeout.
    #[serde(
        with = "time::serde::rfc3339::option",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub turn_inferred_at: Option<OffsetDateTime>,
    /// When this record was made. A record is made afresh when the scanner
    /// reuses a pane id, and its `work_seq` starts over at zero — so a turn
    /// count is comparable only between readings with the same `born_at`.
    /// `None` on a record written before this field existed.
    #[serde(
        with = "time::serde::rfc3339::option",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub born_at: Option<OffsetDateTime>,
    /// The turn in progress, or the last one, began with a background-task
    /// notification (`<task-notification>`) rather than a prompt from a person
    /// or the tower. Set when the turn begins and kept through it and the
    /// settle after; the next turn decides afresh. The idle loop does not let
    /// such turns restart its clock: an agent that only reacts to its watchers
    /// is not advancing, however busy it looks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background_turn: bool,
    /// `work_seq` as of the last turn that was *not* a background one. A
    /// background turn that ends after an ordinary one — both between two
    /// looks — leaves `background_turn` set; this is how the ordinary one is
    /// still seen.
    #[serde(default, skip_serializing_if = "is_zero_seq")]
    pub ordinary_seq: u64,
    /// `ordinary_seq` as it stood before the scanner counted the turn now in
    /// progress. A turn the scanner infers has an unknown origin and counts as
    /// ordinary — until a background hook takes it over, which puts this back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinary_before_inference: Option<u64>,
}

fn is_zero_seq(n: &u64) -> bool {
    *n == 0
}

/// How long a scanner-inferred turn stays claimable by a hook.
///
/// This measures one thing: how far a start hook's *write* can lag the poll that
/// beat it to the record. The hook fired before that poll — the agent had to be
/// working for the backend to say so — so what is left is spawning the hook
/// binary, starting it, and waiting out whatever holds the store lock. Seconds,
/// under load.
///
/// Deliberately not sized to a turn. Anything longer starts covering turns that
/// finish and are followed by another prompt inside the window, and that next
/// prompt would take the claim and go uncounted — the failure this expiry exists
/// to prevent.
pub const INFERRED_CLAIM: time::Duration = time::Duration::seconds(5);

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

fn epoch() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

impl Agent {
    /// A freshly discovered agent.
    pub fn new(id: impl Into<String>, kind: impl Into<String>, now: OffsetDateTime) -> Self {
        Agent {
            id: id.into(),
            kind: kind.into(),
            task: String::new(),
            label: String::new(),
            title: String::new(),
            ask: String::new(),
            state: AgentState::Idle,
            pane: PaneRef::default(),
            cwd: String::new(),
            session_id: String::new(),
            model: String::new(),
            ctx_tokens: 0,
            ctx_window: 0,
            pid: 0,
            updated_at: now,
            last_hook: epoch(),
            state_since: now,
            done_acked: false,
            work_seq: 0,
            turn_inferred_at: None,
            born_at: Some(now),
            background_turn: false,
            ordinary_seq: 0,
            ordinary_before_inference: None,
        }
    }

    /// Moves to a new state, keeping `state_since` meaningful: it only moves
    /// when the state actually changes, so "5m in WORKING" stays true across
    /// repeated heartbeats.
    pub fn transition(&mut self, to: AgentState, now: OffsetDateTime) {
        if self.state != to {
            self.state = to;
            self.state_since = now;
        }
        // Any move other than into IDLE is new activity, so a completion the
        // user already saw is no longer the current one. `mark_read` is the one
        // caller that sets the flag, and it does so after transitioning.
        if to != AgentState::Idle {
            self.done_acked = false;
        }
        // The turn an inferred count belonged to is over, so the claim expires
        // with it: the next start is counted afresh, by whoever sees it first.
        if to != AgentState::Working && to != AgentState::Waiting {
            self.turn_inferred_at = None;
        }
        self.updated_at = now;
    }

    /// Moves to WORKING and counts it as a turn of its own.
    ///
    /// The count cannot be inferred from the states either side of the edge, and
    /// two attempts to do so were wrong in opposite directions. Counting every
    /// edge into WORKING counted a permission request being answered, since all
    /// three CLIs park a running turn at WAITING and return to WORKING; counting
    /// none of them missed a real new turn, because a *missed* Stop hook also
    /// leaves the record at WAITING (`tower_hook::claude`'s idle echo) and the next
    /// prompt starts from there.
    ///
    /// So the caller says. A hook handling `user-prompt-submit` knows a person
    /// asked for something; the scanner, watching an agent with no hooks, knows
    /// only that a settled record started working. Each calls its own entry
    /// point — this one and [`Agent::infer_turn`] — and nothing else touches the
    /// counter.
    ///
    /// A hook arriving after the scanner already inferred this turn's start takes
    /// that count over rather than adding to it; see [`Agent::turn_inferred`]. It
    /// only ever claims an *inferred* count, never a hook's own: a second prompt
    /// typed into an agent still working on the first is a turn of its own, and
    /// the watcher waiting on it has nothing else to go by.
    pub fn begin_turn(&mut self, now: OffsetDateTime) {
        let claimed = self.claim_inferred_turn(now);
        if !claimed {
            self.work_seq = self.work_seq.wrapping_add(1);
        }
        let before = self.ordinary_before_inference.take();
        if !self.background_turn {
            self.ordinary_seq = self.work_seq;
        } else if let (true, Some(before)) = (claimed, before) {
            // The scanner counted this turn as ordinary; the hook knows it is
            // a background one.
            self.ordinary_seq = before;
        }
        self.transition(AgentState::Working, now);
    }

    /// Takes over a fresh inferred count, reporting whether there was one.
    ///
    /// Spends the claim either way. A stale one is not this prompt's — the turn
    /// it was made for ended without anything observing it — and leaving it for
    /// the prompt after would strand that one too.
    fn claim_inferred_turn(&mut self, now: OffsetDateTime) -> bool {
        let claimed = self
            .turn_inferred_at
            .is_some_and(|at| now - at < INFERRED_CLAIM);
        self.turn_inferred_at = None;
        claimed
    }

    /// Counts a turn the scanner deduced from a settled record starting to work.
    ///
    /// Same count, held loosely: a hook for the same prompt may still be on its
    /// way, and if it arrives it takes this one over.
    pub fn infer_turn(&mut self, now: OffsetDateTime) {
        self.work_seq = self.work_seq.wrapping_add(1);
        // No prompt was seen, so the turn's origin is unknown: an ordinary one,
        // unless the hook that follows says otherwise.
        self.background_turn = false;
        self.ordinary_before_inference = Some(self.ordinary_seq);
        self.ordinary_seq = self.work_seq;
        self.transition(AgentState::Working, now);
        self.turn_inferred_at = Some(now);
    }

    /// Notes that this update came from a hook.
    pub fn mark_hook(&mut self, now: OffsetDateTime) {
        self.last_hook = now;
    }

    /// Whether a hook touched this record recently enough to outrank the
    /// backend's own observation.
    pub fn hook_fresh(&self, now: OffsetDateTime, within: time::Duration) -> bool {
        self.last_hook != epoch() && now - self.last_hook < within
    }

    /// Not DEAD: still a session somebody can look at or talk to.
    ///
    /// A DEAD record is recovery data, kept for `resume` and `restore`; the
    /// list, broadcasts and the tower's background readers all pass it over.
    pub fn is_live(&self) -> bool {
        self.state != AgentState::Dead
    }

    /// WORKING but long unheard from — a lost hook is likely.
    ///
    /// Only for a record with a hook to lose. A hookless one hears nothing
    /// between the backend's own transitions, so a long turn would read as a
    /// lost hook that never existed. A kind whose hook is optional — opencode's
    /// plugin — has one to lose once a hook has been heard from it.
    pub fn stale(&self, now: OffsetDateTime) -> bool {
        self.state == AgentState::Working
            && (has_hook(&self.kind) || self.last_hook != epoch())
            && now - self.updated_at > STALE_AFTER
    }

    /// What to show on the row, in the order of how well it describes the pane
    /// *now*.
    ///
    /// The pane's terminal title leads. The agent CLIs rewrite it every turn —
    /// claude a summary of the turn, codex the task and its branch — so it is
    /// the only candidate that keeps up with what the pane is actually doing.
    /// A `/rename` label is fixed at the moment somebody typed it, and the task
    /// is whatever the last prompt happened to say.
    ///
    /// The name a person gave the pane comes next, and takes the cell whenever
    /// there is no title to lead with — a plain shell, or a CLI that has not
    /// titled its pane yet. Then the question, then the recorded task.
    ///
    /// Nothing is buried by this. This is the tower's own row: the state column
    /// beside it says a pane is waiting, and the detail card is one key away.
    /// The surfaces that have neither lead differently — see [`question_first`]
    /// and [`attention`].
    ///
    /// [`question_first`]: Agent::question_first
    /// [`attention`]: Agent::attention
    pub fn headline(&self) -> &str {
        self.first_of(&[
            &self.title,
            self.chosen_name(),
            &self.ask,
            self.task_shown(),
            // A generated worker name is demoted, not discarded: with nothing
            // else to say it still beats an empty cell.
            &self.label,
        ])
    }

    /// What the pane was asked to do: the work instruction prompt,
    /// falling back to a pending question if no prompt was recorded.
    ///
    /// Empty when none of those was ever recorded; the caller says so.
    pub fn assignment(&self) -> &str {
        self.first_of(&[self.task_shown(), &self.ask])
    }

    /// What the tower's TITLE cell says: the name a person gave the pane
    /// ("PR #7", "wiki-compile"), falling back to the CLI's terminal title.
    ///
    /// The name leads here, unlike [`headline`]: the row puts TASK beside it,
    /// so the pane's own name is what this cell adds. A `wt new` worker's
    /// generated name is not one somebody chose, though — see
    /// [`chosen_name`] — so it waits behind the title rather than putting
    /// `auth-api-3f2a91cc` on every worker row in place of what the CLI says
    /// that worker is doing.
    ///
    /// [`headline`]: Agent::headline
    /// [`chosen_name`]: Agent::chosen_name
    pub fn pane_title(&self) -> &str {
        self.first_of(&[self.chosen_name(), &self.title, &self.label])
    }

    /// The pane coordinate (`w1:p2`), or extracted from `id` if pane_id was unset.
    ///
    /// From after the *last* hyphen: a record on a named backend is
    /// `{kind}-{backend}-{pane}`, and neither a backend ID (`b` and sixteen hex
    /// digits) nor a herdr coordinate has a hyphen of its own. Splitting on the
    /// first handed back the backend with the coordinate (#35).
    pub fn pane_id(&self) -> &str {
        let direct = self.pane.pane_id.trim();
        if !direct.is_empty() {
            direct
        } else if let Some((_kind, pane)) = self.id.rsplit_once('-') {
            if pane.contains(':') {
                pane.trim()
            } else {
                ""
            }
        } else {
            ""
        }
    }

    /// Eight characters of the session ID that tell it apart from the others
    /// on screen, like a git short hash.
    ///
    /// Usually the head. Not for a UUIDv7 — codex's: its head is the top of a
    /// millisecond timestamp, which only ticks over every 65 seconds, so codex
    /// workers started together would all show the same eight. Its tail is
    /// random, so that is what it shows.
    ///
    /// Only v7. The tail is no better anywhere else, and in v1 and v6 it is the
    /// node field, which may be the host's MAC address — the same eight on every
    /// session on the machine.
    ///
    /// The whole ID resumes the conversation and screens get shared, so a
    /// surface that only has to tell sessions apart shows this instead.
    pub fn short_session_id(&self) -> &str {
        short_session_id(&self.session_id)
    }

    /// [`headline`], but a pending question takes the line when there is one.
    ///
    /// For a surface with no detail view behind it and no key to press — the
    /// Discord card, `status` in a terminal or over ssh. A question is the one
    /// thing a reader there cannot get anywhere else, so it is the one thing
    /// that outranks what the pane calls itself; when nothing is being asked
    /// there is no competition for the cell, so the row reads like the tower's,
    /// title and name and all.
    ///
    /// Distinct from [`attention`], which demotes the pane's own names all the
    /// way. That is for a notification: one event, read once, where nobody is
    /// scanning a list to work out which pane is which.
    ///
    /// [`headline`]: Agent::headline
    /// [`attention`]: Agent::attention
    pub fn question_first(&self) -> &str {
        if self.ask.is_empty() {
            self.headline()
        } else {
            &self.ask
        }
    }

    /// What needs a person's attention, for a reader with nothing else on
    /// screen.
    ///
    /// The same candidates as [`headline`], with what needs doing first and what
    /// the pane calls itself behind it. A row can afford to lead with a name
    /// because the state column sits beside it and the detail card is one key
    /// away; a desktop or Discord notification has neither. Its own title
    /// already says which session and which state, so the body is the only room
    /// left to say *what* — and a permission prompt that arrives on a phone
    /// reading `dbeaver` has spent that room on something the title already
    /// covered.
    ///
    /// [`headline`]: Agent::headline
    pub fn attention(&self) -> &str {
        self.first_of(&[&self.ask, self.task_shown(), &self.title, &self.label])
    }

    /// The label, but only when a person chose it.
    ///
    /// Not every label is a `/rename`. `wt new` gives each worker pane a name
    /// derived from its task — normalized, truncated, and suffixed with a digest
    /// so two tasks that normalize alike stay distinct — and herdr returns that
    /// as the pane's label like any other. Preferring it to the task would put
    /// `auth-api-3f2a91cc` on every titleless worker row in place of the task
    /// that names what it is doing.
    ///
    /// A generated name is not discarded, only demoted: it is still what
    /// [`first_of`] falls back to through `label` when nothing else has anything
    /// to say, which beats an empty cell.
    ///
    /// [`first_of`]: Agent::first_of
    fn chosen_name(&self) -> &str {
        if is_generated_worker_name(&self.label) {
            ""
        } else {
            &self.label
        }
    }

    /// The recorded task, unless it is a harness wake-up or machine task ID.
    ///
    /// Only this field can carry one: it is written from the submitted prompt,
    /// and [`tower_hook`] declines a wake-up there now — but a record written before
    /// that rule still has one on disk and would show it forever.
    ///
    /// The filter belongs here rather than on every candidate. A person is free
    /// to name a pane `<frontend>`, and dropping a label they deliberately typed
    /// because it happens to be tag-shaped would be a worse bug than the one
    /// this removes. The title is the agent's own, and not a prompt either.
    ///
    /// [`tower_hook`]: https://docs.rs/tower_hook
    fn task_shown(&self) -> &str {
        if is_machine_tag(&self.task) || is_task_id(&self.task) {
            ""
        } else {
            &self.task
        }
    }

    /// The first candidate with something to show.
    ///
    /// 🚨 Blank counts as nothing. A `/rename` to a space would otherwise take
    /// the line, draw as an empty cell, and hide the question and the task
    /// behind it.
    fn first_of<'a>(&'a self, candidates: &[&'a str]) -> &'a str {
        for s in candidates.iter().copied() {
            if !s.trim().is_empty() {
                return s;
            }
        }
        ""
    }
}

/// A line that is one tag and nothing else: `<name>`, `</name>`, `<name/>`,
/// `<name />`, with or without attributes.
///
/// A wrapper, not something a person typed. `tower_hook` skips such a line when
/// choosing what to record from a prompt, and this is the other half: a record
/// written before that rule still has `<task-notification>` on disk and would
/// show it forever. One predicate so the two cannot disagree — a shape the hook
/// declines and the display accepts would be a wake-up nothing removes.
///
/// The line must open and close with angle brackets and hold no other `>`, so a
/// second element or any text after the first disqualifies it. That is what
/// keeps `<cwd>/repo</cwd>` and `<step>refactor authentication</step>` out:
/// both carry content, and nothing in their shape says which of them a machine
/// wrote. `tower_hook`'s doc comment has the reasoning.
///
/// Whatever follows the name has to be attributes or a self-closing slash.
/// Without that, `<not a tag>` reads as an element named `not` with two
/// attributes and somebody's prompt disappears for sitting between angle
/// brackets.
pub fn is_machine_tag(s: &str) -> bool {
    // An element wrapping nothing — `<metadata></metadata>`, or one holding only
    // whitespace — is as empty as a bare tag, and skipping it discards nothing.
    // It is only separate from the test below because the line has two `>` in it.
    if element_text(s).is_some_and(|t| t.trim().is_empty()) {
        return true;
    }
    let Some(inner) = s
        .strip_prefix('<')
        .and_then(|l| l.strip_suffix('>'))
        .filter(|i| !i.contains('>'))
    else {
        return false;
    };
    let (name, rest) = match inner.find(char::is_whitespace) {
        Some(i) => (&inner[..i], inner[i..].trim()),
        None => (inner, ""),
    };
    let name = name.strip_prefix('/').unwrap_or(name);
    let name = name.strip_suffix('/').unwrap_or(name);
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        && (rest.is_empty() || rest == "/" || rest.contains('='))
}

/// [`Agent::short_session_id`] for a session ID on its own — the eight
/// characters the tower's SID column shows for it.
pub fn short_session_id(session_id: &str) -> &str {
    let sid = session_id.trim();
    if is_uuid_v7(sid) {
        return &sid[sid.len() - SHORT_SESSION_ID..];
    }
    sid.char_indices()
        .nth(SHORT_SESSION_ID)
        .map_or(sid, |(i, _)| &sid[..i])
}

/// Whether a string is a version 7 UUID, read from the version digit at the
/// head of its third group.
fn is_uuid_v7(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
        && b[14] == b'7'
}

/// Whether a string is a 17-character hex task ID generated by Claude subagents.
pub fn is_task_id(s: &str) -> bool {
    s.len() == 17
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The text inside a line that is exactly one element: `<name …>text</name>`.
///
/// `None` when the line is anything else — two elements, text outside the tags,
/// a bare tag with no closing half. The text itself is returned verbatim,
/// whitespace and all, because the two callers want different things from it:
/// [`is_machine_tag`] asks whether it is blank, and the display strips the tags
/// off a line it was going to show anyway.
pub fn element_text(line: &str) -> Option<&str> {
    let close_of_open = line.strip_prefix('<')?.find('>')? + 1;
    let name = line[1..close_of_open].split([' ', '\t']).next()?;
    if name.is_empty() || name.starts_with('/') || name.ends_with('/') {
        return None;
    }
    let inner = line[close_of_open + 1..].strip_suffix(&format!("</{name}>"))?;
    (!inner.contains(['<', '>'])).then_some(inner)
}

/// Whether a pane label is the one `tower_wt::agent_name` generates.
///
/// The shape is its contract: at most 32 characters, starting with a lowercase
/// letter, ending in a hyphen and exactly eight hex digits — the FNV digest that
/// keeps two tasks normalizing alike apart. `tower_wt` has a test asserting every
/// name it produces satisfies this, so the two cannot drift; the predicate lives
/// here because this is where labels are ranked, and `tower_state` is below `tower_wt`.
///
/// A person could in principle type a name of that shape and have it demoted.
/// That is the right way for this to be wrong: the cost is one row showing its
/// task instead of its name, against every `wt new` worker row showing a digest.
pub fn is_generated_worker_name(s: &str) -> bool {
    let Some((stem, digest)) = s.rsplit_once('-') else {
        return false;
    };
    s.len() <= 32
        && digest.len() == 8
        && digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && stem.starts_with(|c: char| c.is_ascii_lowercase())
        && stem
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// What the tower knows about one kind of agent that is not written in the
/// code that handles it.
///
/// One entry per kind, so adding a kind is one line here rather than a hunt
/// through every `match` that sorts, filters or launches by kind — the gap
/// #74 fell into, where opencode panes were seen by herdr and dropped by the
/// tower.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    /// The kind a record carries.
    pub name: &'static str,
    /// The names herdr reports for a pane running it. agy is the Antigravity
    /// CLI, supervised as gemini.
    pub herdr: &'static [&'static str],
    /// Whether it reports through a hook of ours. One that does not is
    /// described by the backend alone, so its record can go quiet without any
    /// hook having been lost.
    pub hooked: bool,
}

/// Every kind the tower keeps a row for, in the order rows sort between
/// kinds, so the vendors do not interleave.
pub const KINDS: [Kind; 4] = [
    Kind {
        name: "claude",
        herdr: &["claude"],
        hooked: true,
    },
    Kind {
        name: "codex",
        herdr: &["codex"],
        hooked: true,
    },
    Kind {
        name: "gemini",
        herdr: &["gemini", "agy"],
        hooked: true,
    },
    Kind {
        name: "opencode",
        herdr: &["opencode"],
        hooked: false,
    },
];

/// The names of [`KINDS`]. Broadcasts go to all of them: a kind supervised
/// here but left out of a close-everything is a session torn down without its
/// handoff.
pub const SUPERVISED_KINDS: [&str; KINDS.len()] = {
    let mut names = [""; KINDS.len()];
    let mut i = 0;
    while i < KINDS.len() {
        names[i] = KINDS[i].name;
        i += 1;
    }
    names
};

/// The entry for a record's kind, or `None` for one the tower does not
/// supervise.
pub fn kind_spec(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.name == name)
}

/// The kind the tower supervises a pane as, from the agent name herdr reports.
pub fn kind_from_herdr(agent: &str) -> Option<&'static str> {
    KINDS
        .iter()
        .find(|k| k.herdr.contains(&agent))
        .map(|k| k.name)
}

/// Whether this kind reports through a hook of ours ([`Kind::hooked`]).
pub fn has_hook(kind_name: &str) -> bool {
    kind_spec(kind_name).is_some_and(|k| k.hooked)
}

/// Sort order between kinds: the position in [`KINDS`], with every unknown
/// kind after them all.
pub fn kind_rank(kind_name: &str) -> u8 {
    KINDS
        .iter()
        .position(|k| k.name == kind_name)
        .unwrap_or(KINDS.len()) as u8
}

/// The state directory, honoring `HERDR_TOWER_STATE_DIR`.
pub fn default_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HERDR_TOWER_STATE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    user_dir()
}

/// This user's ordinary state directory, ignoring a per-invocation override.
///
/// Coordination that has to survive two callers selecting different record
/// stores needs a stable per-user reference point.
pub fn user_dir() -> PathBuf {
    // An empty `HOME` counts as no home. A service or container that exports
    // `HOME=` rather than leaving it unset would otherwise get the *relative*
    // path `.local/state/herdr-tower`, resolved against each process's working
    // directory — so a hook (which inherits the agent's cwd) and the tower would
    // write to different stores, supervision would look empty, and state files
    // would land inside whatever project happened to be checked out.
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Path::new(&home)
            .join(".local")
            .join("state")
            .join("herdr-tower"),
        // One directory per *user*, not one for the machine. Several service
        // accounts running with `HOME` unset all resolved to `/tmp/herdr-tower`:
        // whoever got there first owned it, the rest could not write a record or
        // take the lock, and independent installations read each other's agents.
        _ => home_less_dir(),
    }
}

/// Where [`user_dir`] puts the state when there is no home: a per-user name in
/// the shared temporary directory.
fn home_less_dir() -> PathBuf {
    std::env::temp_dir().join(format!("herdr-tower{}", user_tag()))
}

/// Creates a store's root directory, `dir`.
///
/// The home-less fallback is created through [`private_dir`], not
/// `create_dir_all`. Its name is predictable and it sits in a directory every
/// local account can write, so on Linux another user could create it first —
/// or plant a symlink there — and then read every record written into it, or
/// hand this user records of their own. `create_dir_all` accepted either, and
/// the permission narrowing after it ignored its own failure. Now an existing
/// fallback has to be a real directory owned by this user, or the store does
/// not open at all.
///
/// A root under `HOME` keeps the plain `create_dir_all`: its parent is this
/// user's own, and a store that also accepts being reached through a symlink or
/// opened by root on the user's behalf is what `HERDR_TOWER_DIR` promises.
pub(crate) fn make_state_root(dir: &Path) -> io::Result<()> {
    make_state_root_as(dir, dir == home_less_dir())
}

/// [`make_state_root`] with the shared-directory decision made by the caller,
/// so a test can exercise it without unsetting `HOME` for the whole process.
fn make_state_root_as(dir: &Path, shared: bool) -> io::Result<()> {
    if shared {
        private_dir(dir)
    } else {
        fs::create_dir_all(dir)
    }
}

/// A caller-writable runtime directory for state-store-independent locks.
///
/// The herdr socket's parent is not necessarily writable by a client that may
/// connect to it, and `HOME` and `TMPDIR` may differ or be absent for a
/// socket-only client. A private per-user directory under the fixed system
/// temporary root gives every invocation a stable reference point without
/// depending on either environment.
pub fn coordination_dir() -> io::Result<PathBuf> {
    prepare_coordination_dir(&coordination_runtime_root())
}

fn prepare_coordination_dir(runtime_root: &Path) -> io::Result<PathBuf> {
    let user = runtime_root.join(format!("herdr-tower{}", user_tag()));
    private_dir(&user)?;
    let dir = user.join("coordination");
    private_dir(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn coordination_runtime_root() -> PathBuf {
    PathBuf::from("/tmp")
}

#[cfg(not(unix))]
fn coordination_runtime_root() -> PathBuf {
    std::env::temp_dir()
}

/// Creates `path` as an owner-only directory and refuses a path planted by a
/// different local account. The name is predictable, so permissions alone are
/// not enough: another user could create it first and make our locks theirs.
#[cfg(unix)]
fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() || meta.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by this user", path.display()),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path).or_else(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(e)
        }
    })
}

#[cfg(test)]
mod kind_rank_tests {
    use super::*;

    /// A supervised kind has a place of its own: opencode rows sit together
    /// after the three hooked vendors, not mixed in with whatever unknown kind
    /// an old record still carries.
    #[test]
    fn opencode_sorts_after_the_hooked_vendors_and_before_an_unknown_kind() {
        let order: Vec<u8> = ["claude", "codex", "gemini", "opencode", "aider"]
            .iter()
            .map(|k| kind_rank(k))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }

    /// herdr's agent names map onto the tower's kinds through the table:
    /// each kind's own name to itself, agy folded into gemini, and anything
    /// else to no row at all.
    #[test]
    fn herdr_names_map_onto_the_supervised_kinds() {
        for k in SUPERVISED_KINDS {
            assert_eq!(kind_from_herdr(k), Some(k));
            assert_eq!(kind_spec(k).map(|s| s.name), Some(k));
        }
        assert_eq!(kind_from_herdr("agy"), Some("gemini"));
        assert_eq!(kind_from_herdr("aider"), None);
        assert_eq!(kind_spec("agy"), None, "agy is a herdr name, not a kind");
    }

    /// The three kind helpers answer about one set. A hooked kind that is not
    /// supervised would be a hook writing rows broadcasts never reach; two
    /// supervised kinds sharing a rank would interleave in the list; and a
    /// supervised kind ranked with the unknown ones is the gap #74 closed.
    #[test]
    fn the_kind_helpers_agree_on_one_set_of_kinds() {
        for kind in [
            "claude", "codex", "gemini", "opencode", "aider", "", "Claude",
        ] {
            if has_hook(kind) {
                assert!(
                    SUPERVISED_KINDS.contains(&kind),
                    "{kind:?} is hooked but not supervised"
                );
            }
        }
        // Hookless supervised kinds are the ones that are listed as such, not
        // whatever `has_hook` happens to forget.
        let hookless: Vec<&str> = SUPERVISED_KINDS
            .into_iter()
            .filter(|k| !has_hook(k))
            .collect();
        assert_eq!(hookless, ["opencode"]);

        let unknown = kind_rank("aider");
        let mut ranks: Vec<u8> = SUPERVISED_KINDS.iter().map(|k| kind_rank(k)).collect();
        assert!(
            ranks.iter().all(|&r| r < unknown),
            "{ranks:?} vs unknown {unknown}"
        );
        ranks.sort_unstable();
        ranks.dedup();
        assert_eq!(
            ranks.len(),
            SUPERVISED_KINDS.len(),
            "two kinds share a rank"
        );
        // Matching is exact: a different case or an empty kind is unknown.
        assert_eq!(kind_rank("Claude"), unknown);
        assert_eq!(kind_rank(""), unknown);
        assert!(!has_hook("Claude") && !has_hook(""));
    }
}

#[cfg(test)]
mod pane_ref_tests {
    use super::*;

    #[test]
    fn terminal_identity_round_trips_and_old_records_remain_readable() {
        let legacy: PaneRef = serde_json::from_value(serde_json::json!({
            "backend":"lab", "session":"작업", "pane_id":"w2:p1",
            "workspace_id":"w2"
        }))
        .unwrap();
        assert!(legacy.terminal_id.is_empty());
        assert!(serde_json::to_value(&legacy)
            .unwrap()
            .get("terminal_id")
            .is_none());

        let mut moved = legacy;
        moved.pane_id = "w7:p3".into();
        moved.workspace_id = "w7".into();
        moved.terminal_id = "term-한글-1".into();
        let encoded = serde_json::to_value(&moved).unwrap();
        assert_eq!(encoded["terminal_id"], "term-한글-1");
        let decoded: PaneRef = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.pane_id, "w7:p3");
        assert_eq!(decoded.workspace_id, "w7");
        assert_eq!(decoded.terminal_id, "term-한글-1");
        assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);

        for n in 0..64 {
            moved.terminal_id = format!("term-{n}-한글 /:\n");
            let wire = serde_json::to_string(&moved).unwrap();
            let read: PaneRef = serde_json::from_str(&wire).unwrap();
            assert_eq!(read.terminal_id, moved.terminal_id, "case {n}");
            assert_eq!(read.pane_id, moved.pane_id, "case {n}");
            assert_eq!(serde_json::to_string(&read).unwrap(), wire, "case {n}");
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn coordination_is_under_the_runtime_root_not_home() {
        let runtime = tempfile::tempdir().unwrap();
        let dir = prepare_coordination_dir(runtime.path()).unwrap();
        assert_eq!(dir.file_name(), Some(std::ffi::OsStr::new("coordination")));
        assert_eq!(dir.parent().and_then(Path::parent), Some(runtime.path()));
    }

    #[cfg(unix)]
    #[test]
    fn coordination_runtime_root_does_not_follow_tmpdir() {
        assert_eq!(coordination_runtime_root(), Path::new("/tmp"));
    }

    #[cfg(unix)]
    #[test]
    fn coordination_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let runtime = tempfile::tempdir().unwrap();
        let dir = prepare_coordination_dir(runtime.path()).unwrap();
        for path in [dir.parent().unwrap(), dir.as_path()] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is {mode:o}", path.display());
        }
    }

    /// The home-less fallback is named the way the coordination root is, and
    /// sits in the temporary directory — which is why it gets the same checks.
    #[test]
    fn the_home_less_fallback_is_a_per_user_name_in_the_temp_dir() {
        let dir = home_less_dir();
        assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(format!("herdr-tower{}", user_tag()).as_str())
        );
    }

    /// A shared root is born owner-only, and one that already exists wide open
    /// is narrowed rather than accepted as it is.
    #[cfg(unix)]
    #[test]
    fn a_shared_state_root_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("fresh");
        make_state_root_as(&fresh, true).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&fresh), 0o700);

        let wide = tmp.path().join("wide");
        fs::create_dir(&wide).unwrap();
        fs::set_permissions(&wide, fs::Permissions::from_mode(0o777)).unwrap();
        make_state_root_as(&wide, true).unwrap();
        assert_eq!(mode(&wide), 0o700);
    }

    /// The attack itself: a symlink planted where the fallback will be is
    /// refused, where `create_dir_all` followed it. Under `HOME` it is still
    /// followed — the contrast is the point.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_not_accepted_as_a_shared_state_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let planted = tmp.path().join("planted");
        symlink(elsewhere.path(), &planted).unwrap();
        let err = make_state_root_as(&planted, true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        make_state_root_as(&planted, false).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_not_accepted_as_the_user_root() {
        use std::os::unix::fs::symlink;

        let runtime = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let user = runtime.path().join(format!("herdr-tower{}", user_tag()));
        symlink(elsewhere.path(), &user).unwrap();
        let err = prepare_coordination_dir(runtime.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}

/// `-<uid>` for this user.
#[cfg(unix)]
fn user_tag() -> String {
    format!("-{}", effective_uid())
}

// Calling libc::geteuid keeps the shared path independent of the
// caller's HOME, TMPDIR, USER, and current directory.
#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: libc::geteuid(2) takes no pointers or arguments and only returns the
    // effective user ID of this process.
    unsafe { libc::geteuid() as u32 }
}

#[cfg(not(unix))]
fn user_tag() -> String {
    String::new()
}

/// Writes `data` to `path` without ever leaving a half-written file behind:
/// a temp file in the same directory, then a rename.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    // A per-write name, not a per-process one. Two threads saving at the same
    // moment shared `.tmp-<pid>` before: one would truncate or rename the
    // other's file out from under it, so a save failed with `NotFound` or — for
    // two different agent IDs — landed one agent's JSON under the other's
    // filename. The counter makes it unique within the process and the pid
    // across processes.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{n}", std::process::id()));
    {
        // 0600 at creation, not afterwards. `File::create` obeys the umask,
        // which is usually 022, so the temp file was born 0644 and `rename`
        // carried that mode onto the record — even over a 0600 record already
        // on disk. These files hold the task text, the question an agent is
        // asking, and a session ID that resumes the conversation; any local
        // user who can traverse the state directory could read them.
        // Setting the mode after the write would leave a window where they can.
        let mut f =
            tower_os::private(fs::OpenOptions::new().write(true).create_new(true)).open(&tmp)?;
        // A write that fails — a full disk, above all — takes its temp file
        // with it. Left behind, and with the supervisor trying again every two
        // seconds, they piled up by the thousand and kept the disk full.
        if let Err(e) = f.write_all(data).and_then(|()| f.sync_all()) {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Convenience for callers that keep records in memory.
pub type Agents = HashMap<String, Agent>;

#[cfg(test)]
mod atomic_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Two savers at once must not land on one temp file.
    ///
    /// They shared `.tmp-<pid>` before, so one truncated or renamed the other's
    /// file mid-write: the loser failed with `NotFound`, and with two different
    /// agent IDs one agent's JSON could arrive under the other's filename. The
    /// assertion is not that both succeed — it is that each file holds its own
    /// bytes.
    #[cfg(unix)]
    #[test]
    fn concurrent_saves_do_not_overwrite_each_other() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let body = format!("record-{i}").repeat(500).into_bytes();
                    for _ in 0..20 {
                        write_atomic(&dir.join(format!("a{i}.json")), &body).expect("save");
                    }
                    body
                })
            })
            .collect();

        for (i, h) in handles.into_iter().enumerate() {
            let want = h.join().expect("thread");
            let got = fs::read(dir.join(format!("a{i}.json"))).expect("read back");
            assert_eq!(got, want, "a{i}.json holds another writer's bytes");
        }
        // Owner-only, and the record inherits the temp file's mode through the
        // rename. These hold the task text, the pending question, and a session
        // ID that resumes the conversation.
        for i in 0..8 {
            let mode = fs::metadata(dir.join(format!("a{i}.json")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "a{i}.json is readable by others: {mode:o}");
        }
        // Nothing left behind: every temp file was renamed or removed.
        let strays: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(strays.is_empty(), "temp files left over: {strays:?}");
    }

    // ── g4testadd: the short session id ─────────────────────────────────

    /// An independent reading of a version-7 UUID: five dash-separated groups
    /// of 8-4-4-4-12 hex digits, the third starting with `7`.
    fn looks_like_uuid_v7(s: &str) -> bool {
        let groups: Vec<&str> = s.split('-').collect();
        groups.len() == 5
            && groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
            && groups
                .iter()
                .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
            && groups[2].starts_with('7')
    }

    fn uuid(seed: u64, version: char) -> String {
        let hex =
            |n: u64, len: usize| format!("{:0width$x}", n, width = 16)[16 - len..].to_string();
        format!(
            "{}-{}-{}{}-{}-{}",
            hex(seed.wrapping_mul(0x9E37_79B9), 8),
            hex(seed >> 7, 4),
            version,
            hex(seed >> 3, 3),
            hex(seed.wrapping_mul(31), 4),
            hex(seed.wrapping_mul(0x2545_F491_4F6C), 12)
        )
    }

    /// Cross validation: the UUIDv7 test agrees with an independent reading
    /// for every version digit and for strings that are almost UUIDs.
    #[test]
    fn the_uuid_v7_test_agrees_with_a_second_reading() {
        let mut cases: Vec<String> = Vec::new();
        for seed in 0..40u64 {
            for v in ['1', '4', '6', '7', '8'] {
                cases.push(uuid(seed * 7919 + 1, v));
            }
        }
        let v7 = uuid(42, '7');
        cases.extend([
            v7.replace('-', "_"),
            v7.to_uppercase(),
            format!("{v7}0"),
            v7[1..].to_string(),
            v7.replacen('a', "g", 1),
            String::new(),
            "short".into(),
        ]);
        for case in &cases {
            assert_eq!(is_uuid_v7(case), looks_like_uuid_v7(case), "{case:?}");
        }
    }

    /// Property: the short id is at most eight characters, and it is the head
    /// of the trimmed id — or, for a UUIDv7, its tail; never anything else.
    #[test]
    fn the_short_id_is_the_head_or_for_v7_the_tail() {
        let mut ids: Vec<String> = (0..60u64)
            .flat_map(|seed| ['4', '7'].map(|v| uuid(seed * 104_729 + 3, v)))
            .collect();
        ids.extend(
            [
                "",
                "a",
                "abc",
                "  padded  ",
                "세션아이디테스트12345",
                "conversation-loop",
            ]
            .map(String::from),
        );
        for id in &ids {
            let mut a = Agent::new("claude-1", "claude", OffsetDateTime::UNIX_EPOCH);
            a.session_id = id.clone();
            let short = a.short_session_id();
            let trimmed = id.trim();
            assert!(
                short.chars().count() <= SHORT_SESSION_ID,
                "{id:?} -> {short:?}"
            );
            if is_uuid_v7(trimmed) {
                assert!(
                    trimmed.ends_with(short) && short.len() == SHORT_SESSION_ID,
                    "{id:?}"
                );
            } else {
                assert!(trimmed.starts_with(short), "{id:?} -> {short:?}");
                assert_eq!(
                    short.chars().count(),
                    trimmed.chars().count().min(SHORT_SESSION_ID)
                );
            }
        }
    }

    // ── g4testadd: the tab label on disk ────────────────────────────────

    /// A record written before `tab` existed has to keep reading, and a record
    /// with no tab label has to be written exactly as it was before the field
    /// existed — no `"tab": ""` — so an unlabeled record's bytes do not change
    /// and the scanner's write-only-when-changed pass leaves it alone.
    #[test]
    fn a_record_from_before_the_tab_reads_with_none_and_writes_no_key() {
        let old = r#"{
            "id": "claude-w2:p1",
            "kind": "claude",
            "state": "IDLE",
            "pane": {"session": "dev", "window": 7, "pane_id": "w2:p1", "workspace_id": "w2"},
            "updated_at": "2024-10-03T00:00:00Z",
            "state_since": "2024-10-03T00:00:00Z"
        }"#;
        let a: Agent = serde_json::from_str(old).expect("a pre-tab record still reads");
        assert_eq!(a.pane.tab, "");
        assert_eq!(a.pane.pane_id, "w2:p1", "and the rest of the pane with it");

        let written = serde_json::to_value(&a).unwrap();
        assert!(
            written["pane"].get("tab").is_none(),
            "an empty tab is not written: {written}"
        );

        // A record with no `pane` object at all reads the same.
        let bare = r#"{"id":"claude-1","kind":"claude","state":"DEAD",
            "updated_at":"2024-10-03T00:00:00Z","state_since":"2024-10-03T00:00:00Z"}"#;
        let b: Agent = serde_json::from_str(bare).unwrap();
        assert_eq!(b.pane.tab, "");
    }

    /// Property: whatever a person names a tab — blank, spaced, quoted,
    /// escaped, non-Latin, the `›` the TITLE cell draws beside it — the label
    /// survives a write and a read, the key is written exactly when the label is
    /// not empty, and writing what was read gives back the same bytes. The last
    /// is what keeps a record that did not change from being rewritten on every
    /// pass.
    #[test]
    fn any_tab_label_survives_a_round_trip_and_only_an_empty_one_is_left_out() {
        const PIECES: [&str; 10] = [
            "", " ", "pr-2752", "리뷰", "›", "\"q\"", "\\", "\t", "🗼", "7",
        ];
        let mut rng = tower_testrng::Rng::new(0x2545_F491_4F6C_DD1D);
        for _ in 0..300 {
            let tab: String = (0..rng.below(4))
                .map(|_| PIECES[rng.below(PIECES.len())])
                .collect();
            let mut a = Agent::new("claude-w2:p1", "claude", OffsetDateTime::UNIX_EPOCH);
            a.pane.pane_id = "w2:p1".into();
            a.pane.tab = tab.clone();

            let bytes = serde_json::to_vec_pretty(&a).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                value["pane"].get("tab").is_some(),
                !tab.is_empty(),
                "{tab:?}"
            );
            let back: Agent = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(back.pane.tab, tab);
            assert_eq!(
                serde_json::to_vec_pretty(&back).unwrap(),
                bytes,
                "{tab:?} did not write back byte for byte"
            );
        }
    }

    // ── g4testadd: task IDs ─────────────────────────────────────────────

    /// Cross validation: the task-ID test agrees with a plain reading — exactly
    /// seventeen characters, every one of them in `0-9a-f` — over lengths
    /// either side of seventeen, upper case, a non-hex letter, and a string
    /// whose seventeen *bytes* are fewer characters.
    #[test]
    fn the_task_id_test_agrees_with_a_second_reading() {
        let looks_like_task_id =
            |s: &str| s.chars().count() == 17 && s.chars().all(|c| "0123456789abcdef".contains(c));
        let mut cases: Vec<String> = [
            "a77b79fc0975db12e",
            "a77b79fc0975db12",
            "a77b79fc0975db12e0",
            "A77B79FC0975DB12E",
            "a77b79fc0975db12g",
            "a77b79fc0975db 2e",
            "12345678901234567",
            "a77b79fc0975db가",
            "",
            "refactor the auth",
        ]
        .map(String::from)
        .to_vec();
        let mut rng = tower_testrng::Rng::new(17);
        // Mostly lowercase hex, so a good share of the generated strings are
        // task IDs and the rest miss by one character or one length.
        let alphabet = b"0123456789abcdef0123456789abcdefAg";
        for _ in 0..300 {
            let len = 15 + rng.below(4);
            cases.push(
                (0..len)
                    .map(|_| alphabet[rng.below(alphabet.len())] as char)
                    .collect(),
            );
        }
        for case in &cases {
            assert_eq!(is_task_id(case), looks_like_task_id(case), "{case:?}");
        }
    }
}
