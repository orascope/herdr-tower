// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Everything that arrives on its own clock.
//!
//! Each periodic job is a thread with its own loop, not a timer that re-arms
//! itself by returning the next one. That re-arming is where the bugs are: a
//! self-re-arming timer forks into two chains the moment the interval changes.
//! Opening a backend watch slows the tick from two seconds to ten, and a
//! two-second timer already in flight would fire afterwards and start a second
//! chain of its own. Two chains, both polling, forever — and the only fix is a
//! generation number every branch has to remember to bump.
//!
//! A thread that loops on `recv_timeout` cannot fork. It has one interval, it
//! reads it fresh each time round, and there is no second timer to leak. So
//! there is no generation counter and no class of bug for it to guard; what is
//! kept is the property it would protect, which is that opening a watch makes
//! the reader wait longer rather than making it wait twice.
//!
//! The threads:
//!
//!   - **records** — re-reads the store and the resident supervisor's atomic
//!     snapshot. It never talks to Herdr; the supervisor is the sole owner of
//!     that connection and keeps the shared store current.
//!   - **local** — the readings that come from reading files: context share,
//!     the Discord marks, the pinned default models.
//!   - **preview** — captures the selected pane.
//!   - **host** — reads the machine's processor and memory for the header.
//!   - **dock** — keeps the dock workspace at the top of the sidebar, and runs
//!     only in a tower that has a pane of its own.
//!
//! Keyboard input is deliberately *not* a thread here. See [`crate::run`].

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use time::OffsetDateTime;
use tower_state::{Agent, DaemonStore, Store};
use tower_usage::DefaultModels;

use crate::app::{Event, HostLoad, LocalInfo, Settings};
use crate::info::wired_sessions;
use crate::tower::Tower;

/// How long the popup waits before re-reading the supervisor-owned snapshot.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The local readings.
pub const LOCAL_INTERVAL: Duration = Duration::from_secs(15);
/// Agents on saved machines: one ssh round trip per machine, twice.
pub const REMOTE_INTERVAL: Duration = Duration::from_secs(10);
/// The machine's load in the header: as often as btop redraws its own, and a
/// reading is a couple of system calls.
pub const HOST_INTERVAL: Duration = Duration::from_secs(2);
/// The open-PR lookup, slowest of all: one `gh` round trip to GitHub per
/// folder, for a number that changes when a PR is opened or merged.
pub const PR_SCAN_INTERVAL: Duration = Duration::from_secs(60);

/// State the threads read instead of being told.
///
/// The preview request and the agent list are *latest-value* facts, not a
/// stream of messages: a capture of the pane the cursor left is worthless, and
/// so is the agent list from two reads ago. Same reasoning as the watch's
/// capacity-one channel — queueing them would only build a backlog of work
/// nobody wants the answer to any more.
#[derive(Default)]
struct Shared {
    agents: Mutex<Vec<Agent>>,
    /// Which pane to capture, and how many rows of it.
    preview: Mutex<Option<(String, u16)>>,
}

/// The running producers.
pub struct Feed {
    rx: Receiver<Event>,
    /// A spare sender, for work the driver starts on a thread of its own.
    tx: Sender<Event>,
    /// Nudges the reader thread to read now.
    records_kick: Sender<()>,
    /// Nudges the preview thread after the selection changes.
    preview_kick: Sender<()>,
    /// Nudges the local readings when the user presses `r`.
    local_kick: Sender<()>,
    /// Never sent on. Held so that dropping it in [`Feed::stop`] ends the dock
    /// thread.
    dock_kick: Sender<()>,
    /// Nudges the usage board's thread; `true` asks its sources again even
    /// when their figures are fresh.
    usage_kick: Option<Sender<bool>>,
    shared: Arc<Shared>,
}

/// How often the usage board is read again when nobody asks. The board keeps
/// its own cache, and goes to the network only when the figures are older
/// than it allows (five minutes); this is how soon a new figure shows.
pub const USAGE_INTERVAL: Duration = Duration::from_secs(15);

