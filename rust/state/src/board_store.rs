// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The dispatch board: what an advisor handed to which `wt` worker.
//!
//! One board per repository — the base checkout root that `wt new` records as
//! a task's `repo` — because advisors are handed over to fresh sessions several
//! times a day and a board keyed by session would be orphaned each time. The
//! contract is `docs/dispatch-board.md`.
//!
//! Only what the advisor decides is stored here: an item's status is planned,
//! sent, done, dropped or superseded. How the worker is doing is read from its
//! agent record at render time and never written back.
//!
//! **A send is not one step.** The socket round trip happens outside the
//! board's lock, so a send is split into [`Board::reserve_send`] (under the
//! lock), the delivery (not here) and [`Board::apply`] (under the lock again).
//! An outcome lands only while the reservation it came from is still the
//! item's open send (same attempt). `done`, `drop` and recovery end the
//! reservation, so a late result cannot undo them; a `note` does not, because
//! it changes nothing about the delivery. `revision` still counts every change,
//! for readers that want to notice one.
//!
//! **Nothing is resent.** A reservation whose process is gone, or which is
//! older than [`RESERVATION_TTL`], becomes [`ItemStatus::SentUnknown`]: the
//! worker may or may not have the item, and only an explicit `--again` sends it
//! a second time.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::write_atomic;

const DIR: &str = "boards";
const LOCK: &str = "boards.lock";

/// How long a reservation may stay open before it is taken as abandoned even
/// though a process with its pid is alive — a pid can be reused, and no prompt
/// takes this long.
pub const RESERVATION_TTL: time::Duration = time::Duration::minutes(10);

/// How long a reservation to type the view into a pane holds off another.
pub const TYPING_WINDOW: time::Duration = time::Duration::seconds(20);

/// What the advisor has decided about an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Planned,
    /// A send is in flight; see [`BoardItem::reservation`].
    Sending,
    Sent,
    /// The delivery's outcome is unknown: the server may have received it.
    SentUnknown,
    Done,
    Dropped,
    /// Another item was sent to the same worker with `--replace`.
    Superseded,
}

impl ItemStatus {
    /// Whether the item still occupies its worker.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Sending | Self::Sent | Self::SentUnknown)
    }

    /// Whether the advisor has closed the item: drawn struck through.
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Done | Self::Dropped | Self::Superseded)
    }

    /// The word `show` prints.
    pub fn label(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::SentUnknown => "sent?",
            Self::Done => "done",
            Self::Dropped => "dropped",
            Self::Superseded => "superseded",
        }
    }
}

/// What a confirmed delivery captured **before** the prompt went out.
///
/// Captured before, not after: a short turn can start and end between the
/// prompt and a later read, and its evidence would be taken for the baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Empty means the default herdr socket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    /// The worker's native session id. Empty when it was not known yet; the
    /// board then withholds progress rather than guess.
    #[serde(default)]
    pub session_id: String,
    /// The worker's `work_seq` (turns begun) just before the prompt.
    pub work_seq: u64,
    /// The agent record the delivery reached.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    /// Not delivered by the board: the advisor sent the item itself and the
    /// board took it over (`assign --sent`). Its turn may already have begun
    /// or ended, so progress read against it is marked uncertain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adopted: bool,
    /// The worker record's `born_at`: `work_seq` is compared only against a
    /// record with the same one.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub record_born: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The prompt landed but the completion boundary was not recorded, so
    /// telling this item's turn from an earlier one is uncertain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boundary_lost: bool,
}

/// A worker named by its pane rather than by a `wt` task: a long-lived agent
/// that takes items in turn.
///
/// Pinned to the agent the pane held when it was named — record generation
/// and session — because herdr reuses pane ids: a send checks the pane still
/// holds that agent, and refuses otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneWorker {
    /// Empty for the default herdr socket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    pub pane_id: String,
    /// The agent record's id when it was named.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub born_at: Option<OffsetDateTime>,
}

/// Who an item goes to, as a send names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTo {
    Task { task: String, worktree: String },
    Pane(PaneWorker),
}

/// Who an item is assigned to. A label is a worker the tower cannot see —
/// another machine's — and is shown, never sent to or tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignTo {
    Send(SendTo),
    Label(String),
}

/// An open send: who is sending, what it goes back to if herdr refuses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    /// The item's attempt number this reservation is; a ticket carries it.
    pub attempt: u32,
    pub pid: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub worktree: String,
    /// Set instead of `task` when the send names a pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneWorker>,
    /// The agent the send is for, when known: two items cannot be open for one
    /// agent however they name it — by task or by pane.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    /// The status to go back to when herdr refuses the delivery.
    pub previous: ItemStatus,
    /// The items this send replaces (`--replace`), frozen on confirmation.
    /// More than one only after an earlier `--replace` ended `sent?`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardItem {
    /// Stable for life: a closed item keeps its number.
    pub n: u32,
    pub text: String,
    pub status: ItemStatus,
    pub revision: u64,
    /// Who the item is with — at most one of these three. The `wt` task it
    /// went to (or was assigned to).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worktree: String,
    /// …or the pane of a long-lived worker…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneWorker>,
    /// …or a label for a worker the tower cannot see.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Deliveries attempted, confirmed or not.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation: Option<Reservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
    /// Superseded while its own delivery was `sent?`: whether the worker ever
    /// had it is unknown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncertain: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The delivery attempt whose "ready for review" was already announced;
    /// a resend is a new attempt and is announced again.
    #[serde(default, skip_serializing_if = "is_zero_n")]
    pub review_noticed: u32,
    /// 1 (highest) to 3; 0 for none. `next` takes the highest first.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub priority: u8,
    /// A pull request in the board's repository that carries the item.
    #[serde(default, skip_serializing_if = "is_zero_n")]
    pub pr: u32,
    /// The linked PR's merge was already announced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pr_noticed: bool,
    /// The delivery attempt whose review the advisor was already woken for.
    #[serde(default, skip_serializing_if = "is_zero_n")]
    pub woken: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub closed_at: Option<OffsetDateTime>,
}

impl BoardItem {
    /// Who the item is with or on its way to, as shown: a task name, a pane
    /// id or a label. Empty when nobody.
    pub fn target(&self) -> &str {
        match &self.reservation {
            Some(r) if self.status == ItemStatus::Sending => match &r.pane {
                Some(p) => &p.pane_id,
                None => &r.task,
            },
            _ => match &self.pane {
                Some(p) => &p.pane_id,
                None if !self.task.is_empty() => &self.task,
                None => &self.label,
            },
        }
    }

    /// The agent an open item is for, when the board knows it.
    pub fn agent_id(&self) -> &str {
        match (&self.reservation, &self.receipt) {
            (Some(r), _) if self.status == ItemStatus::Sending => &r.agent_id,
            (_, Some(rc)) if !rc.agent_id.is_empty() => &rc.agent_id,
            _ => self
                .pane
                .as_ref()
                .map(|p| p.agent_id.as_str())
                .unwrap_or(""),
        }
    }

    /// The pane worker, while the item is with or on its way to one.
    pub fn pane_worker(&self) -> Option<&PaneWorker> {
        match &self.reservation {
            Some(r) if self.status == ItemStatus::Sending => r.pane.as_ref(),
            _ => self.pane.as_ref(),
        }
    }

    /// Takes the owner a reservation named, clearing the others.
    fn take_target(&mut self, r: &Reservation) {
        self.task = r.task.clone();
        self.worktree = r.worktree.clone();
        self.pane = r.pane.clone();
        self.label.clear();
    }

    fn touch(&mut self) {
        self.revision += 1;
    }
}

/// A board pane being opened, or open, beside one advisor pane.
///
/// Reserved **before** the split, so two sends racing to open the first pane
/// do not both split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneClaim {
    /// Unique on this board, so a late `set` or `release` from an earlier
    /// claim cannot touch a newer one — even one made by a reused pid.
    pub id: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    pub caller_pane: String,
    pub pid: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Set once the split returned.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane_id: String,
    /// The split pane's terminal, the proof that a pane found later under
    /// `pane_id` is still the one this claim made — herdr reuses pane ids.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_id: String,
    /// The watch command was typed into it. Typed again only into our own
    /// pane seen back at an idle shell (the view ended).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub started: bool,
    /// Who is typing the view into the pane right now, and since when: two
    /// `open`s that both see the same idle shell must not both type into it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typing: Option<Typing>,
}

