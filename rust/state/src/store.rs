// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The record store: one JSON file per agent under `<dir>/agents/`.
//!
//! One file per record rather than one big file, because writers are
//! independent — a hook firing in one pane must never lose a write from
//! another. Saves are atomic (temp file, then rename), so a reader never sees
//! a half-written record.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use crate::{write_atomic, Agent, AgentState};

/// A directory of agent records.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    /// What the store lock has cost this store and its clones — see
    /// [`Store::take_lock_times`].
    clock: Arc<LockClock>,
}

/// How long a hook tries for the store lock before it writes without it —
/// see [`Store::try_lock_briefly`].
pub const HOOK_PATIENCE: Duration = Duration::from_millis(HOOK_TRY_EVERY_MS * HOOK_TRIES);
const HOOK_TRIES: u64 = 15;
const HOOK_TRY_EVERY_MS: u64 = 10;

/// What the store lock cost one store and its clones since it was last asked
/// with [`Store::take_lock_times`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LockTimes {
    /// Waiting to take it, which is someone else holding it.
    pub waited: Duration,
    /// Holding it, all told.
    pub held: Duration,
    /// The longest single hold. A hook waits for one hold, not their sum, and
    /// gives up after [`HOOK_PATIENCE`] — so this, not `held`, says whether a
    /// hook went on without the lock.
    pub longest: Duration,
}

/// [`LockTimes`] as it accumulates, in nanoseconds.
#[derive(Debug, Default)]
struct LockClock {
    waited: AtomicU64,
    held: AtomicU64,
    longest: AtomicU64,
}

impl LockClock {
    fn waited(&self, took: Duration) {
        self.waited.fetch_add(nanos(took), Ordering::Relaxed);
    }

    fn held(&self, took: Duration) {
        let n = nanos(took);
        self.held.fetch_add(n, Ordering::Relaxed);
        self.longest.fetch_max(n, Ordering::Relaxed);
    }

    fn take(&self) -> LockTimes {
        let take = |n: &AtomicU64| Duration::from_nanos(n.swap(0, Ordering::Relaxed));
        LockTimes {
            waited: take(&self.waited),
            held: take(&self.held),
            longest: take(&self.longest),
        }
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Store {
    /// Opens (and creates) a store rooted at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        // The root first and on its own: in the home-less fallback it is the
        // part that must be this user's — see `make_state_root`.
        crate::make_state_root(&dir)?;
        fs::create_dir_all(dir.join("agents"))?;
        // Owner-only, both levels. A record carries what a person asked their
        // agent to do, the question it is waiting on and the folder it is in —
        // and under the usual umask this tree was born 0755 for every other
        // account on the machine to read. Narrowed after creation rather than at
        // it, because `create_dir_all` takes no mode; an existing directory is
        // narrowed too.
        dir_private(&dir);
        dir_private(&dir.join("agents"));
        Ok(Store {
            dir,
            clock: Arc::default(),
        })
    }

    /// The store's root directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What the store lock has cost this store and its clones since the last
    /// call, which starts the count again.
    ///
    /// Of the locks the supervisor takes, this is the only one hooks try for,
    /// so the supervisor reads it once a pass to say what a pass cost them. It
    /// has one thread; a clone locking on another at the same time would be
    /// counted into whichever pass it overlapped.
    pub fn take_lock_times(&self) -> LockTimes {
        self.clock.take()
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir
            .join("agents")
            .join(format!("{}.json", tower_os::file_name(id)))
    }

    /// Writes one record.
    pub fn save(&self, a: &Agent) -> std::io::Result<()> {
        let path = self.path(&a.id);
        let before = fs::read(&path).ok();
        write_atomic(&path, &Self::encode(a)?)?;
        self.note_transition(before.as_deref(), a);
        Ok(())
    }

    /// Appends to the [`crate::journal`] when `a`'s state differs from the
    /// one in `before`, the record's bytes on disk before this write.
    ///
    /// A record written for the first time counts as coming from IDLE: a hook
    /// that creates its record already WORKING has started a turn, and leaving
    /// that out would have the report count the whole time before it as work.
    /// An unreadable previous record is no evidence of anything, and says
    /// nothing.
    fn note_transition(&self, before: Option<&[u8]>, a: &Agent) {
        #[derive(serde::Deserialize)]
        struct Prev {
            state: AgentState,
        }
        let prev = match before {
            None => AgentState::Idle,
            Some(b) => match serde_json::from_slice::<Prev>(b) {
                Ok(p) => p.state,
                Err(_) => return,
            },
        };
        if prev != a.state {
            let _ = crate::journal::append(&self.dir, &crate::journal::Entry::new(a, prev));
        }
    }

    /// Writes one record only if its bytes on disk would change, and says
    /// whether it did.
    ///
    /// The scanner rewrites every live record on every pass, and a write here
    /// is a temp file, a full flush and a rename — about 4.6 ms on a Mac's
    /// SSD, so 100 agents cost half a second of every two-second pass, with
    /// the store lock held and every hook waiting on it. Comparing against the
    /// file rather than the record as listed matters: [`Store::list`] fills
    /// some fields in memory, and a record that only looks unchanged there
    /// must still reach the disk.
    pub fn save_if_changed(&self, a: &Agent) -> std::io::Result<bool> {
        let body = Self::encode(a)?;
        let path = self.path(&a.id);
        let before = fs::read(&path).ok();
        if before.as_deref() == Some(body.as_slice()) {
            return Ok(false);
        }
        write_atomic(&path, &body)?;
        self.note_transition(before.as_deref(), a);
        Ok(true)
    }

    /// [`Store::save_if_changed`] for a writer working from a copy it read
    /// earlier, which writes only while the file still holds that copy.
    ///
    /// For the scanner. It lists every record under the store lock and writes
    /// back from that list, while a hook that ran out of patience writes with
    /// no lock at all ([`Store::try_lock_briefly`]) — and on a loaded machine a
    /// pass outlasts that patience. Writing over the hook would put back the
    /// copy from before it and lose its state, task, question and hook mark.
    /// A record that moved on underneath is left alone; the next pass starts
    /// from it. `read` is `None` for a record that had no file then.
    pub fn save_over(&self, a: &Agent, read: Option<&Agent>) -> std::io::Result<bool> {
        let path = self.path(&a.id);
        let before = fs::read(&path).ok();
        // Most passes change nothing, and that answer needs no parsing.
        let body = Self::encode(a)?;
        if before.as_deref() == Some(body.as_slice()) {
            return Ok(false);
        }
        let now: Option<Agent> = before
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok());
        let held = match (read, now) {
            (Some(read), Some(now)) => Self::encode(read)? == Self::encode(&as_listed(now, read))?,
            // A file that cannot be read was skipped by the listing too, so
            // it is not anyone's newer record.
            (None, None) => true,
            _ => false,
        };
        if !held {
            return Ok(false);
        }
        write_atomic(&path, &body)?;
        self.note_transition(before.as_deref(), a);
        Ok(true)
    }

    fn encode(a: &Agent) -> std::io::Result<Vec<u8>> {
        serde_json::to_vec_pretty(a)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Reads one record.
    pub fn load(&self, id: &str) -> std::io::Result<Agent> {
        let body = fs::read(self.path(id))?;
        serde_json::from_slice(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Every record, ordered the way the list is meant to read: workspaces
    /// grouped so a person can see a whole workspace at once, the workspace
    /// with the most urgent agent first, and inside a workspace the vendors
    /// kept apart and whatever needs a person first.
    ///
    /// The outer group used to be the vendor. A workspace is the unit somebody
    /// actually works in — it is a place, with its own tabs and its own folder
    /// — while the vendor is a property of a row, already named in its own
    /// column and coloured. Grouping by place lets the tower answer "what is
    /// happening over there" without reading every row, which is what the
    /// popup is opened for. Vendors still do not interleave; they are now the
    /// inner key rather than the outer one.
    ///
    /// A group's rank is its most urgent member, not its name. Sorting the
    /// groups alphabetically would bury a workspace that has been blocked for
    /// hours below one that is merely called `00.something`, which is the same
    /// mistake as sorting rows by name — and the list exists to surface the row
    /// that needs a person.
    ///
    /// Within one kind and one state the *oldest* `state_since` comes first —
    /// ascending, not descending. An agent that has been blocked for five hours
    /// is more urgent than one blocked for a minute, so it has to be the one at
    /// the top of the group. Getting this backwards is not a cosmetic
    /// difference; it buries the agent that has been stuck longest under the
    /// ones that just changed, which is precisely the row the list exists to
    /// surface.
    ///
    /// The final tie-break on `id` is there because a stable sort alone would
    /// leave equal records in `read_dir` order, which is filesystem order and
    /// varies by machine. `state_since` carries microseconds, so an exact tie
    /// needs two records to change state in the same microsecond; the
    /// tie-break is there to make the impossible case deterministic.
    ///
    /// A file that cannot be read or parsed is skipped rather than failing the
    /// whole listing — one corrupt record must not blind the tower.
    pub fn list(&self) -> std::io::Result<Vec<Agent>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.dir.join("agents"))? {
            let entry = entry?;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") {
                continue;
            }
            let Ok(body) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(agent) = serde_json::from_slice::<Agent>(&body) else {
                continue;
            };
            out.push(agent);
        }
        adopt_ids_by_coordinate(&mut out);
        let ranks = group_ranks(&out);
        let rank = |a: &Agent| {
            ranks
                .get(&group_key(a).owned())
                .copied()
                .unwrap_or((u8::MAX, OffsetDateTime::UNIX_EPOCH))
        };
        out.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| group_key(a).owned().cmp(&group_key(b).owned()))
                .then_with(|| crate::kind_rank(&a.kind).cmp(&crate::kind_rank(&b.kind)))
                .then_with(|| a.state.priority().cmp(&b.state.priority()))
                .then_with(|| a.state_since.cmp(&b.state_since))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    /// Marks an unseen completion as seen.
    ///
    /// Locked across the load and the save, like every other read-modify-write in
    /// this file. Without it, pressing `o` while the tower's records thread was
    /// mid-scan could land between the scanner's snapshot and its save: the
    /// scanner then wrote its stale `DONE_UNREAD` back over the new IDLE *and*
    /// over `done_acked`, so the completion the user had just read reappeared as
    /// unread and notified again.
    ///
    /// Blocking, not the hook's brief attempt. The scanner holds this lock across
    /// local file reads and writes only — never across a socket call — so the wait
    /// is sub-millisecond, and a keypress that silently failed to stick is worse
    /// than one that takes a millisecond.
    pub fn mark_read(&self, id: &str, now: OffsetDateTime) -> std::io::Result<()> {
        let _lock = self.lock();
        let mut a = self.load(id)?;
        if a.state == AgentState::DoneUnread {
            a.transition(AgentState::Idle, now);
            // Remember that *this* completion was seen. Without it the next
            // backend observation reports the same persistent `done` and the row
            // goes straight back to unread — see `Agent::done_acked`.
            a.done_acked = true;
            return self.save(&a);
        }
        Ok(())
    }

