// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Opening the dock again after something closed it.
//!
//! `dock` takes herdr's right-click menu off the tower's *pane*, so the menu
//! carrying *Close pane* never opens there. The tab title and the sidebar row
//! have menus of their own, drawn by herdr, and each carries a *Close* that no
//! client setting removes — checked against the herdr 0.9.1 binary, whose `ui`
//! config has `confirm_close` and nothing that hides a menu entry.
//!
//! So the dock is not defended from inside herdr; it is reopened from outside.
//! The resident supervisor is the one part of herdr-tower that outlives the tower,
//! and it already holds a live view of every workspace and pane, so it is what
//! notices the dock is gone and runs `dock` again.
//!
//! The restart is a child process rather than a call into the dock code. That
//! code arms a kernel deadline that terminates *its own process* to fence a
//! coordination lease (`dockcmd::CoordinationFence`); running it in-process
//! would let the supervisor be killed by that timer.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use herdrx::{Pane, Workspace};

/// How long the supervisor waits before its first retry, and the step by which
/// each consecutive failure lengthens that wait.
const RETRY_STEP: Duration = Duration::from_secs(5);
/// The longest gap between retries. A dock that cannot be restarted (no herdr
/// server, a refused socket) should stop costing a process every few seconds,
/// but must still recover on its own once the cause is gone.
const RETRY_CEILING: Duration = Duration::from_secs(60);

/// The wait after `failures` consecutive failed restarts.
///
/// Zero failures means no wait: the common case is a tab someone just closed,
/// and it should come back at once.
pub fn backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    RETRY_STEP.saturating_mul(failures).min(RETRY_CEILING)
}

/// Whether the dock needs `dock` run again: no dock workspace at all, or one
/// with neither a tower nor a launch under way in it.
///
/// The tower half is what covers a closed *tab* — the workspace survives its
/// last tab only if herdr keeps it, and either way the tower is gone with the
/// pane it lived in.
///
/// A held startup lease counts as present. Otherwise the window between
/// `dock` submitting the tower's command and the tower publishing its active
/// token reads as "gone", and the supervisor starts a second `dock` that can
/// only lose the coordination race — which is exactly the state a herdr
/// restart puts the session in, with the autostart plugin retrying for five
/// minutes.
pub fn dock_is_gone(
    workspaces: &[Workspace],
    panes: &[Pane],
    tower_running: impl Fn(&Pane) -> Option<bool>,
) -> bool {
    let dock: Vec<&str> = workspaces
        .iter()
        .filter(|workspace| workspace.label == tower_ui::DOCK_LABEL)
        .map(|workspace| workspace.workspace_id.as_str())
        .collect();
    if dock.is_empty() {
        return true;
    }
    // The usage board's pane is never the tower's; see `usagetab`.
    !panes
        .iter()
        .filter(|pane| dock.contains(&pane.workspace_id.as_str()))
        .filter(|pane| !crate::usagetab::is_usage_pane(pane))
        .any(|pane| tower_is_present(pane, &tower_running) || launch_is_under_way(pane))
}

/// A live dock whose workspace is wearing the wrong label, or `None`.
///
/// Renaming the dock workspace does not stop the tower in it. Judged by label
/// alone that dock reads as deleted, and the restart then builds a *second*
/// workspace and a second tower while the first goes on running under its new
/// name — two towers, one of them orphaned. So before concluding the dock is
/// gone, look for a workspace that still holds a live tower and give it its
/// name back.
///
/// 🚨 A running tower is not enough to identify one. `herdr-tower` opened by
/// hand in an ordinary workspace is the same executable, so judging by process
/// alone would seize that person's workspace and rename it. Only `dock`
/// publishes the terminal-bound `dock-active` claim, so that token is what
/// says *which* tower this is; the process observation then says it is still
/// alive. This is the mirror of [`dock_is_gone`], where the token alone cannot
/// prove presence because a killed tower leaves it behind — here it cannot
/// prove liveness, only identity.
///
/// 🚨 Liveness must be *confirmed*, not merely un-refuted. An unsupported or
/// failed `process_info` answers `None`, and accepting that would let an
/// abruptly exited tower — persistent `dock-active` token still set — have its
/// renamed workspace relabeled as the dock. Every later pass would then fall
/// back to that same stale token through [`dock_is_gone`] and conclude the
/// dock is present, so the missing tower would never be restarted. A server
/// that cannot say builds a new dock instead, which is recoverable; this is
/// not.
///
/// A launch under way is excluded too: an unlabeled workspace with a pending
/// lease is one a `dock` invocation is in the middle of building, and renaming
/// it would collide with its own reconciliation.
/// What to do about a live dock found outside the dock label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mislabeled {
    /// Give this workspace the dock label back. The label is free.
    Reclaim(String),
    /// A live dock is wearing the wrong name *and* the label is already on
    /// another workspace that has no tower in it.
    ///
    /// Neither obvious move is safe on its own: renaming the live one would
    /// put the public label on two workspaces at once, and restarting would
    /// build a second tower in the empty labeled one — `dock` only inspects
    /// canonically labeled workspaces, so it cannot see the live tower it
    /// would duplicate.
    ///
    /// 🚨 Nor is doing nothing: no later pass changes either workspace, so
    /// every snapshot returns this same answer and the live tower stays hidden
    /// under the wrong name forever. The way out is to free the label first —
    /// `stale` is the empty impostor, which gets a name that is visibly not
    /// the dock's, after which the next pass is an ordinary [`Self::Reclaim`].
    Conflicted { stale: String, live: String },
}

