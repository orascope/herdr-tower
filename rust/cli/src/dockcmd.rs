// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower dock` — the tower in a workspace of its own, for a mouse.
//!
//! herdr runs a custom command, a popup included, only from a key binding. A
//! tab-bar `command` entry displays output and is not clickable, and a plugin
//! action is reached by a key or a Ctrl-clicked link — checked against the
//! herdr 0.9.0 documentation. A workspace, though, is one click in the sidebar,
//! so a tower someone wants to open without the keyboard lives there.
//!
//! The command is meant to be repeated, which is why it plans before it acts.
//! herdr brings a workspace back after a server restart but not the processes in
//! it, so the question is never only "does the workspace exist" but "is a tower
//! running in it". Herdr's host-side process observer is the authoritative
//! answer, including for clients in another mount namespace. A local `flock`
//! remains a fast path and traces a shell made for a
//! replacement tower, so a failed launch is retried there instead of leaving
//! another empty tab behind. A short server-side lease arbitrates launches that
//! cannot share that file.
//!
//! Named after the *terminal*, not the pane. Pane IDs are never reused, but
//! moving a pane can give it a new one, while a terminal ID names one terminal
//! for as long as it lives. Terminal-bound state therefore follows a move
//! instead of becoming stranded on the pane's old coordinate.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use herdrx::{Pane, ProcessInfo, Tab, Workspace};

/// The workspace label the dock answers to.
pub use tower_ui::DOCK_LABEL as LABEL;

/// The label the dock's tab carries.
///
/// The same string as the workspace's, so the tab bar and the sidebar name one
/// place rather than two. `dock` writes it on every run, which is what keeps a
/// tab herdr would otherwise number ("1") or name after the running command
/// showing the dock's name instead.
pub const TAB_LABEL: &str = LABEL;

/// The prefix of the request-owned label a tab wears until its creation has
/// been reconciled.
///
/// Deliberately not [`TAB_LABEL`]: a half-created tab can never be mistaken for
/// a workspace request, which wears the dock label with the same `#nonce`
/// suffix.
const TAB_REQUEST_PREFIX: &str = "tower";

/// Marks a tower started by `dock`, so it clears the target startup lease once
/// Herdr can observe its process.
pub const DOCK_ENV: &str = "HERDR_TOWER_DOCK";
/// The exact startup lease carried by the tower command.
pub const DOCK_LEASE_ENV: &str = "HERDR_TOWER_DOCK_LEASE";
/// The stable terminal selected by `dock`; pane coordinates can change before
/// the launched process begins initialization.
pub const DOCK_TERMINAL_ENV: &str = "HERDR_TOWER_DOCK_TERMINAL";
/// What every dock token's name starts with.
const TOKEN_PREFIX: &str = "dock-";
/// A short server-visible lease that keeps another `dock` from reusing a pane
/// while an accepted command has not become an observable process yet.
pub const STARTING_TOKEN: &str = "dock-starting";
/// Durable high-water mark for the startup lease's ordered source.
pub const STARTING_GENERATION_TOKEN: &str = "dock-start-generation";
/// A separate metadata contribution keeps launch ordering independent from
/// the tower's ordinary sidebar decoration.
pub const STARTING_SOURCE: &str = "herdr-tower-dock-start";
/// The high-water mark does not inherit the launch lease's TTL.
pub const STARTING_GENERATION_SOURCE: &str = "herdr-tower-dock-sequence";
/// Persistent evidence that `dock` created this shell for a tower.
pub const REUSABLE_TOKEN: &str = "dock-reusable";
/// This shell was created or selected by `dock`, but no command was submitted.
pub const REUSABLE_UNSUBMITTED: &str = "unsubmitted-v1";
/// A command may have reached this shell; only live evidence or a local tower
/// claim may make it reusable again.
pub const REUSABLE_SUBMITTED: &str = "submitted-v1";
/// A separate owner keeps the durable trace independent of expiring leases.
pub const REUSABLE_SOURCE: &str = "herdr-tower-dock-trace";
/// A tower entered its UI and has not reported a normal exit.
pub const ACTIVE_TOKEN: &str = "dock-active";
/// A persistent source lets a stopped or killed tower conservatively block
/// reuse without expiring while the process is suspended.
pub const ACTIVE_SOURCE: &str = "herdr-tower-dock-active";
/// Persistent proof that a request-labeled workspace was created by `dock`.
pub const CREATED_TOKEN: &str = "dock-created";
/// Keeps the creation proof independent from the expiring mutation gate.
pub const CREATED_SOURCE: &str = "herdr-tower-dock-create";
/// Marks a request-shaped workspace as deliberately excluded from adoption.
pub const CREATED_RETIRED: &str = "retired";
/// Server-timed evidence that a workspace creator is still in its bounded
/// handoff window.
pub const CREATING_TOKEN: &str = "dock-creating";
pub const CREATING_SOURCE: &str = "herdr-tower-dock-creator";
/// Server-timed evidence that an interrupted request-labeled tab is still safe
/// to recover.
pub const TAB_CREATING_TOKEN: &str = "dock-tab-creating";
pub const TAB_CREATING_SOURCE: &str = "herdr-tower-dock-tab-creator";
pub const TAB_CREATING_INTENT_TOKEN: &str = "dock-tab-creating-intent";
pub const TAB_CREATING_INTENT_SOURCE: &str = "herdr-tower-dock-tab-intent";
/// A server-visible gate placed on the dock workspace before its tabs or panes
/// are mutated.
pub const COORDINATING_TOKEN: &str = "dock-coordinating";
/// Its own sequence domain, independent of the eventual target-pane lease.
pub const COORDINATING_SOURCE: &str = "herdr-tower-dock-gate";
/// Durable high-water mark for the expiring workspace gate.
pub const COORDINATING_GENERATION_TOKEN: &str = "dock-gate-generation";
/// The high-water mark survives expiration of the held gate token.
pub const COORDINATING_GENERATION_SOURCE: &str = "herdr-tower-dock-gate-seq";
/// Long enough for a delayed shell to consume an accepted command, but finite
/// so an ambiguous socket failure cannot reserve the pane forever.
pub const STARTING_TTL: Duration = Duration::from_secs(5 * 60);
/// The local process is terminated before another client may treat its
/// coordination token as expired. This is a fencing deadline, not cleanup:
/// `SIGALRM` is delivered by the kernel even when the process was stopped
/// between its last ownership check and the following socket mutation.
// Herdrx bounds these immediate RPCs at three seconds. Thirty seconds leaves the
// server time to finish a request whose client disappears at the hard fence
// before a successor may advance the generation.
const COORDINATION_FENCE_MARGIN: Duration = Duration::from_secs(30);

/// A kernel-enforced upper bound on one coordinator's lifetime.
///
/// Herdr 0.9 has ordered metadata but no conditional layout mutation. A plain
/// userspace deadline check therefore has an unavoidable preemption gap. An
/// armed real-time interval timer closes that gap by terminating this process
/// before Herdr's server-side TTL can remove the token and admit a successor.
#[derive(Debug)]
pub struct CoordinationFence;

// `run_dock` may hold a create-ownership fence while a nested workspace or
// pane coordinator needs the same process-wide timer. Only the last live guard
// may disarm it; otherwise an inner guard's drop would silently un-fence the
// outer ownership loop.
static COORDINATION_FENCE_USERS: Mutex<usize> = Mutex::new(0);

impl CoordinationFence {
    pub fn arm() -> io::Result<Self> {
        let mut users = COORDINATION_FENCE_USERS
            .lock()
            .map_err(|_| io::Error::other("dock coordination fence state is poisoned"))?;
        if *users != 0 {
            *users = (*users).saturating_add(1);
            return Ok(Self);
        }
        let after = STARTING_TTL
            .checked_sub(COORDINATION_FENCE_MARGIN)
            .ok_or_else(|| io::Error::other("dock coordination deadline is too short"))?;
        arm_alarm(after)?;
        *users = 1;
        Ok(Self)
    }
}

impl Drop for CoordinationFence {
    fn drop(&mut self) {
        let Ok(mut users) = COORDINATION_FENCE_USERS.lock() else {
            return;
        };
        if *users > 1 {
            *users -= 1;
            return;
        }
        if *users == 0 {
            return;
        }
        *users = 0;
        disarm_alarm();
    }
}

/// Arms the fence: the process ends after `after` unless [`disarm_alarm`]
/// runs first.
#[cfg(unix)]
fn arm_alarm(after: Duration) -> io::Result<()> {
    let timer = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: after.as_secs().try_into().map_err(|_| {
                io::Error::other("dock coordination deadline cannot be represented")
            })?,
            // Always below one million, so it fits `suseconds_t` on both
            // macOS (i32) and Linux (i64).
            tv_usec: after.subsec_micros() as libc::suseconds_t,
        },
    };
    // SAFETY: the structures are fully initialized, SIGALRM is restored to
    // its default terminating disposition, and this process is the only
    // object affected. `run_dock` is a short-lived CLI path.
    unsafe {
        if libc::signal(libc::SIGALRM, libc::SIG_DFL) == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        let mut signals = std::mem::zeroed();
        if libc::sigemptyset(&mut signals) != 0 || libc::sigaddset(&mut signals, libc::SIGALRM) != 0
        {
            return Err(io::Error::last_os_error());
        }
        let mask_error = libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
        if mask_error != 0 {
            return Err(io::Error::from_raw_os_error(mask_error));
        }
        if libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Disarms the fence.
#[cfg(unix)]
fn disarm_alarm() {
    let timer = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    // SAFETY: a zero timer only disarms this process's ITIMER_REAL.
    unsafe {
        libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut());
    }
}

/// Windows has no `SIGALRM`. A watchdog thread ends the process instead —
/// `TerminateProcess` on itself, which no other thread of it can stall. Each
/// arming has its own generation, so a watchdog from an earlier, disarmed
/// fence never fires on a later one.
#[cfg(windows)]
static ALARM_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(windows)]
fn arm_alarm(after: Duration) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    let generation = ALARM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::Builder::new()
        .name("dock-fence".into())
        .spawn(move || {
            std::thread::sleep(after);
            if ALARM_GENERATION.load(Ordering::SeqCst) == generation {
                let _ = tower_os::kill(std::process::id());
            }
        })?;
    Ok(())
}

#[cfg(windows)]
fn disarm_alarm() {
    ALARM_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// One contender's value for the server-side first-writer-wins lease.
pub fn starting_lease_value() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    request_id_value(nanos, std::process::id())
}

fn request_id_value(nanos: u128, pid: u32) -> String {
    // The parsers deliberately require a substantial nonce field so ordinary
    // user labels cannot become creation claims. Preserve that shape even on
    // a host whose realtime clock has not reached the Unix epoch.
    format!("{nanos:012x}-{pid:x}")
}

/// A request-owned label used until workspace creation has been reconciled.
pub fn workspace_request_label(request_id: &str) -> String {
    format!("{LABEL}#{request_id}")
}

/// The request ID carried by an unpublished dock workspace candidate.
pub fn workspace_request_id(label: &str) -> Option<&str> {
    let request_id = label
        .strip_prefix(LABEL)?
        .strip_prefix('#')
        .filter(|request_id| !request_id.is_empty())?;
    let mut fields = request_id.split('-');
    let nanos = fields.next()?;
    let pid = fields.next()?;
    (fields.next().is_none()
        && nanos.len() >= 12
        && !pid.is_empty()
        && nanos.bytes().all(|byte| byte.is_ascii_hexdigit())
        && pid.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(request_id)
}

/// Whether this private workspace carries the creation proof matching its
/// request-unique label.
pub fn is_workspace_creation_candidate(workspace: &Workspace) -> bool {
    workspace_request_id(&workspace.label).is_some()
        && workspace.tokens.get(CREATED_TOKEN) == Some(&workspace.label)
}

/// Whether an atomic request label survived before its durable token could be
/// stored. It must be claimed and inspected before it can be published.
pub fn is_unclaimed_workspace_creation_candidate(workspace: &Workspace) -> bool {
    workspace_request_id(&workspace.label).is_some()
        && !workspace.tokens.contains_key(CREATED_TOKEN)
}

fn is_workspace_candidate(workspace: &Workspace) -> bool {
    is_workspace_creation_candidate(workspace)
        || is_unclaimed_workspace_creation_candidate(workspace)
}

/// A request-owned label used until tab creation has been reconciled.
pub fn tab_request_label(request_id: &str) -> String {
    format!("{TAB_REQUEST_PREFIX}#{request_id}")
}

/// The request ID carried by a tab whose creation has not yet reached its
/// durable reusable marker. The same strict shape as workspace requests keeps
/// an ordinary user label from becoming ownership proof.
pub fn tab_request_id(label: &str) -> Option<&str> {
    let request_id = label
        .strip_prefix(TAB_REQUEST_PREFIX)?
        .strip_prefix('#')
        .filter(|request_id| !request_id.is_empty())?;
    let mut fields = request_id.split('-');
    let nanos = fields.next()?;
    let pid = fields.next()?;
    (fields.next().is_none()
        && nanos.len() >= 12
        && !pid.is_empty()
        && nanos.bytes().all(|byte| byte.is_ascii_hexdigit())
        && pid.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(request_id)
}

/// Whether the dock's tab is wearing a name that `dock` should replace with
/// [`TAB_LABEL`].
///
/// A tab still carrying a request label is left alone even though it is not
/// the dock's name: that label is how the invocation that created the tab
/// finds it again after a lost reply, and renaming it here would strand it.
pub fn tab_needs_dock_label(tabs: &[Tab], tab_id: &str) -> bool {
    tabs.iter().any(|tab| {
        tab.tab_id == tab_id
            && tower_ui::needs_tab_label(&tab.label)
            && tab_request_id(&tab.label).is_none()
    })
}

/// Couples a pane startup lease to the ordered generation that issued it.
pub fn pane_starting_lease_value(generation: u64, owner: &str, terminal_id: &str) -> String {
    terminal_bound_value(&format!("held:{generation}:{owner}"), terminal_id)
}

/// The persistent high-water value paired with one contender.
pub fn pane_starting_generation_value(generation: u64, owner: &str, terminal_id: &str) -> String {
    terminal_bound_value(&format!("{generation}:{owner}"), terminal_id)
}

/// The next server-visible startup generation, or `None` while one is held.
///
/// The high-water token is written without a TTL under a separate source, so
/// it survives a crashed held token's expiry along with Herdr's hidden sequence
/// record. Even values are leases and odd values their releases.
pub fn next_pane_starting_generation(
    high_water: Option<&str>,
    lease_state: Option<&str>,
    terminal_id: &str,
) -> Option<u64> {
    if let Some(value) = lease_state {
        if pane_starting_lease_is_held_for(value, terminal_id) {
            return None;
        }
        // An unbound held lease names no terminal, so it cannot be ruled out
        // for this one and fails closed. A bound lease for a different
        // terminal is a stale/misdirected contribution and must not classify
        // this replacement terminal as Starting.
        if value.starts_with("held:") && !value.contains(TERMINAL_BINDING_MARKER) {
            return None;
        }
    }
    if let Some(value) = high_water {
        let (generation, owner) = value.split_once(':')?;
        if owner.is_empty() {
            return None;
        }
        return generation.parse::<u64>().ok()?.checked_add(2);
    }
    match lease_state.filter(|value| !value.starts_with("held:")) {
        Some(value) => {
            let generation = value.strip_prefix("idle:")?.parse::<u64>().ok()?;
            generation.checked_add(1)
        }
        None => Some(2),
    }
}

/// Whether a pane startup token represents a launch still in flight.
///
/// Unknown non-empty values fail closed. Only the exact durable
/// idle form is reusable.
pub fn pane_starting_lease_is_held(value: &str) -> bool {
    let mut fields = value.split(':');
    !matches!(
        (fields.next(), fields.next(), fields.next()),
        (Some("idle"), Some(generation), None)
            if generation.parse::<u64>().is_ok()
    )
}

/// Whether a held startup lease belongs to the pane's current terminal.
pub fn pane_starting_lease_is_held_for(value: &str, terminal_id: &str) -> bool {
    strip_terminal_binding(value, terminal_id).is_some_and(pane_starting_lease_is_held)
}

/// The ordered generation encoded in a pane startup lease.
pub fn pane_starting_lease_generation(value: &str) -> Option<u64> {
    let mut fields = value.split(':');
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some("held"), Some(generation), Some(owner), None) if !owner.is_empty() => {
            generation.parse().ok()
        }
        _ => None,
    }
}