    /// Acknowledges a completion **and** records the dispatch that answered it,
    /// but only while the agent is still on the turn the caller saw.
    ///
    /// 🚨 One lock for the whole decision. Doing this as a read, a compare and
    /// then a write lets a short turn start and finish in between — and the
    /// acknowledgement then lands on the completion that just arrived, which is
    /// the answer the caller is about to wait for. The two writes are paired
    /// here for the same reason: a dispatch whose boundary is not recorded
    /// leaves the next completion looking like the one already read.
    ///
    /// Answers whether the turn still matched. `false` means the agent moved
    /// on and nothing was changed — the new completion is not the caller's to
    /// consume.
    pub fn acknowledge_dispatch(
        &self,
        id: &str,
        work_seq: u64,
        now: OffsetDateTime,
    ) -> std::io::Result<bool> {
        let _lock = self.lock();
        let mut a = self.load(id)?;
        if a.work_seq != work_seq {
            return Ok(false);
        }
        if a.state == AgentState::DoneUnread {
            a.transition(AgentState::Idle, now);
        }
        // The dispatch makes whatever was acknowledged no longer current; see
        // `note_dispatch`, which this replaces for callers that also have a
        // completion to spend.
        a.done_acked = false;
        a.updated_at = now;
        self.save(&a)?;
        Ok(true)
    }

    /// Notes that work was dispatched into this pane, so the completion the user
    /// acknowledged is no longer the current one.
    ///
    /// Dispatching *is* new work by definition, which makes this the one signal
    /// available when no hook is installed and the backend only ever reports a
    /// lifecycle state. Without it, a broadcast answered faster than the poll
    /// interval produced a completion that read as the acknowledged one and a row
    /// that stayed IDLE. See [`tower_state::Agent::done_acked`].
    ///
    /// A record that cannot be read is not an error: the caller just sent a
    /// prompt, and failing that because a file was missing would be worse than
    /// losing one notification.
    pub fn note_dispatch(&self, id: &str, now: OffsetDateTime) {
        // Same cycle, same lock as `mark_read`: a scan in the gap would put
        // `done_acked` back and the next observation would read this dispatch's
        // completion as the one already acknowledged.
        let _lock = self.lock();
        if let Ok(mut a) = self.load(id) {
            if a.done_acked {
                a.done_acked = false;
                a.updated_at = now;
                let _ = self.save(&a);
            }
        }
    }

    /// Removes a record. A missing file is not an error — the caller wanted it
    /// gone and it is gone.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Makes a directory this user's alone. Best effort: a store that cannot be
/// narrowed is still a store, and refusing to open it would be the worse answer.
///
/// That holds only under `HOME`, whose parent no one else can write. The
/// home-less fallback in the shared temporary directory, where a failure here
/// would mean a directory someone else controls, never relies on this: its root
/// is made by `make_state_root`, which fails instead.
#[cfg(unix)]
fn dir_private(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn dir_private(_p: &Path) {}

/// Serializes read-modify-write cycles on the record store, across processes.
///
/// Atomic replacement stops a reader seeing half a file. It does not stop two
/// writers from each loading the same record, each changing a different field,
/// and the later one publishing a snapshot that never had the other's change in
/// it. The scanner and a hook do exactly that on the same pane: the scanner
/// loads, the hook writes DONE_UNREAD with its task and question, and the
/// scanner's save puts the old state back — silently, since nothing failed.
///
/// Kept out of `Store::save` on purpose. A lock around one write is useless; the
/// window is between the *load* and the save, so it has to be the caller's whole
/// cycle that holds it.
pub struct StoreLock {
    /// Holding the handle is holding the lock; dropping it lets go.
    _file: std::fs::File,
    since: Instant,
    clock: Arc<LockClock>,
}

impl StoreLock {
    /// Taken now, after a wait that began at `asked`.
    fn new(file: std::fs::File, clock: &Arc<LockClock>, asked: Instant) -> Self {
        let since = Instant::now();
        clock.waited(since - asked);
        StoreLock {
            _file: file,
            since,
            clock: Arc::clone(clock),
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        self.clock.held(self.since.elapsed());
    }
}

impl Store {
    /// Blocks until this process owns the store.
    ///
    /// For the scanner, which can afford to wait: a hook's cycle is one small
    /// file read and one write.
    pub fn lock(&self) -> Option<StoreLock> {
        let f = self.lock_file()?;
        let asked = Instant::now();
        f.lock().ok()?;
        Some(StoreLock::new(f, &self.clock, asked))
    }

    /// Tries for the store, briefly, then gives up.
    ///
    /// For the hook path, which runs inside the agent's own process and must
    /// never make it wait. Giving up means proceeding *unlocked* — the same race
    /// that existed before, narrowed to the case where a scan is running right
    /// now, which is milliseconds. A hook that blocked instead could stall a turn,
    /// and that is a worse failure than an occasional lost field.
    pub fn try_lock_briefly(&self) -> Option<StoreLock> {
        self.try_lock_briefly_resting(std::thread::sleep)
    }

    /// [`Self::try_lock_briefly`] with the wait between tries handed in, so a
    /// test can count the rests instead of timing them.
    fn try_lock_briefly_resting(&self, rest: impl FnMut(Duration)) -> Option<StoreLock> {
        let f = self.lock_file()?;
        let asked = Instant::now();
        if briefly(|| f.try_lock(), rest) {
            return Some(StoreLock::new(f, &self.clock, asked));
        }
        self.clock.waited(asked.elapsed());
        None
    }

    fn lock_file(&self) -> Option<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(LOCK_FILE))
            .ok()
    }
}

/// Spends a hook's patience on `attempt`, and says whether it got the lock.
///
/// The policy on its own, away from the file handle: how many tries, what a
/// failure means, and when a rest buys anything.
fn briefly(
    mut attempt: impl FnMut() -> Result<(), std::fs::TryLockError>,
    mut rest: impl FnMut(Duration),
) -> bool {
    for taken in 1..=HOOK_TRIES {
        match attempt() {
            Ok(()) => return true,
            // 🚨 Not every failure is somebody else holding it, and a retry only
            // answers the one that is — see [`worth_another_try`]. Retrying the
            // rest spent a hook's whole patience before it wrote unlocked anyway.
            Err(e) if !worth_another_try(&e) => return false,
            Err(_) => {}
        }
        // 🚨 Not after the last try. A rest buys another try, and there is no try
        // after this one — it only made every give-up 10 ms longer, inside a hook
        // that runs in the agent's own process.
        if taken < HOOK_TRIES {
            rest(Duration::from_millis(HOOK_TRY_EVERY_MS));
        }
    }
    false
}

/// Whether a failed `try_lock` is worth another try.
///
/// [`std::fs::TryLockError::WouldBlock`] is the only failure a retry can answer:
/// it says somebody holds the lock right now, which is milliseconds away from
/// being untrue. `Error` is the call itself failing — `flock` unsupported by the
/// filesystem, a handle that cannot be locked — and it will fail the same way in
/// 10 ms and in 150.
fn worth_another_try(e: &std::fs::TryLockError) -> bool {
    matches!(e, std::fs::TryLockError::WouldBlock)
}

/// The coordination file a store keeps beside its records.
///
/// Created by the first lock and never written to — a zero-byte handle for
/// `flock` to hold. Scaffolding rather than data. Named here so that fact has a
/// single home.
pub const LOCK_FILE: &str = "store.lock";

/// What identifies the workspace a record is grouped under.
///
/// The herdr workspace id when the record has one, and the label otherwise.
/// The label is what a person reads and what a person can change: renaming a
/// workspace while one of its agents is a retained dead record splits the group
/// in two, because the scanner refreshes the label only from panes the backend
/// still lists. The id does not move.
///
/// A record written before the field existed has none, and [`Store::list`]
/// fills it in from the pane's coordinate when herdr corroborates it — see
/// [`adopt_ids_by_coordinate`]. The label is what is left when nothing does.
///
/// Callers that group — the tower's rules, the card's headings — read this on
/// records from `list`, which is what keeps the three surfaces agreeing.
pub fn group_key(a: &Agent) -> GroupKey<'_> {
    if a.pane.workspace_id.is_empty() {
        GroupKey::Label(&a.pane.session)
    } else {
        GroupKey::Id(&a.pane.workspace_id)
    }
}

/// The workspace a herdr pane coordinate names: `w2:p4S` → `w2`.
///
/// `None` for anything that is not one, which a colon alone does not prove —
/// an empty prefix is not a workspace either.
fn workspace_of(pane_id: &str) -> Option<&str> {
    match pane_id.split_once(':') {
        Some((ws, _)) if !ws.is_empty() => Some(ws),
        _ => None,
    }
}

/// Which of the two namespaces a group's identity came from.
///
/// They are kept apart because the strings overlap. An unlabeled workspace is
/// reported under its own id, so a label really can read `w2` — and a person is
/// free to name a workspace that way too. An unscanned record carrying the label
/// `w2` and a current workspace whose id is `w2` are then equal as strings and
/// unrelated in fact, and `adopt_ids_by_label` cannot tell them apart: no
/// current record carries that label, so there is nothing to reconcile through.
/// Comparing the variant as well as the string is what stops one workspace's
/// dead row from being drawn under another workspace's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupKey<'a> {
    /// The herdr workspace id, which nobody can change.
    Id(&'a str),
    /// The label, for a record no scan has given an id yet — one a hook created.
    Label(&'a str),
}

impl GroupKey<'_> {
    /// The same identity, owned, for use as a map key.
    fn owned(self) -> (bool, String) {
        match self {
            GroupKey::Id(s) => (true, s.to_string()),
            GroupKey::Label(s) => (false, s.to_string()),
        }
    }
}

