// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The usage tab: `herdr-tower usage --watch` in a tab of its own, beside the
//! dock's, in the dock workspace. Off by default since the tower shows the
//! board itself (`u`); `usage_tab: true` keeps it, and while it is off a tab
//! left from before is closed ([`retire`]).
//!
//! The resident supervisor keeps it, the way it keeps the dock: it creates
//! the tab when there is none, starts the board again in a tab whose shell sits
//! idle (a herdr restart brings tabs back but not their processes), and puts
//! the label back when something renamed it. It never closes anything, and it
//! never touches a tab it cannot tell is the usage tab.
//!
//! **How the tab is known.** While the board runs, its pane carries the
//! expiring token [`TOKEN`], planted by the board itself. Tokens do not
//! survive a herdr restart, and neither does the process, so after a restart
//! the tab is found by its label, [`LABEL`], the one thing herdr keeps.
//!
//! **Why the dock has to know about it.** herdr reports a foreground process
//! by its argument list where it can, and there `herdr-tower usage --watch` is
//! plainly not the tower. Where it cannot, the dock falls back to the process
//! name, and the board's name is the tower's. A pane with [`TOKEN`] is
//! therefore never taken for the tower's ([`is_usage_pane`]).

use std::path::Path;
use std::time::{Duration, Instant};

use herdrx::{Herdr, Pane, ProcessInfo, Tab, TabOptions, Workspace};

/// The tab's label.
pub const LABEL: &str = "🔋 USAGE-GUARD 🔋";
/// Labels the tab had in earlier releases. After a herdr restart the tab is
/// found by its label alone, so a tab still wearing one of these is the usage
/// tab too: its board is started again, and the label then put right.
pub const FORMER_LABELS: &[&str] = &["📊 USAGE-GUARD", "📊 USAGE"];

/// Whether a tab's label names the usage tab, now or in an earlier release —
/// with or without a legacy ID prefix. Typed tab reads already remove the
/// workspace display prefix, so keepers compare the semantic [`LABEL`].
pub fn is_usage_label(label: &str) -> bool {
    let name = herdrx::strip_id_prefix(label);
    name == LABEL || FORMER_LABELS.contains(&name)
}
/// The token the running board keeps on its own pane. Not `dock-`: nothing
/// here is dock state, and the dock's own token sweeps key on that prefix.
pub const TOKEN: &str = "tower-usage";
/// The metadata source the token is reported under.
pub const SOURCE: &str = "herdr-tower-usage";
/// How long the token outlives the board's last refresh of it. The board
/// refreshes it on every redraw ([`crate::usagecmd`]'s watch interval), so a
/// board that died stops claiming its pane within this.
pub const TOKEN_TTL: Duration = Duration::from_secs(60);

/// Whether this pane runs the usage board, and so is never the tower's.
pub fn is_usage_pane(pane: &Pane) -> bool {
    pane.tokens.get(TOKEN).is_some_and(|v| !v.is_empty())
}

/// What the supervisor does about the usage tab on one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// It is there and running, or there is something in its tab that is not
    /// an idle shell and is not ours to replace.
    Nothing,
    /// No tab: create one and start the board in it.
    Create,
    /// The tab is there with an idle shell: start the board in it.
    Launch { pane_id: String },
    /// The board runs, under another label.
    Rename { tab_id: String },
    /// The usage tab sits left of the tower's. herdr can neither move a tab
    /// nor create one anywhere but at the end, so it is closed, and the next
    /// pass creates it again — at the end, right of the tower.
    Reorder { tab_id: String },
}