/// A reservation to type the board view into a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Typing {
    pub pid: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The command went out (or may have) and the view was not seen running.
    /// Such a reservation holds for its whole window even after its process
    /// is gone: the shell may still be getting to the command.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    /// The canonical repository path, checked on every load.
    pub repo: String,
    #[serde(default)]
    pub items: Vec<BoardItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneClaim>,
    /// The last pane claim id handed out.
    #[serde(default)]
    pub last_claim: u64,
    /// The highest item number ever handed out. Removing items never frees a
    /// number: a send's late outcome is keyed by it, and must not land on a
    /// new item that took the number over.
    #[serde(default, skip_serializing_if = "is_zero_n")]
    pub last_item: u32,
    /// The advisor to wake when an item is ready for review (`board wake
    /// on`), pinned like a pane worker so a replaced agent is not woken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waker: Option<PaneWorker>,
}

/// Why a board operation was refused. The operation changed nothing; recovery
/// of abandoned reservations that ran before it may still be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardError {
    NoItem(u32),
    EmptyText,
    /// The item is not in a state this operation accepts.
    Status {
        n: u32,
        status: ItemStatus,
        why: &'static str,
    },
    /// The worker already holds an open item and `--replace` was not given.
    WorkerBusy {
        task: String,
        holds: u32,
    },
    /// `--again` names a different worker than the one the item went to.
    AgainElsewhere {
        n: u32,
        task: String,
    },
    /// The file on disk belongs to another repository.
    RepoMismatch {
        expected: String,
        found: String,
    },
    /// A priority outside 1–3 (0 clears it).
    Priority(u8),
}

impl fmt::Display for BoardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoItem(n) => write!(f, "no item {n} on this board"),
            Self::EmptyText => write!(f, "an item needs some text"),
            Self::Status { n, status, why } => {
                write!(f, "item {n} is {}: {why}", status.label())
            }
            Self::WorkerBusy { task, holds } => write!(
                f,
                "{task} already holds item {holds}; mark it done or drop it, or send with --replace"
            ),
            Self::AgainElsewhere { n, task } => write!(
                f,
                "--again resends item {n} to {task}, the worker it went to; send it elsewhere with --replace"
            ),
            Self::Priority(p) => write!(f, "priority {p}: use 1 (highest) to 3, or - to clear"),
            Self::RepoMismatch { expected, found } => write!(
                f,
                "board file is for {found}, not {expected}"
            ),
        }
    }
}

impl std::error::Error for BoardError {}

/// How a send is asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendFlags {
    /// Resend a `sent?` item to the same worker.
    pub again: bool,
    /// Supersede the worker's open item once this delivery is confirmed.
    pub replace: bool,
}

/// Handed out by [`Board::reserve_send`]; the only key [`Board::apply`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    pub n: u32,
    pub attempt: u32,
}

/// How a delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// herdr refused: an approval dialog, no agent, the pane moved. The worker
    /// certainly does not have it.
    Refused,
    Accepted(Receipt),
    /// A transport or decode error: the server may have received it.
    Unknown,
}

/// What [`Board::apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Yes,
    /// The item moved on while the prompt was in flight — marked done, dropped
    /// or recovered — and the outcome was discarded.
    Stale,
}

impl Board {
    pub fn new(repo: &str) -> Self {
        Self {
            repo: repo.to_string(),
            items: Vec::new(),
            panes: Vec::new(),
            last_claim: 0,
            last_item: 0,
            waker: None,
        }
    }

    pub fn item(&self, n: u32) -> Option<&BoardItem> {
        self.items.iter().find(|i| i.n == n)
    }

    fn item_mut(&mut self, n: u32) -> Result<&mut BoardItem, BoardError> {
        self.items
            .iter_mut()
            .find(|i| i.n == n)
            .ok_or(BoardError::NoItem(n))
    }