/// Gives a record with no workspace id the one its coordinate names, when a
/// record herdr actually reported carries that id too.
///
/// Only the panes herdr still lists are scanned, so the first scan after
/// `workspace_id` arrived writes an id onto every live record and onto none of
/// the retained dead ones. Their coordinates still say where they are —
/// herdr numbers a pane `w2:p4S`, and everything left of the colon is the
/// workspace — so one physical workspace stops splitting into a `w2` group and
/// a label group. Measured against this machine's own store, the coordinate is
/// exactly the current workspace: `w2` holds the labels `00.dbstore_go`,
/// `dbstore_go_v2` and nothing at all; `w7` holds `10.parallax` and nothing;
/// `w8` holds `20.herdr-tower`. Three workspaces, six names.
///
/// **Corroboration is the point.** Reading the coordinate on its own would be
/// wrong, because a coordinate can look like herdr's without being one: the
/// golden fixture's panes were renumbered `%N` → `w2:pN` wholesale, so nine
/// unrelated workspaces there all read `w2`. A coordinate is evidence of *which
/// known workspace* a record belongs to, and with no record from herdr to point
/// at it says nothing. That also settles the
/// case a label cannot reach — a workspace renamed while an agent was dead, or
/// renamed before the first scan — because no label is consulted either way.
///
/// In memory only: nothing is written back to disk here. A record that does get
/// saved afterwards keeps the adopted id, which is correct — herdr is reporting
/// that workspace under that id right now.
fn adopt_ids_by_coordinate(agents: &mut [Agent]) {
    let known: std::collections::HashSet<String> = agents
        .iter()
        .filter(|a| !a.pane.workspace_id.is_empty())
        .map(|a| a.pane.workspace_id.clone())
        .collect();
    for a in agents.iter_mut().filter(|a| a.pane.workspace_id.is_empty()) {
        if let Some(ws) = workspace_of(&a.pane.pane_id).filter(|w| known.contains(*w)) {
            a.pane.workspace_id = ws.to_string();
        }
    }
}

/// The record on disk as [`Store::list`] would have handed it over, given the
/// copy `read` that it did hand over.
///
/// The listing adopts an empty workspace ID in memory
/// ([`adopt_ids_by_coordinate`]), so a copy read through it differs from its
/// own file without anyone having written that file. Compared raw, such a
/// record looked changed underneath on every pass, and [`Store::save_over`]
/// never wrote it again — not even to mark it dead.
fn as_listed(mut now: Agent, read: &Agent) -> Agent {
    if now.pane.workspace_id.is_empty()
        && workspace_of(&now.pane.pane_id) == Some(read.pane.workspace_id.as_str())
    {
        now.pane.workspace_id = read.pane.workspace_id.clone();
    }
    now
}