/// Decides the pass, from the dock workspace's tabs and panes.
///
/// `idle_shell` answers whether a pane's foreground is only its shell
/// (herdr's process observer); `None` when that could not be asked, which
/// counts as "not idle" — typing into a pane that may be running something is
/// the mistake to avoid.
pub fn plan(
    dock_workspace_id: &str,
    tabs: &[Tab],
    panes: &[Pane],
    idle_shell: impl Fn(&Pane) -> Option<bool>,
) -> Action {
    let in_dock = |p: &&Pane| p.workspace_id == dock_workspace_id;
    // Where the tower's tab is: the one holding the pane the tower claimed.
    let tower_at = panes
        .iter()
        .filter(in_dock)
        .find(|p| p.tokens.contains_key(crate::dockcmd::ACTIVE_TOKEN))
        .and_then(|p| tabs.iter().find(|t| t.tab_id == p.tab_id))
        .map(|t| t.number);
    let left_of_tower = |t: &Tab| tower_at.is_some_and(|n| t.number > 0 && t.number < n);
    // A tab is ours to close only when it holds nothing but the board, or
    // an idle shell: one pane, no agent.
    let only = |t: &Tab| -> Option<&Pane> {
        let mine: Vec<&Pane> = panes.iter().filter(|p| p.tab_id == t.tab_id).collect();
        match mine.as_slice() {
            [p] if !p.has_agent() => Some(*p),
            _ => None,
        }
    };
    // The board is running: its place, then its label.
    if let Some(p) = panes.iter().filter(in_dock).find(|p| is_usage_pane(p)) {
        let tab = tabs.iter().find(|t| t.tab_id == p.tab_id);
        if let Some(t) = tab.filter(|t| left_of_tower(t) && only(t).is_some()) {
            return Action::Reorder {
                tab_id: t.tab_id.clone(),
            };
        }
        return if tab.is_some_and(|t| t.label == LABEL) {
            Action::Nothing
        } else {
            Action::Rename {
                tab_id: p.tab_id.clone(),
            }
        };
    }
    let usage_tabs: Vec<&Tab> = tabs
        .iter()
        .filter(|t| t.workspace_id == dock_workspace_id && is_usage_label(&t.label))
        .collect();
    if usage_tabs.is_empty() {
        return Action::Create;
    }
    // A labelled tab with no board: start it again in a single idle shell —
    // or, when it sits left of the tower, close it to be made again after.
    // Several panes, or an agent, means someone is using the tab for
    // something else; it is left alone rather than typed into.
    for t in usage_tabs {
        if let Some(p) = only(t) {
            if idle_shell(p) == Some(true) {
                return if left_of_tower(t) {
                    Action::Reorder {
                        tab_id: t.tab_id.clone(),
                    }
                } else {
                    Action::Launch {
                        pane_id: p.pane_id.clone(),
                    }
                };
            }
        }
    }
    Action::Nothing
}

/// The usage tabs to close now that the tower shows the board itself: in the
/// dock workspace, under the tab's label, holding one pane that is the board
/// or an idle shell. A tab holding anything else is somebody's and stays.
pub fn retire_plan(
    dock_workspace_id: &str,
    tabs: &[Tab],
    panes: &[Pane],
    idle_shell: impl Fn(&Pane) -> Option<bool>,
) -> Vec<String> {
    tabs.iter()
        .filter(|t| t.workspace_id == dock_workspace_id && is_usage_label(&t.label))
        .filter(|t| {
            let mine: Vec<&Pane> = panes.iter().filter(|p| p.tab_id == t.tab_id).collect();
            matches!(mine.as_slice(),
                [p] if !p.has_agent() && (is_usage_pane(p) || idle_shell(p) == Some(true)))
        })
        .map(|t| t.tab_id.clone())
        .collect()
}

/// Closes the usage tab once, when the supervisor no longer keeps it
/// (`usage_tab` off — the default since the tower shows the board on `u`).
/// Answers what it did, for the log.
pub fn retire(h: &Herdr, workspaces: &[Workspace], panes: &[Pane]) -> Option<String> {
    let ws = workspaces
        .iter()
        .find(|w| w.label == tower_ui::DOCK_LABEL)?;
    let tabs = h.tabs().ok()?;
    let closing = retire_plan(&ws.workspace_id, &tabs, panes, |p| {
        h.process_info(&p.pane_id).ok().map(|i| idle_shell(&i))
    });
    let closed: Vec<String> = closing
        .into_iter()
        .filter(|t| h.close_tab(t).is_ok())
        .collect();
    (!closed.is_empty()).then(|| {
        format!(
            "usage tab closed ({}) — the tower shows the usage board on `u`",
            closed.join(", ")
        )
    })
}