    /// The open items a worker holds, oldest first. Normally at most one; two
    /// or more only after a `--replace` whose delivery ended `sent?`.
    pub fn open_items_of<'a>(&'a self, task: &'a str) -> impl Iterator<Item = &'a BoardItem> + 'a {
        self.items
            .iter()
            .filter(move |i| i.status.is_open() && i.target() == task)
    }

    /// Whether an open item `i` is for the worker a send names: the same
    /// target, or — however each names it — the same agent.
    fn same_worker(i: &BoardItem, target: &str, agent_id: &str) -> bool {
        i.status.is_open()
            && (i.target() == target || (!agent_id.is_empty() && i.agent_id() == agent_id))
    }

    /// Assigns item `n` to a worker without sending it: shown on the row, a
    /// queue of several per worker is fine. Only a planned item: one out or
    /// on its way keeps the worker its receipt was read from.
    pub fn assign(&mut self, n: u32, to: AssignTo) -> Result<(), BoardError> {
        let item = self.item_mut(n)?;
        if item.status != ItemStatus::Planned {
            return Err(BoardError::Status {
                n,
                status: item.status,
                why: "only a planned item can be assigned; this one is out or closed",
            });
        }
        item.task.clear();
        item.worktree.clear();
        item.pane = None;
        item.label.clear();
        match to {
            AssignTo::Send(SendTo::Task { task, worktree }) => {
                item.task = task;
                item.worktree = worktree;
            }
            AssignTo::Send(SendTo::Pane(p)) => item.pane = Some(p),
            AssignTo::Label(l) => item.label = l.trim().to_string(),
        }
        item.touch();
        Ok(())
    }

    /// Takes over an item the advisor sent to `pane` itself: it becomes
    /// `sent`, read against `receipt` (marked adopted), and holds the worker
    /// like any delivered item.
    pub fn adopt(
        &mut self,
        n: u32,
        pane: PaneWorker,
        mut receipt: Receipt,
    ) -> Result<(), BoardError> {
        let item = self.item(n).ok_or(BoardError::NoItem(n))?;
        if item.status != ItemStatus::Planned {
            return Err(BoardError::Status {
                n,
                status: item.status,
                why: "only a planned item can be taken over as sent",
            });
        }
        if let Some(h) = self
            .items
            .iter()
            .find(|i| i.n != n && Self::same_worker(i, &pane.pane_id, &receipt.agent_id))
        {
            return Err(BoardError::WorkerBusy {
                task: pane.pane_id.clone(),
                holds: h.n,
            });
        }
        receipt.adopted = true;
        let item = self.item_mut(n)?;
        item.task.clear();
        item.worktree.clear();
        item.label.clear();
        item.pane = Some(pane);
        item.status = ItemStatus::Sent;
        item.attempts += 1;
        item.receipt = Some(receipt);
        item.touch();
        Ok(())
    }

    /// Adds planned items and answers their numbers.
    pub fn add(&mut self, texts: &[String], now: OffsetDateTime) -> Result<Vec<u32>, BoardError> {
        if texts.iter().any(|t| t.trim().is_empty()) {
            return Err(BoardError::EmptyText);
        }
        let mut next = self
            .items
            .iter()
            .map(|i| i.n)
            .max()
            .unwrap_or(0)
            .max(self.last_item);
        let mut added = Vec::with_capacity(texts.len());
        for text in texts {
            next += 1;
            self.items.push(BoardItem {
                n: next,
                text: text.trim().to_string(),
                status: ItemStatus::Planned,
                revision: 1,
                task: String::new(),
                worktree: String::new(),
                pane: None,
                label: String::new(),
                attempts: 0,
                reservation: None,
                receipt: None,
                uncertain: false,
                note: String::new(),
                reason: String::new(),
                review_noticed: 0,
                priority: 0,
                pr: 0,
                pr_noticed: false,
                woken: 0,
                created_at: now,
                closed_at: None,
            });
            added.push(next);
        }
        self.last_item = next;
        Ok(added)
    }

    /// Removes items for good. All or nothing: nothing goes if one cannot.
    /// A send in flight always refuses; an item out with a worker (`sent`,
    /// `sent?`) refuses unless `force`, since the worker may still be on it.
    pub fn remove(&mut self, ns: &[u32], force: bool) -> Result<Vec<u32>, BoardError> {
        for &n in ns {
            let item = self.item(n).ok_or(BoardError::NoItem(n))?;
            match item.status {
                ItemStatus::Sending => {
                    return Err(BoardError::Status {
                        n,
                        status: item.status,
                        why: "a send is in flight; remove it once it has landed",
                    })
                }
                ItemStatus::Sent | ItemStatus::SentUnknown if !force => {
                    return Err(BoardError::Status {
                        n,
                        status: item.status,
                        why: "its worker may still be on it; mark it done or drop it first, or remove it with --force",
                    })
                }
                _ => {}
            }
        }
        self.bump_last_item();
        self.items.retain(|i| !ns.contains(&i.n));
        let mut gone = ns.to_vec();
        gone.sort_unstable();
        gone.dedup();
        Ok(gone)
    }

    /// Removes every closed item (done, dropped, superseded) and answers
    /// their numbers.
    pub fn prune(&mut self) -> Vec<u32> {
        self.bump_last_item();
        let gone: Vec<u32> = self
            .items
            .iter()
            .filter(|i| i.status.is_closed())
            .map(|i| i.n)
            .collect();
        self.items.retain(|i| !i.status.is_closed());
        gone
    }

    /// Removes every item. Refuses while a send is in flight. Pane claims
    /// stay: the board pane goes on showing the (now empty) board.
    pub fn clear(&mut self) -> Result<usize, BoardError> {
        if let Some(i) = self.items.iter().find(|i| i.status == ItemStatus::Sending) {
            return Err(BoardError::Status {
                n: i.n,
                status: i.status,
                why: "a send is in flight; clear once it has landed",
            });
        }
        self.bump_last_item();
        let count = self.items.len();
        self.items.clear();
        Ok(count)
    }

    /// Marks item `n`'s current delivery as announced for review. `false`
    /// when it already was, or the item is no longer out on that attempt.
    pub fn note_review(&mut self, n: u32, attempt: u32) -> bool {
        match self.items.iter_mut().find(|i| i.n == n) {
            Some(i)
                if i.status == ItemStatus::Sent
                    && i.attempts == attempt
                    && i.review_noticed != attempt =>
            {
                i.review_noticed = attempt;
                true
            }
            _ => false,
        }
    }

    /// Sets item `n`'s priority: 1 (highest) to 3, or 0 for none.
    pub fn set_priority(&mut self, n: u32, priority: u8) -> Result<(), BoardError> {
        if priority > 3 {
            return Err(BoardError::Priority(priority));
        }
        let item = self.item_mut(n)?;
        item.priority = priority;
        item.touch();
        Ok(())
    }

    /// Moves item `n` to position `to` (1-based, clamped to the list). Its
    /// number does not change.
    pub fn move_item(&mut self, n: u32, to: usize) -> Result<usize, BoardError> {
        let from = self
            .items
            .iter()
            .position(|i| i.n == n)
            .ok_or(BoardError::NoItem(n))?;
        let item = self.items.remove(from);
        let at = to.max(1).min(self.items.len() + 1) - 1;
        self.items.insert(at, item);
        if let Some(i) = self.items.get_mut(at) {
            i.touch();
        }
        Ok(at + 1)
    }

    /// Links item `n` to pull request `pr` in the board's repository; 0
    /// unlinks. A new link is announced afresh when it merges.
    pub fn link(&mut self, n: u32, pr: u32) -> Result<(), BoardError> {
        let item = self.item_mut(n)?;
        if item.pr != pr {
            item.pr_noticed = false;
        }
        item.pr = pr;
        item.touch();
        Ok(())
    }

    /// Marks item `n`'s linked PR `pr` as announced merged. `false` when it
    /// already was, or the link has changed.
    pub fn note_pr_merged(&mut self, n: u32, pr: u32) -> bool {
        match self.items.iter_mut().find(|i| i.n == n) {
            Some(i) if i.pr == pr && pr != 0 && !i.pr_noticed => {
                i.pr_noticed = true;
                true
            }
            _ => false,
        }
    }

    /// Marks item `n`'s current delivery as woken for. `false` when it
    /// already was, or the item is no longer out on that attempt.
    pub fn note_woken(&mut self, n: u32, attempt: u32) -> bool {
        match self.items.iter_mut().find(|i| i.n == n) {
            Some(i)
                if i.status == ItemStatus::Sent && i.attempts == attempt && i.woken != attempt =>
            {
                i.woken = attempt;
                true
            }
            _ => false,
        }
    }

    fn bump_last_item(&mut self) {
        let max = self.items.iter().map(|i| i.n).max().unwrap_or(0);
        self.last_item = self.last_item.max(max);
    }

    /// Reserves a send of item `n` to `task`. The caller delivers outside the
    /// lock and hands the outcome to [`Board::apply`] with the ticket.
    pub fn reserve_send(
        &mut self,
        n: u32,
        task: &str,
        worktree: &str,
        flags: SendFlags,
        pid: u32,
        now: OffsetDateTime,
    ) -> Result<Ticket, BoardError> {
        let to = SendTo::Task {
            task: task.to_string(),
            worktree: worktree.to_string(),
        };
        self.reserve_send_to(n, to, "", flags, pid, now)
    }

    /// [`Board::reserve_send`] to a task or a pane, for the agent `agent_id`
    /// when the caller knows it (empty otherwise).
    pub fn reserve_send_to(
        &mut self,
        n: u32,
        to: SendTo,
        agent_id: &str,
        flags: SendFlags,
        pid: u32,
        now: OffsetDateTime,
    ) -> Result<Ticket, BoardError> {
        let target = match &to {
            SendTo::Task { task, .. } => task.clone(),
            SendTo::Pane(p) => p.pane_id.clone(),
        };
        let item = self.item(n).ok_or(BoardError::NoItem(n))?;
        match item.status {
            ItemStatus::Planned => {}
            ItemStatus::SentUnknown if flags.again => {
                let same = match &to {
                    SendTo::Task { task, worktree } => {
                        item.task == *task && item.worktree == *worktree
                    }
                    SendTo::Pane(p) => item.pane.as_ref().is_some_and(|q| {
                        q.backend == p.backend && q.pane_id == p.pane_id
                    }),
                };
                if !same {
                    return Err(BoardError::AgainElsewhere {
                        n,
                        task: item.target().to_string(),
                    });
                }
            }
            ItemStatus::SentUnknown => {
                return Err(BoardError::Status {
                    n,
                    status: item.status,
                    why: "the worker may already have it; resend with --again, or mark it done or drop it",
                })
            }
            ItemStatus::Sending => {
                return Err(BoardError::Status {
                    n,
                    status: item.status,
                    why: "a send is in flight",
                })
            }
            ItemStatus::Sent => {
                return Err(BoardError::Status {
                    n,
                    status: item.status,
                    why: "already delivered; a follow-up goes to the worker with `herdr agent prompt`",
                })
            }
            s @ (ItemStatus::Done | ItemStatus::Dropped | ItemStatus::Superseded) => {
                return Err(BoardError::Status {
                    n,
                    status: s,
                    why: "it is closed",
                })
            }
        }
        // The worker's open items — other than this one when resending it.
        // Normally at most one; two after a `--replace` whose delivery ended
        // `sent?`, since neither can then be called superseded.
        let holds: Vec<&BoardItem> = self
            .items
            .iter()
            .filter(|i| i.n != n && Self::same_worker(i, &target, agent_id))
            .collect();
        if let Some(h) = holds.iter().find(|i| i.status == ItemStatus::Sending) {
            return Err(BoardError::Status {
                n: h.n,
                status: ItemStatus::Sending,
                why: "a send to the same worker is in flight",
            });
        }
        if let (Some(h), false) = (holds.first(), flags.replace) {
            return Err(BoardError::WorkerBusy {
                task: target.clone(),
                holds: h.n,
            });
        }
        let replaces: Vec<u32> = holds.iter().map(|i| i.n).collect();
        let item = self.item_mut(n)?;
        let previous = item.status;
        item.touch();
        item.attempts += 1;
        item.status = ItemStatus::Sending;
        let (task, worktree, pane) = match to {
            SendTo::Task { task, worktree } => (task, worktree, None),
            SendTo::Pane(p) => (String::new(), String::new(), Some(p)),
        };
        item.reservation = Some(Reservation {
            attempt: item.attempts,
            pid,
            at: now,
            task,
            worktree,
            pane,
            agent_id: agent_id.to_string(),
            previous,
            replaces,
        });
        Ok(Ticket {
            n,
            attempt: item.attempts,
        })
    }

    /// Lands a delivery's outcome, if the reservation is still the item's
    /// latest state.
    pub fn apply(
        &mut self,
        ticket: Ticket,
        outcome: Outcome,
        now: OffsetDateTime,
    ) -> Result<Applied, BoardError> {
        let item = self.item_mut(ticket.n)?;
        let current = item.status == ItemStatus::Sending
            && item.reservation.as_ref().map(|r| r.attempt) == Some(ticket.attempt);
        if !current {
            return Ok(Applied::Stale);
        }
        let r = item.reservation.take().expect("checked above");
        item.touch();
        let replaces = match outcome {
            Outcome::Refused => {
                item.status = r.previous;
                Vec::new()
            }
            Outcome::Unknown => {
                item.status = ItemStatus::SentUnknown;
                item.take_target(&r);
                // The replaced item cannot be called superseded: the worker may
                // never have received this one.
                Vec::new()
            }
            Outcome::Accepted(receipt) => {
                item.status = ItemStatus::Sent;
                item.take_target(&r);
                item.receipt = Some(receipt);
                r.replaces
            }
        };
        for old in replaces {
            if let Ok(old) = self.item_mut(old) {
                if old.status.is_open() {
                    old.uncertain = old.status == ItemStatus::SentUnknown;
                    old.status = ItemStatus::Superseded;
                    old.reservation = None;
                    old.closed_at = Some(now);
                    old.touch();
                }
            }
        }
        Ok(Applied::Yes)
    }

    /// Strikes items through. Works on any status, a send in flight included:
    /// its late outcome is then discarded.
    pub fn done(&mut self, ns: &[u32], now: OffsetDateTime) -> Result<(), BoardError> {
        self.close(ns, ItemStatus::Done, "", now)
    }

    /// Cancels items, struck through and dimmed.
    pub fn drop_items(
        &mut self,
        ns: &[u32],
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<(), BoardError> {
        self.close(ns, ItemStatus::Dropped, reason, now)
    }

    fn close(
        &mut self,
        ns: &[u32],
        to: ItemStatus,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<(), BoardError> {
        // All or nothing: check every number before changing any.
        if let Some(&missing) = ns.iter().find(|n| self.item(**n).is_none()) {
            return Err(BoardError::NoItem(missing));
        }
        for &n in ns {
            let item = self.item_mut(n)?;
            if item.status == to {
                continue;
            }
            if item.status == ItemStatus::Sending {
                // Keep the target, so the row still says who it went to.
                if let Some(r) = item.reservation.take() {
                    item.take_target(&r);
                }
            }
            item.status = to;
            item.reason = reason.trim().to_string();
            item.closed_at = Some(now);
            item.touch();
        }
        Ok(())
    }

    /// Puts one line under an item; an empty text removes it.
    pub fn note(&mut self, n: u32, text: &str) -> Result<(), BoardError> {
        let item = self.item_mut(n)?;
        item.note = text.trim().to_string();
        item.touch();
        Ok(())
    }

    /// Turns abandoned reservations into `sent?` and forgets pane claims whose
    /// opener died before the split returned. Answers whether anything changed.
    pub fn recover(&mut self, now: OffsetDateTime, alive: &dyn Fn(u32) -> bool) -> bool {
        let mut changed = false;
        for item in &mut self.items {
            let abandoned = item.status == ItemStatus::Sending
                && item
                    .reservation
                    .as_ref()
                    .is_none_or(|r| !alive(r.pid) || now - r.at > RESERVATION_TTL);
            if abandoned {
                if let Some(r) = item.reservation.take() {
                    item.take_target(&r);
                }
                item.status = ItemStatus::SentUnknown;
                item.touch();
                changed = true;
            }
        }
        let before = self.panes.len();
        self.panes
            .retain(|p| !p.pane_id.is_empty() || (alive(p.pid) && now - p.at <= RESERVATION_TTL));
        changed | (self.panes.len() != before)
    }

    /// Claims the right to open the board pane beside `caller_pane` and
    /// answers the claim's id. `None` when a claim is already there — pending
    /// or holding a pane — and the caller then leaves the split to its holder.
    pub fn claim_pane(
        &mut self,
        backend: &str,
        caller_pane: &str,
        pid: u32,
        now: OffsetDateTime,
    ) -> Option<u64> {
        if self.pane_claim(backend, caller_pane).is_some() {
            return None;
        }
        self.last_claim += 1;
        self.panes.push(PaneClaim {
            id: self.last_claim,
            backend: backend.to_string(),
            caller_pane: caller_pane.to_string(),
            pid,
            at: now,
            pane_id: String::new(),
            terminal_id: String::new(),
            started: false,
            typing: None,
        });
        Some(self.last_claim)
    }

    pub fn pane_claim(&self, backend: &str, caller_pane: &str) -> Option<&PaneClaim> {
        self.panes
            .iter()
            .find(|p| p.backend == backend && p.caller_pane == caller_pane)
    }

    /// Records the pane the split made, on claim `id` only.
    pub fn set_pane(&mut self, id: u64, pane_id: &str, terminal_id: &str) -> bool {
        match self.panes.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.pane_id = pane_id.to_string();
                p.terminal_id = terminal_id.to_string();
                true
            }
            None => false,
        }
    }

    /// Reserves typing the view into claim `id`'s pane for `pid`. `false`
    /// when another live process reserved it within [`TYPING_WINDOW`].
    pub fn take_typing(
        &mut self,
        id: u64,
        pid: u32,
        now: OffsetDateTime,
        alive: &dyn Fn(u32) -> bool,
    ) -> bool {
        let Some(p) = self.panes.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        if let Some(t) = p.typing {
            let held = t.sent || alive(t.pid);
            if t.pid != pid && held && now - t.at < TYPING_WINDOW {
                return false;
            }
        }
        p.typing = Some(Typing {
            pid,
            at: now,
            sent: false,
        });
        true
    }

    /// Marks `pid`'s typing reservation on claim `id` as sent but not seen
    /// running: it then holds for its window even after `pid` is gone.
    pub fn typing_unconfirmed(&mut self, id: u64, pid: u32, now: OffsetDateTime) -> bool {
        match self.panes.iter_mut().find(|p| p.id == id) {
            Some(p) if p.typing.is_some_and(|t| t.pid == pid) => {
                p.typing = Some(Typing {
                    pid,
                    at: now,
                    sent: true,
                });
                true
            }
            _ => false,
        }
    }

    /// Ends `pid`'s typing reservation on claim `id`, once the typing is over.
    pub fn release_typing(&mut self, id: u64, pid: u32) -> bool {
        match self.panes.iter_mut().find(|p| p.id == id) {
            Some(p) if p.typing.is_some_and(|t| t.pid == pid) => {
                p.typing = None;
                true
            }
            _ => false,
        }
    }

    /// Notes that the watch command went into claim `id`'s pane.
    pub fn mark_started(&mut self, id: u64) -> bool {
        match self.panes.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.started = true;
                true
            }
            None => false,
        }
    }

    /// Forgets claim `id` — its pane was closed, or found not to be a board
    /// (after a herdr restart the recorded pane id may name anything).
    pub fn release_pane(&mut self, id: u64) -> bool {
        let before = self.panes.len();
        self.panes.retain(|p| p.id != id);
        self.panes.len() != before
    }
}