/// A bounded target-pane lease for an accepted or delivery-ambiguous command.
pub fn starting_lease(pane_id: &str, value: &str, generation: u64) -> herdrx::Metadata {
    herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(STARTING_SOURCE.to_string()),
        tokens: [(STARTING_TOKEN.to_string(), value.to_string())]
            .into_iter()
            .collect(),
        ttl: Some(STARTING_TTL),
        seq: generation,
        ..herdrx::Metadata::default()
    }
}

/// Persists the startup sequence high-water mark independently of lease TTL.
pub fn pane_starting_generation_claim(
    pane_id: &str,
    value: &str,
    generation: u64,
) -> herdrx::Metadata {
    herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(STARTING_GENERATION_SOURCE.to_string()),
        tokens: [(STARTING_GENERATION_TOKEN.to_string(), value.to_string())]
            .into_iter()
            .collect(),
        seq: generation,
        ..herdrx::Metadata::default()
    }
}

/// Releases only this launch generation while retaining its durable ordering.
///
/// A clear advances to the next sequence and records that value as idle. An old
/// tower's delayed release therefore sorts before a successor and cannot erase
/// it, while the following client can derive a clock-independent generation.
pub fn clear_starting_lease(pane_id: &str, generation: u64) -> herdrx::Metadata {
    let released_generation = generation.saturating_add(1);
    herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(STARTING_SOURCE.to_string()),
        tokens: [(
            STARTING_TOKEN.to_string(),
            format!("idle:{released_generation}"),
        )]
        .into_iter()
        .collect(),
        seq: released_generation,
        ..herdrx::Metadata::default()
    }
}

/// The next shared generation, or `None` while another lease remains live.
///
/// The durable high-water token and a clean idle gate both contribute to the
/// next sequence. Concurrent contenders therefore choose the same number, and
/// a post-TTL recovery can advance the marker until it clears Herdr's hidden
/// ordered sequence without consulting a client wall clock.
pub fn next_coordination_generation(
    high_water: Option<&str>,
    gate_state: Option<&str>,
) -> Option<u64> {
    let mut latest = 0;
    if let Some(value) = gate_state {
        let mut fields = value.split(':');
        match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some("held"), Some(_generation), Some(_owner), None) => return None,
            (Some("idle"), Some(generation), None, None) => {
                latest = generation.parse::<u64>().ok()?;
            }
            _ => return None,
        }
    }
    if let Some(value) = high_water {
        let (generation, owner) = value.split_once(':')?;
        if owner.is_empty() {
            return None;
        }
        latest = latest.max(generation.parse::<u64>().ok()?);
    }
    latest.checked_add(1)
}

/// The canonical dock workspace or unpublished creation candidate.
///
/// Once a public dock exists it is sticky: a create request that was already in
/// flight cannot publish a second workspace after observing it. Before the
/// first publication, Herdr's explicit workspace order selects the earliest
/// server-created candidate; response order is the deterministic
/// fallback when that numeric order is omitted. A strictly generated private
/// label participates even when the create acknowledgement or claim write was
/// lost, but an unclaimed candidate must be inspected and claimed before it is
/// published.
pub fn dock_workspace(workspaces: &[Workspace]) -> Option<&Workspace> {
    workspaces
        .iter()
        .filter(|workspace| workspace.label == LABEL)
        .min_by(|a, b| a.workspace_id.cmp(&b.workspace_id))
        .or_else(|| {
            workspaces
                .iter()
                .enumerate()
                .filter(|(_, workspace)| is_workspace_candidate(workspace))
                .min_by_key(|(index, workspace)| (workspace.number, *index))
                .map(|(_, workspace)| workspace)
        })
}

/// Does the evidence say a tower is alive on this terminal?
///
/// The server's process list is the record. A tower that dies without tidying
/// up leaves its `dock-active` token behind — nothing revokes a token on behalf
/// of a process that is gone — so the token is not evidence here at all.
///
/// Reading the token is what once wedged the dock shut. The supervisor read the
/// process list, found no tower and called `dock`; `dock` read the token,
/// answered "already running" and did nothing. Both were satisfied, the pane
/// behind the sidebar button stayed dead, and no amount of clicking it helped
/// until someone cleared the token by hand.
///
/// `lock_held` is the tower's own lifetime lock in this filesystem namespace.
/// It is evidence because the kernel releases it when the holder exits, so
/// unlike a token it cannot outlive what it describes.
pub fn tower_is_evident(running: bool, lock_held: bool) -> bool {
    running || lock_held
}

/// Should this pane's `dock-active` token stop a launch that is about to start?
///
/// The token exists to keep two docks from launching into one pane. That is
/// worth honouring — but only while it describes something. A tower that was
/// killed leaves the token behind, and a launch blocked by a dead tower's token
/// is blocked forever: nothing will ever come along to clear it.
///
/// So the token is believed exactly as far as the process observation backs it.
/// `tower_observed` is `None` when an observation failed, and "could not look"
/// must not read as "no", so the token still blocks. Mutual exclusion between two *launches* does not rest on this
/// check in any case: that is the starting lease's job, and it is taken
/// separately.
pub fn active_token_blocks_launch(
    token: Option<&str>,
    observe: impl FnOnce() -> Option<bool>,
) -> bool {
    // 🚨 The observation is taken only when there is a token to weigh it
    // against. It costs a round trip to the server, and the ordinary case — a
    // pane with no token — does not need one.
    if token.is_none_or(|value| value.is_empty()) {
        return false;
    }
    observe().unwrap_or(true)
}

/// Whether the mutation target still carries this live coordination token.
///
/// Losing this workspace invalidates every following tab or pane mutation:
/// those operations name the workspace itself or a pane contained by it. A
/// replacement dock therefore cannot overlap a mutation still aimed at the
/// closed target.
pub fn owns_coordination(
    workspaces: &[Workspace],
    anchor_workspace_id: &str,
    expected: &str,
) -> bool {
    let target = workspaces.iter().find(|workspace| {
        workspace.workspace_id == anchor_workspace_id
            && (workspace.label == LABEL || is_workspace_candidate(workspace))
    });
    let Some(actual) = target.and_then(|workspace| workspace.tokens.get(COORDINATING_TOKEN)) else {
        return false;
    };
    let mut fields = actual.split(':');
    let live = matches!(
        (fields.next(), fields.next(), fields.next(), fields.next()),
        (Some("held"), Some(_generation), Some(_owner), None)
    );
    live && actual == expected
}

/// Persistent server proof tying a private workspace to its create request.
pub fn workspace_creation_claim(
    workspace_id: &str,
    request_label: &str,
) -> herdrx::WorkspaceMetadata {
    herdrx::WorkspaceMetadata {
        workspace_id: workspace_id.to_string(),
        source: CREATED_SOURCE.to_string(),
        tokens: [(CREATED_TOKEN.to_string(), request_label.to_string())]
            .into_iter()
            .collect(),
        ..herdrx::WorkspaceMetadata::default()
    }
}

/// Plants the server-clocked liveness half of a workspace creation claim.
pub fn workspace_creation_lease(
    workspace_id: &str,
    request_label: &str,
) -> herdrx::WorkspaceMetadata {
    herdrx::WorkspaceMetadata {
        workspace_id: workspace_id.to_string(),
        source: CREATING_SOURCE.to_string(),
        tokens: [(CREATING_TOKEN.to_string(), request_label.to_string())]
            .into_iter()
            .collect(),
        ttl: Some(STARTING_TTL),
        ..herdrx::WorkspaceMetadata::default()
    }
}

pub fn workspace_creator_is_live(workspace: &Workspace, request_label: &str) -> bool {
    workspace.tokens.get(CREATING_TOKEN).map(String::as_str) == Some(request_label)
}

/// Whether the dock workspace still carries the server-clocked intent that
/// preceded creation of this request-labeled tab.
pub fn tab_creation_intent_is_live(workspace: &Workspace, request_label: &str) -> bool {
    workspace
        .tokens
        .get(TAB_CREATING_INTENT_TOKEN)
        .map(String::as_str)
        == Some(request_label)
}

/// Plants a bounded request intent before `tab.create` can make its label
/// visible. The coordination generation orders repeated creates from the same
/// workspace, and Herdr's TTL—not a client clock—bounds crash recovery.
pub fn tab_creation_intent(
    workspace_id: &str,
    request_label: &str,
    generation: u64,
) -> herdrx::WorkspaceMetadata {
    herdrx::WorkspaceMetadata {
        workspace_id: workspace_id.to_string(),
        source: TAB_CREATING_INTENT_SOURCE.to_string(),
        tokens: [(
            TAB_CREATING_INTENT_TOKEN.to_string(),
            request_label.to_string(),
        )]
        .into_iter()
        .collect(),
        ttl: Some(STARTING_TTL),
        seq: generation,
    }
}

/// Acquires one stable generation of the server-side planning gate.
pub fn coordinating_lease(
    workspace_id: &str,
    generation: u64,
    value: &str,
) -> herdrx::WorkspaceMetadata {
    herdrx::WorkspaceMetadata {
        workspace_id: workspace_id.to_string(),
        source: COORDINATING_SOURCE.to_string(),
        tokens: [(
            COORDINATING_TOKEN.to_string(),
            format!("held:{generation}:{value}"),
        )]
        .into_iter()
        .collect(),
        ttl: Some(STARTING_TTL),
        seq: generation,
    }
}

/// One contender's persistent high-water claim for the workspace gate.
pub fn coordinating_generation_claim(
    workspace_id: &str,
    generation: u64,
    owner: &str,
) -> herdrx::WorkspaceMetadata {
    herdrx::WorkspaceMetadata {
        workspace_id: workspace_id.to_string(),
        source: COORDINATING_GENERATION_SOURCE.to_string(),
        tokens: [(
            COORDINATING_GENERATION_TOKEN.to_string(),
            format!("{generation}:{owner}"),
        )]
        .into_iter()
        .collect(),
        seq: generation,
        ..herdrx::WorkspaceMetadata::default()
    }
}

/// Releases the gate with the next ordered generation.
///
/// Ordering prevents a delayed write from this live generation from replacing
/// its release. If the process dies first, the hard fence fires before Herdr's
/// TTL removes the entire held contribution and admits a fresh generation.
pub fn clear_coordinating_lease(workspace_id: &str, generation: u64) -> herdrx::WorkspaceMetadata {
    let released_generation = generation.saturating_add(1);
    herdrx::WorkspaceMetadata {
        workspace_id: workspace_id.to_string(),
        source: COORDINATING_SOURCE.to_string(),
        tokens: [(
            COORDINATING_TOKEN.to_string(),
            format!("idle:{released_generation}"),
        )]
        .into_iter()
        .collect(),
        seq: released_generation,
        ..herdrx::WorkspaceMetadata::default()
    }
}

// Herdr normalizes metadata token values by removing control whitespace, so
// the binding has to use printable bytes that survive an actual server
// round-trip. Exact suffix matching keeps the expected terminal unambiguous.
const TERMINAL_BINDING_MARKER: &str = "|terminal=";

fn terminal_bound_value(state: &str, terminal_id: &str) -> String {
    format!("{state}{TERMINAL_BINDING_MARKER}{terminal_id}")
}

fn strip_terminal_binding<'a>(value: &'a str, terminal_id: &str) -> Option<&'a str> {
    value.strip_suffix(&format!("{TERMINAL_BINDING_MARKER}{terminal_id}"))
}

fn reusable_metadata(pane_id: &str, terminal_id: &str, state: &str) -> herdrx::Metadata {
    herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(REUSABLE_SOURCE.to_string()),
        tokens: [(
            REUSABLE_TOKEN.to_string(),
            terminal_bound_value(state, terminal_id),
        )]
        .into_iter()
        .collect(),
        ..herdrx::Metadata::default()
    }
}

/// Server-visible evidence that no tower command has reached this shell yet.
pub fn remember_reusable_metadata(pane_id: &str, terminal_id: &str) -> herdrx::Metadata {
    reusable_metadata(pane_id, terminal_id, REUSABLE_UNSUBMITTED)
}

/// Marks that a launch is about to be submitted to this shell.
pub fn mark_reusable_submitted_metadata(pane_id: &str, terminal_id: &str) -> herdrx::Metadata {
    reusable_metadata(pane_id, terminal_id, REUSABLE_SUBMITTED)
}

/// Whether no tower command has ever been submitted to this reserved shell.
pub fn was_never_submitted(pane: &Pane) -> bool {
    pane.tokens.get(REUSABLE_TOKEN)
        == Some(&terminal_bound_value(
            REUSABLE_UNSUBMITTED,
            &pane.terminal_id,
        ))
}

/// Plants the server-visible lifetime claim of a dock tower.
pub fn active_claim_value(terminal_id: &str, lease_value: &str) -> String {
    let state = lease_value
        .rsplit_once(TERMINAL_BINDING_MARKER)
        .map_or(lease_value, |(state, _)| state);
    terminal_bound_value(state, terminal_id)
}

pub fn pane_has_bound_active(pane: &Pane) -> bool {
    pane.tokens.get(ACTIVE_TOKEN).is_some_and(|value| {
        strip_terminal_binding(value, &pane.terminal_id).is_some_and(|lease| !lease.is_empty())
    })
}

pub fn active_metadata(
    pane_id: &str,
    terminal_id: &str,
    lease_value: &str,
    generation: u64,
) -> herdrx::Metadata {
    herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(ACTIVE_SOURCE.to_string()),
        tokens: [(
            ACTIVE_TOKEN.to_string(),
            active_claim_value(terminal_id, lease_value),
        )]
        .into_iter()
        .collect(),
        seq: generation,
        ..herdrx::Metadata::default()
    }
}