impl Feed {
    /// Starts every producer.
    ///
    pub fn spawn(store: Store, tower: Arc<dyn Tower>, settings: Settings) -> Feed {
        let (tx, rx) = mpsc::channel::<Event>();
        let shared = Arc::new(Shared::default());

        let (records_kick, records_rx) = mpsc::channel::<()>();
        spawn_named(
            "tower_ui-records",
            records_thread(store.clone(), tx.clone(), shared.clone(), records_rx),
        );

        let (local_kick, local_rx) = mpsc::channel::<()>();
        spawn_named(
            "tower_ui-local",
            local_thread(settings.clone(), tx.clone(), shared.clone(), local_rx),
        );

        let (preview_kick, preview_rx) = mpsc::channel::<()>();
        spawn_named(
            "tower_ui-preview",
            preview_thread(
                tower.clone(),
                tx.clone(),
                shared.clone(),
                settings.preview_interval.max(Duration::from_millis(200)),
                preview_rx,
            ),
        );

        let (dock_kick, dock_rx) = mpsc::channel::<()>();
        if tower.self_pane().is_some() {
            spawn_named("tower_ui-dock", dock_thread(tower.clone(), dock_rx));
        }

        spawn_named("tower_ui-remote", remote_thread(tower.clone(), tx.clone()));

        spawn_named("tower_ui-host", host_thread(tx.clone()));

        spawn_named("tower_ui-prs", pr_thread(tower, tx.clone(), shared.clone()));

        Feed {
            rx,
            tx,
            records_kick,
            preview_kick,
            local_kick,
            dock_kick,
            usage_kick: None,
            shared,
        }
    }