/// The line typed into the tab's shell. Only the state directory and the
/// config file are passed on: never the dock's own `HERDR_TOWER_DOCK*`
/// variables, which would make the board's process a docked tower.
pub fn launch_command(exe: &str, state_dir: &str, config: Option<&str>) -> String {
    let mut vars = vec![("HERDR_TOWER_STATE_DIR", state_dir)];
    if let Some(c) = config {
        vars.push(("HERDR_TOWER_CONFIG", c));
    }
    crate::dockcmd::shell_line(&vars, exe, &["usage", "--watch"])
}

/// What puts things back after the tab is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settle {
    FocusPane(String),
    FocusWorkspace(String),
}

/// herdr may make a new tab its workspace's active one, and the view may
/// follow it, whatever `focus` asked. Neither may stick:
/// - a click on the dock must keep landing on the tower, so its tab is made
///   the active one again (herdr sets a workspace's active tab only by
///   focusing it);
/// - the person must stay where they were.
///
/// With nothing known about where they were, the view is not touched at all:
/// focusing the tower would take them to the dock.
pub fn settle(
    was: Option<&str>,
    dock_ws: &str,
    new_tab: &str,
    active_after: Option<&str>,
    focused_after: Option<&str>,
    tower_pane: Option<&str>,
) -> Vec<Settle> {
    let Some(was) = was else { return Vec::new() };
    let mut steps = Vec::new();
    if active_after == Some(new_tab) {
        if let Some(p) = tower_pane {
            steps.push(Settle::FocusPane(p.to_string()));
        }
    }
    let moved = !steps.is_empty() || focused_after != Some(was);
    if moved && was != dock_ws {
        steps.push(Settle::FocusWorkspace(was.to_string()));
    }
    steps
}

/// Whether a pane's foreground is only its shell: nothing is running there
/// that typing a command would interrupt.
pub fn idle_shell(info: &ProcessInfo) -> bool {
    info.shell_pid > 0
        && !info.foreground_processes.is_empty()
        && info
            .foreground_processes
            .iter()
            .all(|process| process.pid == info.shell_pid)
}

/// The shortest time between two things the supervisor does to the tab. A
/// board that will not start (a missing binary, a broken shell) is tried
/// again at this pace rather than on every two-second pass, and a new tab's
/// shell gets this long to come up before the board is typed into it.
pub const PACE: Duration = Duration::from_secs(10);

/// The wait after creating the tab before the board is typed into it: long
/// enough for the new shell to reach its prompt (and it is typed only once
/// herdr sees the shell idle), short enough that the tab is not an empty
/// shell for long.
pub const AFTER_CREATE: Duration = Duration::from_secs(2);

/// How long after `previous` the keeper may act again.
pub fn pace_after(previous: Option<&Action>) -> Duration {
    match previous {
        Some(Action::Create) => AFTER_CREATE,
        _ => PACE,
    }
}

/// Keeps the usage tab on the supervisor's passes. Holds only the pace.
#[derive(Default)]
pub struct Keeper {
    last: Option<(Instant, Action)>,
}