/// Clears only the lifetime claim paired with one startup generation.
pub fn clear_active_metadata(pane_id: &str, generation: u64) -> herdrx::Metadata {
    herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(ACTIVE_SOURCE.to_string()),
        tokens: [(ACTIVE_TOKEN.to_string(), String::new())]
            .into_iter()
            .collect(),
        seq: generation.saturating_add(1),
        ..herdrx::Metadata::default()
    }
}

/// A short server-clocked lease for a request-labeled tab. If the client dies
/// before writing the persistent reusable state, the tab is recoverable only
/// until this contribution expires.
pub fn tab_creation_lease(pane_id: &str, request_label: &str) -> herdrx::Metadata {
    herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(TAB_CREATING_SOURCE.to_string()),
        tokens: [(TAB_CREATING_TOKEN.to_string(), request_label.to_string())]
            .into_iter()
            .collect(),
        ttl: Some(STARTING_TTL),
        ..herdrx::Metadata::default()
    }
}

/// Whether this tower was launched into a shell by `dock`.
pub fn is_dock_launch<F>(env: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    env(DOCK_ENV).as_deref() == Some("1")
}

/// Whether herdr's host-side process observer sees this exact tower binary.
///
/// `argv` is preferred because another `herdr-tower` subcommand can briefly
/// have the same process name. The name/argv0 fallback is for platforms where
/// herdr cannot recover the full argument vector.
pub fn is_tower_process(info: &ProcessInfo, executable: &Path) -> bool {
    let Some(wanted) = executable.file_name() else {
        return false;
    };
    info.foreground_processes.iter().any(|process| {
        if let Some(program) = process.argv.first() {
            return process.argv.len() == 1 && Path::new(program).file_name() == Some(wanted);
        }
        Path::new(&process.argv0).file_name() == Some(wanted)
            || Path::new(&process.name).file_name() == Some(wanted)
    })
}

/// [`is_tower_process`] as an observation. Windows has no foreground process
/// group, and herdr reports the pane's shell there however busy it is — the
/// tower running in it is never listed — so there the shell's children are
/// read from the process table; where even that fails the answer is "could
/// not tell", and callers fall back to the tower's `dock-active` token.
pub fn tower_observed(info: &ProcessInfo, executable: &Path) -> Option<bool> {
    if is_tower_process(info, executable) {
        return Some(true);
    }
    if !cfg!(windows) {
        return Some(false);
    }
    // The pane's shell's own children, from the process table — what herdr
    // cannot say there. A tower started in the shell is one of them.
    let wanted = executable.file_name()?.to_string_lossy().to_lowercase();
    let pid = u32::try_from(info.shell_pid).ok().filter(|p| *p > 0)?;
    tower_os::child_names(pid).map(|names| names.iter().any(|n| n.to_lowercase() == wanted))
}

/// Whether a failed socket exchange may still have delivered its request.
///
/// A structured API rejection is definitive. An I/O or decoding failure can
/// happen after the server accepted the command but before its reply reached
/// us, so its short server-side lease must be allowed to expire naturally.
pub fn delivery_is_uncertain(error: &herdrx::Error) -> bool {
    matches!(error, herdrx::Error::Io(_) | herdrx::Error::Decode(_))
}

/// What `dock` has to do, decided from what herdr reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// No public or request-labeled dock workspace exists.
    Create,
    /// Publish the canonical request-labeled workspace under [`LABEL`].
    Publish {
        workspace_id: String,
        request_label: String,
    },
    /// Recover a strict request label whose durable creation proof is absent.
    Claim {
        workspace_id: String,
        request_label: String,
    },
    /// The workspace is there with no shell worth reusing.
    NewTab { workspace_id: String },
    /// Type the tower into this shell pane.
    Launch {
        pane_id: String,
        terminal_id: String,
    },
    /// A command was accepted for this shell but has not yet claimed it.
    Starting {
        pane_id: String,
        workspace_id: String,
    },
    /// A tower already holds its lock in this pane.
    Running {
        pane_id: String,
        workspace_id: String,
    },
}

/// Decides what to do. `running` says whether a tower holds a terminal's lock,
/// `starting` whether an accepted launch has not finished yet, and `reusable`
/// whether the terminal is a shell previously reserved for the dock.
/// `shell_ready` is the host-side observation that the shell itself, rather
/// than an editor or another foreground program, currently owns the terminal.
///
/// Typing a command into a pane is only safe where a shell is waiting, and
/// nothing herdr reports says that. So the pane is picked by what is known about
/// its terminal: one reserved for a tower before (which either failed to start
/// or quit and left its shell), or the only pane of the workspace — which is
/// what a server restart leaves.
/// Anything else gets a tab of its own rather than a command typed into
/// somebody's editor. A pane whose terminal herdr does not name cannot be
/// vouched for either way, so it is never typed into.
pub fn plan(
    workspaces: &[Workspace],
    panes: &[Pane],
    running: impl Fn(&str) -> bool,
    starting: impl Fn(&str) -> bool,
    reusable: impl Fn(&str) -> bool,
    shell_ready: impl Fn(&str) -> bool,
) -> Plan {
    let Some(ws) = dock_workspace(workspaces) else {
        return Plan::Create;
    };
    let mut dock_panes: Vec<&Pane> = panes
        .iter()
        .filter(|pane| {
            workspaces.iter().any(|workspace| {
                workspace.label == LABEL && workspace.workspace_id == pane.workspace_id
            })
        })
        .collect();
    dock_panes.sort_by_key(|pane| (pane.workspace_id.as_str(), pane.tab_id.as_str()));
    // The usage board runs the same binary, and where herdr cannot report a
    // process's arguments it looks like the tower: its pane is never one.
    let known = |p: &&&Pane| {
        !p.terminal_id.is_empty()
            && !crate::usagetab::is_usage_pane(p)
            && !crate::boardwatch::is_board_pane(p)
    };
    // A stale losing candidate from an interrupted older run must never cause
    // a second tower. Observe running/starting towers across every reserved
    // dock workspace before choosing the canonical shell for a new launch.
    if let Some(p) = dock_panes
        .iter()
        .filter(known)
        .find(|p| running(&p.terminal_id))
    {
        return Plan::Running {
            pane_id: p.pane_id.clone(),
            workspace_id: p.workspace_id.clone(),
        };
    }
    if let Some(p) = dock_panes
        .iter()
        .filter(known)
        .find(|p| !p.has_agent() && starting(&p.terminal_id))
    {
        return Plan::Starting {
            pane_id: p.pane_id.clone(),
            workspace_id: p.workspace_id.clone(),
        };
    }
    if ws.label != LABEL {
        let candidate = if is_workspace_creation_candidate(ws) {
            Plan::Publish {
                workspace_id: ws.workspace_id.clone(),
                request_label: ws.label.clone(),
            }
        } else {
            Plan::Claim {
                workspace_id: ws.workspace_id.clone(),
                request_label: ws.label.clone(),
            }
        };
        return candidate;
    }
    let mut mine: Vec<&Pane> = panes
        .iter()
        .filter(|p| p.workspace_id == ws.workspace_id)
        .collect();
    // Stable, so panes inside one tab keep herdr's order.
    mine.sort_by_key(|pane| pane.tab_id.as_str());
    let reusable = mine
        .iter()
        .filter(known)
        .find(|p| !p.has_agent() && reusable(&p.terminal_id) && shell_ready(&p.terminal_id))
        .or_else(|| match mine.as_slice() {
            [only]
                if !only.has_agent()
                    && !only.terminal_id.is_empty()
                    && shell_ready(&only.terminal_id) =>
            {
                Some(only)
            }
            _ => None,
        });
    match reusable {
        Some(p) => Plan::Launch {
            pane_id: p.pane_id.clone(),
            terminal_id: p.terminal_id.clone(),
        },
        None => Plan::NewTab {
            workspace_id: ws.workspace_id.clone(),
        },
    }
}

/// The dock's own shells, brought back by a herdr restart: in the published
/// dock workspace, each shell that is the only pane of a tab still wearing
/// [`TAB_LABEL`] and carries no dock metadata at all. The one to use first:
/// the shell of the tab the workspace shows, then in herdr's tab order.
///
/// herdr restores a workspace, its tabs and their names, and starts a fresh
/// shell in each pane — under a new terminal ID, and with none of the
/// metadata, since herdr keeps neither across a restart (its session file has
/// no terminal IDs and no tokens). So nothing terminal-bound vouches for this
/// shell: the claim file and the never-submitted token both named the old
/// terminal. The tab's name does. Only `dock` and the tower put [`TAB_LABEL`]
/// on a tab, and herdr persists it (`custom_name`).
///
/// 🚨 Only until a `dock` has coordinated in the workspace since the restart,
/// which leaves dock tokens on it for the rest of the session. The proof is
/// that nothing has happened since herdr brought these shells back; once a
/// tower has been launched, a spare dock tab or a shell split off in one may
/// be someone's, and is judged by its own trace like any other pane. Without
/// that bound, a spare tab outranked the tower's own traced pane on its next
/// launch, and a person's shell in it was typed into.
///
/// Narrow on purpose. The only pane of its tab, because a second shell in a
/// dock tab is someone's own. No `dock-*` token on the pane, because that is
/// what "just restored" looks like. Every such tab counts, not just a lone
/// one: a dock an earlier restart left with two tabs has two restored shells,
/// and proving neither built a third tab on every restart (#41).
///
/// The tab the workspace shows comes first because that is where a click on
/// the dock lands — a tower launched into another tab is hidden behind an
/// empty shell.
pub fn restored_dock_shells<'a>(
    workspaces: &[Workspace],
    tabs: &[Tab],
    panes: &'a [Pane],
) -> Vec<&'a Pane> {
    let Some(workspace) = dock_workspace(workspaces).filter(|w| w.label == LABEL) else {
        return Vec::new();
    };
    if has_dock_token(&workspace.tokens) {
        return Vec::new();
    }
    let mut found: Vec<(bool, &Pane)> = tabs
        .iter()
        // The typed reader removes workspace display prefixes. Also accept
        // a legacy tab/pane prefix when recovering the dock after a restart.
        .filter(|tab| herdrx::strip_id_prefix(&tab.label) == TAB_LABEL)
        .filter_map(|tab| {
            let mut in_tab = panes.iter().filter(|pane| pane.tab_id == tab.tab_id);
            let only = in_tab.next()?;
            if in_tab.next().is_some()
                || only.workspace_id != workspace.workspace_id
                || only.has_agent()
                || only.terminal_id.is_empty()
                || has_dock_token(&only.tokens)
            {
                return None;
            }
            let shown = workspace.active_tab_id.as_deref() == Some(tab.tab_id.as_str());
            Some((!shown, only))
        })
        .collect();
    // Stable: after the shown tab, herdr's own order.
    found.sort_by_key(|(hidden, _)| *hidden);
    found.into_iter().map(|(_, pane)| pane).collect()
}

/// The one restored shell `dock` offers its plan ([`restored_dock_shells`]
/// gives them in order).
///
/// Before the one wait on it, the first in line, idle or not: the shells all
/// start together, and one that happens to be probed later is not the one to
/// prefer over the tab the dock shows. After the wait, the first in line that
/// is an idle shell, so a slow `.zshrc` in the shown tab does not cost a new
/// tab while another dock tab's shell is ready.
pub fn restored_to_offer<'a>(
    restored: &[&'a Pane],
    idle: impl Fn(&str) -> bool,
    waited: bool,
) -> Option<&'a Pane> {
    let first = restored.first().copied();
    if !waited {
        return first;
    }
    restored
        .iter()
        .copied()
        .find(|pane| idle(&pane.terminal_id))
        .or(first)
}

fn has_dock_token(tokens: &std::collections::HashMap<String, String>) -> bool {
    tokens.keys().any(|key| key.starts_with(TOKEN_PREFIX))
}

/// The dock's workspace, when it is not already first in the sidebar.
pub fn to_pin(workspaces: &[Workspace]) -> Option<String> {
    let w = dock_workspace(workspaces)?;
    if w.label != LABEL {
        return None;
    }
    to_pin_workspace(workspaces, &w.workspace_id)
}

/// A specific dock workspace that should move to the top of the sidebar.
pub fn to_pin_workspace(workspaces: &[Workspace], workspace_id: &str) -> Option<String> {
    workspaces
        .iter()
        .any(|workspace| workspace.workspace_id == workspace_id && workspace.label == LABEL)
        .then(|| workspace_id.to_string())
        .filter(|workspace_id| tower_ui::needs_pin(workspaces, workspace_id))
}

/// The herdr pane this process runs in.
pub fn own_pane<F>(env: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    env("HERDR_PANE_ID").filter(|v| !v.is_empty())
}

/// `s` as one file name: letters, digits, `-` and `.` kept, every other byte
/// written as `_` and two hex digits. `_` itself is escaped, so two different
/// inputs never share a name.
fn file_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}

/// FNV-1a, 64 bits. Spelled out because the name it produces is on disk and has
/// to come out the same from every build — `DefaultHasher` promises no such
/// thing across compiler releases, and a tower and a `dock` from different
/// builds would then look for each other in different directories.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The directory holding the locks of towers attached to the herdr server at
/// `socket`.
///
/// Under the caller-writable `root`, keyed by the herdr server at `socket`.
///
/// `root` must be independent of `HERDR_TOWER_STATE_DIR`: choosing another
/// record store must not hide a tower still attached to this server. The socket
/// is canonicalized so the spelling inherited by a pane and the spelling used
/// by `dock` agree. Its device/inode distinguishes two mount namespaces that
/// expose different servers at the same pathname; the pathname is the fallback
/// before a socket exists. The fixed-length hash also keeps a long socket path
/// below the file system's component limit.
fn socket_path_key(socket: &Path) -> u64 {
    let real = fs::canonicalize(socket).unwrap_or_else(|_| socket.to_path_buf());
    fnv1a64(real.as_os_str().as_encoded_bytes())
}

pub fn server_dir(root: &Path, socket: &Path) -> PathBuf {
    let real = fs::canonicalize(socket).unwrap_or_else(|_| socket.to_path_buf());
    let mut identity = real.as_os_str().as_encoded_bytes().to_vec();
    if let Ok((dev, ino)) = tower_os::path_identity(&real) {
        identity.push(0);
        identity.extend_from_slice(&dev.to_le_bytes());
        identity.extend_from_slice(&ino.to_le_bytes());
    }
    let key = fnv1a64(&identity);
    root.join(format!("{key:016x}"))
}

/// One server's locks under a state directory.
///
/// The second local root beside the runtime directory, and the only one where
/// that is unavailable — a read-only `/tmp`. Clients that share a home but not
/// a `/tmp` still meet here.
pub fn state_server_dir(state_dir: &Path, socket: &Path) -> PathBuf {
    state_dir
        .join("towers")
        .join(format!("{:016x}", socket_path_key(socket)))
}