    /// Starts reading the usage board, every [`USAGE_INTERVAL`] and when
    /// asked; each read is followed by [`Event::Usage`].
    pub fn attach_usage(&mut self, usage: Arc<dyn crate::usage::Usage>) {
        let (kick, rx) = mpsc::channel::<bool>();
        let tx = self.tx.clone();
        spawn_named("tower_ui-usage", move || loop {
            let force = match rx.recv_timeout(USAGE_INTERVAL) {
                Ok(force) => force,
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            usage.refresh(force);
            if tx.send(Event::Usage).is_err() {
                return;
            }
        });
        // The first reading at once, from the cache when it is fresh.
        let _ = kick.send(false);
        self.usage_kick = Some(kick);
    }

    /// Asks the usage board's sources again now.
    pub fn refresh_usage(&self) {
        if let Some(k) = &self.usage_kick {
            let _ = k.send(true);
        }
    }

    /// A sender for work the driver runs off the loop, so its result comes back
    /// as an [`Event`] like everything else.
    pub fn notifier(&self) -> Sender<Event> {
        self.tx.clone()
    }

    /// Anything already waiting, without blocking.
    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Re-read the store, and the local readings, now.
    pub fn refresh(&self) {
        let _ = self.records_kick.send(());
        let _ = self.local_kick.send(());
    }

    /// Tells the preview thread what to capture next.
    pub fn set_preview(&self, req: Option<(String, u16)>) {
        let changed = {
            let mut slot = self
                .shared
                .preview
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let changed = *slot != req;
            *slot = req;
            changed
        };
        if changed {
            // A capture of the pane the cursor just left is worthless, so a new
            // selection is worth waking the thread for rather than waiting out
            // its interval.
            let _ = self.preview_kick.send(());
        }
    }

    /// Asks the interval threads to stop. They are deliberately **not** joined:
    /// one of them can be inside a slow capture or sweep, and joining it would
    /// leave the user staring at a restored terminal after they pressed `q`.
    /// They own no resource that outlives the process, and dropping the nudge
    /// channels tells the ones that are merely waiting to stop.
    pub fn stop(&mut self) {
        let (dead_tx, _) = mpsc::channel();
        self.records_kick = dead_tx.clone();
        self.preview_kick = dead_tx.clone();
        self.local_kick = dead_tx.clone();
        self.dock_kick = dead_tx;
        self.usage_kick = None;
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_named<F>(name: &str, f: F) -> JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .expect("a control tower that cannot start its own threads has nothing to show")
}

/// Waits for a nudge or for the interval to pass. `false` means the other end
/// is gone and the thread should stop.
fn wait(rx: &Receiver<()>, every: Duration) -> bool {
    match rx.recv_timeout(every) {
        Ok(()) => true,
        Err(RecvTimeoutError::Timeout) => true,
        Err(RecvTimeoutError::Disconnected) => false,
    }
}

fn records_thread(
    store: Store,
    tx: Sender<Event>,
    shared: Arc<Shared>,
    kick: Receiver<()>,
) -> impl FnOnce() + Send + 'static {
    // Whether the last pass reported a storage failure. An error line that
    // cannot clear itself is its own kind of lie: a one-off full disk or a
    // permissions glitch would otherwise leave "state store … failed" on screen for
    // as long as the tower ran, over records that were updating perfectly well.
    let mut failing = false;
    let daemon_store = DaemonStore::new(store.dir());
    // A transient heartbeat failure must not reshuffle a correct tree back into
    // urgency order. Keep the last layout until the supervisor supplies a newer one.
    let mut tab_roots = HashSet::new();
    let mut parents = std::collections::BTreeMap::new();
    move || loop {
        let now = OffsetDateTime::now_utc();
        let mut failed = false;
        let daemon = match daemon_store.read() {
            Ok(Some(snapshot)) if snapshot.fresh(now) => {
                tab_roots = snapshot.tab_roots.iter().cloned().collect();
                parents = snapshot.parents.clone();
                if !snapshot.connected || !snapshot.error.is_empty() {
                    failed = true;
                    let detail = if snapshot.error.is_empty() {
                        "Herdr is disconnected".to_string()
                    } else {
                        snapshot.error.clone()
                    };
                    if tx.send(Event::Health(Err(detail))).is_err() {
                        return;
                    }
                }
                Some(snapshot)
            }
            Ok(Some(snapshot)) => {
                failed = true;
                if tx
                    .send(Event::Health(Err(
                        "resident supervisor heartbeat is stale".into()
                    )))
                    .is_err()
                {
                    return;
                }
                Some(snapshot)
            }
            Ok(None) => {
                failed = true;
                if tx
                    .send(Event::Health(Err(
                        "resident supervisor is not reporting".into()
                    )))
                    .is_err()
                {
                    return;
                }
                None
            }
            Err(e) => {
                failed = true;
                if tx
                    .send(Event::Health(Err(format!(
                        "resident supervisor snapshot failed: {e}"
                    ))))
                    .is_err()
                {
                    return;
                }
                None
            }
        };
        let agents = match store.list() {
            Ok(a) => a,
            Err(e) => {
                // No `Records` event: the screen keeps the rows it has, with the
                // failure on the error line, rather than emptying itself.
                failing = true;
                if tx
                    .send(Event::Health(Err(format!("state store read failed: {e}"))))
                    .is_err()
                {
                    return;
                }
                if !wait(&kick, POLL_INTERVAL) {
                    return;
                }
                continue;
            }
        };
        // Recovered. Sent only on the pass that recovers — a health event every
        // two seconds would be its own noise — and on its own channel, so taking
        // this line down cannot take down a failure the person is reading.
        if failing && !failed && tx.send(Event::Health(Ok(()))).is_err() {
            return;
        }
        failing = failed;
        *shared.agents.lock().unwrap_or_else(|e| e.into_inner()) = handed_on(&agents);
        let sent = tx.send(Event::Records {
            agents,
            branches: worktree_branches(),
            tab_roots: tab_roots.clone(),
            parents: parents.clone(),
            daemon,
            now,
        });
        if sent.is_err() {
            return;
        }
        if !wait(&kick, POLL_INTERVAL) {
            return;
        }
    }
}

/// Puts the dock back the way a click expects to find it, whenever something
/// has changed it.
fn dock_thread(tower: Arc<dyn Tower>, kick: Receiver<()>) -> impl FnOnce() + Send + 'static {
    move || loop {
        tower.hold_dock();
        if !wait(&kick, crate::dock::DOCK_PIN_INTERVAL) {
            return;
        }
    }
}

/// The local readings, on their own clock: file reads that take
/// milliseconds, kept off the records thread so neither waits on the other.
fn local_thread(
    settings: Settings,
    tx: Sender<Event>,
    shared: Arc<Shared>,
    kick: Receiver<()>,
) -> impl FnOnce() + Send + 'static {
    // One for the thread's life: what each codex rollout said last time, so
    // the next pass reads only what has changed since.
    let mut codex = tower_usage::CtxCache::default();
    move || loop {
        let agents = shared
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if tx
            .send(Event::Local(collect_local(&agents, &settings, &mut codex)))
            .is_err()
        {
            return;
        }
        if !wait(&kick, LOCAL_INTERVAL) {
            return;
        }
    }
}