/// What an empty workspace holding the dock label is renamed to, so the label
/// is free for the tower that actually has a dock in it. Deliberately legible:
/// someone who finds it should see both what it was and that it is not the
/// dock.
pub const STALE_LABEL: &str = "🗼 HERDR-TOWER 🗼 (stale)";

pub fn dock_to_reclaim(
    workspaces: &[Workspace],
    panes: &[Pane],
    tower_running: impl Fn(&Pane) -> Option<bool>,
) -> Option<Mislabeled> {
    let labeled = workspaces
        .iter()
        .find(|workspace| workspace.label == tower_ui::DOCK_LABEL)
        .map(|workspace| workspace.workspace_id.clone());
    let mislabeled: Vec<&str> = workspaces
        .iter()
        .filter(|workspace| workspace.label != tower_ui::DOCK_LABEL)
        .map(|workspace| workspace.workspace_id.as_str())
        .collect();
    let live = panes.iter().find(|pane| {
        mislabeled.contains(&pane.workspace_id.as_str())
            && crate::dockcmd::pane_has_bound_active(pane)
            && tower_running(pane) == Some(true)
    })?;
    Some(match labeled {
        Some(stale) => Mislabeled::Conflicted {
            stale,
            live: live.workspace_id.clone(),
        },
        None => Mislabeled::Reclaim(live.workspace_id.clone()),
    })
}

/// The tabs of the live dock towers, whatever those tabs are named.
///
/// Read on its own, every pass, and not as a step of putting the workspace's
/// label back ([`dock_to_reclaim`]). 🚨 That was the bug (#40): the tab was only
/// ever renamed *after* a successful workspace rename, so a reclaim whose
/// `rename_workspace` succeeded and whose `rename_tab` failed left the tab
/// mislabeled for good — the next pass saw a correctly named workspace, never
/// asked again, and `dock` built a second tab after the next herdr restart,
/// because [`crate::dockcmd::restored_dock_shells`] knows the restored shell by
/// its tab's name.
///
/// This says *which* tabs are the dock's, not which need renaming: the caller
/// asks the server for the labels only if there is a tab to ask about, and
/// [`crate::dockcmd::tab_needs_dock_label`] owns what a name has to be. Same
/// identity rule as the reclaim — the terminal-bound `dock-active` claim says
/// which tower this is, and a confirmed live process says it is still there —
/// so a `herdr-tower` somebody opened by hand keeps its own tab name.
pub fn dock_tower_tabs(
    workspaces: &[Workspace],
    panes: &[Pane],
    tower_running: impl Fn(&Pane) -> Option<bool>,
) -> Vec<String> {
    let dock: Vec<&str> = workspaces
        .iter()
        .filter(|workspace| workspace.label == tower_ui::DOCK_LABEL)
        .map(|workspace| workspace.workspace_id.as_str())
        .collect();
    let mut tabs: Vec<String> = panes
        .iter()
        .filter(|pane| {
            dock.contains(&pane.workspace_id.as_str())
                && !pane.tab_id.is_empty()
                && crate::dockcmd::pane_has_bound_active(pane)
                && tower_running(pane) == Some(true)
        })
        .map(|pane| pane.tab_id.clone())
        .collect();
    tabs.sort();
    tabs.dedup();
    tabs
}

/// Whether a pane holds a tower, preferring what the server can see over what
/// the last tower left behind.
///
/// `dock-active` is persistent and cleared on a normal exit, so a tower that
/// crashed or was killed leaves it set on an empty shell. Trusting the token
/// alone would make that shell look occupied forever and is exactly how a
/// dock stays broken: observed on this machine, where a killed tower left the
/// token behind and `dock` reported "already running" over an idle prompt
/// until the token was removed by hand.
///
/// `tower_running` answers `None` when the process observation failed. The
/// token is the fallback there — "could not look" must not read as "gone".
fn tower_is_present(pane: &Pane, tower_running: &impl Fn(&Pane) -> Option<bool>) -> bool {
    match tower_running(pane) {
        Some(running) => running,
        None => crate::dockcmd::pane_has_bound_active(pane),
    }
}

/// Whether a `dock` invocation holds this pane's startup lease.
fn launch_is_under_way(pane: &Pane) -> bool {
    pane.tokens
        .get(crate::dockcmd::STARTING_TOKEN)
        .is_some_and(|value| {
            crate::dockcmd::pane_starting_lease_is_held_for(value, &pane.terminal_id)
        })
}

/// What [`Keeper::tick`] set off, for the supervisor's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restart {
    /// A `dock` was started to bring the tower back.
    Started,
    /// Starting one failed, with why; the next try waits out the backoff.
    Failed(String),
}

/// Runs `dock` again, at most one at a time, with a lengthening wait after
/// failures.
pub struct Keeper {
    executable: PathBuf,
    /// Where the install locations are looked for; `None` in an environment
    /// with no home, where only an absolute path can be resolved.
    home: Option<PathBuf>,
    running: Option<Child>,
    failures: u32,
    /// `None` means "may start now"; the supervisor's clock is the only one
    /// consulted, so a system clock change cannot postpone a restart.
    hold_until: Option<Instant>,
}

impl Keeper {
    pub fn new(executable: impl Into<PathBuf>, home: Option<PathBuf>) -> Self {
        Keeper {
            executable: executable.into(),
            home,
            running: None,
            failures: 0,
            hold_until: None,
        }
    }