/// The runtime directory followed by the state directory's.
///
/// The runtime root comes first so every caller agrees on the first lock
/// before it reaches the second.
pub fn server_dirs(coordination_root: &Path, selected_state: &Path, socket: &Path) -> Vec<PathBuf> {
    let current = server_dir(coordination_root, socket);
    let state = state_server_dir(selected_state, socket);
    if current == state {
        vec![current]
    } else {
        vec![current, state]
    }
}

/// The local candidates when the fixed runtime directory is unavailable.
///
/// An optional fast path. Server metadata and process observation are the
/// cross-namespace authority, so a read-only `/tmp` must not prevent a socket
/// client from docking.
pub fn server_dirs_without_runtime(selected_state: &Path, socket: &Path) -> Vec<PathBuf> {
    vec![state_server_dir(selected_state, socket)]
}

/// Creates one server's coordination directory.
fn make_server_dir(server_dir: &Path) -> io::Result<()> {
    tower_os::create_private_dir(server_dir)
}

/// Opens one coordination lock without exposing it to other local accounts.
fn open_lock(path: &Path) -> io::Result<File> {
    let f = tower_os::private(OpenOptions::new().create(true).write(true).truncate(false))
        .open(path)?;
    // A previous build may already have created this file under a wider umask.
    tower_os::set_file_mode(&f, 0o600)?;
    Ok(f)
}

/// Where the lock for a tower in `terminal_id` lives, under [`server_dir`].
pub fn lock_path(server_dir: &Path, terminal_id: &str) -> PathBuf {
    server_dir.join(format!("{}.lock", file_name(terminal_id)))
}

/// Serializes `dock` runs that share this filesystem, from plan to confirmation.
///
/// Two runs that planned at once would both create a workspace, or both type
/// the tower into the same shell before either tower had claimed it. Blocks
/// until the other run is done; released when dropped.
pub fn exclusive(server_dir: &Path) -> io::Result<Held> {
    let f = exclusive_file(server_dir)?;
    tower_os::lock(&f)?;
    Ok(Held(f))
}

/// Tries to lock the directory that exposes the Herdr socket.
///
/// Connecting to one filesystem socket already requires every local client to
/// reach that server-visible mount. Locking the directory itself needs no
/// sibling lock file and therefore still works when the mount is read-only.
/// Unlike a lease attached to a pane, this gate cannot move when panes are
/// created, closed, reused, or moved between tabs. It remains optional and
/// therefore never waits for a stopped or wedged holder; the recoverable
/// server-side coordination path handles contention instead.
pub fn exclusive_socket_parent(socket: &Path) -> io::Result<Held> {
    let parent = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("the herdr socket has no parent directory"))?;
    let f = File::open(parent)?;
    match f.try_lock() {
        Ok(()) => Ok(Held(f)),
        Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the Herdr socket directory is already coordinated",
        )),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn exclusive_file(server_dir: &Path) -> io::Result<File> {
    make_server_dir(server_dir)?;
    open_lock(&server_dir.join("dock.lock"))
}

/// Serializes `dock` runs on every local root.
///
/// [`server_dirs`] puts the one shared runtime directory first. Every caller
/// therefore agrees on the first lock before it reaches the state root, which
/// also gives all callers one deterministic lock order. A later root that this
/// user cannot create or open is skipped, since nothing running as this user
/// can hold a lock there.
///
/// File identity is checked before each `flock`. A selected state root may be a
/// symlink or a case-insensitive spelling of the ordinary root, and locking the
/// same open file description twice would deadlock this process against itself.
pub fn exclusive_in(server_dirs: &[PathBuf]) -> io::Result<Vec<Held>> {
    let mut held = Vec::with_capacity(server_dirs.len());
    let mut identities = Vec::with_capacity(server_dirs.len());
    for (index, dir) in server_dirs.iter().enumerate() {
        let f = match exclusive_file(dir) {
            Ok(f) => f,
            Err(_) if index > 0 => continue,
            Err(e) => return Err(e),
        };
        let identity = tower_os::file_identity(&f)?;
        if identities.contains(&identity) {
            continue;
        }
        tower_os::lock(&f)?;
        identities.push(identity);
        held.push(Held(f));
    }
    Ok(held)
}

/// Tries every caller-writable local candidate and skips unavailable roots.
///
/// This is deliberately best effort for current `dock`: ordered server
/// metadata serializes clients in different mount namespaces, while these
/// locks only provide a fast path. A container with read-only `/tmp` may still have a writable state
/// directory, or may have no shared writable directory at all.
pub fn exclusive_available_in(server_dirs: &[PathBuf]) -> LocalClaim {
    let files = ordered_available_lock_files(server_dirs);
    let mut held = Vec::with_capacity(files.len());
    for (_, f) in files {
        match hold_file(f) {
            Ok(Some(claim)) => held.push(claim),
            Ok(None) => return LocalClaim::Contended,
            Err(_) => {}
        }
    }
    if held.is_empty() {
        LocalClaim::Unavailable
    } else {
        LocalClaim::Acquired(held)
    }
}

type FileIdentity = (u64, u64);

fn ordered_available_lock_files(server_dirs: &[PathBuf]) -> Vec<(FileIdentity, File)> {
    let mut files = Vec::with_capacity(server_dirs.len());
    let mut identities = Vec::with_capacity(server_dirs.len());
    for dir in server_dirs {
        let Ok(f) = exclusive_file(dir) else {
            continue;
        };
        let Ok(identity) = tower_os::file_identity(&f) else {
            continue;
        };
        if identities.contains(&identity) {
            continue;
        }
        identities.push(identity);
        files.push((identity, f));
    }
    // Two containers can name the same pair of roots in opposite orders. Blocking while holding caller-order
    // locks deadlocks those clients before they reach the server gate. Device
    // and inode identify the shared lock files independently of spelling.
    files.sort_by_key(|(identity, _)| *identity);
    files
}

/// Finds the one tab an ambiguous `tab.create` added.
///
/// Terminal IDs survive pane-coordinate reuse and moves, so a pane is new only
/// when its terminal was absent from the pre-call snapshot. Refuse zero or
/// several candidates: adopting an unrelated shell would be worse than leaving
/// the bounded coordination lease to expire.
pub fn reconcile_created_tab(
    before: &[Pane],
    after_tabs: &[herdrx::Tab],
    after_panes: &[Pane],
    workspace_id: &str,
    request_label: &str,
) -> Option<Pane> {
    let mut tabs = after_tabs.iter().filter(|tab| {
        tab.workspace_id == workspace_id
            && tab.label == request_label
            && !before.iter().any(|pane| pane.tab_id == tab.tab_id)
    });
    let tab = tabs.next()?;
    if tabs.next().is_some() {
        return None;
    }
    let mut panes = after_panes.iter().filter(|pane| {
        pane.tab_id == tab.tab_id
            && !pane.terminal_id.is_empty()
            && !pane.has_agent()
            && !before.iter().any(|old| old.terminal_id == pane.terminal_id)
    });
    let only = panes.next()?.clone();
    panes.next().is_none().then_some(only)
}

/// The one workspace carrying a request-specific creation label.
pub fn workspace_for_request<'a>(
    workspaces: &'a [Workspace],
    request_label: &str,
) -> Option<&'a Workspace> {
    let mut matching = workspaces
        .iter()
        .filter(|workspace| workspace.label == request_label);
    let only = matching.next()?;
    matching.next().is_none().then_some(only)
}