/// The records the background readers are handed: the live ones.
///
/// The list never shows a dead agent, so nothing they work out for one is ever
/// drawn — and a dead codex agent's rollout is exactly the large, finished file
/// that cost the most to read, for nothing, every pass. Filtered here, once,
/// rather than in each reader.
fn handed_on(agents: &[Agent]) -> Vec<Agent> {
    agents.iter().filter(|a| a.is_live()).cloned().collect()
}

/// The readings that come from reading files: context share, the Discord
/// marks, the pinned default models.
///
/// Collected together and off the redraw path because none of it is fast enough
/// to do per frame and all of it is fast enough to do together.
pub fn collect_local(
    agents: &[Agent],
    settings: &Settings,
    codex: &mut tower_usage::CtxCache,
) -> LocalInfo {
    let home = settings.home.clone().unwrap_or_else(|| ".".into());
    let ctx = tower_usage::agent_ctx_cached(
        agents,
        &settings.resume.codex_root,
        &settings.resume.gemini_dir,
        &settings.resume.claude_dir,
        codex,
    );
    LocalInfo {
        ctx,
        wired: wired_sessions(agents, &settings.wiring, &settings.channel_labels),
        def_models: Some(DefaultModels::read(&home)),
    }
}

fn preview_thread(
    tower: Arc<dyn Tower>,
    tx: Sender<Event>,
    shared: Arc<Shared>,
    every: Duration,
    kick: Receiver<()>,
) -> impl FnOnce() + Send + 'static {
    move || loop {
        let req = shared
            .preview
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((pane_id, lines)) = req {
            if let Some(body) = tower.capture_pane(&pane_id, lines) {
                if tx.send(Event::Preview { pane_id, body }).is_err() {
                    return;
                }
            }
        }
        if !wait(&kick, every) {
            return;
        }
    }
}

/// Agents on saved machines, read again every [`REMOTE_INTERVAL`]. No nudge
/// channel: nothing the user does makes a machine's agents change sooner, and
/// it ends when the receiver goes away, which the send notices.
///
/// Each machine's processor counters are remembered to the next read, which
/// is what turns two of them into a busy share — over the whole interval, so
/// the remote end never has to wait to measure one.
fn remote_thread(tower: Arc<dyn Tower>, tx: Sender<Event>) -> impl FnOnce() + Send + 'static {
    move || {
        let (_dead, never) = mpsc::channel::<()>();
        let mut counters = HashMap::new();
        loop {
            let machines = with_counters_before(tower.remote_machines(), &mut counters);
            if tx.send(Event::Remote(machines)).is_err() {
                return;
            }
            match never.recv_timeout(REMOTE_INTERVAL) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
    }
}

/// Gives each machine the counters it had at the last read, and remembers
/// this read's. A machine that missed a read keeps its older counters, so the
/// next share spans the gap rather than going blank twice.
fn with_counters_before(
    mut machines: Vec<crate::RemoteMachine>,
    counters: &mut HashMap<String, tower_os::host::CpuTicks>,
) -> Vec<crate::RemoteMachine> {
    for m in &mut machines {
        m.cpu_before = counters.get(&m.label).copied();
        if let Some(now) = m.cpu {
            counters.insert(m.label.clone(), now);
        }
    }
    machines
}

/// The machine's processor and memory, read every [`HOST_INTERVAL`]. The
/// processor's share is the busy part of the time between two readings, so
/// the first one comes an interval after the start. No nudge channel, like
/// the remote read.
fn host_thread(tx: Sender<Event>) -> impl FnOnce() + Send + 'static {
    move || {
        let (_dead, never) = mpsc::channel::<()>();
        let mut last = tower_os::host::cpu_ticks();
        loop {
            match never.recv_timeout(HOST_INTERVAL) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            let now = tower_os::host::cpu_ticks();
            let load = host_load(last, now, tower_os::host::memory());
            last = now;
            if tx.send(Event::Host(load)).is_err() {
                return;
            }
        }
    }
}

/// One reading of the machine's load from two readings of the counters.
fn host_load(
    last: Option<tower_os::host::CpuTicks>,
    now: Option<tower_os::host::CpuTicks>,
    memory: Option<tower_os::host::Memory>,
) -> HostLoad {
    HostLoad {
        cpu_pct: last
            .zip(now)
            .and_then(|(last, now)| now.busy_pct_since(&last)),
        memory,
    }
}