    /// One supervisor pass.
    ///
    /// `keep` is the setting, `gone` this pass's observation. They are
    /// separate because "do not restart the dock" and "the dock is fine" call
    /// for different things from a restart that is already running: the first
    /// has to stop it, and the second must not — a restart that just succeeded
    /// is precisely what makes the dock fine again.
    ///
    /// `gone` is three-valued. 🚨 `None` — the backend could not be read this
    /// pass — is not `Some(false)`: treating an unreadable snapshot as a
    /// recovered dock would let one transient failure discard an accumulated
    /// backoff, and the next readable snapshot showing the dock still missing
    /// would restart immediately. Unknown means do nothing but reap.
    ///
    /// Says what it started, if anything, for the supervisor's log.
    pub fn tick(&mut self, keep: bool, gone: Option<bool>, now: Instant) -> Option<Restart> {
        self.reap();
        if !keep {
            // Someone turned the dock loose while a restart was in flight.
            // That child never reads the setting, so leaving it be would
            // reopen the dock the user just asked to be allowed to close.
            self.cancel();
            return None;
        }
        if self.running.is_some() {
            return None;
        }
        match gone {
            // A dock that is *seen* to be there ends the run of failures,
            // whoever fixed it — the startup plugin, a person, or the last
            // restart. Keeping a stale deadline would make the next close wait
            // out as much as a minute, for failures no longer consecutive.
            Some(false) => {
                self.rested();
                return None;
            }
            None => return None,
            Some(true) => {}
        }
        if self.hold_until.is_some_and(|until| now < until) {
            return None;
        }
        match spawn_dock(&self.executable, self.home.as_deref()) {
            Ok(child) => {
                self.running = Some(child);
                Some(Restart::Started)
            }
            Err(e) => {
                self.failed(now);
                Some(Restart::Failed(e.to_string()))
            }
        }
    }

    /// Stops a restart in flight and waits for it.
    ///
    /// Best effort by nature: a `dock` that has already submitted the tower's
    /// command to a shell has done that, and killing the client does not
    /// un-send it. What this does guarantee is that no *further* dock work
    /// begins, and that the child is never left unwaited for.
    fn cancel(&mut self) {
        // The reset happens whether or not there was a child: turning keeping
        // off and on again must not inherit a backoff from before.
        self.rested();
        let Some(mut child) = self.running.take() else {
            return;
        };
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Forgets a run of failures.
    fn rested(&mut self) {
        self.failures = 0;
        self.hold_until = None;
    }

    /// Whether a restart is in flight, for the supervisor's own status.
    pub fn restarting(&self) -> bool {
        self.running.is_some()
    }

    fn reap(&mut self) {
        let Some(child) = self.running.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let now = Instant::now();
                self.running = None;
                if status.success() {
                    self.rested();
                } else {
                    self.failed(now);
                }
            }
            // Still working. `dock` waits up to five seconds for the tower to
            // publish its active token, which is longer than one pass.
            Ok(None) => {}
            Err(_) => {
                let now = Instant::now();
                self.running = None;
                self.failed(now);
            }
        }
    }

    fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.hold_until = Some(now + backoff(self.failures));
    }
}

/// The Linux marker for a running binary whose file has been replaced.
const DELETED_SUFFIX: &str = " (deleted)";

/// What to run, given the path this supervisor was started from.
///
/// An upgrade replaces the binary at that path with a new inode —
/// deliberately, because macOS binds an ad-hoc signature to the inode and
/// overwriting one in use kills every process mapped to it. A supervisor
/// started before the upgrade keeps running, and on Linux its `current_exe()`
/// then reads `…/herdr-tower (deleted)`, which spawns nothing.
///
/// 🚨 The bare name is not a fallback here. A supervisor runs from a LaunchAgent
/// or a systemd unit, neither of which puts `~/.local/bin` on `PATH`: the unit
/// this binary writes sets only `HOME` and the state dir
/// ([`crate::daemoncmd`]), so `PATH` is the service default and every install
/// location is off it — the same fact `tower_usage::ToolSearch` exists for.
/// Resolving the bare name there fails forever, silently, which is the exact
/// failure this function was added to prevent.
///
/// So the replacement is found by path: strip Linux's marker, then try the
/// install locations the dock's own startup plugin tries, in its order.
pub fn launcher(executable: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if is_runnable(executable) {
        return Some(executable.to_path_buf());
    }
    let text = executable.to_string_lossy();
    if let Some(original) = text.strip_suffix(DELETED_SUFFIX) {
        let original = PathBuf::from(original);
        if is_runnable(&original) {
            return Some(original);
        }
    }
    let name = executable
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .map(|name| name.trim_end_matches(DELETED_SUFFIX).to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "herdr-tower".to_string());
    home.map(|home| home.join(".local/bin").join(&name))
        .into_iter()
        .chain([
            PathBuf::from("/opt/homebrew/bin").join(&name),
            PathBuf::from("/usr/local/bin").join(&name),
        ])
        .find(|candidate| is_runnable(candidate))
}

/// A file this process could actually exec.
///
/// Existence is not enough: a leftover non-executable `herdr-tower` in the
/// first location searched would otherwise shadow a working one further down
/// the list, and every restart would fail with permission denied until someone
/// noticed. `tower_usage::ToolSearch` and `tower_mux` test the same bit for
/// the same reason.
fn is_runnable(path: &Path) -> bool {
    tower_os::is_executable_file(path)
}