fn is_zero_n(n: &u32) -> bool {
    *n == 0
}

fn is_zero_u8(n: &u8) -> bool {
    *n == 0
}

/// Whether a process with this pid exists.
pub fn pid_alive(pid: u32) -> bool {
    tower_os::pid_alive(pid)
}

/// The boards, one file per repository under `<state>/boards/`.
pub struct BoardStore {
    dir: PathBuf,
}

impl BoardStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let state_dir = state_dir.into();
        crate::make_state_root(&state_dir)?;
        let dir = state_dir.join(DIR);
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Where `repo`'s board lives: a readable prefix and a hash of the whole
    /// path, since two repositories can share a basename.
    pub fn path_for(&self, repo: &str) -> PathBuf {
        let base: String = Path::new(repo)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .take(40)
            .collect();
        self.dir
            .join(format!("{base}-{:016x}.json", fnv1a(repo.as_bytes())))
    }

    /// The repositories that have a board, read from the files themselves.
    /// A file that does not read is skipped.
    pub fn repos(&self) -> std::io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(body) = fs::read(&path) else { continue };
            let Ok(board) = serde_json::from_slice::<Board>(&body) else {
                continue;
            };
            if self.path_for(&board.repo) == path {
                out.push(board.repo);
            }
        }
        out.sort();
        Ok(out)
    }

    /// The board as it stands, abandoned reservations shown as `sent?` without
    /// writing — a reader never takes the lock.
    pub fn load(&self, repo: &str, now: OffsetDateTime) -> std::io::Result<Board> {
        let mut board = self.read(repo)?;
        board.recover(now, &pid_alive);
        Ok(board)
    }

    /// Changes the board under its lock. Recovery runs first, so `f` never
    /// sees an abandoned reservation. A refusal from `f` writes nothing.
    pub fn update<T>(
        &self,
        repo: &str,
        now: OffsetDateTime,
        f: impl FnOnce(&mut Board) -> Result<T, BoardError>,
    ) -> std::io::Result<Result<T, BoardError>> {
        self.update_with(repo, now, &pid_alive, f)
    }

    fn update_with<T>(
        &self,
        repo: &str,
        now: OffsetDateTime,
        alive: &dyn Fn(u32) -> bool,
        f: impl FnOnce(&mut Board) -> Result<T, BoardError>,
    ) -> std::io::Result<Result<T, BoardError>> {
        let _lock = BoardLock::acquire(&self.dir)?;
        let mut board = self.read(repo)?;
        let recovered = board.recover(now, alive);
        // What a refusal leaves behind is the recovery alone: `f` may have
        // changed some of the board before it refused.
        let recovered_only = recovered.then(|| board.clone());
        match f(&mut board) {
            Ok(value) => {
                self.write(&board)?;
                Ok(Ok(value))
            }
            Err(e) => {
                if let Some(snapshot) = recovered_only {
                    self.write(&snapshot)?;
                }
                Ok(Err(e))
            }
        }
    }

    fn read(&self, repo: &str) -> std::io::Result<Board> {
        match fs::read(self.path_for(repo)) {
            Ok(body) => {
                let board: Board = serde_json::from_slice(&body)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                if board.repo != repo {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        BoardError::RepoMismatch {
                            expected: repo.to_string(),
                            found: board.repo,
                        },
                    ));
                }
                Ok(board)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Board::new(repo)),
            Err(e) => Err(e),
        }
    }

    /// Where the supervisor keeps `repo`'s linked PR states: beside the
    /// board, under an extension [`BoardStore::repos`] does not read.
    fn pr_states_path(&self, repo: &str) -> PathBuf {
        self.path_for(repo).with_extension("prs")
    }

    /// Records the linked PRs' states the supervisor last read with `gh`
    /// (`OPEN`, `MERGED`, `CLOSED`), so the board pane draws them without
    /// asking `gh` itself. Replaces what was there.
    pub fn save_pr_states(
        &self,
        repo: &str,
        states: &std::collections::HashMap<u32, String>,
        now: OffsetDateTime,
    ) -> std::io::Result<()> {
        let body = serde_json::to_vec(&PrStatesFile {
            repo: repo.to_string(),
            at: now,
            states: states.clone(),
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.pr_states_path(repo), &body)
    }

    /// The states [`BoardStore::save_pr_states`] last recorded for `repo`;
    /// none when there are none or the file does not read as `repo`'s.
    pub fn pr_states(&self, repo: &str) -> std::collections::HashMap<u32, String> {
        fs::read(self.pr_states_path(repo))
            .ok()
            .and_then(|body| serde_json::from_slice::<PrStatesFile>(&body).ok())
            .filter(|f| f.repo == repo)
            .map(|f| f.states)
            .unwrap_or_default()
    }

    fn write(&self, board: &Board) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(board)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.path_for(&board.repo), &body)
    }
}