impl Keeper {
    /// One pass: called only while the dock is there with its tower in it.
    /// Answers what it did, for the log.
    pub fn tick(
        &mut self,
        h: &Herdr,
        workspaces: &[Workspace],
        panes: &[Pane],
        exe: &Path,
        now: Instant,
    ) -> Option<String> {
        if let Some((at, previous)) = &self.last {
            if now.duration_since(*at) < pace_after(Some(previous)) {
                return None;
            }
        }
        let ws = workspaces
            .iter()
            .find(|w| w.label == tower_ui::DOCK_LABEL)?;
        let tabs = h.tabs().ok()?;
        let action = plan(&ws.workspace_id, &tabs, panes, |p| {
            h.process_info(&p.pane_id).ok().map(|i| idle_shell(&i))
        });
        if action == Action::Nothing {
            return None;
        }
        self.last = Some((now, action.clone()));
        let command = || {
            launch_command(
                &exe.to_string_lossy(),
                &tower_state::default_dir().to_string_lossy(),
                std::env::var("HERDR_TOWER_CONFIG")
                    .ok()
                    .filter(|c| !c.is_empty())
                    .as_deref(),
            )
        };
        let done = match &action {
            // The board goes in on a later pass, once the new shell sits idle:
            // typed now, it could reach a shell that has not started reading.
            Action::Create => {
                let was = h.focused_workspace().ok().flatten().map(|w| w.workspace_id);
                let tower_pane = panes
                    .iter()
                    .find(|p| {
                        p.workspace_id == ws.workspace_id
                            && p.tokens.contains_key(crate::dockcmd::ACTIVE_TOKEN)
                    })
                    .map(|p| p.pane_id.clone());
                h.new_tab(&TabOptions {
                    workspace_id: Some(ws.workspace_id.clone()),
                    label: Some(LABEL.to_string()),
                    cwd: std::env::var("HOME").ok(),
                    env: Default::default(),
                    focus: false,
                })
                .map(|(tab, _)| {
                    let after = h.workspaces().unwrap_or_default();
                    let active = after
                        .iter()
                        .find(|w| w.workspace_id == ws.workspace_id)
                        .and_then(|w| w.active_tab_id.clone());
                    let focused = after
                        .iter()
                        .find(|w| w.focused)
                        .map(|w| w.workspace_id.clone());
                    for step in settle(
                        was.as_deref(),
                        &ws.workspace_id,
                        &tab.tab_id,
                        active.as_deref(),
                        focused.as_deref(),
                        tower_pane.as_deref(),
                    ) {
                        let _ = match step {
                            Settle::FocusPane(p) => h.focus_pane(&p),
                            Settle::FocusWorkspace(w) => h.focus_workspace(&w),
                        };
                    }
                    "usage tab: created".to_string()
                })
            }
            Action::Launch { pane_id } => h
                .run(pane_id, &command())
                .map(|()| "usage tab: started the board".to_string()),
            Action::Rename { tab_id } => h
                .rename_tab(tab_id, LABEL)
                .map(|()| "usage tab: put its name back".to_string()),
            Action::Reorder { tab_id } => h.close_tab(tab_id).map(|()| {
                "usage tab: left of the tower — closed, to be made again right of it".to_string()
            }),
            Action::Nothing => return None,
        };
        Some(done.unwrap_or_else(|e| format!("usage tab: {action:?} failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const WS: &str = "w1";

    #[test]
    fn only_a_usage_tab_holding_the_board_or_an_idle_shell_is_retired() {
        let tabs = [
            tab("t1", "w", "tower"),
            tab("t2", "w", LABEL),
            tab("t3", "w", FORMER_LABELS[0]),
            tab("t4", "w", LABEL),
            tab("t5", "other", LABEL),
        ];
        let panes = [
            pane("p2", "t2", "w", &[(TOKEN, "1")], None),
            pane("p3", "t3", "w", &[], None),
            // Someone put an agent in this one.
            pane("p4", "t4", "w", &[], Some("claude")),
            pane("p5", "t5", "other", &[(TOKEN, "1")], None),
        ];
        assert_eq!(
            retire_plan("w", &tabs, &panes, |_| Some(true)),
            ["t2", "t3"]
        );
        // A shell that may be running something is not idle.
        assert_eq!(retire_plan("w", &tabs, &panes, |_| None), ["t2"]);
    }

    fn tab(id: &str, ws: &str, label: &str) -> Tab {
        // Numbered by the id's tail, as herdr numbers them left to right.
        let number: u32 = id
            .rsplit(":t")
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        serde_json::from_value(serde_json::json!({
            "tab_id": id, "workspace_id": ws, "label": label, "number": number
        }))
        .unwrap()
    }

    fn pane(id: &str, tab: &str, ws: &str, tokens: &[(&str, &str)], agent: Option<&str>) -> Pane {
        let tokens: HashMap<String, String> = tokens
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        serde_json::from_value(serde_json::json!({
            "pane_id": id, "tab_id": tab, "workspace_id": ws, "terminal_id": format!("t-{id}"),
            "tokens": tokens, "agent": agent,
        }))
        .unwrap()
    }

    fn dock_tab() -> (Tab, Pane) {
        (
            tab("w1:t1", WS, crate::dockcmd::TAB_LABEL),
            pane("p1", "w1:t1", WS, &[("dock-active", "x")], None),
        )
    }

    #[test]
    fn a_new_tab_leaves_the_tower_active_and_the_view_where_it_was() {
        let f = |p: &str| Settle::FocusPane(p.into());
        let w = |p: &str| Settle::FocusWorkspace(p.into());
        // herdr made the new tab active and took the view: both go back.
        assert_eq!(
            settle(
                Some("wF"),
                "w26",
                "w26:t2",
                Some("w26:t2"),
                Some("w26"),
                Some("w26:p1")
            ),
            [f("w26:p1"), w("wF")]
        );
        // Active changed, view did not: the tower is re-activated through a
        // focus, so the view is put back all the same.
        assert_eq!(
            settle(
                Some("wF"),
                "w26",
                "w26:t2",
                Some("w26:t2"),
                Some("wF"),
                Some("w26:p1")
            ),
            [f("w26:p1"), w("wF")]
        );
        // They were on the dock: the tower's tab comes back and they stay.
        assert_eq!(
            settle(
                Some("w26"),
                "w26",
                "w26:t2",
                Some("w26:t2"),
                Some("w26"),
                Some("w26:p1")
            ),
            [f("w26:p1")]
        );
        // Nothing changed: nothing is done.
        assert_eq!(
            settle(
                Some("wF"),
                "w26",
                "w26:t2",
                Some("w26:t1"),
                Some("wF"),
                Some("w26:p1")
            ),
            []
        );
        // Only the view moved.
        assert_eq!(
            settle(
                Some("wF"),
                "w26",
                "w26:t2",
                Some("w26:t1"),
                Some("w26"),
                Some("w26:p1")
            ),
            [w("wF")]
        );
        // Where they were is unknown: nothing that moves the view.
        assert_eq!(
            settle(
                None,
                "w26",
                "w26:t2",
                Some("w26:t2"),
                Some("w26"),
                Some("w26:p1")
            ),
            []
        );
    }

    /// A new tab gets its board as soon as its shell is idle; a board that
    /// keeps failing to start is tried again at the slow pace.
    #[test]
    fn the_board_follows_a_new_tab_quickly_and_retries_slowly() {
        assert_eq!(pace_after(Some(&Action::Create)), AFTER_CREATE);
        assert_eq!(
            pace_after(Some(&Action::Launch {
                pane_id: "p".into()
            })),
            PACE
        );
        assert_eq!(pace_after(None), PACE);
        assert!(AFTER_CREATE < PACE);
    }

    #[test]
    fn no_tab_is_created() {
        let (t, p) = dock_tab();
        assert_eq!(plan(WS, &[t], &[p], |_| Some(true)), Action::Create);
    }

    #[test]
    fn a_running_board_is_left_alone_or_relabelled() {
        let (t, p) = dock_tab();
        let board = pane("p2", "w1:t2", WS, &[(TOKEN, "t-p2")], None);
        let tabs = [t.clone(), tab("w1:t2", WS, LABEL)];
        assert_eq!(
            plan(WS, &tabs, &[p.clone(), board.clone()], |_| Some(true)),
            Action::Nothing
        );
        let tabs = [t, tab("w1:t2", WS, "2")];
        assert_eq!(
            plan(WS, &tabs, &[p, board], |_| Some(true)),
            Action::Rename {
                tab_id: "w1:t2".into()
            }
        );
    }

    /// The usage tab is exempt from `id_labels`: one that got an ID prefix
    /// anyway is still recognised, and has the plain name put back.
    #[test]
    fn a_prefixed_usage_tab_is_still_it_and_gets_its_plain_name_back() {
        assert!(is_usage_label("[w3B:t2] 🔋 USAGE-GUARD 🔋"));
        assert!(
            is_usage_label("[w3B:t2] 📊 USAGE"),
            "a former name, prefixed"
        );
        assert!(!is_usage_label("[wip] 🔋 USAGE-GUARD 🔋"));

        let (t, p) = dock_tab();
        let board = pane("p2", "w1:t2", WS, &[(TOKEN, "t-p2")], None);
        let tabs = [t.clone(), tab("w1:t2", WS, "[w1:t2] 🔋 USAGE-GUARD 🔋")];
        assert_eq!(
            plan(WS, &tabs, &[p.clone(), board], |_| Some(true)),
            Action::Rename {
                tab_id: "w1:t2".into()
            }
        );
        // Restored with the prefix and no board: found, and the board goes in.
        let shell = pane("p2", "w1:t2", WS, &[], None);
        assert_eq!(
            plan(WS, &tabs, &[p, shell], |_| Some(true)),
            Action::Launch {
                pane_id: "p2".into()
            }
        );
    }

    #[test]
    fn a_restored_tab_gets_its_board_back_only_in_an_idle_shell() {
        let (t, p) = dock_tab();
        let tabs = [t, tab("w1:t2", WS, LABEL)];
        let shell = pane("p2", "w1:t2", WS, &[], None);
        let panes = [p.clone(), shell.clone()];
        assert_eq!(
            plan(WS, &tabs, &panes, |_| Some(true)),
            Action::Launch {
                pane_id: "p2".into()
            }
        );
        // Busy, unknowable, an agent, or two panes: not ours to type into.
        assert_eq!(plan(WS, &tabs, &panes, |_| Some(false)), Action::Nothing);
        assert_eq!(plan(WS, &tabs, &panes, |_| None), Action::Nothing);
        let agent = pane("p2", "w1:t2", WS, &[], Some("claude"));
        assert_eq!(
            plan(WS, &tabs, &[p.clone(), agent], |_| Some(true)),
            Action::Nothing
        );
        let second = pane("p3", "w1:t2", WS, &[], None);
        assert_eq!(
            plan(WS, &tabs, &[p, shell, second], |_| Some(true)),
            Action::Nothing
        );
    }

    /// A tab restored under the label an earlier release gave it is still
    /// the usage tab: the board starts in it, and no second tab is made.
    #[test]
    fn a_restored_tab_under_a_former_label_gets_its_board_back() {
        let (t, p) = dock_tab();
        let tabs = [t, tab("w1:t2", WS, FORMER_LABELS[0])];
        let panes = [p, pane("p2", "w1:t2", WS, &[], None)];
        assert_eq!(
            plan(WS, &tabs, &panes, |_| Some(true)),
            Action::Launch {
                pane_id: "p2".into()
            }
        );
        // Once the board runs there, the label is put right.
        let (t, p) = dock_tab();
        let tabs = [t, tab("w1:t2", WS, FORMER_LABELS[0])];
        let board = pane("p2", "w1:t2", WS, &[(TOKEN, "1")], None);
        assert_eq!(
            plan(WS, &tabs, &[p, board], |_| Some(true)),
            Action::Rename {
                tab_id: "w1:t2".into()
            }
        );
    }

    /// The tower's tab was made again after the usage tab, so the usage tab
    /// now sits left of it: the usage tab is closed, to come back right of it.
    #[test]
    fn a_usage_tab_left_of_the_tower_is_closed_to_be_made_again_right_of_it() {
        let tower = tab("w1:t2", WS, crate::dockcmd::TAB_LABEL);
        let tower_pane = pane("p2", "w1:t2", WS, &[("dock-active", "x")], None);
        // Running board, left of the tower.
        let board = pane("p1", "w1:t1", WS, &[(TOKEN, "1")], None);
        let tabs = [tab("w1:t1", WS, LABEL), tower.clone()];
        assert_eq!(
            plan(WS, &tabs, &[board.clone(), tower_pane.clone()], |_| Some(
                true
            )),
            Action::Reorder {
                tab_id: "w1:t1".into()
            }
        );
        // An idle labelled shell, left of the tower: the same.
        let shell = pane("p1", "w1:t1", WS, &[], None);
        assert_eq!(
            plan(WS, &tabs, &[shell, tower_pane.clone()], |_| Some(true)),
            Action::Reorder {
                tab_id: "w1:t1".into()
            }
        );
        // Right of the tower: where it belongs, nothing to do.
        let tabs = [tower, tab("w1:t3", WS, LABEL)];
        let board = pane("p3", "w1:t3", WS, &[(TOKEN, "1")], None);
        assert_eq!(
            plan(WS, &tabs, &[tower_pane.clone(), board], |_| Some(true)),
            Action::Nothing
        );
    }

    #[test]
    fn a_tab_that_is_not_only_ours_is_never_closed() {
        let tower = tab("w1:t2", WS, crate::dockcmd::TAB_LABEL);
        let tower_pane = pane("p2", "w1:t2", WS, &[("dock-active", "x")], None);
        let tabs = [tab("w1:t1", WS, LABEL), tower];
        // The board and a second pane someone split off: not ours alone.
        let board = pane("p1", "w1:t1", WS, &[(TOKEN, "1")], None);
        let other = pane("p9", "w1:t1", WS, &[], None);
        assert_ne!(
            plan(WS, &tabs, &[board, other, tower_pane.clone()], |_| Some(
                true
            )),
            Action::Reorder {
                tab_id: "w1:t1".into()
            }
        );
        // An agent in a labelled tab: never.
        let agent = pane("p1", "w1:t1", WS, &[], Some("claude"));
        assert_eq!(
            plan(WS, &tabs, &[agent, tower_pane.clone()], |_| Some(true)),
            Action::Nothing
        );
        // No tower to be right of: nothing is reordered.
        let board = pane("p1", "w1:t1", WS, &[(TOKEN, "1")], None);
        assert_eq!(
            plan(WS, &[tab("w1:t1", WS, LABEL)], &[board], |_| Some(true)),
            Action::Nothing
        );
    }

    #[test]
    fn another_workspace_is_not_looked_at() {
        let (t, p) = dock_tab();
        // A board and a labelled tab elsewhere do not count for the dock.
        let tabs = [t, tab("w9:t1", "w9", LABEL)];
        let elsewhere = pane("p9", "w9:t1", "w9", &[(TOKEN, "x")], None);
        assert_eq!(
            plan(WS, &tabs, &[p, elsewhere], |_| Some(true)),
            Action::Create
        );
    }

    #[test]
    fn an_empty_token_does_not_mark_a_pane() {
        let p = pane("p", "t", WS, &[(TOKEN, "")], None);
        assert!(!is_usage_pane(&p));
        assert!(is_usage_pane(&pane("p", "t", WS, &[(TOKEN, "x")], None)));
    }

    #[cfg(unix)]
    #[test]
    fn the_launch_line_carries_no_dock_variables() {
        let line = launch_command("/opt/herdr tower/bin/herdr-tower", "/s", Some("/c.json"));
        assert_eq!(
            line,
            "env HERDR_TOWER_STATE_DIR=/s HERDR_TOWER_CONFIG=/c.json '/opt/herdr tower/bin/herdr-tower' usage --watch"
        );
        assert!(!line.contains("HERDR_TOWER_DOCK"));
        assert_eq!(
            launch_command("herdr-tower", "/s", None),
            "env HERDR_TOWER_STATE_DIR=/s herdr-tower usage --watch"
        );
    }
}