/// `--require-active` because only a tower that actually started counts as a
/// restored dock: a handoff to another invocation is reported as success by
/// the plain command, and the supervisor would stop retrying on it.
fn spawn_dock(executable: &Path, home: Option<&Path>) -> std::io::Result<Child> {
    let launcher = launcher(executable, home).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no herdr-tower binary to restart the dock with",
        )
    })?;
    Command::new(launcher)
        .arg("dock")
        .arg("--require-active")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_testrng::Rng;

    fn ws(id: &str, label: &str) -> Workspace {
        serde_json::from_value(serde_json::json!({ "workspace_id": id, "label": label })).unwrap()
    }

    /// A server that cannot inspect processes, which is what the token
    /// fallback exists for.
    fn unobserved(_: &Pane) -> Option<bool> {
        None
    }

    fn pane(id: &str, workspace: &str, terminal: &str, active: bool) -> Pane {
        let tokens = if active {
            serde_json::json!({ "dock-active": format!("held:1:owner|terminal={terminal}") })
        } else {
            serde_json::json!({})
        };
        serde_json::from_value(serde_json::json!({
            "pane_id": id,
            "workspace_id": workspace,
            "terminal_id": terminal,
            "tokens": tokens,
        }))
        .unwrap()
    }

    /// The same pane, in a named tab: the tab is what `dock` reads after a
    /// herdr restart, so these tests need one.
    fn pane_in_tab(id: &str, workspace: &str, tab: &str, terminal: &str, active: bool) -> Pane {
        let mut pane = pane(id, workspace, terminal, active);
        pane.tab_id = tab.to_string();
        pane
    }

    fn observed_tower(_: &Pane) -> Option<bool> {
        Some(true)
    }

    fn no_tower(_: &Pane) -> Option<bool> {
        Some(false)
    }

    #[test]
    fn a_missing_dock_workspace_is_gone() {
        let list = [ws("w2", "00.dbstore")];
        assert!(dock_is_gone(
            &list,
            &[pane("w2:p1", "w2", "term-a", true)],
            unobserved
        ));
    }

    /// The usage board runs the same binary; where herdr reports processes by
    /// name alone it looks like the tower. Its pane never keeps a dead dock
    /// from reading as gone.
    #[test]
    fn the_usage_board_does_not_stand_in_for_the_tower() {
        let list = [ws("w9", tower_ui::DOCK_LABEL)];
        let mut board = pane("w9:p2", "w9", "term-b", false);
        board
            .tokens
            .insert(crate::usagetab::TOKEN.into(), "1".into());
        // The process check says "tower" for it, as a name-only match would.
        assert!(dock_is_gone(&list, &[board.clone()], observed_tower));
        // Beside a live tower it changes nothing.
        let tower = pane("w9:p1", "w9", "term-a", true);
        assert!(!dock_is_gone(&list, &[tower, board], unobserved));
    }

    #[test]
    fn a_dock_with_a_live_tower_is_left_alone() {
        let list = [ws("w9", tower_ui::DOCK_LABEL)];
        let panes = [pane("w9:p1", "w9", "term-a", true)];
        assert!(!dock_is_gone(&list, &panes, unobserved));
    }

    /// The tab was closed: the workspace may still be listed, but the pane the
    /// tower claimed went with it.
    #[test]
    fn a_dock_without_a_tower_is_gone() {
        let list = [ws("w9", tower_ui::DOCK_LABEL)];
        assert!(dock_is_gone(&list, &[], unobserved));
        assert!(dock_is_gone(
            &list,
            &[pane("w9:p1", "w9", "term-a", false)],
            unobserved
        ));
    }

    /// The gap between `dock` sending the tower's command and the tower
    /// claiming the pane is not an empty dock.
    #[test]
    fn a_launch_under_way_is_not_gone() {
        let list = [ws("w9", tower_ui::DOCK_LABEL)];
        let mut starting = pane("w9:p1", "w9", "term-a", false);
        starting.tokens.insert(
            crate::dockcmd::STARTING_TOKEN.to_string(),
            crate::dockcmd::pane_starting_lease_value(3, "owner", "term-a"),
        );
        assert!(!dock_is_gone(&list, &[starting.clone()], unobserved));

        // A released lease is not a launch: `idle:N` is what `dock` leaves
        // behind, and the pane is free for the next one.
        let mut released = starting;
        released.tokens.insert(
            crate::dockcmd::STARTING_TOKEN.to_string(),
            "idle:4".to_string(),
        );
        assert!(dock_is_gone(&list, &[released], unobserved));
    }

    /// A lease bound to another terminal says nothing about this pane — the
    /// pane moved, or the value is stale.
    #[test]
    fn a_lease_bound_elsewhere_does_not_count_as_a_launch() {
        let list = [ws("w9", tower_ui::DOCK_LABEL)];
        let mut pane = pane("w9:p1", "w9", "term-a", false);
        pane.tokens.insert(
            crate::dockcmd::STARTING_TOKEN.to_string(),
            crate::dockcmd::pane_starting_lease_value(3, "owner", "term-other"),
        );
        assert!(dock_is_gone(&list, &[pane], unobserved));
    }

    /// A tower running in an ordinary workspace is not the dock's.
    #[test]
    fn an_active_pane_elsewhere_does_not_answer_for_the_dock() {
        let list = [ws("w9", tower_ui::DOCK_LABEL), ws("w2", "20.herdr-tower")];
        let panes = [pane("w2:p1", "w2", "term-a", true)];
        assert!(dock_is_gone(&list, &panes, unobserved));
    }

    /// The binary this supervisor started from can be replaced out from under
    /// it, and on Linux the path then names a file that is not there.
    ///
    /// 🚨 Every answer must be a path. A supervisor's `PATH` is the service
    /// default (`/usr/bin:/bin`), which has no install location on it, so a
    /// bare name resolves to nothing there — silently, forever.
    #[cfg(unix)]
    #[test]
    fn a_replaced_binary_is_resolved_by_path_never_by_name() {
        use std::os::unix::fs::PermissionsExt;

        let mine = std::env::current_exe().expect("test binary path");
        assert_eq!(
            launcher(&mine, None).as_deref(),
            Some(mine.as_path()),
            "a real file is used as given"
        );

        let deleted = PathBuf::from(format!("{}{DELETED_SUFFIX}", mine.display()));
        assert_eq!(
            launcher(&deleted, None).as_deref(),
            Some(mine.as_path()),
            "Linux's marker is stripped to find the replacement in place"
        );

        // A home whose install location holds the binary.
        let home = std::env::temp_dir().join(format!("tower-launcher-{}", std::process::id()));
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).expect("make a fake install location");
        let installed = bin.join("herdr-tower");
        std::fs::write(&installed, b"stand-in").expect("write the installed binary");
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        assert_eq!(
            launcher(
                Path::new("/nowhere/herdr-tower (deleted)"),
                Some(home.as_path())
            ),
            Some(installed.clone()),
            "the install location is searched, not PATH"
        );

        // A leftover file with no execute bit is skipped rather than chosen
        // and then failing to spawn on every pass.
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o644))
            .expect("drop the execute bit");
        assert_eq!(
            launcher(
                Path::new("/nowhere/herdr-tower (deleted)"),
                Some(home.as_path())
            ),
            None,
            "a non-executable candidate does not shadow the rest of the search"
        );

        assert_eq!(
            launcher(Path::new("/nowhere/herdr-tower (deleted)"), None),
            None,
            "nothing found is None, not a name the service PATH cannot resolve"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A tower that was killed leaves its persistent token behind. The server
    /// can see there is no such process, and that has to win — otherwise the
    /// empty shell looks occupied for as long as the pane stays open.
    #[test]
    fn a_stale_active_token_does_not_stand_in_for_a_tower() {
        let list = [ws("w9", tower_ui::DOCK_LABEL)];
        let panes = [pane("w9:p1", "w9", "term-a", true)];

        assert!(
            dock_is_gone(&list, &panes, |_| Some(false)),
            "the server sees no tower, whatever the token says"
        );
        assert!(
            !dock_is_gone(&list, &panes, |_| Some(true)),
            "a live tower is a present dock"
        );
        assert!(
            !dock_is_gone(&list, &panes, unobserved),
            "an unanswered observation has only the token to go on"
        );
    }

    /// A launch in flight is still present even before any process exists —
    /// the lease is the evidence there, not the process table.
    #[test]
    fn a_launch_under_way_outranks_an_empty_process_table() {
        let list = [ws("w9", tower_ui::DOCK_LABEL)];
        let mut starting = pane("w9:p1", "w9", "term-a", false);
        starting.tokens.insert(
            crate::dockcmd::STARTING_TOKEN.to_string(),
            crate::dockcmd::pane_starting_lease_value(3, "owner", "term-a"),
        );
        assert!(!dock_is_gone(&list, &[starting], |_| Some(false)));
    }

    /// Renaming the dock does not stop its tower. Treating the rename as a
    /// deletion would leave that tower running under the new name while a
    /// second one is built beside it.
    #[test]
    fn a_renamed_dock_is_reclaimed_rather_than_rebuilt() {
        let renamed = [ws("w9", "my stuff")];
        let panes = [pane("w9:p1", "w9", "term-a", true)];

        assert_eq!(
            dock_to_reclaim(&renamed, &panes, |_| Some(true)),
            Some(Mislabeled::Reclaim("w9".to_string())),
            "a live tower under another label is the dock, mislabeled"
        );

        // No tower there: an ordinary workspace someone happens to own.
        assert_eq!(dock_to_reclaim(&renamed, &panes, |_| Some(false)), None);

        // 🚨 `herdr-tower` run by hand is the same executable, and its
        // workspace is not the dock's to take. Only `dock` publishes the
        // bound active claim.
        let by_hand = [pane("w9:p1", "w9", "term-a", false)];
        assert_eq!(
            dock_to_reclaim(&renamed, &by_hand, |_| Some(true)),
            None,
            "a tower someone opened themselves keeps its workspace"
        );

        // 🚨 Unknown liveness is not liveness. Relabeling a shell whose tower
        // has exited would make every later pass read its leftover token as a
        // present dock, and the tower would never come back.
        assert_eq!(
            dock_to_reclaim(&renamed, &panes, unobserved),
            None,
            "a server that cannot confirm the tower reclaims nothing"
        );

        // The label as it was before it was upper-cased: an upgrade finds the
        // running dock under it, and takes it back rather than building a
        // second one.
        let before_upper_case = [ws("w9", "🗼 herdr-tower 🗼")];
        assert_eq!(
            dock_to_reclaim(&before_upper_case, &panes, |_| Some(true)),
            Some(Mislabeled::Reclaim("w9".to_string())),
            "the dock under its old spelling is renamed, not duplicated"
        );

        // Already correctly labeled: nothing to reclaim.
        let proper = [ws("w9", tower_ui::DOCK_LABEL)];
        assert_eq!(dock_to_reclaim(&proper, &panes, |_| Some(true)), None);

        // 🚨 An empty workspace already wearing the label, with a live dock
        // renamed elsewhere. Renaming would duplicate the public label;
        // restarting would build a second tower in the empty one, because
        // `dock` only inspects canonically labeled workspaces and cannot see
        // the live tower it would duplicate. Neither — say so, so the caller
        // suppresses the restart too.
        let both = [ws("w8", tower_ui::DOCK_LABEL), ws("w9", "my stuff")];
        assert_eq!(
            dock_to_reclaim(&both, &panes, |_| Some(true)),
            Some(Mislabeled::Conflicted {
                stale: "w8".to_string(),
                live: "w9".to_string()
            }),
            "name the impostor so the label can be freed for the live one"
        );

        // A launch under way is not a tower yet — that workspace belongs to
        // the `dock` invocation building it.
        let mut starting = pane("w9:p1", "w9", "term-a", false);
        starting.tokens.insert(
            crate::dockcmd::STARTING_TOKEN.to_string(),
            crate::dockcmd::pane_starting_lease_value(3, "owner", "term-a"),
        );
        assert_eq!(
            dock_to_reclaim(&renamed, &[starting], |_| Some(false)),
            None
        );
    }

    /// The three boundary counts are here rather than generated: the closed form
    /// is three lines, and the only values that are not the line above or below
    /// them are the last step under the ceiling, the first one at it, and one
    /// past.
    #[test]
    fn retries_lengthen_and_then_stop_growing() {
        assert_eq!(backoff(0), Duration::ZERO);
        assert_eq!(backoff(1), RETRY_STEP);
        assert_eq!(backoff(2), RETRY_STEP * 2);
        // The last step still under the minute, the first one at it, and the one
        // after that — where `min` starts doing the work and the growth stops.
        assert_eq!(backoff(11), RETRY_STEP * 11);
        assert!(backoff(11) < RETRY_CEILING);
        assert_eq!(backoff(12), RETRY_CEILING);
        assert_eq!(backoff(13), RETRY_CEILING);
        assert_eq!(backoff(100), RETRY_CEILING);
        // And the count that would overflow a multiplication done without
        // `saturating_mul`.
        assert_eq!(backoff(u32::MAX), RETRY_CEILING);
    }

    /// A file that exists and cannot be executed.
    ///
    /// 🚨 Not a path that is simply absent: [`launcher`] answers that with the
    /// installed `herdr-tower` on `PATH`, and a test meant to observe a failed
    /// start would launch a real tower into the developer's own session
    /// instead. That is not hypothetical — it happened while writing this.
    fn unrunnable() -> PathBuf {
        // No execute bit on Unix, and no extension Windows runs.
        let path = std::env::temp_dir().join(format!(
            "tower-dockkeep-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"not executable").expect("write probe file");
        tower_os::set_mode(&path, 0o600).expect("drop the execute bit");
        path
    }

    /// A dock that is present starts nothing.
    #[test]
    fn a_present_dock_starts_nothing() {
        let mut keeper = Keeper::new(unrunnable(), None);
        assert_eq!(keeper.tick(true, Some(false), Instant::now()), None);
        assert!(!keeper.restarting());
        assert_eq!(keeper.failures, 0);
    }

    /// A failed start is counted once and then held off, rather than retried
    /// on every pass of a two-second loop.
    #[test]
    fn a_failed_restart_is_held_off_before_the_next_try() {
        let start = Instant::now();
        let mut keeper = Keeper::new(unrunnable(), None);
        assert!(
            matches!(
                keeper.tick(true, Some(true), start),
                Some(Restart::Failed(_))
            ),
            "a failed start is reported, for the supervisor's log"
        );
        assert!(!keeper.restarting());
        assert_eq!(keeper.failures, 1);

        assert_eq!(
            keeper.tick(true, Some(true), start),
            None,
            "nothing is tried, so nothing is reported"
        );
        assert_eq!(keeper.failures, 1, "held off inside the backoff");

        keeper.tick(
            true,
            Some(true),
            start + backoff(1) + Duration::from_millis(1),
        );
        assert_eq!(keeper.failures, 2, "tried again once the wait elapsed");
    }

    /// Turning the setting off while a restart runs has to stop that restart —
    /// otherwise it finishes and reopens the dock the user just asked to be
    /// able to close.
    #[cfg(unix)]
    #[test]
    fn turning_keeping_off_stops_a_restart_in_flight() {
        let mut keeper = Keeper::new(PathBuf::from("/bin/sleep"), None);
        keeper.running = Some(
            Command::new("/bin/sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn a long-running stand-in"),
        );
        let pid = keeper.running.as_ref().map(Child::id).expect("child pid");

        keeper.tick(false, Some(true), Instant::now());

        assert!(!keeper.restarting(), "the child was taken and waited for");
        // Waited for, so the PID is not a zombie this process still owns.
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists() || cfg!(target_os = "macos"),
            "the stand-in is gone"
        );
    }

    /// 🚨 An unreadable backend is not a recovered dock. One transient read
    /// failure must not discard a backoff that several real failures earned.
    #[test]
    fn an_unknown_observation_neither_restarts_nor_forgets() {
        let start = Instant::now();
        let mut keeper = Keeper::new(unrunnable(), None);
        keeper.tick(true, Some(true), start);
        assert_eq!(keeper.failures, 1);
        let deadline = keeper.hold_until;

        keeper.tick(true, None, start + backoff(1) + Duration::from_millis(1));
        assert_eq!(keeper.failures, 1, "nothing was attempted");
        assert_eq!(keeper.hold_until, deadline, "and nothing was forgotten");

        // A dock actually seen present is what clears it.
        keeper.tick(
            true,
            Some(false),
            start + backoff(1) + Duration::from_millis(2),
        );
        assert_eq!(keeper.failures, 0);
        assert_eq!(keeper.hold_until, None);
    }

    /// A dock that is gone is left alone while the setting is off, and the
    /// backoff is not spent on restarts nobody asked for.
    #[test]
    fn keeping_off_starts_nothing_for_a_gone_dock() {
        let mut keeper = Keeper::new(unrunnable(), None);
        keeper.tick(false, Some(true), Instant::now());
        assert!(!keeper.restarting());
        assert_eq!(keeper.failures, 0);
    }

    /// A dock that cannot be restarted at all — no herdr server, a binary that
    /// will not exec — must cost less and less. The wait grows by one step per
    /// consecutive failure and then stops at the ceiling, so a machine that
    /// will never recover is not spawning a process every couple of seconds
    /// forever, while a machine whose cause goes away still recovers by itself
    /// within a minute.
    ///
    /// Driven by explicit instants: the keeper's clock is an argument, so none
    /// of this waits on a real one.
    ///
    /// The ladder is written out rather than asked of [`backoff`]. Asking would
    /// restate the production line under test — `failed` sets
    /// `now + backoff(self.failures)` — and the whole ladder would then be
    /// whatever `backoff` says it is: replacing it with "no wait, then straight
    /// to the ceiling" would keep this test green.
    #[test]
    fn consecutive_failures_lengthen_the_wait_up_to_the_ceiling() {
        // Attempt 1 through 20, five seconds more each time until the minute is
        // reached at the twelfth, and that same minute from then on.
        let ladder = [
            RETRY_STEP,
            RETRY_STEP * 2,
            RETRY_STEP * 3,
            RETRY_STEP * 4,
            RETRY_STEP * 5,
            RETRY_STEP * 6,
            RETRY_STEP * 7,
            RETRY_STEP * 8,
            RETRY_STEP * 9,
            RETRY_STEP * 10,
            RETRY_STEP * 11,
            RETRY_CEILING,
            RETRY_CEILING,
            RETRY_CEILING,
            RETRY_CEILING,
            RETRY_CEILING,
            RETRY_CEILING,
            RETRY_CEILING,
            RETRY_CEILING,
            RETRY_CEILING,
        ];
        assert!(
            RETRY_STEP * 11 < RETRY_CEILING,
            "the ladder above has to reach the ceiling exactly at the twelfth"
        );

        let mut keeper = Keeper::new(unrunnable(), None);
        let mut now = Instant::now();
        for attempt in 1..=20u32 {
            assert!(
                matches!(keeper.tick(true, Some(true), now), Some(Restart::Failed(_))),
                "attempt {attempt}: the wait had elapsed, so a try was due"
            );
            assert_eq!(keeper.failures, attempt);
            let until = keeper.hold_until.expect("a failure always sets a deadline");
            assert_eq!(
                until - now,
                ladder[attempt as usize - 1],
                "attempt {attempt}"
            );

            // One instant short of the deadline nothing is tried, and the run
            // of failures is not added to by a pass that did not start anything.
            assert_eq!(
                keeper.tick(true, Some(true), until - Duration::from_nanos(1)),
                None,
                "attempt {attempt}: started one inside the wait"
            );
            assert_eq!(keeper.failures, attempt, "attempt {attempt}");

            // The deadline itself is when the next try is allowed — the guard
            // is `now < until`, so reaching it releases the hold.
            now = until;
        }
        assert_eq!(
            keeper.failures, 20,
            "every pass in the loop was a failure of its own"
        );
    }

    /// A dock that is seen again ends the run of failures, whoever fixed it.
    /// Keeping the deadline would make the next tab someone closes wait out as
    /// much as a minute, for failures that are no longer consecutive — and that
    /// tab is the common case the whole feature exists for.
    #[test]
    fn a_dock_seen_again_clears_a_wait_that_had_reached_the_ceiling() {
        let mut keeper = Keeper::new(unrunnable(), None);
        let mut now = Instant::now();
        for _ in 0..15 {
            keeper.tick(true, Some(true), now);
            now = keeper.hold_until.expect("a failure always sets a deadline");
        }
        assert_eq!(backoff(keeper.failures), RETRY_CEILING);

        assert_eq!(
            keeper.tick(true, Some(false), now),
            None,
            "a dock that is there needs nothing started"
        );
        assert_eq!(keeper.failures, 0);
        assert_eq!(keeper.hold_until, None);

        assert!(
            matches!(keeper.tick(true, Some(true), now), Some(Restart::Failed(_))),
            "the next close is served at once, not a minute later"
        );
        assert_eq!(
            keeper.failures, 1,
            "the ladder starts from the bottom again"
        );
    }

    /// Turning keeping off and on again is someone saying "leave it alone" and
    /// then changing their mind. The wait earned before must not be inherited:
    /// the dock they just asked for back would stay shut for up to a minute.
    #[test]
    fn turning_keeping_off_and_on_again_discards_the_wait() {
        let mut keeper = Keeper::new(unrunnable(), None);
        let mut now = Instant::now();
        for _ in 0..3 {
            keeper.tick(true, Some(true), now);
            now = keeper.hold_until.expect("a failure always sets a deadline");
        }
        assert_eq!(keeper.failures, 3);

        assert_eq!(
            keeper.tick(false, Some(true), now),
            None,
            "the setting is off: nothing started, nothing to report"
        );
        assert_eq!(keeper.failures, 0);
        assert_eq!(keeper.hold_until, None);

        assert!(
            matches!(keeper.tick(true, Some(true), now), Some(Restart::Failed(_))),
            "the pass after it is turned back on tries at once"
        );
    }

    /// xorshift64*: the same sequence on every machine, so a failure names the
    /// seed that replays it — from [`tower_testrng`], which is the workspace's
    /// one generator.
    /// The same sequence of passes, judged by a plain state machine written out
    /// here from the rules in [`Keeper::tick`]'s own doc comment. The real one
    /// carries a child process and a clock; if the two disagree, one of them
    /// has picked up a rule nobody wrote down — the setting overriding the
    /// observation, an unknown snapshot quietly resetting the backoff, a hold
    /// released one pass early.
    ///
    /// The executable cannot be exec'd, so every start fails and no restart is
    /// ever in flight — which the model relies on, and which is asserted every
    /// pass.
    #[test]
    fn the_keeper_agrees_with_a_plain_model_of_its_rules() {
        let executable = unrunnable();
        for seed in 0..200u64 {
            let mut rng = Rng::new(seed);
            let mut keeper = Keeper::new(executable.clone(), None);
            let mut now = Instant::now();

            // The model.
            let mut failures = 0u32;
            let mut hold: Option<Instant> = None;

            for pass in 0..30 {
                now += Duration::from_secs(rng.below(90) as u64);
                let keep = rng.below(100) < 80;
                let gone = match rng.below(4) {
                    0 => None,
                    1 => Some(false),
                    _ => Some(true),
                };

                let want = if !keep {
                    failures = 0;
                    hold = None;
                    None
                } else {
                    match gone {
                        Some(false) => {
                            failures = 0;
                            hold = None;
                            None
                        }
                        None => None,
                        Some(true) if hold.is_some_and(|until| now < until) => None,
                        Some(true) => {
                            failures += 1;
                            hold = Some(now + backoff(failures));
                            Some("failed")
                        }
                    }
                };

                let got = match keeper.tick(keep, gone, now) {
                    None => None,
                    Some(Restart::Failed(_)) => Some("failed"),
                    Some(Restart::Started) => Some("started"),
                };
                let at = format!("seed {seed} pass {pass}: keep={keep} gone={gone:?}");
                assert_eq!(got, want, "{at}");
                assert_eq!(keeper.failures, failures, "{at}");
                assert_eq!(keeper.hold_until, hold, "{at}");
                assert!(
                    !keeper.restarting(),
                    "{at}: a restart is in flight that could never have started"
                );
            }
        }
    }
    /// 🚨 The tab of a live dock tower is reported whatever that tab is named,
    /// and on every pass — which is the whole of #40. A reclaim that renamed the
    /// workspace and then failed on the tab leaves exactly this state: the
    /// workspace wears the dock label, the tab wears the old name. Nothing
    /// looked at it again, and the next herdr restart built a second tab.
    #[test]
    fn a_live_dock_tower_reports_its_tab_every_pass() {
        let dock = [ws("w9", tower_ui::DOCK_LABEL)];
        let panes = [pane_in_tab("w9:p1", "w9", "w9:t1", "term-a", true)];
        // Unconditional on the workspace's own label being wrong, which is what
        // the reclaim path was: the workspace here wears the dock label already.
        assert_eq!(dock_tower_tabs(&dock, &panes, observed_tower), ["w9:t1"]);
        // And unconditional on what the tab is called. The name is the caller's
        // question — `dockcmd::tab_needs_dock_label` answers it from the server's
        // own tab list, which is where the labels are.
        assert!(crate::dockcmd::tab_needs_dock_label(
            &[serde_json::from_value(serde_json::json!({
                "tab_id": "w9:t1", "workspace_id": "w9", "label": "20.herdr-tower"
            }))
            .unwrap()],
            "w9:t1"
        ));
        assert!(!crate::dockcmd::tab_needs_dock_label(
            &[serde_json::from_value(serde_json::json!({
                "tab_id": "w9:t1", "workspace_id": "w9", "label": crate::dockcmd::TAB_LABEL
            }))
            .unwrap()],
            "w9:t1"
        ));
    }

    /// The same identity rule as the reclaim: the terminal-bound claim says
    /// which tower this is, and a confirmed live process says it is still there.
    /// 🚨 Neither alone is enough — a `herdr-tower` somebody opened by hand in
    /// the dock workspace is the same executable, and a killed tower leaves its
    /// token behind — and renaming the wrong tab takes a name off someone's
    /// work.
    #[test]
    fn only_a_claimed_and_confirmed_tower_has_its_tab_renamed() {
        let dock = [ws("w9", tower_ui::DOCK_LABEL)];
        let claimed = [pane_in_tab("w9:p1", "w9", "w9:t1", "term-a", true)];
        let unclaimed = [pane_in_tab("w9:p1", "w9", "w9:t1", "term-a", false)];

        assert!(dock_tower_tabs(&dock, &claimed, no_tower).is_empty());
        assert!(dock_tower_tabs(&dock, &claimed, unobserved).is_empty());
        assert!(dock_tower_tabs(&dock, &unclaimed, observed_tower).is_empty());
        assert_eq!(dock_tower_tabs(&dock, &claimed, observed_tower), ["w9:t1"]);
    }

    /// A tower outside the dock's workspace is not the dock's, and a pane whose
    /// tab the server did not name has no tab to rename.
    #[test]
    fn a_tower_elsewhere_and_a_pane_with_no_tab_are_left_alone() {
        let list = [ws("w9", tower_ui::DOCK_LABEL), ws("w2", "20.herdr-tower")];
        let elsewhere = [pane_in_tab("w2:p1", "w2", "w2:t1", "term-a", true)];
        assert!(dock_tower_tabs(&list, &elsewhere, observed_tower).is_empty());

        let untabbed = [pane("w9:p1", "w9", "term-a", true)];
        assert!(dock_tower_tabs(&list, &untabbed, observed_tower).is_empty());
    }

    /// Two panes of one dock tab — a split — name that tab once, and two dock
    /// tabs are both reported, in a stable order. The caller renames what it is
    /// given, so a repeated id would be a repeated server call.
    #[test]
    fn each_dock_tab_is_named_once() {
        let dock = [ws("w9", tower_ui::DOCK_LABEL)];
        let panes = [
            pane_in_tab("w9:p2", "w9", "w9:t2", "term-b", true),
            pane_in_tab("w9:p1", "w9", "w9:t1", "term-a", true),
            pane_in_tab("w9:p3", "w9", "w9:t1", "term-c", true),
        ];
        assert_eq!(
            dock_tower_tabs(&dock, &panes, observed_tower),
            ["w9:t1", "w9:t2"]
        );
    }
}