/// What [`BoardStore::save_pr_states`] writes. The repository is kept so a
/// hash collision cannot hand one board another's states.
#[derive(Serialize, Deserialize)]
struct PrStatesFile {
    repo: String,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    states: std::collections::HashMap<u32, String>,
}

/// FNV-1a, 64 bits: stable across releases, unlike `DefaultHasher`, which a
/// file name has to be.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct BoardLock(#[allow(dead_code)] fs::File);

impl BoardLock {
    fn acquire(dir: &Path) -> std::io::Result<Self> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK))?;
        file.lock()?;
        Ok(Self(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PR states the supervisor records come back as written, for that
    /// repository only, and do not make a board appear.
    #[test]
    fn pr_states_round_trip_beside_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        assert!(store.pr_states("/w/a").is_empty(), "nothing recorded yet");

        let states: std::collections::HashMap<u32, String> =
            [(7, "MERGED".to_string()), (12, "OPEN".to_string())].into();
        store.save_pr_states("/w/a", &states, at).unwrap();
        assert_eq!(store.pr_states("/w/a"), states);
        assert!(store.pr_states("/w/b").is_empty(), "another repository's");
        assert!(
            store.repos().unwrap().is_empty(),
            "a states file is not a board"
        );

        // Replaced, not merged: a PR no longer linked is not kept.
        let later: std::collections::HashMap<u32, String> = [(12, "CLOSED".to_string())].into();
        store.save_pr_states("/w/a", &later, at).unwrap();
        assert_eq!(store.pr_states("/w/a"), later);
    }

    /// A states file that does not read, or names another repository (a
    /// hash collision), gives nothing rather than wrong states.
    #[test]
    fn pr_states_ignore_a_file_that_is_not_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let path = store.pr_states_path("/w/a");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(store.pr_states("/w/a").is_empty());

        let states: std::collections::HashMap<u32, String> = [(7, "OPEN".to_string())].into();
        store.save_pr_states("/w/other", &states, at).unwrap();
        std::fs::copy(store.pr_states_path("/w/other"), &path).unwrap();
        assert!(
            store.pr_states("/w/a").is_empty(),
            "another repository's file"
        );
    }

    const REPO: &str = "/work/dbstore";
    const ME: u32 = 4242;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    fn later(mins: i64) -> OffsetDateTime {
        now() + time::Duration::minutes(mins)
    }

    fn alive(_: u32) -> bool {
        true
    }

    fn dead(_: u32) -> bool {
        false
    }

    fn receipt() -> Receipt {
        Receipt {
            backend: String::new(),
            session_id: "sess-1".into(),
            work_seq: 7,
            agent_id: String::new(),
            adopted: false,
            record_born: None,
            at: now(),
            boundary_lost: false,
        }
    }

    fn board(items: &[&str]) -> Board {
        let mut b = Board::new(REPO);
        let texts: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        b.add(&texts, now()).unwrap();
        b
    }

    fn send(b: &mut Board, n: u32, task: &str, flags: SendFlags) -> Result<Ticket, BoardError> {
        b.reserve_send(
            n,
            task,
            &format!("{REPO}/.herdr-tower/worktrees/{task}"),
            flags,
            ME,
            now(),
        )
    }

    fn sent(b: &mut Board, n: u32, task: &str) {
        let t = send(b, n, task, SendFlags::default()).unwrap();
        assert_eq!(
            b.apply(t, Outcome::Accepted(receipt()), now()),
            Ok(Applied::Yes)
        );
    }

    fn status(b: &Board, n: u32) -> ItemStatus {
        b.item(n).unwrap().status
    }

    #[test]
    fn numbers_are_stable_and_never_reused() {
        let mut b = board(&["a", "b"]);
        b.done(&[2], now()).unwrap();
        let added = b.add(&["c".into()], now()).unwrap();
        assert_eq!(added, vec![3]);
        assert_eq!(
            b.items.iter().map(|i| i.n).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn empty_text_is_refused_and_nothing_is_added() {
        let mut b = board(&["a"]);
        assert_eq!(
            b.add(&["b".into(), "  ".into()], now()),
            Err(BoardError::EmptyText)
        );
        assert_eq!(b.items.len(), 1);
    }

    #[test]
    fn an_accepted_send_records_the_target_and_the_receipt() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        assert_eq!(status(&b, 1), ItemStatus::Sending);
        assert_eq!(b.item(1).unwrap().target(), "i-e");
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        let i = b.item(1).unwrap();
        assert_eq!(i.status, ItemStatus::Sent);
        assert_eq!(i.task, "i-e");
        assert!(i.worktree.ends_with("/i-e"));
        assert_eq!(i.receipt, Some(receipt()));
        assert!(i.reservation.is_none());
        assert_eq!(i.attempts, 1);
    }

    #[test]
    fn a_refused_send_goes_back_to_planned_with_no_target() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.apply(t, Outcome::Refused, now()).unwrap();
        let i = b.item(1).unwrap();
        assert_eq!(i.status, ItemStatus::Planned);
        assert!(i.task.is_empty());
        assert!(i.reservation.is_none());
        assert!(b.open_items_of("i-e").next().is_none());
    }

    #[test]
    fn an_unknown_outcome_is_sent_question_and_holds_the_worker() {
        let mut b = board(&["a", "b"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.apply(t, Outcome::Unknown, now()).unwrap();
        assert_eq!(status(&b, 1), ItemStatus::SentUnknown);
        assert_eq!(b.open_items_of("i-e").next().map(|i| i.n), Some(1));
        assert_eq!(
            send(&mut b, 2, "i-e", SendFlags::default()),
            Err(BoardError::WorkerBusy {
                task: "i-e".into(),
                holds: 1
            })
        );
    }

    #[test]
    fn a_sent_question_item_is_resent_only_with_again_and_only_to_its_worker() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.apply(t, Outcome::Unknown, now()).unwrap();
        assert!(matches!(
            send(&mut b, 1, "i-e", SendFlags::default()),
            Err(BoardError::Status {
                status: ItemStatus::SentUnknown,
                ..
            })
        ));
        let again = SendFlags {
            again: true,
            replace: false,
        };
        assert_eq!(
            send(&mut b, 1, "i-9", again),
            Err(BoardError::AgainElsewhere {
                n: 1,
                task: "i-e".into()
            })
        );
        let t = send(&mut b, 1, "i-e", again).unwrap();
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        assert_eq!(status(&b, 1), ItemStatus::Sent);
        assert_eq!(b.item(1).unwrap().attempts, 2);
    }

    #[test]
    fn a_refused_again_goes_back_to_sent_question() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.apply(t, Outcome::Unknown, now()).unwrap();
        let t = send(
            &mut b,
            1,
            "i-e",
            SendFlags {
                again: true,
                replace: false,
            },
        )
        .unwrap();
        b.apply(t, Outcome::Refused, now()).unwrap();
        assert_eq!(status(&b, 1), ItemStatus::SentUnknown);
        assert_eq!(b.item(1).unwrap().task, "i-e");
    }

    #[test]
    fn sent_and_closed_items_cannot_be_sent() {
        let mut b = board(&["a", "b"]);
        sent(&mut b, 1, "i-e");
        assert!(matches!(
            send(&mut b, 1, "i-9", SendFlags::default()),
            Err(BoardError::Status {
                status: ItemStatus::Sent,
                ..
            })
        ));
        b.done(&[2], now()).unwrap();
        assert!(matches!(
            send(&mut b, 2, "i-9", SendFlags::default()),
            Err(BoardError::Status {
                status: ItemStatus::Done,
                ..
            })
        ));
        assert_eq!(
            send(&mut b, 9, "i-9", SendFlags::default()),
            Err(BoardError::NoItem(9))
        );
    }

    #[test]
    fn one_open_item_per_worker_unless_replace() {
        let mut b = board(&["a", "b", "c"]);
        sent(&mut b, 1, "i-e");
        assert_eq!(
            send(&mut b, 2, "i-e", SendFlags::default()),
            Err(BoardError::WorkerBusy {
                task: "i-e".into(),
                holds: 1
            })
        );
        // Another worker is free.
        sent(&mut b, 3, "i-9");
        assert_eq!(status(&b, 3), ItemStatus::Sent);
    }

    #[test]
    fn replace_supersedes_the_old_item_only_on_confirmation() {
        let mut b = board(&["a", "b"]);
        sent(&mut b, 1, "i-e");
        let replace = SendFlags {
            again: false,
            replace: true,
        };
        let t = send(&mut b, 2, "i-e", replace).unwrap();
        assert_eq!(status(&b, 1), ItemStatus::Sent, "untouched while in flight");
        b.apply(t, Outcome::Accepted(receipt()), later(1)).unwrap();
        assert_eq!(status(&b, 1), ItemStatus::Superseded);
        assert!(!b.item(1).unwrap().uncertain);
        assert_eq!(b.item(1).unwrap().closed_at, Some(later(1)));
        assert_eq!(b.open_items_of("i-e").next().map(|i| i.n), Some(2));
    }

    #[test]
    fn a_refused_or_unknown_replace_leaves_the_old_item_open() {
        let replace = SendFlags {
            again: false,
            replace: true,
        };
        for outcome in [Outcome::Refused, Outcome::Unknown] {
            let mut b = board(&["a", "b"]);
            sent(&mut b, 1, "i-e");
            let t = send(&mut b, 2, "i-e", replace).unwrap();
            b.apply(t, outcome.clone(), now()).unwrap();
            assert_eq!(status(&b, 1), ItemStatus::Sent, "{outcome:?}");
        }
    }

    #[test]
    fn replacing_a_sent_question_item_marks_it_uncertain() {
        let mut b = board(&["a", "b"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.apply(t, Outcome::Unknown, now()).unwrap();
        let t = send(
            &mut b,
            2,
            "i-e",
            SendFlags {
                again: false,
                replace: true,
            },
        )
        .unwrap();
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        let old = b.item(1).unwrap();
        assert_eq!(old.status, ItemStatus::Superseded);
        assert!(old.uncertain);
    }

    #[test]
    fn a_send_in_flight_to_the_same_worker_blocks_even_replace() {
        let mut b = board(&["a", "b"]);
        send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        assert!(matches!(
            send(
                &mut b,
                2,
                "i-e",
                SendFlags {
                    again: false,
                    replace: true
                }
            ),
            Err(BoardError::Status {
                n: 1,
                status: ItemStatus::Sending,
                ..
            })
        ));
        assert!(matches!(
            send(&mut b, 1, "i-e", SendFlags::default()),
            Err(BoardError::Status {
                status: ItemStatus::Sending,
                ..
            })
        ));
    }

    #[test]
    fn done_during_a_send_wins_over_the_late_outcome() {
        for outcome in [
            Outcome::Accepted(receipt()),
            Outcome::Unknown,
            Outcome::Refused,
        ] {
            let mut b = board(&["a"]);
            let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
            b.done(&[1], later(1)).unwrap();
            assert_eq!(
                b.apply(t, outcome.clone(), later(2)),
                Ok(Applied::Stale),
                "{outcome:?}"
            );
            let i = b.item(1).unwrap();
            assert_eq!(i.status, ItemStatus::Done, "{outcome:?}");
            assert_eq!(i.task, "i-e", "the row still says who it went to");
            assert!(i.receipt.is_none());
        }
    }

    #[test]
    fn drop_during_a_send_wins_and_keeps_its_reason() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.drop_items(&[1], " not needed ", now()).unwrap();
        assert_eq!(
            b.apply(t, Outcome::Accepted(receipt()), now()),
            Ok(Applied::Stale)
        );
        let i = b.item(1).unwrap();
        assert_eq!(i.status, ItemStatus::Dropped);
        assert_eq!(i.reason, "not needed");
    }

    #[test]
    fn an_old_ticket_does_not_land_on_a_newer_attempt() {
        let mut b = board(&["a"]);
        let first = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.recover(later(11), &alive); // the TTL passed: sent?
        let second = send(
            &mut b,
            1,
            "i-e",
            SendFlags {
                again: true,
                replace: false,
            },
        )
        .unwrap();
        assert_eq!(b.apply(first, Outcome::Refused, now()), Ok(Applied::Stale));
        assert_eq!(status(&b, 1), ItemStatus::Sending);
        assert_eq!(
            b.apply(second, Outcome::Accepted(receipt()), now()),
            Ok(Applied::Yes)
        );
        assert_eq!(status(&b, 1), ItemStatus::Sent);
    }

    #[test]
    fn a_note_during_a_send_does_not_discard_its_outcome() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.note(1, "x").unwrap();
        assert_eq!(
            b.apply(t, Outcome::Accepted(receipt()), now()),
            Ok(Applied::Yes)
        );
        let i = b.item(1).unwrap();
        assert_eq!(i.status, ItemStatus::Sent);
        assert_eq!(i.receipt, Some(receipt()));
        assert_eq!(i.note, "x");
    }

    #[test]
    fn replacing_after_an_unknown_replace_closes_every_open_item() {
        let replace = SendFlags {
            again: false,
            replace: true,
        };
        let mut b = board(&["a", "b", "c"]);
        sent(&mut b, 1, "i-e");
        let t = send(&mut b, 2, "i-e", replace).unwrap();
        b.apply(t, Outcome::Unknown, now()).unwrap();
        // Neither can be called superseded: both stay open.
        assert_eq!(status(&b, 1), ItemStatus::Sent);
        assert_eq!(status(&b, 2), ItemStatus::SentUnknown);
        assert_eq!(
            send(&mut b, 3, "i-e", SendFlags::default()),
            Err(BoardError::WorkerBusy {
                task: "i-e".into(),
                holds: 1
            })
        );
        let t = send(&mut b, 3, "i-e", replace).unwrap();
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        assert_eq!(status(&b, 1), ItemStatus::Superseded);
        assert_eq!(status(&b, 2), ItemStatus::Superseded);
        assert!(b.item(2).unwrap().uncertain);
        let open: Vec<u32> = b
            .items
            .iter()
            .filter(|i| i.status.is_open())
            .map(|i| i.n)
            .collect();
        assert_eq!(open, vec![3]);
    }

    #[test]
    fn again_refuses_a_different_worktree_under_the_same_task_name() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.apply(t, Outcome::Unknown, now()).unwrap();
        let again = SendFlags {
            again: true,
            replace: false,
        };
        assert_eq!(
            b.reserve_send(1, "i-e", "/elsewhere/i-e", again, ME, now()),
            Err(BoardError::AgainElsewhere {
                n: 1,
                task: "i-e".into()
            })
        );
        assert_eq!(status(&b, 1), ItemStatus::SentUnknown);
    }

    #[test]
    fn close_is_all_or_nothing() {
        let mut b = board(&["a", "b"]);
        assert_eq!(b.done(&[1, 5], now()), Err(BoardError::NoItem(5)));
        assert_eq!(status(&b, 1), ItemStatus::Planned);
    }

    #[test]
    fn done_twice_keeps_the_first_time() {
        let mut b = board(&["a"]);
        b.done(&[1], now()).unwrap();
        b.done(&[1], later(5)).unwrap();
        assert_eq!(b.item(1).unwrap().closed_at, Some(now()));
    }

    #[test]
    fn a_reservation_whose_process_died_becomes_sent_question() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        assert!(b.recover(now(), &dead));
        let i = b.item(1).unwrap();
        assert_eq!(i.status, ItemStatus::SentUnknown);
        assert_eq!(i.task, "i-e");
        assert!(i.reservation.is_none());
        // Its process did not die after all: the outcome is discarded.
        assert_eq!(
            b.apply(t, Outcome::Accepted(receipt()), now()),
            Ok(Applied::Stale)
        );
    }

    #[test]
    fn a_live_reservation_is_kept_until_the_ttl() {
        let mut b = board(&["a"]);
        send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        assert!(!b.recover(later(9), &alive));
        assert_eq!(status(&b, 1), ItemStatus::Sending);
        assert!(b.recover(later(11), &alive));
        assert_eq!(status(&b, 1), ItemStatus::SentUnknown);
    }

    #[test]
    fn one_pane_claim_per_caller_and_a_dead_pending_claim_is_forgotten() {
        let mut b = Board::new(REPO);
        let p1 = b.claim_pane("", "w2:p1", ME, now()).unwrap();
        assert_eq!(
            b.claim_pane("", "w2:p1", ME + 1, now()),
            None,
            "a racing send does not split"
        );
        assert!(
            b.claim_pane("other", "w2:p1", ME, now()).is_some(),
            "backends are separate"
        );
        assert!(
            b.claim_pane("", "w2:p2", ME, now()).is_some(),
            "another advisor gets its own"
        );
        assert!(b.set_pane(p1, "w2:p9", "term_1"));
        // The opener died: a pending claim goes, one holding a pane stays.
        assert!(b.recover(now(), &dead));
        assert_eq!(b.pane_claim("", "w2:p1").unwrap().pane_id, "w2:p9");
        assert!(b.pane_claim("", "w2:p2").is_none());
        assert!(b.release_pane(p1));
        assert!(b.claim_pane("", "w2:p1", ME, now()).is_some());
    }

    #[test]
    fn a_late_set_or_release_from_an_old_claim_leaves_the_new_one_alone() {
        let mut b = Board::new(REPO);
        let old = b.claim_pane("", "w2:p1", ME, now()).unwrap();
        // It was abandoned (TTL) and the same pid claims again.
        assert!(b.recover(later(11), &alive));
        let new = b.claim_pane("", "w2:p1", ME, later(11)).unwrap();
        assert_ne!(old, new);
        assert!(!b.set_pane(old, "w2:pOLD", "t0"));
        assert!(!b.release_pane(old));
        assert!(b.set_pane(new, "w2:pNEW", "t1"));
        assert_eq!(b.pane_claim("", "w2:p1").unwrap().pane_id, "w2:pNEW");
    }

    #[test]
    fn only_one_opener_types_into_a_pane_at_a_time() {
        let mut b = Board::new(REPO);
        let id = b.claim_pane("", "w2:p1", ME, now()).unwrap();
        assert!(b.take_typing(id, ME, now(), &alive));
        assert!(!b.take_typing(id, ME + 1, now(), &alive), "a racing open");
        assert!(b.take_typing(id, ME, now(), &alive), "the holder again");
        assert!(b.take_typing(id, ME + 1, now(), &dead), "the holder died");
        assert!(
            b.take_typing(id, ME + 2, later(1), &alive),
            "the window passed"
        );
        assert!(!b.take_typing(99, ME, now(), &alive), "no such claim");
        assert!(!b.release_typing(id, ME), "held by ME + 2 now");
        assert!(b.release_typing(id, ME + 2));
        assert!(
            b.take_typing(id, ME, later(1), &alive),
            "released: free at once"
        );
        // Sent but not seen running: held for the window even once dead.
        assert!(b.typing_unconfirmed(id, ME, later(1)));
        assert!(!b.take_typing(id, ME + 1, later(1), &dead));
        assert!(
            b.take_typing(id, ME + 1, later(2), &dead),
            "the window passed"
        );
    }

    #[test]
    fn the_store_round_trips_and_writes_nothing_on_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        let added = store
            .update(REPO, now(), |b| b.add(&["a".into()], now()))
            .unwrap()
            .unwrap();
        assert_eq!(added, vec![1]);
        let before = fs::read(store.path_for(REPO)).unwrap();
        let refused = store.update(REPO, now(), |b| b.done(&[7], now())).unwrap();
        assert_eq!(refused, Err(BoardError::NoItem(7)));
        assert_eq!(fs::read(store.path_for(REPO)).unwrap(), before);
        assert_eq!(store.load(REPO, now()).unwrap().items.len(), 1);
    }

    #[test]
    fn recovery_is_persisted_even_when_the_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        store
            .update_with(REPO, now(), &alive, |b| {
                b.add(&["a".into()], now())?;
                send(b, 1, "i-e", SendFlags::default())
            })
            .unwrap()
            .unwrap();
        let refused = store
            .update_with(REPO, now(), &dead, |b| b.done(&[9], now()))
            .unwrap();
        assert!(refused.is_err());
        let on_disk: Board =
            serde_json::from_slice(&fs::read(store.path_for(REPO)).unwrap()).unwrap();
        assert_eq!(on_disk.item(1).unwrap().status, ItemStatus::SentUnknown);
    }

    #[test]
    fn a_refusal_after_a_partial_change_saves_only_the_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        store
            .update_with(REPO, now(), &alive, |b| {
                b.add(&["a".into()], now())?;
                send(b, 1, "i-e", SendFlags::default()).map(|_| ())
            })
            .unwrap()
            .unwrap();
        let refused = store
            .update_with(REPO, now(), &dead, |b| {
                b.add(&["b".into()], now())?;
                b.done(&[9], now())
            })
            .unwrap();
        assert_eq!(refused, Err(BoardError::NoItem(9)));
        let on_disk: Board =
            serde_json::from_slice(&fs::read(store.path_for(REPO)).unwrap()).unwrap();
        assert_eq!(
            on_disk.items.len(),
            1,
            "the add before the refusal is not saved"
        );
        assert_eq!(on_disk.item(1).unwrap().status, ItemStatus::SentUnknown);
    }

    #[test]
    fn a_reader_sees_recovery_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        store
            .update_with(REPO, now(), &alive, |b| {
                b.add(&["a".into()], now())?;
                send(b, 1, "i-e", SendFlags::default()).map(|_| ())
            })
            .unwrap()
            .unwrap();
        let before = fs::read(store.path_for(REPO)).unwrap();
        // Our own pid is alive, so only the TTL can recover it here.
        let seen = store.load(REPO, later(11)).unwrap();
        assert_eq!(seen.item(1).unwrap().status, ItemStatus::SentUnknown);
        assert_eq!(fs::read(store.path_for(REPO)).unwrap(), before);
    }

    #[test]
    fn two_repositories_with_one_basename_get_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        let a = store.path_for("/a/dbstore");
        let b = store.path_for("/b/dbstore");
        assert_ne!(a, b);
        assert!(a
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("dbstore-"));
    }

    #[test]
    fn a_file_for_another_repository_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        let other = Board::new("/somewhere/else");
        fs::write(store.path_for(REPO), serde_json::to_vec(&other).unwrap()).unwrap();
        let err = store.load(REPO, now()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("/somewhere/else"));
    }

    #[test]
    fn a_board_file_from_before_pane_workers_still_reads() {
        let old = r#"{"repo":"/r","items":[{"n":1,"text":"a","status":"sent","revision":3,
            "task":"i-e","worktree":"/r/wt/i-e","attempts":1,
            "receipt":{"session_id":"S","work_seq":4,"at":"2026-09-29T00:00:00Z"},
            "created_at":"2026-09-29T00:00:00Z"}],"last_claim":0}"#;
        let b: Board = serde_json::from_str(old).unwrap();
        let i = b.item(1).unwrap();
        assert_eq!(i.target(), "i-e");
        assert!(i.pane.is_none() && i.label.is_empty());
        assert_eq!(i.agent_id(), "");
    }

    #[test]
    fn assigning_replaces_the_owner_and_only_while_planned() {
        let mut b = board(&["a"]);
        let pin = PaneWorker {
            backend: String::new(),
            pane_id: "w2:pBR".into(),
            agent_id: "claude-w2:pBR".into(),
            session_id: "S".into(),
            born_at: None,
        };
        b.assign(1, AssignTo::Label("buildbox".into())).unwrap();
        assert_eq!(b.item(1).unwrap().target(), "buildbox");
        b.assign(1, AssignTo::Send(SendTo::Pane(pin.clone())))
            .unwrap();
        let i = b.item(1).unwrap();
        assert_eq!(i.target(), "w2:pBR");
        assert!(i.label.is_empty(), "one owner at a time");
        let t = b
            .reserve_send_to(
                1,
                SendTo::Pane(pin),
                "claude-w2:pBR",
                SendFlags::default(),
                ME,
                now(),
            )
            .unwrap();
        assert!(
            b.assign(1, AssignTo::Label("x".into())).is_err(),
            "in flight"
        );
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        assert!(b.assign(1, AssignTo::Label("x".into())).is_err(), "out");
        assert_eq!(b.item(1).unwrap().target(), "w2:pBR");
    }

    #[test]
    fn removed_numbers_are_never_handed_out_again() {
        let mut b = board(&["a", "b", "c"]);
        assert_eq!(b.remove(&[3], false), Ok(vec![3]));
        assert_eq!(b.add(&["d".into()], now()).unwrap(), vec![4]);
        assert_eq!(b.prune(), Vec::<u32>::new());
        b.done(&[4], now()).unwrap();
        assert_eq!(b.prune(), vec![4]);
        assert_eq!(b.clear(), Ok(2));
        assert_eq!(b.add(&["e".into()], now()).unwrap(), vec![5]);
    }

    #[test]
    fn remove_refuses_what_a_worker_may_still_have_unless_forced() {
        let mut b = board(&["a", "b", "c"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        assert!(matches!(
            b.remove(&[1], true),
            Err(BoardError::Status {
                status: ItemStatus::Sending,
                ..
            })
        ));
        assert!(matches!(b.clear(), Err(BoardError::Status { .. })));
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        assert!(matches!(
            b.remove(&[2, 1], false),
            Err(BoardError::Status {
                n: 1,
                status: ItemStatus::Sent,
                ..
            })
        ));
        assert_eq!(b.items.len(), 3, "all or nothing");
        assert_eq!(b.remove(&[1, 2], true), Ok(vec![1, 2]));
        assert_eq!(b.remove(&[9], false), Err(BoardError::NoItem(9)));
        assert_eq!(b.items.len(), 1);
    }

    #[test]
    fn a_late_outcome_for_a_removed_item_lands_nowhere() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        b.recover(later(11), &alive); // abandoned: sent?
        b.remove(&[1], true).unwrap();
        b.add(&["b".into()], now()).unwrap();
        assert_eq!(
            b.apply(t, Outcome::Accepted(receipt()), now()),
            Err(BoardError::NoItem(1))
        );
        assert_eq!(b.item(2).unwrap().status, ItemStatus::Planned);
    }

    #[test]
    fn a_review_is_announced_once_per_delivery() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        assert!(!b.note_review(1, 1), "still sending");
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        assert!(b.note_review(1, 1));
        assert!(!b.note_review(1, 1), "once");
        assert!(!b.note_review(1, 2), "not the current attempt");
    }

    #[test]
    fn the_store_lists_the_repositories_with_a_board() {
        let dir = tempfile::tempdir().unwrap();
        let store = BoardStore::new(dir.path()).unwrap();
        for repo in ["/b/two", "/a/one"] {
            store
                .update(repo, now(), |b| b.add(&["x".into()], now()))
                .unwrap()
                .unwrap();
        }
        fs::write(dir.path().join("boards/junk.json"), b"not json").unwrap();
        assert_eq!(store.repos().unwrap(), vec!["/a/one", "/b/two"]);
    }

    #[test]
    fn priorities_are_one_to_three_or_none() {
        let mut b = board(&["a"]);
        b.set_priority(1, 1).unwrap();
        assert_eq!(b.item(1).unwrap().priority, 1);
        b.set_priority(1, 0).unwrap();
        assert_eq!(b.item(1).unwrap().priority, 0);
        assert_eq!(b.set_priority(1, 4), Err(BoardError::Priority(4)));
        assert_eq!(b.set_priority(9, 1), Err(BoardError::NoItem(9)));
    }

    #[test]
    fn moving_an_item_keeps_its_number() {
        let mut b = board(&["a", "b", "c", "d"]);
        assert_eq!(b.move_item(4, 1), Ok(1));
        assert_eq!(
            b.items.iter().map(|i| i.n).collect::<Vec<_>>(),
            vec![4, 1, 2, 3]
        );
        assert_eq!(b.move_item(4, 99), Ok(4), "clamped to the end");
        assert_eq!(
            b.items.iter().map(|i| i.n).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(b.move_item(2, 0), Ok(1), "clamped to the top");
        assert_eq!(b.move_item(9, 1), Err(BoardError::NoItem(9)));
    }

    #[test]
    fn a_pr_merge_is_announced_once_per_link() {
        let mut b = board(&["a"]);
        b.link(1, 3231).unwrap();
        assert!(b.note_pr_merged(1, 3231));
        assert!(!b.note_pr_merged(1, 3231), "once");
        assert!(!b.note_pr_merged(1, 3232), "not the linked one");
        b.link(1, 3240).unwrap();
        assert!(b.note_pr_merged(1, 3240), "a new link is new");
        b.link(1, 0).unwrap();
        assert_eq!(b.item(1).unwrap().pr, 0);
    }

    #[test]
    fn the_advisor_is_woken_once_per_delivery() {
        let mut b = board(&["a"]);
        let t = send(&mut b, 1, "i-e", SendFlags::default()).unwrap();
        assert!(!b.note_woken(1, 1), "still sending");
        b.apply(t, Outcome::Accepted(receipt()), now()).unwrap();
        assert!(b.note_woken(1, 1));
        assert!(!b.note_woken(1, 1));
        let json = serde_json::to_string(&b).unwrap();
        assert!(!json.contains("waker"), "no waker, no field: {json}");
    }

    #[test]
    fn the_file_name_hash_is_pinned() {
        // A change here orphans every board on disk.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"/work/dbstore"), fnv1a(b"/work/dbstore"));
        assert_eq!(format!("{:016x}", fnv1a(b"a")), "af63dc4c8601ec8c");
    }

    #[test]
    fn statuses_serialise_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&ItemStatus::SentUnknown).unwrap(),
            "\"sent_unknown\""
        );
        assert_eq!(ItemStatus::SentUnknown.label(), "sent?");
    }

    #[test]
    fn our_own_pid_is_alive_and_zero_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }
}