/// Each workspace's most urgent agent, which is what orders the groups.
///
/// The pair is the member's own sort key — state priority, then how long it has
/// been there — so a workspace whose worst row is WAITING comes before one whose
/// worst is WORKING, and between two blocked workspaces the one blocked longest
/// leads. That is the row-level rule lifted one level up, rather than a second
/// idea of urgency that could disagree with the rows underneath it.
///
/// A record with no workspace (herdr reports none for a pane whose workspace is
/// gone) groups under the empty key like any other, and sinks or rises on the
/// same rule.
fn group_ranks(agents: &[Agent]) -> HashMap<(bool, String), (u8, OffsetDateTime)> {
    let mut out: HashMap<(bool, String), (u8, OffsetDateTime)> = HashMap::new();
    for a in agents {
        let key = (a.state.priority(), a.state_since);
        out.entry(group_key(a).owned())
            .and_modify(|best| {
                if key < *best {
                    *best = key;
                }
            })
            .or_insert(key);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaneRef;

    /// Epoch itself means "never" for last_hook, so the fixture clock starts
    /// well after it — otherwise a legitimately hooked record reads as unhooked.
    fn at(mins: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::minutes(mins)
    }

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path()).unwrap();
        (d, s)
    }

    /// The scanner's copy goes back only over the record it was read from. A
    /// hook that wrote in between — unlocked, because the pass outlasted its
    /// patience — keeps what it wrote, and so does a record created or removed
    /// meanwhile.
    #[test]
    fn a_copy_is_written_back_only_over_what_it_was_read_from() {
        let (_d, s) = store();
        let id = "claude-w2:p1";
        let read = Agent::new(id, "claude", at(0));
        s.save(&read).unwrap();
        let mut scanned = read.clone();
        scanned.title = "from the scan".into();

        let mut hooked = read.clone();
        hooked.transition(AgentState::DoneUnread, at(1));
        hooked.task = "from the hook".into();
        s.save(&hooked).unwrap();
        assert!(!s.save_over(&scanned, Some(&read)).unwrap(), "a hook wrote");
        let kept = s.load(id).unwrap();
        assert_eq!(kept.task, "from the hook");
        assert_eq!(kept.state, AgentState::DoneUnread);

        assert!(s.save_over(&scanned, Some(&hooked)).unwrap(), "unchanged");
        assert_eq!(s.load(id).unwrap().title, "from the scan");
        assert!(
            !s.save_over(&scanned, Some(&scanned)).unwrap(),
            "the same bytes"
        );

        s.delete(id).unwrap();
        assert!(!s.save_over(&scanned, Some(&scanned)).unwrap(), "removed");
        assert!(s.load(id).is_err(), "not brought back");

        assert!(s.save_over(&scanned, None).unwrap(), "new, and still new");
        assert!(!s.save_over(&read, None).unwrap(), "created meanwhile");
        assert_eq!(s.load(id).unwrap().title, "from the scan");

        std::fs::write(s.path(id), b"{broken").unwrap();
        assert!(
            s.save_over(&scanned, None).unwrap(),
            "a broken file is nobody's record"
        );
    }

    /// Every combination of what is on disk, what the writer read and what it
    /// writes: the file takes the new copy exactly when it still holds the
    /// one read (or there was none and still is none) and the bytes would
    /// change, the answer says whether it did, and what is read back after a
    /// write is what was written.
    #[test]
    fn save_over_writes_exactly_when_the_file_still_holds_the_copy_read() {
        let id = "claude-w2:p1";
        let version = |title: &str, state: AgentState| {
            let mut a = Agent::new(id, "claude", at(0));
            a.title = title.into();
            a.state = state;
            a
        };
        let a = version("a", AgentState::Idle);
        let b = version("b", AgentState::Working);
        let c = version("c", AgentState::DoneUnread);
        #[derive(Clone, Copy, Debug)]
        enum Disk {
            Absent,
            Holds(usize),
            Broken,
        }
        let all = [a, b, c];
        let disks = [Disk::Absent, Disk::Holds(0), Disk::Holds(1), Disk::Broken];
        let reads = [None, Some(0), Some(1)];
        for disk in disks {
            for read in reads {
                for new in 0..all.len() {
                    let (_d, s) = store();
                    let path = s.path(id);
                    match disk {
                        Disk::Absent => {}
                        Disk::Holds(i) => s.save(&all[i]).unwrap(),
                        Disk::Broken => std::fs::write(&path, b"{broken").unwrap(),
                    }
                    let before = std::fs::read(&path).ok();
                    let held = match (disk, read) {
                        (Disk::Absent | Disk::Broken, None) => true,
                        (Disk::Holds(i), Some(r)) => i == r,
                        _ => false,
                    };
                    let changes = !matches!(disk, Disk::Holds(i) if i == new);
                    let case = format!("disk {disk:?}, read {read:?}, new {new}");

                    let wrote = s.save_over(&all[new], read.map(|r| &all[r])).unwrap();
                    assert_eq!(wrote, held && changes, "{case}");
                    if wrote {
                        let back = s.load(id).unwrap();
                        assert_eq!(back.title, all[new].title, "{case}");
                        assert_eq!(back.state, all[new].state, "{case}");
                    } else {
                        assert_eq!(std::fs::read(&path).ok(), before, "{case}: untouched");
                    }
                }
            }
        }
    }

    /// Over the copy it read, `save_over` is `save_if_changed`: the same
    /// answer, the same bytes and the same journal. And a write it declines
    /// leaves the journal alone — the transition belongs to whoever wrote.
    #[test]
    fn save_over_on_a_held_copy_matches_save_if_changed() {
        let id = "claude-w2:p1";
        let read = Agent::new(id, "claude", at(0));
        let mut next = read.clone();
        next.transition(AgentState::Working, at(1));
        for new in [read.clone(), next.clone()] {
            let (over_dir, over) = store();
            let (plain_dir, plain) = store();
            over.save(&read).unwrap();
            plain.save(&read).unwrap();
            let journal = |d: &std::path::Path| crate::journal::read_since(d, at(-1)).len();
            let (over_before, plain_before) = (journal(over_dir.path()), journal(plain_dir.path()));
            assert_eq!(
                over.save_over(&new, Some(&read)).unwrap(),
                plain.save_if_changed(&new).unwrap()
            );
            assert_eq!(
                std::fs::read(over.path(id)).unwrap(),
                std::fs::read(plain.path(id)).unwrap()
            );
            assert_eq!(
                journal(over_dir.path()) - over_before,
                journal(plain_dir.path()) - plain_before
            );
        }

        let (d, s) = store();
        s.save(&read).unwrap();
        let mut hooked = read.clone();
        hooked.task = "from the hook".into();
        s.save(&hooked).unwrap();
        let entries = crate::journal::read_since(d.path(), at(-1)).len();
        assert!(!s.save_over(&next, Some(&read)).unwrap());
        assert_eq!(crate::journal::read_since(d.path(), at(-1)).len(), entries);
    }

    /// A copy taken through [`Store::list`] carries a workspace ID its file
    /// does not; the file still holds that copy. A workspace that does not
    /// match the coordinate is a real difference, not the listing's.
    #[test]
    fn a_listed_copy_is_held_by_its_own_file() {
        let (_d, s) = store();
        let mut known = Agent::new("claude-w2:p1", "claude", at(0));
        known.pane.pane_id = "w2:p1".into();
        known.pane.workspace_id = "w2".into();
        s.save(&known).unwrap();
        let mut bare = Agent::new("claude-w2:p2", "claude", at(0));
        bare.pane.pane_id = "w2:p2".into();
        s.save(&bare).unwrap();

        let listed = s.list().unwrap();
        let read = listed.iter().find(|a| a.id == bare.id).unwrap();
        assert_eq!(read.pane.workspace_id, "w2");
        let mut scanned = read.clone();
        scanned.title = "from the scan".into();
        assert!(s.save_over(&scanned, Some(read)).unwrap());
        assert_eq!(s.load(&bare.id).unwrap().title, "from the scan");

        s.save(&bare).unwrap();
        let mut elsewhere = read.clone();
        elsewhere.pane.workspace_id = "w9".into();
        assert!(!s.save_over(&scanned, Some(&elsewhere)).unwrap());
    }

    /// A missing record is written, an identical one is left alone, and one
    /// whose bytes would differ is rewritten — judged against the file, not
    /// against a copy in memory.
    #[test]
    fn a_record_is_written_only_when_its_bytes_change() {
        let (_d, s) = store();
        let mut a = Agent::new("claude-w2:p1", "claude", at(0));
        assert!(s.save_if_changed(&a).unwrap(), "no file yet");
        assert!(!s.save_if_changed(&a).unwrap(), "the same bytes");
        a.title = "a new turn".into();
        assert!(s.save_if_changed(&a).unwrap(), "a changed field");
        assert_eq!(s.load("claude-w2:p1").unwrap().title, "a new turn");
        std::fs::write(s.path("claude-w2:p1"), b"{broken").unwrap();
        assert!(
            s.save_if_changed(&a).unwrap(),
            "a file that differs, however"
        );
    }

    /// `mark_read` has to wait for the scanner rather than writing into the
    /// middle of its cycle. Proven by holding the lock the scanner holds and
    /// watching the call sit still.
    ///
    /// The assertion only fails in the broken direction: if `mark_read` finishes
    /// while the lock is held, it never took it.
    #[test]
    fn mark_read_waits_for_the_store_lock() {
        let (_d, s) = store();
        let mut a = Agent::new("claude-w2:p1", "claude", at(0));
        a.state = AgentState::DoneUnread;
        s.save(&a).unwrap();

        let held = s.lock().expect("the lock file has to be creatable");
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dir = s.dir().to_path_buf();
        let flag = std::sync::Arc::clone(&done);
        let t = std::thread::spawn(move || {
            let mine = Store::new(&dir).unwrap();
            mine.mark_read("claude-w2:p1", at(1)).unwrap();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "mark_read wrote while the scanner held the store"
        );

        drop(held);
        t.join().unwrap();
        let after = s.load("claude-w2:p1").unwrap();
        assert_eq!(after.state, AgentState::Idle);
        assert!(after.done_acked);
    }

    /// Waiting for the lock and holding it are counted apart, the longest
    /// single hold is kept beside their sum, and taking the times starts the
    /// count again. A clone is the same store — the tower hands one to its
    /// records thread — so a clone locking on another thread adds to it; a
    /// second `Store::new` on the same directory keeps its own.
    ///
    /// Lower bounds only, with wide margins, so a loaded machine cannot fail
    /// it: the waiter sits behind a 300 ms hold and then holds its own for
    /// nothing.
    #[test]
    fn lock_times_count_waiting_holding_and_the_longest_hold_per_store() {
        let (_d, s) = store();
        assert_eq!(s.take_lock_times(), LockTimes::default());

        let dir = s.dir().to_path_buf();
        let (taken, wait) = std::sync::mpsc::channel();
        let other = std::thread::spawn(move || {
            let theirs = Store::new(&dir).unwrap();
            let lock = theirs.lock().expect("the lock file has to be creatable");
            taken.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            drop(lock);
            theirs.take_lock_times()
        });
        wait.recv().unwrap();
        let clone = s.clone();
        std::thread::spawn(move || drop(clone.lock().expect("the lock file has to be creatable")))
            .join()
            .unwrap();
        let theirs = other.join().unwrap();

        assert!(theirs.held >= Duration::from_millis(300), "{theirs:?}");
        let ours = s.take_lock_times();
        assert!(ours.waited >= Duration::from_millis(100), "{ours:?}");
        assert!(
            ours.held < Duration::from_millis(150),
            "the wait was counted as holding: {ours:?}"
        );
        assert_eq!(ours.longest, ours.held, "one hold is its own longest");
        assert_eq!(s.take_lock_times(), LockTimes::default(), "taken, so reset");

        let first = s.lock().expect("nobody else holds it");
        std::thread::sleep(Duration::from_millis(30));
        drop(first);
        let second = s.try_lock_briefly().expect("nobody else holds it");
        std::thread::sleep(Duration::from_millis(60));
        drop(second);
        let two = s.take_lock_times();
        assert!(two.held >= Duration::from_millis(90), "{two:?}");
        assert!(two.longest >= Duration::from_millis(60), "{two:?}");
        assert!(
            two.longest < two.held,
            "the longest is one hold, not the sum: {two:?}"
        );
    }

    /// A record says what a person asked their agent to do and what it is waiting
    /// on. Under the usual umask the store was born readable by every account on
    /// the machine.
    #[cfg(unix)]
    #[test]
    fn the_store_is_this_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (d, s) = store();
        for p in [s.dir().to_path_buf(), s.dir().join("agents")] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", p.display());
        }

        // An existing wide-open store is narrowed when it is opened.
        let wide = d.path().join("old");
        std::fs::create_dir_all(wide.join("agents")).unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
        Store::new(&wide).unwrap();
        assert_eq!(
            std::fs::metadata(&wide).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// Only the caller that knows a turn began counts one.
    ///
    /// The states either side of the edge cannot say. Every edge into WORKING
    /// counts a permission request being answered — all three CLIs park a running
    /// turn at WAITING and come back — and no edge from WAITING misses a real new
    /// prompt, because a *missed* Stop hook also leaves the record at WAITING and
    /// the next prompt starts from there.
    #[test]
    fn only_an_explicit_turn_start_moves_the_count() {
        let mut a = Agent::new("claude-w2:p1", "claude", at(0));
        assert_eq!(a.work_seq, 0);

        a.begin_turn(at(1));
        assert_eq!(a.state, AgentState::Working);
        assert_eq!(a.work_seq, 1, "a prompt is a turn");

        // A permission request and its answer: the same turn throughout.
        a.transition(AgentState::Waiting, at(2));
        a.transition(AgentState::Working, at(3));
        assert_eq!(a.work_seq, 1);

        // A completion, then a prompt from WAITING — which is where a missed Stop
        // hook leaves the record.
        a.transition(AgentState::Waiting, at(4));
        a.begin_turn(at(5));
        assert_eq!(a.work_seq, 2, "a prompt counts wherever it starts from");

        // And a repeated observation is not a turn at all.
        a.transition(AgentState::Working, at(6));
        assert_eq!(a.work_seq, 2);
    }

    /// One prompt is one turn, whichever of its two witnesses arrives first.
    ///
    /// The scanner's poll can beat the start hook to the record. Counting both
    /// would tell `tower_cli::allcmd` that a turn began which nobody asked for, and
    /// that turn's completion would be reported as the answer to an instruction
    /// it never saw.
    #[test]
    fn a_late_hook_claims_the_inferred_turn_instead_of_counting_it_again() {
        let mut a = Agent::new("claude-w2:p1", "claude", at(0));

        // The poll lands first: a settled record is suddenly working.
        a.infer_turn(at(1));
        assert_eq!(a.work_seq, 1);

        // Then the prompt's own hook, for that same prompt, a moment behind.
        a.begin_turn(at(1) + time::Duration::seconds(2));
        assert_eq!(a.work_seq, 1, "one prompt, one turn");

        // A second prompt into an agent still working is a turn of its own —
        // the claim is spent, and only an inferred count can be claimed.
        a.begin_turn(at(1) + time::Duration::seconds(3));
        assert_eq!(a.work_seq, 2);

        // The claim does not outlive its turn: once the record settles, the next
        // inferred start is a new count again.
        a.transition(AgentState::DoneUnread, at(4));
        a.infer_turn(at(5));
        assert_eq!(a.work_seq, 3);
        a.transition(AgentState::Idle, at(6));
        a.begin_turn(at(6) + time::Duration::seconds(1));
        assert_eq!(a.work_seq, 4, "a settled turn releases nothing to claim");
    }

    /// A claim nobody came for does not wait around for the next prompt.
    ///
    /// The hook it is held for fired before the poll that inferred the turn, so
    /// it is moments behind at most. If the turn instead ends unobserved — the
    /// Stop hook missed and the backend's brief DONE missed with it — the claim
    /// would otherwise still be standing when the *next* prompt arrives, and that
    /// prompt would go uncounted: a `close-all --watch` on it waits out the whole
    /// timeout for a turn that finished.
    #[test]
    fn an_inferred_turn_stops_being_claimable_once_its_hook_is_plainly_not_coming() {
        let mut a = Agent::new("claude-w2:p1", "claude", at(0));
        a.infer_turn(at(0));
        assert_eq!(a.work_seq, 1);

        // Nothing observed the turn ending, so the record is still WORKING when
        // the next prompt lands. The window covers a hook's write lagging a poll
        // — spawn, start-up, the store lock — and a short turn answered and
        // followed up inside it is already past that.
        a.begin_turn(at(0) + time::Duration::seconds(10));
        assert_eq!(
            a.work_seq, 2,
            "ten seconds is a turn that happened, not a hook that is late"
        );

        // And the spent claim does not strand the prompt after it either.
        a.begin_turn(at(0) + time::Duration::seconds(11));
        assert_eq!(a.work_seq, 3);

        // The race it does cover: the write landing moments behind the poll.
        a.transition(AgentState::Idle, at(1));
        a.infer_turn(at(2));
        assert_eq!(a.work_seq, 4);
        a.begin_turn(at(2) + time::Duration::seconds(2));
        assert_eq!(a.work_seq, 4, "that is the prompt the scanner saw");
    }

    #[test]
    fn save_and_load_round_trip() {
        let (_d, s) = store();
        let mut a = Agent::new("claude-w2:p1", "claude", at(0));
        a.pane = PaneRef {
            backend: String::new(),
            session: "dev".into(),
            window: 7,
            pane_id: "w2:p1".into(),
            terminal_id: String::new(),
            tab: String::new(),
            workspace_id: "w2".into(),
        };
        a.cwd = "/proj".into();
        s.save(&a).unwrap();

        let got = s.load("claude-w2:p1").unwrap();
        assert_eq!(got.kind, "claude");
        assert_eq!(got.pane.pane_id, "w2:p1");
        assert_eq!(got.cwd, "/proj");
    }

    /// A colon in the record ID is a herdr pane coordinate, and it has to
    /// survive being used as a filename.
    #[test]
    fn ids_with_colons_work_as_filenames() {
        let (_d, s) = store();
        let a = Agent::new("claude-w2:p4S", "claude", at(0));
        s.save(&a).unwrap();
        assert_eq!(s.load("claude-w2:p4S").unwrap().id, "claude-w2:p4S");
    }

    /// The list is ordered for reading: kinds grouped, then what needs a person.
    #[test]
    fn list_groups_kinds_then_urgency() {
        let (_d, s) = store();
        let mut idle = Agent::new("claude-2", "claude", at(0));
        idle.state = AgentState::Idle;
        let mut waiting = Agent::new("claude-1", "claude", at(0));
        waiting.state = AgentState::Waiting;
        let mut codex = Agent::new("codex-1", "codex", at(0));
        codex.state = AgentState::Waiting;
        for a in [&idle, &waiting, &codex] {
            s.save(a).unwrap();
        }

        let got = s.list().unwrap();
        let ids: Vec<&str> = got.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-1", "claude-2", "codex-1"]);
    }

    /// The outer group is the workspace, and a group is ranked by its most
    /// urgent agent rather than by its name.
    ///
    /// Against the vendor-first order this fails on both counts: it put every
    /// claude ahead of every codex, so `b`'s two rows were split around `a`'s.
    #[test]
    fn list_groups_workspaces_by_their_most_urgent_agent() {
        let (_d, s) = store();
        let mut a_idle = Agent::new("claude-a", "claude", at(0));
        a_idle.state = AgentState::Idle;
        a_idle.pane.session = "a".into();
        let mut b_wait = Agent::new("claude-b", "claude", at(0));
        b_wait.state = AgentState::Waiting;
        b_wait.pane.session = "b".into();
        let mut b_codex = Agent::new("codex-b", "codex", at(0));
        b_codex.state = AgentState::Idle;
        b_codex.pane.session = "b".into();
        for r in [&a_idle, &b_wait, &b_codex] {
            s.save(r).unwrap();
        }

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec!["claude-b", "codex-b", "claude-a"],
            "the workspace holding the blocked agent leads, and it stays whole"
        );
    }

    /// A rename must not split one workspace into two groups.
    ///
    /// The scanner refreshes the label only from panes herdr still lists, and a
    /// retained dead record is not one of them — so after a rename the live rows
    /// carry the new label and the dead row still carries the old. Keyed on the
    /// label they land in separate groups with separate urgency ranks. The tower
    /// now hides dead rows, but recovery commands still consume this grouping.
    #[test]
    fn a_rename_does_not_split_a_workspace() {
        let (_d, s) = store();
        let mut live = Agent::new("claude-live", "claude", at(0));
        live.state = AgentState::Waiting;
        live.pane.session = "new-name".into();
        live.pane.workspace_id = "w2".into();
        let mut gone = Agent::new("claude-dead", "claude", at(0));
        gone.state = AgentState::Dead;
        gone.pane.session = "old-name".into(); // never refreshed — it is dead
        gone.pane.workspace_id = "w2".into();
        let mut other = Agent::new("claude-other", "claude", at(0));
        other.state = AgentState::DoneUnread;
        other.pane.session = "elsewhere".into();
        other.pane.workspace_id = "w3".into();
        for r in [&live, &gone, &other] {
            s.save(r).unwrap();
        }

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec!["claude-live", "claude-dead", "claude-other"],
            "w2's two records stay adjacent despite carrying different labels"
        );
    }

    /// A record written before the id existed is joined to its workspace by its
    /// coordinate, when a record herdr reported corroborates that workspace.
    ///
    /// The label cannot reach this: the workspace was renamed while the agent
    /// was dead, so the two labels no longer match and there is nothing to
    /// reconcile through.
    #[test]
    fn an_unscanned_record_is_joined_by_its_corroborated_coordinate() {
        let (_d, s) = store();
        let mut live = Agent::new("claude-live", "claude", at(0));
        live.state = AgentState::Waiting;
        live.pane.session = "new-name".into();
        live.pane.pane_id = "w2:p1".into();
        live.pane.workspace_id = "w2".into();
        let mut unscanned = Agent::new("claude-unscanned", "claude", at(0));
        unscanned.state = AgentState::Dead;
        unscanned.pane.session = "old-name".into(); // renamed while it was dead
        unscanned.pane.pane_id = "w2:p9".into(); // and no id: never rescanned
        let mut other = Agent::new("claude-other", "claude", at(0));
        other.state = AgentState::DoneUnread;
        other.pane.session = "beta".into();
        other.pane.pane_id = "w3:p1".into();
        other.pane.workspace_id = "w3".into();
        for r in [&live, &unscanned, &other] {
            s.save(r).unwrap();
        }

        let got = s.list().unwrap();
        let ids: Vec<&str> = got.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["claude-live", "claude-unscanned", "claude-other"],
            "the coordinate joins them even though the label moved"
        );
        assert_eq!(crate::group_key(&got[1]), crate::GroupKey::Id("w2"));
    }

    /// An uncorroborated coordinate says nothing.
    ///
    /// A coordinate can look like herdr's without being one: the golden
    /// fixture's panes were renumbered `%N` → `w2:pN` wholesale, so nine
    /// unrelated workspaces there all read `w2`. Adopting that would merge them
    /// into one.
    #[test]
    fn a_coordinate_no_live_record_confirms_is_not_adopted() {
        let (_d, s) = store();
        for (id, session) in [("claude-a", "alpha"), ("claude-b", "beta")] {
            let mut a = Agent::new(id, "claude", at(0));
            a.state = AgentState::Idle;
            a.pane.session = session.into();
            a.pane.pane_id = format!("w2:p{id}"); // the same made-up workspace
            s.save(&a).unwrap();
        }

        let got = s.list().unwrap();
        assert_eq!(crate::group_key(&got[0]), crate::GroupKey::Label("alpha"));
        assert_eq!(crate::group_key(&got[1]), crate::GroupKey::Label("beta"));
    }

    /// A coordinate with no workspace in it names none, so the label is what
    /// is left.
    #[test]
    fn a_coordinate_with_no_workspace_falls_back_to_the_label() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.pane.pane_id = "%3".into(); // a coordinate herdr does not use
        a.pane.session = "alpha".into();
        assert_eq!(crate::group_key(&a), crate::GroupKey::Label("alpha"));

        a.pane.pane_id = ":p1".into(); // a colon is not a workspace
        assert_eq!(crate::group_key(&a), crate::GroupKey::Label("alpha"));
    }

    /// A label that reads like an id is not that id.
    ///
    /// An unlabeled workspace is reported under its own id, so `w2` is a label a
    /// record can genuinely carry — and a person may name a workspace that way
    /// too. Comparing the strings alone drew one workspace's dead row under
    /// another workspace's name, and `adopt_ids_by_label` cannot catch it: no
    /// current record carries that label, so there is nothing to reconcile
    /// through.
    #[test]
    fn a_label_that_looks_like_an_id_is_not_that_id() {
        let (_d, s) = store();
        let mut unscanned = Agent::new("claude-unscanned", "claude", at(0));
        unscanned.state = AgentState::Dead;
        unscanned.pane.session = "w2".into(); // a label, not an id
        unscanned.pane.pane_id = "%7".into(); // and a coordinate with no workspace
        let mut live = Agent::new("claude-live", "claude", at(0));
        live.state = AgentState::Waiting;
        live.pane.session = "alpha".into();
        live.pane.workspace_id = "w2".into(); // a different workspace entirely
        for r in [&unscanned, &live] {
            s.save(r).unwrap();
        }

        let got = s.list().unwrap();
        assert_ne!(
            crate::group_key(&got[0]),
            crate::group_key(&got[1]),
            "equal strings from two namespaces are not one workspace"
        );
        assert_eq!(crate::group_key(&live), crate::GroupKey::Id("w2"));
        assert_eq!(crate::group_key(&unscanned), crate::GroupKey::Label("w2"));
    }

    /// A record with no workspace id yet falls back to its label,
    /// and the empty id must not collect unrelated workspaces into one group.
    #[test]
    fn records_without_a_workspace_id_group_by_label() {
        let (_d, s) = store();
        let mut a = Agent::new("claude-a", "claude", at(0));
        a.state = AgentState::Waiting;
        a.pane.session = "alpha".into();
        let mut b = Agent::new("claude-b", "claude", at(0));
        b.state = AgentState::Idle;
        b.pane.session = "beta".into();
        let mut a2 = Agent::new("claude-a2", "claude", at(0));
        a2.state = AgentState::Idle;
        a2.pane.session = "alpha".into();
        for r in [&a, &b, &a2] {
            s.save(r).unwrap();
        }

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["claude-a", "claude-a2", "claude-b"]);
    }

    /// Ranking groups by name would bury the workspace that needs a person.
    #[test]
    fn a_workspace_is_not_ranked_by_its_name() {
        let (_d, s) = store();
        let mut early = Agent::new("claude-1", "claude", at(0));
        early.state = AgentState::Idle;
        early.pane.session = "00.aaa".into();
        let mut late = Agent::new("claude-2", "claude", at(0));
        late.state = AgentState::Waiting;
        late.pane.session = "99.zzz".into();
        for r in [&early, &late] {
            s.save(r).unwrap();
        }

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["claude-2", "claude-1"]);
    }

    /// Two workspaces blocked at the same level: the one blocked longest leads,
    /// which is the row rule lifted to the group.
    #[test]
    fn between_equal_workspaces_the_one_stuck_longest_leads() {
        let (_d, s) = store();
        let mut recent = Agent::new("claude-recent", "claude", at(0));
        recent.state = AgentState::Waiting;
        recent.state_since = at(300);
        recent.pane.session = "aaa".into();
        let mut old = Agent::new("claude-old", "claude", at(0));
        old.state = AgentState::Waiting;
        old.state_since = at(0);
        old.pane.session = "zzz".into();
        for r in [&recent, &old] {
            s.save(r).unwrap();
        }

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["claude-old", "claude-recent"]);
    }

    /// The agent that has been stuck longest goes on top.
    ///
    /// This is the case the test above cannot see: every record there shares
    /// one `state_since`, so the comparison never runs and the `id` tie-break
    /// carries the assertion. A reversed `state_since` order passes it, and
    /// reversed is what shipped — the live listing put the agent that had just
    /// changed above one blocked for five hours, which is the row the list
    /// exists to surface.
    #[test]
    fn within_a_state_the_longest_stuck_comes_first() {
        let (_d, s) = store();
        let mut recent = Agent::new("claude-recent", "claude", at(0));
        recent.state = AgentState::Waiting;
        recent.state_since = at(300); // blocked a minute ago
        let mut old = Agent::new("claude-old", "claude", at(0));
        old.state = AgentState::Waiting;
        old.state_since = at(0); // blocked five hours ago

        for a in [&recent, &old] {
            s.save(a).unwrap();
        }

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            vec!["claude-old", "claude-recent"],
            "the longest-waiting agent must not be buried under a fresh change"
        );
    }

    /// One unreadable record must not blind the whole listing.
    #[test]
    fn list_skips_corrupt_records() {
        let (_d, s) = store();
        s.save(&Agent::new("claude-1", "claude", at(0))).unwrap();
        fs::write(s.dir().join("agents").join("broken.json"), b"{ not json").unwrap();

        let got = s.list().unwrap();
        assert_eq!(
            got.len(),
            1,
            "good record should survive a corrupt neighbour"
        );
    }

    #[test]
    fn mark_read_only_touches_unseen_completions() {
        let (_d, s) = store();
        let mut done = Agent::new("claude-1", "claude", at(0));
        done.state = AgentState::DoneUnread;
        s.save(&done).unwrap();
        s.mark_read("claude-1", at(5)).unwrap();
        assert_eq!(s.load("claude-1").unwrap().state, AgentState::Idle);

        let mut working = Agent::new("claude-2", "claude", at(0));
        working.state = AgentState::Working;
        s.save(&working).unwrap();
        s.mark_read("claude-2", at(5)).unwrap();
        assert_eq!(s.load("claude-2").unwrap().state, AgentState::Working);
    }

    #[test]
    fn delete_is_idempotent() {
        let (_d, s) = store();
        s.save(&Agent::new("claude-1", "claude", at(0))).unwrap();
        s.delete("claude-1").unwrap();
        s.delete("claude-1").unwrap(); // deleting what is not there is not a failure
        assert!(s.load("claude-1").is_err());
    }

    /// state_since only moves on a real change, so "5m in WORKING" survives
    /// repeated heartbeats.
    #[test]
    fn transition_keeps_state_since_meaningful() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.transition(AgentState::Working, at(1));
        let since = a.state_since;
        a.transition(AgentState::Working, at(9));
        assert_eq!(a.state_since, since, "same state must not reset the clock");
        assert_eq!(a.updated_at, at(9), "but the heartbeat still lands");
        a.transition(AgentState::Idle, at(10));
        assert_eq!(a.state_since, at(10));
    }

    #[test]
    fn hook_freshness_expires() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        assert!(
            !a.hook_fresh(at(0), time::Duration::minutes(2)),
            "never hooked"
        );
        a.mark_hook(at(0));
        assert!(a.hook_fresh(at(1), time::Duration::minutes(2)));
        assert!(!a.hook_fresh(at(10), time::Duration::minutes(2)));
    }

    /// `WORK?` says a hook was lost. A kind with no hook cannot lose one, and
    /// a long turn it is really working through must not be flagged as wedged.
    #[test]
    fn only_a_hooked_kind_goes_stale() {
        let later = at(0) + crate::STALE_AFTER + time::Duration::minutes(1);
        for (kind, want) in [("claude", true), ("opencode", false)] {
            let mut a = Agent::new("x", kind, at(0));
            a.state = AgentState::Working;
            a.updated_at = at(0);
            assert_eq!(a.stale(later), want, "{kind}");
        }
        // An opencode record its plugin has spoken for has a hook to lose.
        let mut a = Agent::new("x", "opencode", at(0));
        a.state = AgentState::Working;
        a.mark_hook(at(0));
        a.updated_at = at(0);
        assert!(a.stale(later), "a plugin-fed opencode record goes stale");
    }

    /// Every save that moves the state leaves a journal line; a save that
    /// does not leaves none, and a record's first save counts from IDLE.
    #[test]
    fn a_state_change_is_journaled_whoever_saves_it() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path()).unwrap();
        let mut a = Agent::new("claude-1", "claude", at(0));
        s.save(&a).unwrap();
        let mut born_working = Agent::new("codex-1", "codex", at(0));
        born_working.state = AgentState::Waiting;
        s.save(&born_working).unwrap();
        a.transition(AgentState::Working, at(1));
        s.save(&a).unwrap();
        a.updated_at = at(2);
        s.save(&a).unwrap();
        a.transition(AgentState::DoneUnread, at(3));
        assert!(s.save_if_changed(&a).unwrap());
        let got = crate::journal::read_since(d.path(), at(0));
        let moves: Vec<_> = got.iter().map(|e| (e.from, e.to, e.at)).collect();
        assert_eq!(
            moves,
            [
                (AgentState::Idle, AgentState::Waiting, at(0)),
                (AgentState::Idle, AgentState::Working, at(1)),
                (AgentState::Working, AgentState::DoneUnread, at(3)),
            ]
        );
    }

    #[test]
    fn headline_leads_with_the_title_the_cli_keeps_current() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        assert_eq!(a.headline(), "", "nothing known yet");

        // The floor: whatever the last prompt said, rather than a blank cell.
        a.task = "refactor".into();
        assert_eq!(a.headline(), "refactor");

        // A pending question beats the task: it is the newer fact.
        a.ask = "awaiting approval".into();
        assert_eq!(a.headline(), "awaiting approval");

        // A `/rename` label beats both, because a person typed it about this
        // pane. The state column still says the pane is waiting, so the
        // question is not lost — only the cell that was never about it.
        a.label = "dbeaver".into();
        assert_eq!(a.headline(), "dbeaver");

        // And the terminal title leads, because the CLI rewrites it every turn
        // while the name was fixed at the moment somebody typed it.
        a.title = "2550 pg_stat_activity".into();
        assert_eq!(a.headline(), "2550 pg_stat_activity");
    }

    /// The TASK column's rule: what the pane was asked, never its title or label.
    #[test]
    fn the_assignment_never_repeats_the_title() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.title = "2550 pg_stat_activity".into();
        assert_eq!(a.assignment(), "", "a title alone is not an assignment");
        assert_eq!(a.pane_title(), "2550 pg_stat_activity");

        // The work instruction prompt leads.
        a.task = "refactor".into();
        assert_eq!(a.assignment(), "refactor");
        // A label is the pane title.
        a.label = "dbeaver".into();
        assert_eq!(a.assignment(), "refactor");
        assert_eq!(a.pane_title(), "dbeaver");

        // When prompt is empty, falls back to ask.
        a.task.clear();
        a.ask = "awaiting approval".into();
        assert_eq!(a.assignment(), "awaiting approval");

        // When label is empty, pane_title falls back to terminal title.
        a.label.clear();
        assert_eq!(a.pane_title(), "2550 pg_stat_activity");
    }

    /// Cross-checked: the assignment is strictly the work instruction prompt
    /// or pending question, never the label or machine tags.
    #[test]
    fn assignment_is_strictly_task_or_ask() {
        for label in ["", "dbeaver", "worker-1a2b3c4d", " "] {
            for ask in ["", "awaiting approval"] {
                for task in ["", "refactor", "<task-notification>"] {
                    let mut a = Agent::new("claude-1", "claude", at(0));
                    a.label = label.into();
                    a.ask = ask.into();
                    a.task = task.into();

                    let expected = if task == "refactor" {
                        "refactor"
                    } else if !ask.is_empty() {
                        "awaiting approval"
                    } else {
                        ""
                    };
                    assert_eq!(a.assignment(), expected, "case {label:?} {ask:?} {task:?}");
                }
            }
        }
        // Machine tags and task IDs are ignored.
        let mut h = Agent::new("claude-1", "claude", at(0));
        h.task = "<task-notification>".into();
        assert_eq!(h.assignment(), "");
        h.task = "a77b79fc0975db12e".into();
        assert_eq!(h.assignment(), "");
    }

    #[test]
    fn pane_id_resolves_from_coordinate_or_agent_id() {
        let mut a = Agent::new("claude-w1:p2", "claude", at(0));
        assert_eq!(a.pane_id(), "w1:p2");

        a.pane.pane_id = "w2:p4S".into();
        assert_eq!(a.pane_id(), "w2:p4S");

        let b = Agent::new("claude-7", "claude", at(0));
        assert_eq!(b.pane_id(), "");

        // Only a herdr coordinate (`w2:p4`) is read out of an ID.
        let c = Agent::new("claude-%3", "claude", at(0));
        assert_eq!(c.pane_id(), "");
    }

    /// A worker's generated name is not a name somebody chose: the TITLE cell
    /// shows what the CLI says the worker is doing, and keeps the generated
    /// name only for a pane with no title yet.
    #[test]
    fn the_pane_title_puts_a_generated_worker_name_behind_the_terminal_title() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.label = "auth-api-3f2a91cc".into();
        a.title = "Wiring the token refresh".into();
        assert_eq!(a.pane_title(), "Wiring the token refresh");

        a.title.clear();
        assert_eq!(a.pane_title(), "auth-api-3f2a91cc");

        // A name a person typed still leads the title.
        a.label = "auth-api".into();
        a.title = "Wiring the token refresh".into();
        assert_eq!(a.pane_title(), "auth-api");
    }

    // ── g4testadd: the PANE and TITLE cells ─────────────────────────────

    /// The PANE cell reads a coordinate out of the record ID only when there
    /// is a colon in it. A leading `%` used to count too; nothing hands out
    /// those IDs any more, so `claude-%3` is no coordinate. A blank stored
    /// coordinate is no coordinate either, and a stored one is shown trimmed.
    /// On a named backend the coordinate follows the backend ID, and only the
    /// coordinate is shown (#35).
    #[test]
    fn pane_id_reads_a_coordinate_out_of_an_id_only_when_it_has_a_colon() {
        for (id, stored, want) in [
            ("claude-w1:p2", "", "w1:p2"),
            ("claude-b0123456789abcdef-w2:p1", "", "w2:p1"),
            ("codex-b0123456789abcdef-7", "", ""),
            ("claude-b0123456789abcdef-w2:p1", "w9:p9", "w9:p9"),
            ("claude-%3", "", ""),
            ("claude-%12", "", ""),
            ("claude-7", "", ""),
            ("claude-", "", ""),
            ("claude", "", ""),
            ("", "", ""),
            ("claude-w1:p2", "   ", "w1:p2"),
            ("claude-7", " w2:p4S\n", "w2:p4S"),
            ("codex-w2:p가", "", "w2:p가"),
        ] {
            let mut a = Agent::new(id, "claude", at(0));
            a.pane.pane_id = stored.into();
            assert_eq!(a.pane_id(), want, "id {id:?}, stored {stored:?}");
        }
    }

    /// Property: with no coordinate stored, the PANE cell is the coordinate
    /// part of the ID when that part holds a colon, and empty otherwise —
    /// whatever else the part holds, a leading `%` included. Generated over
    /// default-backend IDs (`{kind}-{pane}`) and named-backend ones
    /// (`{kind}-{backend}-{pane}`), with coordinates that have no hyphen of
    /// their own, as herdr's never do.
    #[test]
    fn pane_id_is_the_coordinate_part_of_the_id_exactly_when_it_has_a_colon() {
        let alphabet = ['w', 'p', '1', '2', 'S', ':', '%', ' ', '가'];
        let mut rng = tower_testrng::Rng::new(23);
        for _ in 0..400 {
            let kind = ["claude", "codex", "gemini"][rng.below(3)];
            let part: String = (0..rng.below(7))
                .map(|_| alphabet[rng.below(alphabet.len())])
                .collect();
            let id = if rng.below(2) == 0 {
                format!("{kind}-{part}")
            } else {
                // Two draws, since one is 31 bits: a backend ID's full sixteen
                // hex digits, not one that always starts `b0000000`.
                let backend = ((rng.draw()) << 32) | rng.draw();
                format!("{kind}-b{backend:016x}-{part}")
            };
            let a = Agent::new(id, kind, at(0));
            let want = if part.contains(':') { part.trim() } else { "" };
            assert_eq!(a.pane_id(), want, "{:?}", a.id);
        }
    }

    /// Cross validation: the TITLE cell against its rule written out plainly,
    /// over every combination of these names and titles — a name a person
    /// chose leads, then the terminal title, then a generated worker name, and
    /// blank counts as nothing throughout.
    #[test]
    fn the_pane_title_agrees_with_its_rule_over_every_combination() {
        // (label, whether `wt new` generated it)
        let labels = [
            ("", false),
            (" ", false),
            ("dbeaver", false),
            ("<frontend>", false),
            ("리뷰", false),
            ("auth-api-3f2a91cc", true),
        ];
        for (label, generated) in labels {
            for title in ["", "\t", "Wiring the token refresh", "🗼 tower"] {
                for task in ["", "refactor"] {
                    let mut a = Agent::new("claude-1", "claude", at(0));
                    a.label = label.into();
                    a.title = title.into();
                    a.task = task.into();
                    let chosen = !generated && !label.trim().is_empty();
                    let want = if chosen {
                        label
                    } else if !title.trim().is_empty() {
                        title
                    } else if !label.trim().is_empty() {
                        label
                    } else {
                        ""
                    };
                    assert_eq!(
                        a.pane_title(),
                        want,
                        "{label:?} {title:?} {task:?}, never the task"
                    );
                }
            }
        }
    }

    /// A Claude subagent's task ID is a machine string, not something a person
    /// asked for, so every surface that shows the task passes over it — the
    /// row, the notification and the TASK cell alike — and falls through to
    /// what comes next. One character off and it is an ordinary task again.
    #[test]
    fn a_stored_task_id_is_passed_over_everywhere_the_task_is_shown() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.task = "a77b79fc0975db12e".into();
        assert_eq!(a.headline(), "");
        assert_eq!(a.attention(), "");
        assert_eq!(a.assignment(), "");

        a.title = "2550 pg_stat_activity".into();
        assert_eq!(a.headline(), "2550 pg_stat_activity");
        assert_eq!(a.attention(), "2550 pg_stat_activity");
        a.ask = "awaiting approval".into();
        assert_eq!(a.assignment(), "awaiting approval");

        for near in [
            "a77b79fc0975db12",
            "a77b79fc0975db12e0",
            "A77B79FC0975DB12E",
            "a77b79fc0975db12g",
        ] {
            let mut b = Agent::new("claude-2", "claude", at(0));
            b.task = near.into();
            assert_eq!(b.assignment(), near, "{near} is not a task ID");
            assert_eq!(b.headline(), near);
        }
    }

    /// Only DEAD is not live: every other state is a session somebody can
    /// still look at or talk to, and the list and broadcasts rely on that.
    #[test]
    fn every_state_but_dead_is_live() {
        for (state, live) in [
            (AgentState::Working, true),
            (AgentState::Waiting, true),
            (AgentState::DoneUnread, true),
            (AgentState::Idle, true),
            (AgentState::Error, true),
            (AgentState::Dead, false),
        ] {
            let mut a = Agent::new("claude-1", "claude", at(0));
            a.transition(state, at(1));
            assert_eq!(a.is_live(), live, "{state:?}");
        }
    }

    /// The tab label goes to disk and comes back, non-Latin and all, and a
    /// record with none leaves no `tab` key in its file.
    #[test]
    fn a_tab_label_is_saved_and_loaded_and_an_empty_one_leaves_no_key() {
        let (_d, s) = store();
        let mut a = Agent::new("claude-w2:p1", "claude", at(0));
        a.pane.pane_id = "w2:p1".into();
        a.pane.tab = "pr-2752 리뷰 🗼".into();
        s.save(&a).unwrap();
        assert_eq!(s.load("claude-w2:p1").unwrap().pane.tab, "pr-2752 리뷰 🗼");

        a.pane.tab.clear();
        assert!(s.save_if_changed(&a).unwrap(), "clearing it is a change");
        assert_eq!(s.load("claude-w2:p1").unwrap().pane.tab, "");
        let body = std::fs::read_to_string(s.path("claude-w2:p1")).unwrap();
        assert!(!body.contains("\"tab\""), "{body}");
    }

    /// A hook that gives up on the lock still waited for it, and that wait is
    /// the one worth knowing about: it is what sends a hook on without the
    /// lock. A lower bound only, and half the patience rather than all of it:
    /// the claim is that giving up is counted, not how the tries are spaced —
    /// dropping the sleep after the last try must not fail this.
    #[test]
    fn a_hook_that_gives_up_on_the_lock_still_has_its_wait_counted() {
        let (_d, s) = store();
        let theirs = Store::new(s.dir()).unwrap();
        let held = theirs.lock().expect("the lock file has to be creatable");

        assert!(s.try_lock_briefly().is_none(), "somebody else holds it");
        let ours = s.take_lock_times();
        assert!(ours.waited >= HOOK_PATIENCE / 2, "{ours:?}");
        assert_eq!(ours.held, Duration::ZERO, "it never held the lock");
        drop(held);
    }

    /// 🚨 The rests are what a hook pays, and it must not pay for one that buys
    /// nothing. Fifteen tries need fourteen rests: the last try has no try after
    /// it, and the sleep that used to follow it added 10 ms to every give-up in
    /// the agent's own process.
    #[test]
    fn a_hook_rests_between_tries_and_not_after_the_last_one() {
        let (_d, s) = store();
        let theirs = Store::new(s.dir()).unwrap();
        let held = theirs.lock().expect("the lock file has to be creatable");

        let mut rests = Vec::new();
        assert!(
            s.try_lock_briefly_resting(|nap| rests.push(nap)).is_none(),
            "somebody else holds it"
        );
        assert_eq!(rests.len() as u64, HOOK_TRIES - 1);
        assert!(
            rests.iter().sum::<Duration>() < HOOK_PATIENCE,
            "the rests alone must stay inside a hook's patience: {rests:?}"
        );
        drop(held);

        // A free lock is taken on the first try and rests not at all.
        let mut rests = Vec::new();
        let ours = s.try_lock_briefly_resting(|nap| rests.push(nap));
        assert!(ours.is_some(), "nobody holds it");
        assert!(rests.is_empty(), "{rests:?}");
    }

    /// The policy, driven by hand: what each answer costs in tries and rests.
    ///
    /// 🚨 The `Error` row is the one a real filesystem cannot be asked for in a
    /// test — `flock` works here — and it is the whole of #43: every hook paid a
    /// full 150 ms of patience for an answer that never changes.
    #[test]
    fn a_hooks_patience_is_spent_only_where_another_try_could_answer() {
        let would_block = || Err(std::fs::TryLockError::WouldBlock);
        let unsupported = || {
            Err(std::fs::TryLockError::Error(std::io::Error::other(
                "no flock",
            )))
        };

        // Tries as well as rests: the count of tries is the patience contract
        // (`HOOK_PATIENCE` is `HOOK_TRY_EVERY_MS * HOOK_TRIES`), and a loop that
        // quietly took one try fewer would leave every rest count intact.
        for (name, answers, took, tries, rests) in [
            ("free at once", vec![Ok(())], true, 1, 0),
            (
                "busy, then free",
                vec![would_block(), would_block(), Ok(())],
                true,
                3,
                2,
            ),
            (
                "busy throughout",
                (0..HOOK_TRIES).map(|_| would_block()).collect(),
                false,
                HOOK_TRIES,
                HOOK_TRIES - 1,
            ),
            ("unsupported", vec![unsupported()], false, 1, 0),
            (
                "busy, then unsupported",
                vec![would_block(), unsupported()],
                false,
                2,
                1,
            ),
        ] {
            let mut answers = answers.into_iter();
            let mut tried = 0;
            let mut rested = 0;
            let got = briefly(
                || {
                    tried += 1;
                    answers.next().expect("one answer per try")
                },
                |_| rested += 1,
            );
            assert_eq!(got, took, "{name}");
            assert_eq!(tried, tries, "{name}: tries");
            assert_eq!(rested, rests, "{name}: rests");
        }
    }

    /// Only a lock that *would block* is worth trying again. `flock` failing
    /// because the filesystem does not support it answers the same way in 10 ms
    /// and in 150, and a hook that retried it spent its whole patience before
    /// writing unlocked anyway.
    #[test]
    fn a_flock_error_is_not_worth_another_try() {
        assert!(worth_another_try(&std::fs::TryLockError::WouldBlock));
        assert!(!worth_another_try(&std::fs::TryLockError::Error(
            std::io::Error::from_raw_os_error(libc::ENOTSUP)
        )));
        assert!(!worth_another_try(&std::fs::TryLockError::Error(
            std::io::Error::other("flock is not supported here")
        )));
    }

    #[test]
    fn short_session_id_keeps_the_head_of_a_random_uuid() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.session_id = "c376295e-95bf-45d2-9cd0-e26b0e837218".into();
        assert_eq!(a.short_session_id(), "c376295e");

        a.session_id = "  c376295e-95bf-45d2-9cd0-e26b0e837218\n".into();
        assert_eq!(a.short_session_id(), "c376295e", "surrounding blanks");

        a.session_id = "short".into();
        assert_eq!(a.short_session_id(), "short");

        a.session_id = "".into();
        assert_eq!(a.short_session_id(), "");

        // Cut on a character boundary, not a byte one.
        a.session_id = "세션아이디테스트12345".into();
        assert_eq!(a.short_session_id(), "세션아이디테스트");

        // v1's and v6's tails are the node field — possibly the host's MAC,
        // the same on every session — so they keep the head like any other.
        a.session_id = "1f0a3b2c-4d5e-6f70-8a1b-001122334455".into();
        assert_eq!(a.short_session_id(), "1f0a3b2c");
        a.session_id = "c232ab00-9414-11ec-b3c8-001122334455".into();
        assert_eq!(a.short_session_id(), "c232ab00");
    }

    /// 🚨 codex's session IDs are UUIDv7: the head is a millisecond timestamp
    /// that only changes every 65 seconds, so three workers spawned together
    /// all showed `01a0b8e0`. The random tail tells them apart.
    #[test]
    fn short_session_id_keeps_the_tail_of_a_uuid_v7() {
        let mut a = Agent::new("codex-1", "codex", at(0));
        let spawned_together = [
            "01a0b8e0-9917-7af1-b920-ab518ce5d454",
            "01a0b8e0-99a2-7c03-8d11-4e0f97a2c1b6",
            "01a0b8e0-9a0c-7e58-a4f2-02d6b3e9f870",
        ];
        let shown: std::collections::HashSet<String> = spawned_together
            .iter()
            .map(|sid| {
                a.session_id = (*sid).into();
                a.short_session_id().to_string()
            })
            .collect();
        assert_eq!(shown.len(), spawned_together.len(), "{shown:?}");

        a.session_id = spawned_together[0].into();
        assert_eq!(a.short_session_id(), "8ce5d454");
    }

    /// 🚨 A blank name is no name. A `/rename` to a space took the line, drew
    /// as an empty cell, and hid the question and the task behind it.
    #[test]
    fn a_blank_name_hides_nothing() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.label = " ".into();
        a.task = "refactor".into();
        assert_eq!(a.assignment(), "refactor");
        assert_eq!(a.headline(), "refactor");
        a.title = "\t".into();
        assert_eq!(a.headline(), "refactor", "nor is a blank title");
    }

    /// A record written before the hook learned to decline a harness wake-up
    /// still has one on disk, so the display declines it too and falls through.
    #[test]
    fn a_stored_harness_wakeup_is_not_shown() {
        let mut a = Agent::new("claude-1", "claude", at(0));

        // Every spelling the hook declines, so a record from before that rule
        // cannot show one the hook would have refused to write. Nothing else is
        // recorded here, so the cell is empty rather than falling through to
        // something that would hide the filter.
        for wake in [
            "<task-notification>",
            "<task-notification />",
            "<task-notification/>",
            "<environment_context source=\"harness\">",
            "</instructions>",
            "<metadata></metadata>",
            "<metadata>   </metadata>",
        ] {
            a.task = wake.into();
            assert_eq!(a.headline(), "", "{wake}");
        }

        // Narrow, the same as at the writing end.
        a.task = "<task-notification> and then run the tests".into();
        assert_eq!(a.headline(), "<task-notification> and then run the tests");
        a.task = "explain Vec<String> to me".into();
        assert_eq!(a.headline(), "explain Vec<String> to me");

        // And a declined task falls through to the rest of the row.
        a.task = "<task-notification>".into();
        a.title = "2550 pg_stat_activity".into();
        assert_eq!(a.headline(), "2550 pg_stat_activity");
    }

    /// A notification has no state column beside it and no detail card one key
    /// away, so the label goes last there: the title already says which session
    /// and which state, and the body is the only room left to say what needs
    /// doing.
    #[test]
    fn a_notification_says_what_needs_doing_not_what_the_pane_is_called() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.label = "dbeaver".into();
        a.title = "2546".into();
        a.task = "refactor".into();
        a.ask = "awaiting approval".into();

        assert_eq!(a.headline(), "2546", "the row leads with the title");
        assert_eq!(
            a.attention(),
            "awaiting approval",
            "the notification does not"
        );

        a.ask.clear();
        assert_eq!(a.attention(), "refactor");
        a.task.clear();
        assert_eq!(a.attention(), "2546");
        // With nothing else to say, the label is better than the folder path.
        a.title.clear();
        assert_eq!(a.attention(), "dbeaver");
    }

    /// A person is free to name a pane `<frontend>`, and only `task` can carry a
    /// harness wake-up — it is written from the submitted prompt. Filtering
    /// every candidate dropped a label somebody deliberately typed.
    #[test]
    fn a_tag_shaped_name_is_still_a_name() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.label = "<frontend>".into();
        a.task = "refactor".into();
        assert_eq!(a.headline(), "<frontend>");

        // The same on the notification side, where the label is last.
        let mut b = Agent::new("claude-2", "claude", at(0));
        b.label = "<frontend>".into();
        assert_eq!(b.attention(), "<frontend>");

        // A terminal title is the agent's own, and not a prompt either.
        let mut c = Agent::new("claude-3", "claude", at(0));
        c.title = "<build>".into();
        assert_eq!(c.headline(), "<build>");

        // Only the task is filtered, and only when it is the whole string.
        let mut d = Agent::new("claude-4", "claude", at(0));
        d.title = "2546".into();
        d.task = "<task-notification>".into();
        assert_eq!(d.headline(), "2546");
    }

    /// Not every label is a `/rename`. `wt new` names each worker pane after its
    /// task — normalized, truncated, digest-suffixed — and herdr returns that as
    /// the label like any other, so leading with it put `auth-api-3f2a91cc` on
    /// every worker row in place of the task.
    #[test]
    fn a_generated_worker_name_sits_behind_the_task() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.label = "auth-api-3f2a91cc".into();
        a.task = "wire up the token refresh".into();
        assert_eq!(a.headline(), "wire up the token refresh");

        // Demoted, not discarded: better than an empty cell.
        a.task.clear();
        assert_eq!(a.headline(), "auth-api-3f2a91cc");

        // A name a person typed still leads, digits and hyphens and all.
        let mut b = Agent::new("claude-2", "claude", at(0));
        b.task = "wire up the token refresh".into();
        for name in [
            "dbeaver",
            "auth-api",
            "pr-2550",
            "api-3f2a91c",
            "api-3f2a91ccd",
        ] {
            b.label = name.into();
            assert_eq!(b.headline(), name, "{name} is not a generated name");
        }
    }

    /// Un-naming a pane has to fall back through the same order, not strand the
    /// label as the last thing anyone saw — and a title covers the name the same
    /// way, without erasing it.
    #[test]
    fn clearing_the_label_uncovers_what_was_under_it() {
        let mut a = Agent::new("claude-1", "claude", at(0));
        a.task = "refactor".into();
        a.label = "dbeaver".into();
        assert_eq!(a.headline(), "dbeaver");

        a.title = "2550 pg_stat_activity".into();
        assert_eq!(a.headline(), "2550 pg_stat_activity");
        a.title.clear();
        assert_eq!(a.headline(), "dbeaver", "covered, not lost");

        a.label.clear();
        assert_eq!(a.headline(), "refactor");
        a.task.clear();
        assert_eq!(a.headline(), "");
    }

    fn completed(store: &Store, id: &str, work_seq: u64) {
        let mut a = Agent::new(id, "claude", at(0));
        a.state = AgentState::DoneUnread;
        a.work_seq = work_seq;
        store.save(&a).unwrap();
    }

    /// The write a dispatch leaves behind: the completion it answers is read,
    /// and `done_acked` is cleared so the *next* completion arrives unread.
    #[test]
    fn acknowledging_a_dispatch_reads_the_completion_and_clears_the_flag() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path()).unwrap();
        completed(&store, "hero", 4);

        assert!(store.acknowledge_dispatch("hero", 4, at(1)).unwrap());
        let a = store.load("hero").unwrap();
        assert_eq!(a.state, AgentState::Idle);
        assert!(!a.done_acked, "the next completion must arrive unread");
        assert_eq!(a.work_seq, 4, "a dispatch is not a turn");
    }

    /// Edges: a moved count writes nothing, and an agent that is not there is
    /// an error — never a silent `Ok(false)` a caller could read as "moved on".
    #[test]
    fn acknowledging_refuses_a_moved_turn_and_reports_a_missing_agent() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path()).unwrap();
        completed(&store, "hero", 5);

        for stale in [0, 4, 6, u64::MAX] {
            assert!(
                !store.acknowledge_dispatch("hero", stale, at(1)).unwrap(),
                "read {stale}, is 5"
            );
        }
        let untouched = store.load("hero").unwrap();
        assert_eq!(untouched.state, AgentState::DoneUnread, "nothing written");

        assert!(store.acknowledge_dispatch("nobody", 0, at(1)).is_err());
    }

    /// Property: acknowledging twice is acknowledging once. A retried dispatch
    /// must not wander the record anywhere a single one would not.
    #[test]
    fn acknowledging_twice_is_acknowledging_once() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path()).unwrap();
        completed(&store, "hero", 4);

        assert!(store.acknowledge_dispatch("hero", 4, at(1)).unwrap());
        let once = store.load("hero").unwrap();
        assert!(store.acknowledge_dispatch("hero", 4, at(1)).unwrap());
        let twice = store.load("hero").unwrap();
        assert_eq!(
            (twice.state, twice.done_acked, twice.work_seq),
            (once.state, once.done_acked, once.work_seq)
        );
    }

    /// Cross-checked against `mark_read`, the other way a completion leaves
    /// `DoneUnread`. Both land on `Idle`; they must differ in exactly one bit,
    /// because that bit is how a drive tells new work from somebody opening the
    /// row — `tower_wt::Worker` reads it.
    #[test]
    fn a_dispatch_and_a_read_differ_only_in_whether_the_completion_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path()).unwrap();
        completed(&store, "dispatched", 4);
        completed(&store, "read", 4);

        store.acknowledge_dispatch("dispatched", 4, at(1)).unwrap();
        store.mark_read("read", at(1)).unwrap();
        let dispatched = store.load("dispatched").unwrap();
        let read = store.load("read").unwrap();

        assert_eq!(dispatched.state, read.state, "both settle to Idle");
        assert_eq!(dispatched.work_seq, read.work_seq);
        assert!(read.done_acked, "a read keeps the completion");
        assert!(!dispatched.done_acked, "a dispatch supersedes it");
    }
}