/// Folder to the number of what it works on, for the `#52` beside the branch:
/// its branch's open PR, else the issue its branch is named after.
///
/// Waits one records pass before the first lookup, so it asks about the
/// folders on screen rather than an empty list.
fn pr_thread(
    tower: Arc<dyn Tower>,
    tx: Sender<Event>,
    shared: Arc<Shared>,
) -> impl FnOnce() + Send + 'static {
    move || {
        let (_dead, never) = mpsc::channel::<()>();
        let mut every = POLL_INTERVAL;
        loop {
            match never.recv_timeout(every) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            let folders = live_folders(&shared);
            if folders.is_empty() {
                every = POLL_INTERVAL;
                continue;
            }
            let numbers = work_numbers(
                &folders,
                &tower.open_prs(&folders),
                &tower.checked_out_branches(&folders),
            );
            if tx.send(Event::Prs(numbers)).is_err() {
                return;
            }
            every = PR_SCAN_INTERVAL;
        }
    }
}

/// Each folder's number: the open PR first, the issue in its branch name
/// otherwise.
fn work_numbers(
    folders: &[String],
    prs: &std::collections::HashMap<String, u64>,
    branches: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, u64> {
    folders
        .iter()
        .filter_map(|f| {
            let n = prs
                .get(f)
                .copied()
                .or_else(|| branches.get(f).and_then(|b| issue_in_branch(b)))?;
            Some((f.clone(), n))
        })
        .collect()
}

/// The issue number a branch is named after: the first part between `/`,
/// `-` or `_` that is two to six digits — `fix/2955-bundle-setting` is 2955.
///
/// Two digits at least, so `phase-5-slice-4` names no issue; six at most, so
/// the date `/gnew` puts at the end (`feat/x-20260927`) is not one either.
/// Digits inside a word (`stage2`, `v0.34.0`) do not count.
pub(crate) fn issue_in_branch(branch: &str) -> Option<u64> {
    branch
        .split(['/', '-', '_'])
        .find(|part| (2..=6).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|part| part.parse().ok())
        .filter(|n| *n > 0)
}

/// The folders the live agents work in, each once.
fn live_folders(shared: &Shared) -> Vec<String> {
    let agents = shared.agents.lock().unwrap_or_else(|e| e.into_inner());
    let mut f: Vec<String> = agents
        .iter()
        .filter(|a| !a.cwd.is_empty())
        .map(|a| a.cwd.clone())
        .collect();
    f.sort();
    f.dedup();
    f
}

/// Worktree folder to branch, for the `⎇` column and the info card.
///
/// Read every refresh rather than cached: `wt new` and `wt clean` change this
/// while the tower is open, and the file count is the number of live tasks —
/// single digits, beside a herdr round trip that already happened.
///
/// An unreadable state directory is an empty map, not an error. The branch is a
/// label; failing a refresh over it would cost the whole screen.
fn worktree_branches() -> std::collections::HashMap<String, String> {
    tower_wt::live_branches(&tower_state::default_dir())
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use time::OffsetDateTime;
    use tower_state::AgentState;

    #[test]
    fn a_branch_names_its_issue_only_by_a_standalone_number() {
        assert_eq!(
            issue_in_branch("agent/vector-2921-storage-keys"),
            Some(2921)
        );
        assert_eq!(issue_in_branch("agent/explain-2969-stage2"), Some(2969));
        assert_eq!(issue_in_branch("fix/2955-bundle-setting"), Some(2955));
        assert_eq!(issue_in_branch("issue_48"), Some(48));
        assert_eq!(issue_in_branch("phase-5-slice-4-codex-worker"), None);
        assert_eq!(
            issue_in_branch("feat/pr-number-beside-branch-20260927"),
            None
        );
        assert_eq!(issue_in_branch("release/v0.34.0"), None);
        assert_eq!(issue_in_branch("main"), None);
        assert_eq!(issue_in_branch("fix/00-x"), None);
    }

    /// The rule written the slow way: split on every separator by hand and
    /// keep the first all-digit part of the allowed length.
    fn naive_issue(branch: &str) -> Option<u64> {
        let mut part = String::new();
        let mut parts = Vec::new();
        for c in branch.chars() {
            if c == '/' || c == '-' || c == '_' {
                parts.push(std::mem::take(&mut part));
            } else {
                part.push(c);
            }
        }
        parts.push(part);
        for p in parts {
            if (2..=6).contains(&p.len()) && p.chars().all(|c| c.is_ascii_digit()) {
                let n: u64 = p.parse().unwrap();
                return (n > 0).then_some(n);
            }
        }
        None
    }

    /// Round trip: a number of two to six digits put between any separators
    /// is the number read back; seven digits or more is none. Cross-checked
    /// against the slow reading on generated names.
    #[test]
    fn a_number_placed_in_a_branch_is_the_number_read_back() {
        let numbers = [10u64, 48, 219, 2921, 2969, 99_999, 999_999];
        let shapes: [fn(u64) -> String; 6] = [
            |n| format!("fix/{n}-bundle"),
            |n| format!("agent/vector-{n}-storage"),
            |n| format!("issue_{n}"),
            |n| format!("{n}"),
            |n| format!("feat/x-{n}"),
            |n| format!("a/b/{n}/c"),
        ];
        for n in numbers {
            for shape in shapes {
                let b = shape(n);
                assert_eq!(issue_in_branch(&b), Some(n), "{b}");
                assert_eq!(issue_in_branch(&b), naive_issue(&b), "{b}");
            }
        }
        for n in [1_000_000u64, 20_260_927, u64::MAX] {
            let b = format!("feat/x-{n}");
            assert_eq!(issue_in_branch(&b), None, "{b}");
        }

        // Generated names from a fixed seed: the fast and the slow reading agree.
        let alphabet: Vec<char> = "ab9-/_0.1x2".chars().collect();
        let mut seed: u64 = 0x2969_2921;
        for _ in 0..2000 {
            let mut b = String::new();
            for _ in 0..(seed % 14) {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                b.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            seed = seed.wrapping_add(1);
            assert_eq!(issue_in_branch(&b), naive_issue(&b), "{b:?}");
        }
    }

    /// Edges: leading zeros are the number, all zeros is none, and only the
    /// first qualifying part counts.
    #[test]
    fn issue_in_branch_edges() {
        let cases: &[(&str, Option<u64>)] = &[
            ("", None),
            ("-", None),
            ("//", None),
            ("fix/007-x", Some(7)),
            ("fix/0000-x", None),
            ("fix/12-34", Some(12)),
            ("fix/1-23", Some(23)),
            ("fix/v12-x", None),
            ("fix/12a-x", None),
            ("fix/１２-x", None), // full-width digits are not ASCII digits
            ("fix/12.5-x", None),
        ];
        for (b, want) in cases {
            assert_eq!(issue_in_branch(b), *want, "{b:?}");
        }
    }

    /// Every folder asked about gets at most one number, only folders asked
    /// about get one, and a PR with no branch known still counts.
    #[test]
    fn work_numbers_only_answers_for_the_folders_asked() {
        let folders = ["/a".to_string(), "/b".to_string()];
        let prs = std::collections::HashMap::from([
            ("/a".to_string(), 61),
            ("/elsewhere".to_string(), 5),
        ]);
        let branches =
            std::collections::HashMap::from([("/elsewhere".to_string(), "fix/2955-x".to_string())]);
        let got = work_numbers(&folders, &prs, &branches);
        assert_eq!(got.len(), 1);
        assert_eq!(got.get("/a"), Some(&61));
        assert!(work_numbers(&[], &prs, &branches).is_empty());
    }

    #[test]
    fn an_open_pr_wins_over_the_issue_in_the_branch() {
        let folders = ["/a".to_string(), "/b".to_string(), "/c".to_string()];
        let prs = std::collections::HashMap::from([("/a".to_string(), 3034)]);
        let branches = std::collections::HashMap::from([
            ("/a".to_string(), "agent/vector-2933-struct".to_string()),
            (
                "/b".to_string(),
                "agent/vector-2921-storage-keys".to_string(),
            ),
            ("/c".to_string(), "main".to_string()),
        ]);
        let got = work_numbers(&folders, &prs, &branches);
        assert_eq!(got.get("/a"), Some(&3034));
        assert_eq!(got.get("/b"), Some(&2921));
        assert_eq!(got.get("/c"), None);
    }

    /// A dead agent is never drawn, so the background readers are not handed
    /// one — its rollout is the large finished file that cost the most to read.
    #[test]
    fn the_background_readers_are_handed_live_agents_only() {
        let live = Agent::new("codex-live", "codex", OffsetDateTime::UNIX_EPOCH);
        let mut dead = Agent::new("codex-dead", "codex", OffsetDateTime::UNIX_EPOCH);
        dead.state = AgentState::Dead;
        let ids: Vec<_> = handed_on(&[live, dead]).into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["codex-live"]);
    }

    /// The context lookup reads the rollout the agent's session names, through
    /// the cache the thread keeps.
    #[test]
    fn collect_local_finds_a_codex_agents_context() {
        let codex = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = codex.path().join("2026/09/20");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rollout-2026-09-20T00-00-00-sid-live.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"sid-live\",\"cwd\":\"/w\"}}\n\
             {\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-live\"}}\n",
        )
        .unwrap();
        let settings = Settings {
            home: Some(home.path().to_path_buf()),
            resume: crate::ResumePaths {
                codex_root: codex.path().to_path_buf(),
                gemini_dir: home.path().join("gemini"),
                claude_dir: home.path().join("claude"),
            },
            ..Settings::default()
        };
        let mut cache = tower_usage::CtxCache::default();
        let info = collect_local(&[live_codex_agent()], &settings, &mut cache);
        assert_eq!(info.ctx["codex-live"].model, "gpt-live");
        assert_eq!(cache.len(), 1);

        // The cache is an optimisation, so a second pass over the same
        // unchanged rollout has to answer what the first one did — and what a
        // pass that has never seen the file answers. The same builder for all
        // three, so no pass can be handed a different agent by accident.
        let again = collect_local(&[live_codex_agent()], &settings, &mut cache);
        let cold = collect_local(
            &[live_codex_agent()],
            &settings,
            &mut tower_usage::CtxCache::default(),
        );
        assert_eq!(again.ctx, info.ctx);
        assert_eq!(again.ctx, cold.ctx);
    }

    /// The live codex agent whose rollout the test above writes: the session ID
    /// names the file and the cwd has to match the one recorded in it.
    fn live_codex_agent() -> Agent {
        let mut a = Agent::new("codex-live", "codex", OffsetDateTime::UNIX_EPOCH);
        a.cwd = "/w".into();
        a.session_id = "sid-live".into();
        a
    }

    /// Property: the filter only ever removes, never reorders and never
    /// invents. The readers below it key on the list they are handed, so a
    /// reordering would hand one agent's context to the row beside it, and the
    /// rows are already ordered with what needs a person first.
    #[test]
    fn the_live_filter_only_ever_removes() {
        const STATES: [AgentState; 6] = [
            AgentState::Working,
            AgentState::Waiting,
            AgentState::DoneUnread,
            AgentState::Idle,
            AgentState::Error,
            AgentState::Dead,
        ];
        let mut rng = tower_testrng::Rng::new(0xFEED_1105);
        // The generator is [`tower_testrng`]'s: xorshift64*, whose output
        // multiply matters here because `% n` reads the low bits and every
        // draw in this test is a small `n`.
        let mut next = move |n: u64| rng.below(n as usize) as u64;
        for step in 0..120 {
            let agents: Vec<Agent> = (0..next(9))
                .map(|i| {
                    let mut a = Agent::new(format!("a{i}"), "codex", OffsetDateTime::UNIX_EPOCH);
                    a.state = STATES[next(6) as usize];
                    a
                })
                .collect();
            let kept = handed_on(&agents);

            assert!(kept.len() <= agents.len(), "step {step}");
            assert!(kept.iter().all(|a| a.is_live()), "step {step}");
            // Order preserved, and nothing live dropped: the survivors are the
            // live records read off the input in the order they arrived.
            let want: Vec<&Agent> = agents.iter().filter(|a| a.is_live()).collect();
            assert_eq!(kept.len(), want.len(), "step {step}");
            for (got, want) in kept.iter().zip(want) {
                assert_eq!(got.id, want.id, "step {step}");
                assert_eq!(got.state, want.state, "step {step}");
            }
        }
    }

    /// Nothing to read is still a reading: the pinned defaults are reported as
    /// `Some`, because an empty map alone would mean both "nothing pinned" and
    /// "not looked yet" and the header would announce all three CLIs on
    /// automatic before a single file had been opened.
    #[test]
    fn a_pass_with_no_agents_still_says_it_looked() {
        let home = tempfile::tempdir().unwrap();
        let settings = Settings {
            home: Some(home.path().to_path_buf()),
            resume: crate::ResumePaths {
                codex_root: home.path().join("no-codex"),
                gemini_dir: home.path().join("no-gemini"),
                claude_dir: home.path().join("no-claude"),
            },
            ..Settings::default()
        };
        let mut cache = tower_usage::CtxCache::default();
        let info = collect_local(&[], &settings, &mut cache);
        assert!(info.ctx.is_empty());
        assert!(info.wired.is_empty());
        // `Some`, holding nothing — the state the `Option` exists to tell apart
        // from `None`, and the one a bare `is_some()` cannot see, since
        // `collect_local` sets the field on every pass.
        let def = info
            .def_models
            .as_ref()
            .expect("a pass that read no file still reports that it read");
        assert_eq!(
            [def.claude.as_str(), def.codex.as_str(), def.gemini.as_str()],
            ["", "", ""],
            "this HOME has no settings file for any of the three"
        );
        assert_eq!(cache.len(), 0, "a pass with no codex row remembers nothing");
    }

    /// The intervals, as a set: the supervisor-owned snapshot is re-read most
    /// often, the saved machines less often because each is an ssh round trip,
    /// and the file readings least often of all. Each is a thread's own
    /// `recv_timeout`, so the only way to pin the relationship without waiting
    /// out fifteen seconds of real clock is on the values themselves.
    #[test]
    fn the_readers_that_cost_more_run_less_often() {
        assert!(
            POLL_INTERVAL < REMOTE_INTERVAL,
            "an ssh round trip must not run as often as a store read"
        );
        assert!(
            REMOTE_INTERVAL < LOCAL_INTERVAL,
            "the file readings are the slowest clock"
        );
        // And none of them is zero, which would turn a thread into a spin.
        for d in [
            POLL_INTERVAL,
            REMOTE_INTERVAL,
            LOCAL_INTERVAL,
            HOST_INTERVAL,
        ] {
            assert!(d >= Duration::from_secs(1), "{d:?}");
        }
    }

    #[test]
    fn the_processor_share_needs_two_readings_and_memory_needs_none() {
        use tower_os::host::{CpuTicks, Memory};
        let a = CpuTicks {
            busy: 10,
            total: 100,
        };
        let b = CpuTicks {
            busy: 60,
            total: 200,
        };
        let m = Memory { used: 1, total: 4 };
        assert_eq!(host_load(Some(a), Some(b), Some(m)).cpu_pct, Some(50.0));
        let first = host_load(None, Some(b), Some(m));
        assert_eq!(first.cpu_pct, None, "nothing to compare with yet");
        assert_eq!(first.memory, Some(m));
        assert_eq!(host_load(Some(a), None, None), HostLoad::default());
    }

    #[test]
    fn each_machine_gets_the_counters_of_its_own_last_reading() {
        use tower_os::host::CpuTicks;
        let ticks = |busy, total| Some(CpuTicks { busy, total });
        let machine = |label: &str, cpu| crate::RemoteMachine {
            label: label.into(),
            cpu,
            ..Default::default()
        };
        let mut counters = HashMap::new();
        let first = with_counters_before(
            vec![machine("a", ticks(1, 10)), machine("b", ticks(5, 50))],
            &mut counters,
        );
        assert_eq!((first[0].cpu_before, first[1].cpu_before), (None, None));
        // `b` misses a read; `a` reads on.
        let second = with_counters_before(
            vec![machine("a", ticks(6, 20)), machine("b", None)],
            &mut counters,
        );
        assert_eq!(second[0].cpu_before, ticks(1, 10));
        assert_eq!(second[0].load().cpu_pct, Some(50.0));
        assert_eq!(second[1].cpu_before, ticks(5, 50));
        // And when it answers again, the share spans the gap.
        let third = with_counters_before(vec![machine("b", ticks(25, 150))], &mut counters);
        assert_eq!(third[0].cpu_before, ticks(5, 50));
        assert_eq!(third[0].load().cpu_pct, Some(20.0));
    }
}