/// A held lock, released when dropped or when the process ends.
#[derive(Debug)]
pub struct Held(#[allow(dead_code)] File);

const TOWER_CLAIM_MARKER: &[u8] = b"tower-claimed-v1\n";

fn mark_tower_claim(claim: &mut Held) -> io::Result<()> {
    claim.0.set_len(0)?;
    claim.0.write_all(TOWER_CLAIM_MARKER)?;
    Ok(())
}

/// Result of attempting the namespace-local lifetime claim.
#[derive(Debug)]
pub enum LocalClaim {
    Acquired(Vec<Held>),
    Contended,
    Unavailable,
}

/// Takes the lock for a tower running in `terminal_id`.
///
/// `None` when another process holds it. A few short retries first, because
/// [`is_held`] takes the lock for an instant to test it: a tower starting in
/// that instant would otherwise run without its claim, and the next `dock`
/// would type a command into the tower itself.
pub fn hold(server_dir: &Path, terminal_id: &str) -> io::Result<Option<Held>> {
    let path = lock_path(server_dir, terminal_id);
    make_server_dir(server_dir)?;
    let f = open_lock(&path)?;
    let Some(mut claim) = hold_file(f)? else {
        return Ok(None);
    };
    // The marker controls later reuse; the flock controls the live tower. A
    // full filesystem must not make us drop an already-acquired lifetime lock.
    let _ = mark_tower_claim(&mut claim);
    Ok(Some(claim))
}

fn hold_file(f: File) -> io::Result<Option<Held>> {
    for _ in 0..20 {
        if tower_os::try_lock(&f)? {
            return Ok(Some(Held(f)));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(None)
}

/// Claims a tower terminal in every namespace-local root.
///
/// Unavailable roots are irrelevant in this namespace. A root that is
/// available but already locked is different: another tower owns it, so all
/// locks acquired by this attempt are released and the claim fails closed.
pub fn hold_available_in(server_dirs: &[PathBuf], terminal_id: &str) -> LocalClaim {
    let mut files = Vec::with_capacity(server_dirs.len());
    let mut identities = Vec::with_capacity(server_dirs.len());
    for dir in server_dirs {
        let path = lock_path(dir, terminal_id);
        let Ok(()) = make_server_dir(dir) else {
            continue;
        };
        let Ok(file) = open_lock(&path) else {
            continue;
        };
        let Ok(identity) = tower_os::file_identity(&file) else {
            continue;
        };
        if identities.contains(&identity) {
            continue;
        }
        identities.push(identity);
        files.push((identity, file));
    }
    // Two namespace-visible roots can be presented in opposite lexical order,
    // and aliases can name the same lock inode. Deduplicate and then use the
    // same caller-independent order as the invocation-wide dock locks.
    files.sort_by_key(|(identity, _)| *identity);

    let mut claims = Vec::with_capacity(files.len());
    for (_, file) in files {
        match hold_file(file) {
            Ok(Some(mut claim)) => {
                // Keep every acquired lifetime lock even when the optional
                // post-exit reuse marker cannot be written. Its absence makes
                // later reuse conservative; releasing this flock would make a
                // live tower invisible to older local dock clients.
                let _ = mark_tower_claim(&mut claim);
                claims.push(claim);
            }
            Ok(None) => return LocalClaim::Contended,
            Err(_) => {}
        }
    }
    if claims.is_empty() {
        LocalClaim::Unavailable
    } else {
        LocalClaim::Acquired(claims)
    }
}

/// Whether a tower holds the lock for `terminal_id` right now.
pub fn is_held(server_dir: &Path, terminal_id: &str) -> bool {
    let Ok(f) = OpenOptions::new()
        .write(true)
        .open(lock_path(server_dir, terminal_id))
    else {
        return false;
    };
    // `tower_os`, as every holder locks it: the same byte on Windows.
    tower_os::try_lock(&f).is_ok_and(|got| !got)
}

/// Whether any local root shows a running tower.
pub fn is_held_in(server_dirs: &[PathBuf], terminal_id: &str) -> bool {
    server_dirs.iter().any(|d| is_held(d, terminal_id))
}

/// Waits for a launched tower to publish evidence that initialization finished.
///
/// An early process observation or lifetime flock is not enough: both precede
/// the fallible server-side active claim. The caller supplies an observation of
/// that later readiness marker and may return the pane carrying it.
pub fn wait_for_ready<T>(
    within: Duration,
    mut observe_ready: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Some(ready) = observe_ready() {
            return Some(ready);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether `terminal_id` is a shell reserved for this server's dock.
pub fn is_reusable(server_dir: &Path, terminal_id: &str) -> bool {
    lock_path(server_dir, terminal_id).exists()
}

/// Whether any local root reserves this terminal for a dock.
pub fn is_reusable_in(server_dirs: &[PathBuf], terminal_id: &str) -> bool {
    server_dirs.iter().any(|d| is_reusable(d, terminal_id))
}

/// Whether this namespace observed a tower actually claim the terminal.
///
/// `dock` itself creates an empty retry file before command submission. Only
/// the tower writes this marker while holding the lifetime lock, so a client in
/// another mount namespace cannot mistake its own pre-launch trace for proof
/// that it can observe a suspended tower's lock.
pub fn was_claimed_in(server_dirs: &[PathBuf], terminal_id: &str) -> bool {
    server_dirs.iter().any(|server_dir| {
        fs::read(lock_path(server_dir, terminal_id))
            .is_ok_and(|contents| contents == TOWER_CLAIM_MARKER)
    })
}

/// Leaves the trace that makes `terminal_id` safe for a later dock retry.
///
/// A replacement tab is an empty shell made by `dock` itself, but a transient
/// failure after its creation can happen before the tower claims its lock. The
/// trace is therefore written immediately after `tab.create`; on the next run,
/// [`plan`] can reuse that known shell instead of adding another empty tab.
pub fn remember_reusable(server_dir: &Path, terminal_id: &str) -> io::Result<()> {
    let path = lock_path(server_dir, terminal_id);
    make_server_dir(server_dir)?;
    open_lock(&path).map(|_| ())
}

/// `path` as one word for a POSIX shell.
pub fn sh_quote(path: &str) -> String {
    if !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b))
    {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// `value` as one single-quoted PowerShell word: `'` is doubled.
pub fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A line for a pane's shell that runs `program` with `args` and the
/// environment `vars`.
///
/// Through `env` on Unix, so it works in shells whose own assignment syntax
/// differs. On Windows a herdr pane's shell is PowerShell, which has no `env`:
/// the variables are set in the pane's session and the program run with `&`,
/// the call operator a quoted path needs there. The pane is the tower's own,
/// so the variables staying set in it costs nothing.
pub fn shell_line(vars: &[(&str, &str)], program: &str, args: &[&str]) -> String {
    let mut line = String::new();
    if cfg!(windows) {
        for (name, value) in vars {
            line.push_str(&format!("$env:{name}={}; ", ps_quote(value)));
        }
        line.push_str(&format!("& {}", ps_quote(program)));
        for a in args {
            line.push(' ');
            line.push_str(&ps_quote(a));
        }
        return line;
    }
    line.push_str("env");
    for (name, value) in vars {
        line.push_str(&format!(" {name}={}", sh_quote(value)));
    }
    line.push(' ');
    line.push_str(&sh_quote(program));
    for a in args {
        line.push(' ');
        line.push_str(&sh_quote(a));
    }
    line
}

/// The line typed into the dock's shell.
///
/// The state and configuration directories travel with it. The shell belongs
/// to the pane, not to whoever ran `dock`, so a store chosen with
/// `HERDR_TOWER_STATE_DIR` for a named server would otherwise be lost on the
/// way — the tower would open the default store, write the named server's panes
/// into it — and a `HERDR_TOWER_CONFIG` would silently fall back to the default
/// settings.
/// The pane resolves its local lock in its own mount namespace;
/// passing the client's pathname made socket-only containers point the host
/// shell at an unrelated or nonexistent directory.
/// Every path must already be absolute: the pane's shell starts somewhere else.
/// Through `env` so it works in shells whose own assignment syntax differs.
pub fn launch_command(
    exe: &str,
    state_dir: &Path,
    config: Option<&Path>,
    lease_value: &str,
    terminal_id: &str,
) -> String {
    let state = state_dir.to_string_lossy();
    let config = config.map(|c| c.to_string_lossy());
    let mut vars = vec![
        (DOCK_ENV, "1"),
        (DOCK_LEASE_ENV, lease_value),
        (DOCK_TERMINAL_ENV, terminal_id),
        ("HERDR_TOWER_STATE_DIR", &*state),
    ];
    if let Some(c) = &config {
        vars.push(("HERDR_TOWER_CONFIG", c));
    }
    shell_line(&vars, exe, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Waits for a just-released lock to be seen as released, and returns
    /// what `probe` got once it was.
    ///
    /// 🚨 A release is not instant in a test process. On macOS a child spawned
    /// by any other thread — and the tests in this binary run `git` and `sh` —
    /// briefly holds a copy of every open file in the process, `O_CLOEXEC`
    /// ones included, until its spawn completes. A lock dropped in that instant
    /// is still held until then. Measured in a scratch A/B (8s per arm) of
    /// drop-then-relock cycles: 0 of 472,935 found the lock still held with no
    /// children being spawned; 1.5% did with fork+exec children and 12.7% with
    /// `posix_spawn` ones. So these tests assert the property that holds — the
    /// lock is released *soon* — and a lock that is never released still fails
    /// them, after the deadline.
    ///
    /// The product is not affected the same way: a lock outliving its holder by
    /// the length of a spawn is invisible to a dock that retries anyway.
    fn soon<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(got) = probe() {
                return got;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a released lock was still held 5s later"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn ws(id: &str, label: &str) -> Workspace {
        serde_json::from_value(serde_json::json!({ "workspace_id": id, "label": label })).unwrap()
    }

    fn numbered_ws(id: &str, label: &str, number: u32) -> Workspace {
        serde_json::from_value(
            serde_json::json!({ "workspace_id": id, "label": label, "number": number }),
        )
        .unwrap()
    }

    fn candidate_ws(id: &str, label: &str, number: u32) -> Workspace {
        let mut workspace = numbered_ws(id, label, number);
        workspace.tokens.insert(CREATED_TOKEN.into(), label.into());
        workspace
    }

    /// A pane whose terminal is `term-<pane id>`.
    fn pane(id: &str, tab: &str, agent: Option<&str>) -> Pane {
        let ws = id.split(':').next().unwrap();
        serde_json::from_value(serde_json::json!({
            "pane_id": id, "tab_id": tab, "workspace_id": ws, "agent": agent,
            "terminal_id": format!("term-{id}"),
        }))
        .unwrap()
    }

    fn tab(id: &str, workspace: &str, label: &str) -> herdrx::Tab {
        serde_json::from_value(serde_json::json!({
            "tab_id": id, "workspace_id": workspace, "label": label,
        }))
        .unwrap()
    }

    fn never(_: &str) -> bool {
        false
    }

    fn always(_: &str) -> bool {
        true
    }

    #[test]
    fn no_workspace_by_that_label_is_created() {
        let spaces = [ws("w8", "20.herdr-tower")];
        let panes = [pane("w8:p2", "w8:t1", None)];
        assert_eq!(
            plan(&spaces, &panes, never, never, never, always),
            Plan::Create
        );
    }

    #[test]
    fn an_empty_server_gets_a_dock_created() {
        assert_eq!(plan(&[], &[], never, never, never, always), Plan::Create);
    }

    /// The label is bracketed so a workspace named after a checkout of this
    /// repository is never taken for the dock.
    #[test]
    fn a_workspace_named_after_the_repository_is_not_the_dock() {
        let spaces = [ws("w8", "herdr-tower")];
        let panes = [pane("w8:p2", "w8:t1", None)];
        assert_eq!(
            plan(&spaces, &panes, never, never, never, always),
            Plan::Create
        );
    }

    #[test]
    fn the_dock_is_pinned_only_when_it_is_not_first() {
        assert_eq!(
            to_pin(&[ws("w2", "00.dbstore"), ws("w9", LABEL)]),
            Some("w9".into())
        );
        assert_eq!(to_pin(&[ws("w9", LABEL), ws("w2", "00.dbstore")]), None);
        assert_eq!(to_pin(&[ws("w2", "00.dbstore")]), None);
        assert_eq!(
            to_pin_workspace(&[ws("wA", LABEL), ws("wB", LABEL)], "wB"),
            Some("wB".into()),
            "the workspace holding a tower wins over canonical selection"
        );
    }

    #[test]
    fn a_running_tower_is_left_alone() {
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p1", "w9:t1", None), pane("w9:p2", "w9:t2", None)];
        let got = plan(&spaces, &panes, |t| t == "term-w9:p2", never, never, always);
        assert_eq!(
            got,
            Plan::Running {
                pane_id: "w9:p2".into(),
                workspace_id: "w9".into(),
            }
        );
    }

    #[test]
    fn a_running_tower_in_a_losing_candidate_still_wins() {
        let spaces = [ws("wA", LABEL), ws("wB", LABEL)];
        let panes = [pane("wA:p1", "wA:t1", None), pane("wB:p1", "wB:t1", None)];
        let got = plan(&spaces, &panes, |t| t == "term-wB:p1", never, never, always);
        assert_eq!(
            got,
            Plan::Running {
                pane_id: "wB:p1".into(),
                workspace_id: "wB".into(),
            }
        );
    }

    fn restored_ids<'a>(spaces: &[Workspace], tabs: &[Tab], panes: &'a [Pane]) -> Vec<&'a str> {
        restored_dock_shells(spaces, tabs, panes)
            .into_iter()
            .map(|p| p.pane_id.as_str())
            .collect()
    }

    /// After a herdr restart the dock's shell has a new terminal ID and no
    /// metadata, so only its tab's persisted name says it is the dock's. The
    /// only shell of a labeled tab in the published dock is; a second pane in
    /// that tab (someone's own shell), an agent, a tab someone renamed, a
    /// terminal herdr does not name, a `dock-*` token (a shell the tower quit
    /// from, judged by its own trace), or a workspace that is not the
    /// published dock each withdraw the proof.
    #[test]
    fn a_restored_dock_shell_is_known_by_its_tab_name() {
        let spaces = [ws("w9", LABEL), ws("w2", "00.dbstore")];
        let dock_tab = [tab("w9:t1", "w9", TAB_LABEL)];
        let lone = [pane("w9:p7", "w9:t1", None)];
        assert_eq!(restored_ids(&spaces, &dock_tab, &lone), ["w9:p7"]);

        let two = [pane("w9:p7", "w9:t1", None), pane("w9:p8", "w9:t1", None)];
        let agent = [pane("w9:p7", "w9:t1", Some("claude"))];
        let mut unnamed = pane("w9:p7", "w9:t1", None);
        unnamed.terminal_id.clear();
        let unnamed = [unnamed];
        let mut traced = pane("w9:p7", "w9:t1", None);
        traced
            .tokens
            .insert(REUSABLE_TOKEN.into(), "submitted-v1|terminal=x".into());
        let traced = [traced];
        let renamed = [tab("w9:t1", "w9", "1")];
        let request = [candidate_ws(
            "w9",
            &workspace_request_label("100000000000-1"),
            1,
        )];
        assert!(
            dock_workspace(&request).is_some(),
            "a real candidate, so the published-label check is what refuses it"
        );
        for (spaces, tabs, panes, why) in [
            (&spaces[..], &dock_tab[..], &two[..], "two panes"),
            (&spaces[..], &dock_tab[..], &agent[..], "an agent"),
            (&spaces[..], &dock_tab[..], &unnamed[..], "no terminal ID"),
            (&spaces[..], &dock_tab[..], &traced[..], "a dock token"),
            (
                &spaces[..],
                &renamed[..],
                &lone[..],
                "a tab without the dock's name",
            ),
            (
                &request[..],
                &dock_tab[..],
                &lone[..],
                "an unpublished workspace",
            ),
            (&spaces[..], &[][..], &lone[..], "no tab listing"),
        ] {
            assert!(restored_ids(spaces, tabs, panes).is_empty(), "{why}");
        }
    }

    fn showing(id: &str, active_tab: &str) -> Workspace {
        serde_json::from_value(serde_json::json!({
            "workspace_id": id, "label": LABEL, "active_tab_id": active_tab,
        }))
        .unwrap()
    }

    fn dock_tab(id: &str) -> herdrx::Tab {
        tab(id, id.split(':').next().unwrap(), TAB_LABEL)
    }

    /// 🚨 #41. A dock an earlier restart already left with two tabs comes
    /// back with a restored shell in each. Proving only a lone one proved
    /// neither, and every restart built a third tab. Each labeled tab's only
    /// shell counts — while a tab someone opened in the dock workspace, and a
    /// shell split off inside a dock tab, are still left alone.
    #[test]
    fn every_labeled_dock_tab_gives_back_its_restored_shell() {
        let spaces = [ws("w9", LABEL)];
        let tabs = [
            dock_tab("w9:t1"),
            dock_tab("w9:t2"),
            tab("w9:t3", "w9", "notes"),
        ];
        let panes = [
            pane("w9:p8", "w9:t2", None),
            pane("w9:p7", "w9:t1", None),
            pane("w9:p9", "w9:t3", None),
        ];
        assert_eq!(restored_ids(&spaces, &tabs, &panes), ["w9:p7", "w9:p8"]);

        let split = [
            pane("w9:p7", "w9:t1", None),
            pane("w9:p8", "w9:t2", None),
            pane("w9:p6", "w9:t2", None),
        ];
        assert_eq!(
            restored_ids(&spaces, &tabs, &split),
            ["w9:p7"],
            "a dock tab with a second shell in it is someone's"
        );

        // What `run_dock` hands `plan` after a restart: nothing vouches for
        // either shell but the restored proof, and that is offered for one
        // shell only, in both predicates.
        let first = format!("term-{}", restored_ids(&spaces, &tabs, &panes)[0]);
        let offered = |t: &str| t == first;
        assert_eq!(
            plan(&spaces, &panes, never, never, offered, offered),
            Plan::Launch {
                pane_id: "w9:p7".into(),
                terminal_id: "term-w9:p7".into(),
            },
            "the tower goes back to the first tab, and no third one is built"
        );
        assert_eq!(
            plan(&spaces, &panes, never, never, never, offered),
            Plan::NewTab {
                workspace_id: "w9".into(),
            },
            "🚨 offered only as shell_ready, as #37 did, two tabs built a third"
        );
    }

    /// 🚨 The proof is "nothing has happened since the restart". Once a `dock`
    /// has coordinated in the workspace — its tokens are there for the rest
    /// of the session — no shell is restored any more: a spare dock tab, or a
    /// shell split off in one, may be someone's by then, and a spare tab must
    /// not outrank the tower's own traced pane.
    #[test]
    fn after_a_dock_has_run_since_the_restart_nothing_is_restored() {
        let tabs = [dock_tab("w9:t1"), dock_tab("w9:t2")];
        let panes = [pane("w9:p7", "w9:t1", None), pane("w9:p8", "w9:t2", None)];
        for token in [
            COORDINATING_TOKEN,
            COORDINATING_GENERATION_TOKEN,
            CREATED_TOKEN,
        ] {
            let mut coordinated = ws("w9", LABEL);
            coordinated.tokens.insert(token.into(), "x".into());
            assert!(
                restored_ids(&[coordinated], &tabs, &panes).is_empty(),
                "{token}"
            );
        }
    }

    /// The tower goes back to the tab a click on the dock shows — herdr's
    /// active tab — and otherwise to the first in herdr's own tab order, not
    /// by ID: `t12` sorts before `t9` as a string.
    #[test]
    fn the_restored_shell_first_in_line_is_the_one_the_dock_shows() {
        let tabs = [dock_tab("w9:t9"), dock_tab("w9:t12")];
        let panes = [pane("w9:p1", "w9:t12", None), pane("w9:p2", "w9:t9", None)];
        assert_eq!(
            restored_ids(&[ws("w9", LABEL)], &tabs, &panes),
            ["w9:p2", "w9:p1"],
            "in herdr's tab order"
        );
        assert_eq!(
            restored_ids(&[showing("w9", "w9:t12")], &tabs, &panes),
            ["w9:p1", "w9:p2"],
            "the tab the workspace shows comes first"
        );
        let null: Workspace = serde_json::from_value(serde_json::json!({
            "workspace_id": "w9", "label": LABEL, "active_tab_id": null,
        }))
        .unwrap();
        assert_eq!(
            restored_ids(&[null], &tabs, &panes),
            ["w9:p2", "w9:p1"],
            "a null active tab reads as none, and the listing still parses"
        );
    }

    /// Before the wait, the first in line whatever it is doing — the shells
    /// start together, and being probed later is no reason to be preferred
    /// over the shown tab. After it, the first idle one, else the first.
    #[test]
    fn the_shell_offered_is_the_first_until_the_wait_then_the_first_idle() {
        let panes = [pane("w9:p1", "w9:t1", None), pane("w9:p2", "w9:t2", None)];
        let restored: Vec<&Pane> = panes.iter().collect();
        let second_idle = |t: &str| t == "term-w9:p2";
        let offered = |idle: &dyn Fn(&str) -> bool, waited| {
            restored_to_offer(&restored, idle, waited).map(|p| p.pane_id.as_str())
        };
        assert_eq!(offered(&second_idle, false), Some("w9:p1"));
        assert_eq!(offered(&second_idle, true), Some("w9:p2"));
        assert_eq!(offered(&never, true), Some("w9:p1"));
        assert_eq!(
            restored_to_offer(&[], always, true).map(|p| p.pane_id.clone()),
            None
        );
    }

    /// What a server restart leaves: the workspace, one fresh shell, new IDs.
    #[test]
    fn a_lone_shell_is_where_the_tower_goes_back() {
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p7", "w9:t1", None)];
        assert_eq!(
            plan(&spaces, &panes, never, never, never, always),
            Plan::Launch {
                pane_id: "w9:p7".into(),
                terminal_id: "term-w9:p7".into(),
            }
        );
    }

    #[test]
    fn a_pane_that_ran_a_tower_is_reused_among_several() {
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p1", "w9:t1", None), pane("w9:p2", "w9:t1", None)];
        assert_eq!(
            plan(&spaces, &panes, never, never, |t| t == "term-w9:p2", always,),
            Plan::Launch {
                pane_id: "w9:p2".into(),
                terminal_id: "term-w9:p2".into(),
            }
        );
    }

    #[test]
    fn a_traced_pane_is_not_reused_while_another_program_is_foreground() {
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p1", "w9:t1", None), pane("w9:p2", "w9:t2", None)];
        assert_eq!(
            plan(&spaces, &panes, never, never, |_| true, never),
            Plan::NewTab {
                workspace_id: "w9".into(),
            }
        );

        assert_eq!(
            plan(&spaces, &panes[..1], never, never, |_| true, never),
            Plan::NewTab {
                workspace_id: "w9".into(),
            },
            "even the lone-pane restart fallback needs a foreground shell"
        );
    }

    /// The usage board's pane is never taken for a running or starting tower,
    /// even when the process check would say so (a name-only match), so the
    /// dock is never "found" in it and its tab is never renamed to the dock's.
    #[test]
    fn the_usage_board_is_not_the_running_tower() {
        let spaces = [ws("w9", LABEL)];
        let mut board = pane("w9:p2", "w9:t2", None);
        board
            .tokens
            .insert(crate::usagetab::TOKEN.into(), "1".into());
        let panes = [board];
        assert_eq!(
            plan(&spaces, &panes, always, always, never, never),
            Plan::NewTab {
                workspace_id: "w9".into()
            }
        );
        // The tower's own pane is still found beside it.
        let panes = [pane("w9:p1", "w9:t1", None), panes[0].clone()];
        assert_eq!(
            plan(&spaces, &panes, |t| t == "term-w9:p1", never, never, never),
            Plan::Running {
                pane_id: "w9:p1".into(),
                workspace_id: "w9".into()
            }
        );
    }

    /// herdr gives a closed pane's ID to a later pane. The trace belongs to the
    /// terminal, so the newcomer — a different terminal — is not vouched for.
    #[test]
    fn a_recycled_pane_id_inherits_no_trace() {
        let spaces = [ws("w9", LABEL)];
        let mut recycled = pane("w9:p1", "w9:t1", None);
        recycled.terminal_id = "term-new".into();
        let panes = [recycled, pane("w9:p2", "w9:t2", None)];
        let traces = |t: &str| t == "term-w9:p1"; // the old terminal's
        assert_eq!(
            plan(&spaces, &panes, never, never, traces, always),
            Plan::NewTab {
                workspace_id: "w9".into()
            }
        );
    }

    /// Nothing says which of two shells is sitting at a prompt, so neither gets
    /// a command typed into it.
    #[test]
    fn unknown_shells_get_a_tab_of_their_own_instead() {
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p1", "w9:t1", None), pane("w9:p2", "w9:t2", None)];
        assert_eq!(
            plan(&spaces, &panes, never, never, never, always),
            Plan::NewTab {
                workspace_id: "w9".into()
            }
        );
    }

    #[test]
    fn an_agent_pane_is_never_typed_into() {
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p1", "w9:t1", Some("claude"))];
        assert_eq!(
            plan(&spaces, &panes, never, never, |_| true, always),
            Plan::NewTab {
                workspace_id: "w9".into()
            }
        );
    }

    /// Without a terminal ID there is no lock to consult, so a tower running
    /// there would look like a free shell.
    #[test]
    fn a_pane_without_a_terminal_id_is_never_typed_into() {
        let spaces = [ws("w9", LABEL)];
        let mut p = pane("w9:p1", "w9:t1", None);
        p.terminal_id.clear();
        assert_eq!(
            plan(&spaces, &[p], never, never, |_| true, always),
            Plan::NewTab {
                workspace_id: "w9".into()
            }
        );
    }

    #[test]
    fn only_a_dock_launch_carries_the_marker() {
        let env = |value: Option<&'static str>| {
            move |key: &str| {
                (key == DOCK_ENV)
                    .then_some(value)
                    .flatten()
                    .map(str::to_string)
            }
        };
        assert!(is_dock_launch(env(Some("1"))));
        assert!(!is_dock_launch(env(Some("0"))));
        assert!(!is_dock_launch(env(Some(""))));
        assert!(!is_dock_launch(env(None)));
    }

    #[test]
    fn the_lock_is_held_while_the_claim_lives_and_not_after() {
        let d = tempfile::tempdir().unwrap();
        assert!(!is_held(d.path(), "term-a"));
        assert!(!is_reusable(d.path(), "term-a"));
        remember_reusable(d.path(), "term-a").unwrap();
        assert!(is_reusable(d.path(), "term-a"));
        assert!(
            !was_claimed_in(&[d.path().to_path_buf()], "term-a"),
            "a client-created retry trace is not a tower namespace proof"
        );

        let claim = hold(d.path(), "term-a").unwrap().expect("free lock");
        assert!(is_held(d.path(), "term-a"));
        assert!(was_claimed_in(&[d.path().to_path_buf()], "term-a"));
        assert!(
            hold(d.path(), "term-a").unwrap().is_none(),
            "a second tower in the same terminal does not get it"
        );
        assert!(!is_held(d.path(), "term-b"), "locks are per terminal");

        drop(claim);
        soon(|| (!is_held(d.path(), "term-a")).then_some(()));
        assert!(is_reusable(d.path(), "term-a"), "the file stays as a trace");
        assert!(was_claimed_in(&[d.path().to_path_buf()], "term-a"));
        if cfg!(unix) {
            let mode = |p: &Path| tower_os::file_mode(&fs::metadata(p).unwrap()).unwrap();
            assert_eq!(mode(d.path()) & 0o777, 0o700);
            assert_eq!(mode(&lock_path(d.path(), "term-a")) & 0o777, 0o600);
        }
    }

    #[test]
    fn a_marker_write_failure_keeps_the_acquired_lifetime_lock() {
        let d = tempfile::tempdir().unwrap();
        let path = lock_path(d.path(), "term-a");
        make_server_dir(d.path()).unwrap();
        fs::write(&path, b"").unwrap();
        let file = File::open(&path).unwrap();
        tower_os::lock(&file).unwrap();
        let mut claim = Held(file);

        assert!(mark_tower_claim(&mut claim).is_err());
        assert!(is_held(d.path(), "term-a"));
        drop(claim);
        soon(|| (!is_held(d.path(), "term-a")).then_some(()));
    }

    #[test]
    fn a_tower_claims_every_available_local_root() {
        let d = tempfile::tempdir().unwrap();
        let current = d.path().join("current");
        let state = d.path().join("state");
        let blocked_parent = d.path().join("not-a-directory");
        fs::write(&blocked_parent, b"x").unwrap();
        let unavailable = blocked_parent.join("server");

        let LocalClaim::Acquired(claims) = hold_available_in(
            &[current.clone(), unavailable.clone(), state.clone()],
            "term-a",
        ) else {
            panic!("available roots should be acquired");
        };
        assert_eq!(claims.len(), 2);
        assert!(is_held(&current, "term-a"));
        assert!(is_held(&state, "term-a"));
        drop(claims);

        let other = soon(|| hold(&state, "term-a").unwrap());
        assert!(matches!(
            hold_available_in(&[current.clone(), state], "term-a"),
            LocalClaim::Contended
        ));
        // `current` was released just above, by the contended attempt.
        let _ = soon(|| hold(&current, "term-a").unwrap());
        drop(other);

        assert!(matches!(
            hold_available_in(&[unavailable], "term-a"),
            LocalClaim::Unavailable
        ));
    }

    #[test]
    fn probing_a_lock_does_not_take_it_away_from_a_tower() {
        let d = tempfile::tempdir().unwrap();
        let _claim = hold(d.path(), "term-a").unwrap().unwrap();
        for _ in 0..3 {
            assert!(is_held(d.path(), "term-a"));
        }
    }

    #[test]
    fn a_launch_waits_for_its_delayed_ready_marker() {
        let mut observations = 0;
        let ready = wait_for_ready(Duration::from_secs(2), || {
            observations += 1;
            (observations == 2).then_some("moved-pane")
        });

        assert_eq!(ready, Some("moved-pane"));
        assert_eq!(observations, 2);
    }

    #[test]
    fn an_early_lifetime_lock_does_not_confirm_tower_initialization() {
        let d = tempfile::tempdir().unwrap();
        let _claim = hold(d.path(), "term-a").unwrap().unwrap();

        assert_eq!(wait_for_ready(Duration::ZERO, || None::<()>), None);
        assert!(is_held(d.path(), "term-a"));
    }

    #[test]
    fn only_the_argumentless_tower_process_counts_as_the_dock() {
        let info = |argv: serde_json::Value, argv0: &str, name: &str| {
            serde_json::from_value::<ProcessInfo>(serde_json::json!({
                "foreground_processes": [{ "argv": argv, "argv0": argv0, "name": name }]
            }))
            .unwrap()
        };
        let exe = Path::new("/new/install/herdr-tower");
        assert!(is_tower_process(
            &info(serde_json::json!(["/old/install/herdr-tower"]), "", ""),
            exe
        ));
        assert!(!is_tower_process(
            &info(
                serde_json::json!(["/old/install/herdr-tower", "status"]),
                "",
                ""
            ),
            exe
        ));
        assert!(is_tower_process(
            &info(serde_json::json!([]), "", "herdr-tower"),
            exe
        ));
        assert!(!is_tower_process(
            &info(serde_json::json!(["sh"]), "sh", "sh"),
            exe
        ));
    }

    #[test]
    fn only_an_ambiguous_exchange_keeps_the_startup_lease() {
        assert!(delivery_is_uncertain(&herdrx::Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "late reply"
        ))));
        assert!(delivery_is_uncertain(&herdrx::Error::Decode(
            serde_json::from_str::<serde_json::Value>("{").unwrap_err()
        )));
        assert!(!delivery_is_uncertain(&herdrx::Error::Api(
            herdrx::ApiError {
                code: "pane_not_found".into(),
                message: "gone".into(),
            }
        )));
        assert!(!delivery_is_uncertain(&herdrx::Error::NoSocket));
    }

    /// A request label is the dock label, `#`, and the request id — and a label
    /// that merely starts with the name is not one.
    #[test]
    fn a_request_label_carries_its_id() {
        let id = "100000000000-a";
        assert_eq!(workspace_request_id(&workspace_request_label(id)), Some(id));
        assert_eq!(workspace_request_id(&format!("{LABEL}#{id}")), Some(id));
        assert_eq!(workspace_request_id(LABEL), None, "the name alone");
        assert_eq!(workspace_request_id(&format!("{LABEL}#")), None, "no id");
        assert_eq!(
            workspace_request_id(&format!("{LABEL} #{id}")),
            None,
            "a stray space is not the name"
        );
        assert_eq!(
            workspace_request_id(&format!("[herdr-dock]#{id}")),
            None,
            "a name this build never gave the dock"
        );
        assert_eq!(workspace_request_id("20.herdr-tower#100000000000-a"), None);
    }

    #[test]
    fn generated_request_ids_remain_parseable_before_the_unix_epoch() {
        let request = request_id_value(0, 1);
        assert_eq!(request, "000000000000-1");
        assert_eq!(
            workspace_request_id(&workspace_request_label(&request)),
            Some(request.as_str())
        );
        assert_eq!(
            tab_request_id(&tab_request_label(&request)),
            Some(request.as_str())
        );
    }

    #[test]
    fn startup_and_coordination_leases_have_independent_durable_state() {
        assert!(
            STARTING_TTL - COORDINATION_FENCE_MARGIN < STARTING_TTL,
            "the kernel fence fires before server takeover"
        );
        let lease_value = pane_starting_lease_value(8, "contender-a", "term-a");
        let lease = starting_lease("w9:p2", &lease_value, 8);
        assert_eq!(lease.source.as_deref(), Some(STARTING_SOURCE));
        assert_eq!(lease.seq, 8);
        assert_eq!(lease.ttl, Some(STARTING_TTL));
        assert_eq!(
            lease.tokens[STARTING_TOKEN],
            "held:8:contender-a|terminal=term-a"
        );
        assert!(
            lease.tokens[STARTING_TOKEN]
                .chars()
                .all(|character| !character.is_control()),
            "metadata bindings must survive Herdr token normalization"
        );
        assert_eq!(pane_starting_lease_generation(&lease_value), Some(8));
        assert!(pane_starting_lease_is_held(&lease_value));
        assert!(pane_starting_lease_is_held_for(&lease_value, "term-a"));
        assert!(!pane_starting_lease_is_held_for(
            &lease_value,
            "replacement-term"
        ));

        let cleared = clear_starting_lease("w9:p2", 8);
        assert_eq!(cleared.source.as_deref(), Some(STARTING_SOURCE));
        assert_eq!(cleared.seq, 9, "the clear precedes the next coordinator");
        assert_eq!(cleared.tokens[STARTING_TOKEN], "idle:9");
        assert!(!pane_starting_lease_is_held("idle:9"));
        assert_eq!(
            next_pane_starting_generation(None, Some("idle:9"), "term-a"),
            Some(10)
        );
        assert_eq!(
            next_pane_starting_generation(None, Some(&lease_value), "term-a"),
            None
        );
        assert_eq!(
            next_pane_starting_generation(None, Some(&lease_value), "replacement-term"),
            Some(2),
            "a terminal-bound lease cannot block a replacement terminal"
        );
        assert_eq!(
            next_pane_starting_generation(None, Some("held:8:unbound"), "term-a"),
            None,
            "an unbound lease fails closed"
        );
        assert_eq!(next_pane_starting_generation(None, None, "term-a"), Some(2));
        assert_eq!(
            next_pane_starting_generation(Some("8:contender-a"), None, "term-a"),
            Some(10)
        );
        assert_eq!(
            next_pane_starting_generation(Some("broken"), None, "term-a"),
            None
        );
        let generation_value = pane_starting_generation_value(8, "contender-a", "term-a");
        assert_eq!(
            next_pane_starting_generation(Some(&generation_value), None, "term-a"),
            Some(10)
        );
        let generation = pane_starting_generation_claim("w9:p2", &generation_value, 8);
        assert_eq!(
            generation.source.as_deref(),
            Some(STARTING_GENERATION_SOURCE)
        );
        assert_eq!(
            generation.tokens[STARTING_GENERATION_TOKEN],
            generation_value
        );
        assert_eq!(generation.seq, 8);
        assert_eq!(
            generation.ttl, None,
            "the high-water mark survives TTL expiry"
        );
        let successor = starting_lease(
            "w9:p2",
            &pane_starting_lease_value(10, "contender-b", "term-a"),
            10,
        );
        assert!(
            cleared.seq < successor.seq,
            "a delayed old clear cannot overtake a successor lease"
        );
        assert!(!starting_lease_value().is_empty());

        assert_eq!(next_coordination_generation(None, None), Some(1));
        assert_eq!(next_coordination_generation(None, Some("idle:7")), Some(8));
        assert_eq!(
            next_coordination_generation(Some("8:owner-a"), None),
            Some(9)
        );
        assert_eq!(
            next_coordination_generation(Some("8:owner-a"), Some("idle:9")),
            Some(10),
            "a clean release advances beyond both visible sequence records"
        );
        assert_eq!(
            next_coordination_generation(Some("8:owner-a"), Some("idle:9")),
            Some(10),
            "a clean release can be newer than the held high-water marker"
        );
        assert_eq!(
            next_coordination_generation(Some("8:owner-a"), Some("held:8:owner-a")),
            None
        );
        assert_eq!(
            next_coordination_generation(None, Some("held:7:200:other-owner")),
            None,
            "an unrecognized held value also fails closed instead of comparing clocks"
        );
        assert_eq!(next_coordination_generation(Some("broken"), None), None);

        let gate = coordinating_lease("w1", 8, "contender-b");
        assert_eq!(gate.source, COORDINATING_SOURCE);
        assert_eq!(gate.seq, 8);
        assert_eq!(gate.ttl, Some(STARTING_TTL));
        assert_eq!(gate.tokens[COORDINATING_TOKEN], "held:8:contender-b");
        let gate_clear = clear_coordinating_lease("w1", 8);
        assert_eq!(gate_clear.source, COORDINATING_SOURCE);
        assert_eq!(gate_clear.seq, 9);
        assert_eq!(gate_clear.tokens[COORDINATING_TOKEN], "idle:9");
        let gate_generation = coordinating_generation_claim("w1", 8, "contender-b");
        assert_eq!(gate_generation.source, COORDINATING_GENERATION_SOURCE);
        assert_eq!(gate_generation.seq, 8);
        assert_eq!(gate_generation.ttl, None);
        assert_eq!(gate_generation.ttl, None);
        assert_eq!(
            gate_generation.tokens[COORDINATING_GENERATION_TOKEN],
            "8:contender-b"
        );
        assert_eq!(gate_generation.ttl, None);

        let tab_intent = tab_creation_intent("w9", "tower#request-a", 8);
        assert_eq!(tab_intent.source, TAB_CREATING_INTENT_SOURCE);
        assert_eq!(tab_intent.seq, 8);
        assert_eq!(tab_intent.ttl, Some(STARTING_TTL));
        assert_eq!(
            tab_intent.tokens[TAB_CREATING_INTENT_TOKEN],
            "tower#request-a"
        );
        let mut intent_workspace = ws("w9", LABEL);
        intent_workspace.tokens = tab_intent.tokens;
        assert!(tab_creation_intent_is_live(
            &intent_workspace,
            "tower#request-a"
        ));
        assert!(!tab_creation_intent_is_live(
            &intent_workspace,
            "tower#request-b"
        ));
        let mut anchor = ws("w9", LABEL);
        anchor
            .tokens
            .insert(COORDINATING_TOKEN.into(), "held:8:contender-b".into());
        assert!(owns_coordination(
            &[anchor.clone()],
            "w9",
            "held:8:contender-b",
        ));
        assert!(owns_coordination(
            &[ws("w1", "ordinary"), anchor.clone()],
            "w9",
            "held:8:contender-b",
        ));
        assert!(!owns_coordination(
            &[anchor.clone()],
            "w9",
            "held:8:someone-else",
        ));
        assert!(!owns_coordination(
            &[ws("w2", LABEL)],
            "w9",
            "held:8:contender-b",
        ));
        let request = "100000000000-a";
        let mut private_anchor = ws("w8", &workspace_request_label(request));
        private_anchor
            .tokens
            .insert(CREATED_TOKEN.into(), workspace_request_label(request));
        private_anchor
            .tokens
            .insert(COORDINATING_TOKEN.into(), "held:8:contender-b".into());
        assert!(owns_coordination(
            &[private_anchor.clone()],
            "w8",
            "held:8:contender-b",
        ));
        let creating = workspace_creation_lease("w8", &workspace_request_label(request));
        assert_eq!(creating.source, CREATING_SOURCE);
        assert_eq!(creating.ttl, Some(STARTING_TTL));
        private_anchor.tokens.extend(creating.tokens);
        assert!(workspace_creator_is_live(
            &private_anchor,
            &workspace_request_label(request)
        ));

        let terminal_id = "term-w9:p2";
        let reusable = remember_reusable_metadata("w9:p2", terminal_id);
        assert_eq!(reusable.source.as_deref(), Some(REUSABLE_SOURCE));
        assert_eq!(
            reusable.tokens[REUSABLE_TOKEN],
            terminal_bound_value(REUSABLE_UNSUBMITTED, terminal_id)
        );
        assert_eq!(reusable.ttl, None, "the shell trace does not expire");
        let submitted = mark_reusable_submitted_metadata("w9:p2", terminal_id);
        assert_eq!(
            submitted.tokens[REUSABLE_TOKEN],
            terminal_bound_value(REUSABLE_SUBMITTED, terminal_id)
        );
        let mut reusable_pane = pane("w9:p2", "w9:t2", None);
        reusable_pane.tokens = reusable.tokens.clone();
        assert!(was_never_submitted(&reusable_pane));
        reusable_pane.terminal_id = "replacement-terminal".into();
        assert!(!was_never_submitted(&reusable_pane));
        reusable_pane.terminal_id = terminal_id.into();
        reusable_pane.tokens = submitted.tokens;
        assert!(!was_never_submitted(&reusable_pane));

        let active = active_metadata("w9:p2", terminal_id, &lease_value, 8);
        assert_eq!(active.source.as_deref(), Some(ACTIVE_SOURCE));
        assert_eq!(
            active.tokens[ACTIVE_TOKEN],
            active_claim_value(terminal_id, &lease_value)
        );
        assert_eq!(
            active.tokens[ACTIVE_TOKEN],
            terminal_bound_value("held:8:contender-a", terminal_id),
            "the active claim replaces rather than duplicates the binding"
        );
        assert_eq!(active.seq, 8);
        assert_eq!(active.ttl, None, "suspension must not expire the claim");
        reusable_pane.tokens = active.tokens.clone();
        assert!(pane_has_bound_active(&reusable_pane));
        reusable_pane.terminal_id = "replacement-terminal".into();
        assert!(!pane_has_bound_active(&reusable_pane));
        let inactive = clear_active_metadata("w9:p2", 8);
        assert_eq!(inactive.source.as_deref(), Some(ACTIVE_SOURCE));
        assert_eq!(inactive.tokens[ACTIVE_TOKEN], "");
        assert_eq!(inactive.seq, 9);
    }

    #[test]
    fn nested_coordination_fences_keep_the_outer_deadline_armed() {
        let outer = CoordinationFence::arm().unwrap();
        assert_eq!(*COORDINATION_FENCE_USERS.lock().unwrap(), 1);
        let inner = CoordinationFence::arm().unwrap();
        assert_eq!(*COORDINATION_FENCE_USERS.lock().unwrap(), 2);
        drop(inner);
        assert_eq!(*COORDINATION_FENCE_USERS.lock().unwrap(), 1);
        drop(outer);
        assert_eq!(*COORDINATION_FENCE_USERS.lock().unwrap(), 0);
    }

    #[test]
    fn opaque_workspace_ids_have_one_deterministic_canonical_order() {
        let workspaces = [ws("w10", LABEL), ws("wZ", LABEL), ws("w2", "ordinary")];
        assert_eq!(
            dock_workspace(&workspaces).map(|workspace| workspace.workspace_id.as_str()),
            Some("w10")
        );
    }

    #[test]
    fn first_private_candidate_is_the_shared_creation_gate_until_publication() {
        let first_label = workspace_request_label("100000000000-1");
        let later_label = workspace_request_label("100000000001-2");
        let first = candidate_ws("opaque-z", &first_label, 4);
        let later = candidate_ws("opaque-a", &later_label, 5);

        assert_eq!(
            dock_workspace(&[later.clone(), first.clone()])
                .map(|workspace| workspace.workspace_id.as_str()),
            Some("opaque-z"),
            "Herdr's creation order, not the opaque ID, elects the private gate"
        );
        assert_eq!(
            plan(
                &[later, first.clone()],
                &[],
                |_| false,
                |_| false,
                |_| false,
                always,
            ),
            Plan::Publish {
                workspace_id: "opaque-z".into(),
                request_label: first_label,
            }
        );

        let public = numbered_ws("opaque-public", LABEL, 99);
        assert_eq!(
            dock_workspace(&[first, public.clone()])
                .map(|workspace| workspace.workspace_id.as_str()),
            Some("opaque-public"),
            "publication is sticky even if an older create was still in flight"
        );

        let user_label = workspace_request_label("notes");
        let user_workspace = numbered_ws("ordinary", &user_label, 1);
        assert!(dock_workspace(&[user_workspace]).is_none());
    }

    #[test]
    fn an_unmarked_strict_candidate_is_claimed_before_publication() {
        let request_label = workspace_request_label("100000000000-a");
        let unclaimed = numbered_ws("opaque-unclaimed", &request_label, 7);

        assert_eq!(
            plan(&[unclaimed], &[], never, never, never, always),
            Plan::Claim {
                workspace_id: "opaque-unclaimed".into(),
                request_label: request_label.clone(),
            }
        );

        let mut retired = numbered_ws("opaque-retired", &request_label, 7);
        retired
            .tokens
            .insert(CREATED_TOKEN.into(), CREATED_RETIRED.into());
        assert!(dock_workspace(&[retired]).is_none());
    }

    #[test]
    fn ids_become_plain_file_names() {
        let p = lock_path(Path::new("/s"), "w9:p1A");
        assert_eq!(p, Path::new("/s/w9_3ap1A.lock"));
    }

    #[test]
    fn names_that_differ_stay_different() {
        assert_ne!(file_name("/a_b"), file_name("/a/b"));
        assert_ne!(file_name("w1:p1"), file_name("w1_p1"));
    }

    /// Two servers give out the same IDs; their towers must not share a claim.
    #[test]
    fn towers_on_two_servers_do_not_share_a_lock() {
        let d = tempfile::tempdir().unwrap();
        let one = server_dir(d.path(), Path::new("/x/herdr.sock"));
        let two = server_dir(d.path(), Path::new("/x/sessions/b/herdr.sock"));
        assert_ne!(one, two);
        let _claim = hold(&one, "term-a").unwrap().unwrap();
        assert!(is_held(&one, "term-a"));
        assert!(!is_held(&two, "term-a"));
        assert!(hold(&two, "term-a").unwrap().is_some());
    }

    /// A pane's `HERDR_SOCKET_PATH` and the path `dock` resolves may be spelled
    /// differently; they name one server.
    #[test]
    fn one_socket_spelled_two_ways_is_one_server() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("herdr.sock");
        std::fs::write(&sock, b"").unwrap();
        let other = d.path().join(".").join("herdr.sock");
        assert_eq!(server_dir(d.path(), &sock), server_dir(d.path(), &other));
    }

    #[test]
    fn two_servers_at_one_socket_path_get_distinct_current_locks() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("herdr.sock");
        std::fs::write(&sock, b"first").unwrap();
        let first_file = File::open(&sock).unwrap();
        let first = server_dir(d.path(), &sock);

        std::fs::remove_file(&sock).unwrap();
        std::fs::write(&sock, b"second").unwrap();
        let second = server_dir(d.path(), &sock);

        assert_ne!(first, second, "the socket inode is part of server identity");
        drop(first_file);
    }

    /// Coordination follows the server, not whichever record store a caller
    /// selected. The root is passed separately from the server identity.
    #[test]
    fn the_coordination_root_holds_a_server_directory() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("runtime");
        let socket = Path::new("/run/herdr.sock");
        let dir = server_dir(&root, socket);
        assert_eq!(dir.parent(), Some(root.as_path()));
        assert_eq!(dir.file_name().unwrap().to_string_lossy().len(), 16);
        assert_eq!(
            state_server_dir(Path::new("/state/one"), socket),
            Path::new("/state/one/towers").join(dir.file_name().unwrap())
        );
    }

    /// The runtime root first, so every caller takes the same lock first; the
    /// selected state root second, where a tower started with that store is
    /// seen by a client that shares the home but not the `/tmp`.
    #[test]
    fn the_runtime_root_comes_before_the_state_root() {
        let d = tempfile::tempdir().unwrap();
        let coordination = d.path().join("runtime");
        let selected = d.path().join("selected");
        let socket = Path::new("/run/herdr.sock");
        let dirs = server_dirs(&coordination, &selected, socket);
        assert_eq!(
            dirs,
            vec![
                server_dir(&coordination, socket),
                state_server_dir(&selected, socket)
            ]
        );

        let _tower = hold(&state_server_dir(&selected, socket), "term-a")
            .unwrap()
            .unwrap();
        assert!(is_held_in(&dirs, "term-a"));
        assert!(is_reusable_in(&dirs, "term-a"));

        assert_eq!(
            server_dirs_without_runtime(&selected, socket),
            vec![state_server_dir(&selected, socket)]
        );
    }

    /// A socket path near `sockaddr_un`'s limit, full of bytes that would each
    /// expand to three when escaped, still makes one short name.
    #[test]
    fn a_long_socket_path_makes_a_short_directory_name() {
        let d = tempfile::tempdir().unwrap();
        let parent = d.path().join("_".repeat(95));
        std::fs::create_dir(&parent).unwrap();
        let long = parent.join("herdr.sock");
        let dir = server_dir(d.path(), &long);
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 16);
        exclusive(&dir).expect("the directory can be created");
    }

    /// The name is on disk, so it must not depend on the build.
    #[test]
    fn the_server_hash_is_fixed() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn a_second_dock_run_waits_for_the_first() {
        let d = tempfile::tempdir().unwrap();
        let first = exclusive(d.path()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = d.path().to_path_buf();
        let t = std::thread::spawn(move || {
            let _second = exclusive(&dir).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the second run is still waiting"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the second run proceeds once the first is done");
        t.join().unwrap();
    }

    #[test]
    fn a_dock_also_serializes_on_the_state_lock() {
        let d = tempfile::tempdir().unwrap();
        let current = d.path().join("current");
        let state = d.path().join("state");
        let first = exclusive_in(&[current, state.clone()]).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            // A dock whose runtime root was unavailable takes only this one.
            let _other_dock = exclusive(&state).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a dock holding only the state lock is still waiting on it"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn an_unavailable_state_root_does_not_block_the_runtime_lock() {
        let d = tempfile::tempdir().unwrap();
        let current = d.path().join("current");
        let not_a_dir = d.path().join("ordinary-home");
        fs::write(&not_a_dir, b"unavailable").unwrap();
        let state = not_a_dir.join("towers/server");

        let held = exclusive_in(&[current.clone(), state]).unwrap();
        assert_eq!(held.len(), 1);
        assert!(current.join("dock.lock").is_file());
    }

    #[test]
    fn an_unavailable_current_root_is_only_a_lost_fast_path() {
        let d = tempfile::tempdir().unwrap();
        let not_a_dir = d.path().join("read-only-runtime");
        fs::write(&not_a_dir, b"unavailable").unwrap();
        let current = not_a_dir.join("server");
        let selected = d.path().join("selected");

        let LocalClaim::Acquired(held) =
            exclusive_available_in(&[current.clone(), selected.clone()])
        else {
            panic!("one local gate should be available");
        };
        assert_eq!(held.len(), 1);
        assert!(selected.join("dock.lock").is_file());
        drop(held);

        let _other = soon(|| exclusive(&selected).ok());
        assert!(matches!(
            exclusive_available_in(&[current, selected]),
            LocalClaim::Contended
        ));
    }

    /// Directory modes are a Unix notion.
    #[cfg(unix)]
    #[test]
    fn the_read_only_socket_directory_is_a_stable_gate() {
        let d = tempfile::tempdir().unwrap();
        let socket = d.path().join("herdr.sock");
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let held = exclusive_socket_parent(&socket).unwrap();

        let contended = exclusive_socket_parent(&socket).unwrap_err();
        assert_eq!(contended.kind(), io::ErrorKind::WouldBlock);

        let same_directory = File::open(d.path()).unwrap();
        assert!(matches!(
            same_directory.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(held);
        soon(|| same_directory.try_lock().ok());
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Directory modes are a Unix notion.
    #[cfg(unix)]
    #[test]
    fn a_search_only_socket_directory_skips_the_local_fast_path() {
        let d = tempfile::tempdir().unwrap();
        let socket = d.path().join("herdr.sock");
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o100)).unwrap();
        let result = exclusive_socket_parent(&socket);
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
    }

    #[test]
    fn local_locks_are_ordered_by_identity_not_caller_preference() {
        let d = tempfile::tempdir().unwrap();
        let first = d.path().join("first");
        let second = d.path().join("second");

        let forward: Vec<_> = ordered_available_lock_files(&[first.clone(), second.clone()])
            .into_iter()
            .map(|(identity, _)| identity)
            .collect();
        let reverse: Vec<_> = ordered_available_lock_files(&[second, first])
            .into_iter()
            .map(|(identity, _)| identity)
            .collect();

        assert_eq!(forward, reverse);
    }

    #[cfg(unix)]
    #[test]
    fn aliased_roots_lock_the_same_file_only_once() {
        use std::os::unix::fs::symlink;

        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = d.path().join("alias");
        symlink(&real, &alias).unwrap();

        let held = exclusive_in(&[real, alias]).unwrap();
        assert_eq!(held.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn aliased_roots_claim_one_terminal_lock_only_once() {
        use std::os::unix::fs::symlink;

        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = d.path().join("alias");
        symlink(&real, &alias).unwrap();

        let LocalClaim::Acquired(held) = hold_available_in(&[real, alias], "term-a") else {
            panic!("aliased available roots should be acquired");
        };
        assert_eq!(held.len(), 1);
    }

    #[test]
    fn a_retry_reuses_the_tab_created_before_a_failed_launch() {
        let d = tempfile::tempdir().unwrap();
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p1", "w9:t1", None), pane("w9:p2", "w9:t2", None)];
        remember_reusable(d.path(), "term-w9:p2").unwrap();

        assert_eq!(
            plan(
                &spaces,
                &panes,
                |t| is_held(d.path(), t),
                never,
                |t| is_reusable(d.path(), t),
                always,
            ),
            Plan::Launch {
                pane_id: "w9:p2".into(),
                terminal_id: "term-w9:p2".into(),
            }
        );
    }

    #[test]
    fn one_new_tower_tab_can_be_reconciled_after_a_lost_reply() {
        let before = [pane("w9:p1", "w9:t1", None)];
        let created = pane("w9:p2", "w9:t2", None);
        let unrelated = pane("w9:p3", "w9:t3", None);
        let request_label = tab_request_label("request-a");
        let tabs = [
            tab("w9:t1", "w9", "old"),
            tab("w9:t2", "w9", &request_label),
            tab("w9:t3", "w9", TAB_LABEL),
        ];
        let after = [before[0].clone(), created.clone(), unrelated];

        assert_eq!(
            reconcile_created_tab(&before, &tabs, &after, "w9", &request_label)
                .map(|pane| pane.terminal_id),
            Some(created.terminal_id)
        );
    }

    #[test]
    fn ambiguous_or_unrelated_new_tabs_are_not_adopted() {
        let before = [pane("w9:p1", "w9:t1", None)];
        let one = pane("w9:p2", "w9:t2", None);
        let two = pane("w9:p3", "w9:t3", None);
        let elsewhere = pane("w8:p1", "w8:t1", None);
        let one_tab = tab("w9:t2", "w9", TAB_LABEL);
        let two_tabs = [one_tab.clone(), tab("w9:t3", "w9", TAB_LABEL)];
        let elsewhere_tab = tab("w8:t1", "w8", TAB_LABEL);

        assert!(
            reconcile_created_tab(&before, &[], &[before[0].clone()], "w9", TAB_LABEL).is_none()
        );
        assert!(
            reconcile_created_tab(&before, &two_tabs, &[one.clone(), two], "w9", TAB_LABEL)
                .is_none()
        );
        assert!(reconcile_created_tab(
            &before,
            &[elsewhere_tab],
            &[one, elsewhere],
            "w9",
            TAB_LABEL,
        )
        .is_none());
    }

    /// 🚨 The two dock tokens do not share a shape, and only one of them is
    /// what a clear has to outrank. The active claim carries
    /// `held:<generation>:<owner>` and parses; the high-water marker carries
    /// `<generation>:<owner>` and does not. Reading the wrong one answers zero
    /// silently, and a clear written at sequence 1 cannot displace a claim
    /// made at the real generation — the first of which is 2.
    #[test]
    fn only_the_active_claim_yields_its_generation_to_the_lease_parser() {
        let lease = pane_starting_lease_value(7, "owner-a", "term-a");
        let active = active_claim_value("term-a", &lease);
        assert_eq!(pane_starting_lease_generation(&active), Some(7));

        let high_water = pane_starting_generation_value(7, "owner-a", "term-a");
        assert_eq!(
            pane_starting_lease_generation(&high_water),
            None,
            "the high-water marker is not a lease value"
        );
    }

    #[test]
    fn a_drifted_dock_tab_is_renamed_but_a_request_label_is_not() {
        let tabs = [
            tab("w9:t1", "w9", TAB_LABEL),
            tab("w9:t2", "w9", "1"),
            tab("w9:t3", "w9", &tab_request_label("100000000000-a")),
            tab("w9:t4", "w9", "herdr-tower"),
        ];

        assert!(!tab_needs_dock_label(&tabs, "w9:t1"));
        assert!(tab_needs_dock_label(&tabs, "w9:t2"));
        assert!(!tab_needs_dock_label(&tabs, "w9:t3"));
        assert!(tab_needs_dock_label(&tabs, "w9:t4"));
        assert!(!tab_needs_dock_label(&tabs, "w9:t9"));
        // Exempt from `id_labels`: a prefix on the dock's tab is taken off.
        let prefixed = [tab("w9:t1", "w9", &format!("[w9:t1] {TAB_LABEL}"))];
        assert!(tab_needs_dock_label(&prefixed, "w9:t1"));
    }

    /// A dock tab that came back from a herdr restart wearing an ID prefix is
    /// still the dock's, while a bracket that is not an ID is somebody's.
    #[test]
    fn a_restored_dock_tab_is_found_with_or_without_an_id_prefix() {
        let spaces = [ws("w9", LABEL)];
        let shell = || {
            [serde_json::from_value::<Pane>(serde_json::json!({
                "pane_id": "w9:p1", "tab_id": "w9:t1", "workspace_id": "w9",
                "terminal_id": "term_1"
            }))
            .unwrap()]
        };
        for label in [
            TAB_LABEL.to_string(),
            format!("[w9:t1] {TAB_LABEL}"),
            format!("[w9:t7] {TAB_LABEL}"),
        ] {
            let tabs = [tab("w9:t1", "w9", &label)];
            let panes = shell();
            assert_eq!(
                restored_dock_shells(&spaces, &tabs, &panes).len(),
                1,
                "{label:?}"
            );
        }
        let tabs = [tab("w9:t1", "w9", &format!("[wip] {TAB_LABEL}"))];
        let panes = shell();
        assert!(restored_dock_shells(&spaces, &tabs, &panes).is_empty());
    }

    /// The tab and the workspace show one name, while the label a half-created
    /// tab wears keeps its own shape, so it is never taken for a workspace
    /// request.
    #[test]
    fn the_dock_tab_is_named_after_the_dock() {
        assert_eq!(TAB_LABEL, LABEL);
        assert!(tab_request_label("100000000000-a").starts_with("tower#"));
        assert_eq!(
            workspace_request_id(&tab_request_label("100000000000-a")),
            None
        );
    }

    #[test]
    fn request_labels_identify_only_their_own_created_workspace() {
        let request = starting_lease_value();
        let workspace_label = workspace_request_label(&request);
        let tab_label = tab_request_label(&request);
        assert_ne!(workspace_label, LABEL);
        assert_ne!(tab_label, TAB_LABEL);
        assert_eq!(tab_request_id(&tab_label), Some(request.as_str()));
        assert_eq!(tab_request_id(TAB_LABEL), None);
        assert_eq!(tab_request_id("tower#notes"), None);
        assert_eq!(tab_request_id("tower#100000000000-a-extra"), None);
        let tab_lease = tab_creation_lease("w9:p2", &tab_label);
        assert_eq!(tab_lease.source.as_deref(), Some(TAB_CREATING_SOURCE));
        assert_eq!(tab_lease.tokens[TAB_CREATING_TOKEN], tab_label);
        assert_eq!(tab_lease.ttl, Some(STARTING_TTL));

        let workspaces = [ws("w8", LABEL), ws("w9", &workspace_label)];
        assert_eq!(
            workspace_for_request(&workspaces, &workspace_label)
                .map(|workspace| workspace.workspace_id.as_str()),
            Some("w9")
        );
        assert!(workspace_for_request(&workspaces, LABEL).is_some());
        assert!(workspace_for_request(
            &[ws("w9", &workspace_label), ws("wA", &workspace_label)],
            &workspace_label,
        )
        .is_none());

        let claim = workspace_creation_claim("w9", &workspace_label);
        assert_eq!(claim.source, CREATED_SOURCE);
        assert_eq!(claim.tokens[CREATED_TOKEN], workspace_label);
        let mut marked = ws("w9", &workspace_label);
        marked.tokens = claim.tokens;
        assert!(is_workspace_creation_candidate(&marked));
        marked
            .tokens
            .insert(CREATED_TOKEN.into(), "some-other-label".into());
        assert!(!is_workspace_creation_candidate(&marked));
    }

    #[test]
    fn an_accepted_launch_is_not_reused_before_it_finishes() {
        let d = tempfile::tempdir().unwrap();
        let spaces = [ws("w9", LABEL)];
        let panes = [pane("w9:p1", "w9:t1", None), pane("w9:p2", "w9:t2", None)];
        remember_reusable(d.path(), "term-w9:p2").unwrap();
        assert_eq!(
            plan(
                &spaces,
                &panes,
                |t| is_held(d.path(), t),
                |t| t == "term-w9:p2",
                |t| is_reusable(d.path(), t),
                always,
            ),
            Plan::Starting {
                pane_id: "w9:p2".into(),
                workspace_id: "w9".into(),
            }
        );
    }

    #[test]
    fn a_path_is_quoted_only_when_it_needs_to_be() {
        assert_eq!(
            sh_quote("/Users/q/.local/bin/herdr-tower"),
            "/Users/q/.local/bin/herdr-tower"
        );
        assert_eq!(sh_quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    /// PowerShell has no `env`: the variables are set in the pane's session
    /// and the program is run with `&`, every value one single-quoted word.
    #[cfg(windows)]
    #[test]
    fn the_windows_launch_line_is_powershell() {
        assert_eq!(
            launch_command(
                r"C:\Users\u\.local\bin\herdr-tower.exe",
                Path::new(r"C:\Users\u\.local\state\herdr-tower"),
                Some(Path::new(r"C:\cfg\it's.json")),
                "8:owner",
                "term_1",
            ),
            "$env:HERDR_TOWER_DOCK='1'; $env:HERDR_TOWER_DOCK_LEASE='8:owner'; \
             $env:HERDR_TOWER_DOCK_TERMINAL='term_1'; \
             $env:HERDR_TOWER_STATE_DIR='C:\\Users\\u\\.local\\state\\herdr-tower'; \
             $env:HERDR_TOWER_CONFIG='C:\\cfg\\it''s.json'; \
             & 'C:\\Users\\u\\.local\\bin\\herdr-tower.exe'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_launch_line_carries_the_state_directory_and_the_config() {
        assert_eq!(
            launch_command(
                "/bin/herdr-tower",
                Path::new("/state/lab"),
                None,
                "8:owner-a",
                "terminal-a",
            ),
            "env HERDR_TOWER_DOCK=1 HERDR_TOWER_DOCK_LEASE='8:owner-a' HERDR_TOWER_DOCK_TERMINAL=terminal-a HERDR_TOWER_STATE_DIR=/state/lab /bin/herdr-tower"
        );
        assert_eq!(
            launch_command(
                "/my bin/herdr-tower",
                Path::new("/st ate"),
                Some(Path::new("/conf/lab.json")),
                "8:owner a",
                "terminal a",
            ),
            "env HERDR_TOWER_DOCK=1 HERDR_TOWER_DOCK_LEASE='8:owner a' HERDR_TOWER_DOCK_TERMINAL='terminal a' HERDR_TOWER_STATE_DIR='/st ate' HERDR_TOWER_CONFIG=/conf/lab.json '/my bin/herdr-tower'"
        );
    }

    /// Every combination, against the rule written a second way — and 🚨 the
    /// token is not among the inputs at all: the exact shape that wedged the
    /// dock was a killed tower's `dock-active` token read as a live tower.
    #[test]
    fn the_evidence_rule_holds_on_every_combination() {
        for bits in 0u8..4 {
            let running = bits & 1 != 0;
            let lock = bits & 2 != 0;
            assert_eq!(
                tower_is_evident(running, lock),
                running || lock,
                "running={running} lock={lock}"
            );
        }
    }

    #[test]
    fn a_launch_is_not_blocked_by_a_dead_towers_leftover_token() {
        // The pane still carries `dock-active` from a tower that was killed.
        // Nothing will ever clear it, so honouring it here blocks the launch
        // permanently — the sidebar button stops working for good.
        assert!(!active_token_blocks_launch(
            Some("held:4:owner|terminal=term_a"),
            || Some(false)
        ));
    }

    #[test]
    fn a_launch_is_blocked_while_a_tower_is_actually_there() {
        assert!(active_token_blocks_launch(
            Some("held:4:owner|terminal=term_a"),
            || Some(true)
        ));
    }

    #[test]
    fn an_unanswerable_observation_leaves_the_token_in_charge() {
        // Every attempt failed. "Could not look" must not read as "no".
        assert!(active_token_blocks_launch(
            Some("held:4:owner|terminal=term_a"),
            || None
        ));
    }

    #[test]
    fn no_token_blocks_nothing_however_the_observation_went() {
        for observed in [Some(true), Some(false), None] {
            assert!(!active_token_blocks_launch(None, || observed));
            assert!(!active_token_blocks_launch(Some(""), || observed));
        }
        // 🚨 And without asking. The observation costs a round trip, so the
        // no-token path must not take one.
        assert!(!active_token_blocks_launch(None, || panic!(
            "no token means no question to ask"
        )));
    }
}
