// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Dock command and coordination implementation.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::common::*;
use super::{CliError, Result};

/// Terminals in strict request-labeled tabs whose creator may have died before
/// writing the reusable token. A server-clocked workspace intent is planted
/// before `tab.create`, while the pane lease extends that proof after creation.
/// The label is recoverable only while either exact contribution remains live,
/// so an old request tab that a person may have adopted is preserved.
fn recoverable_request_tab_terminals(
    workspaces: &[herdrx::Workspace],
    tabs: &[herdrx::Tab],
    panes: &[herdrx::Pane],
) -> HashSet<String> {
    let dock_workspaces: HashSet<&str> = workspaces
        .iter()
        .filter(|workspace| workspace.label == tower_cli::dockcmd::LABEL)
        .map(|workspace| workspace.workspace_id.as_str())
        .collect();
    tabs.iter()
        .filter(|tab| {
            dock_workspaces.contains(tab.workspace_id.as_str())
                && tower_cli::dockcmd::tab_request_id(&tab.label).is_some()
        })
        .filter_map(|tab| {
            let workspace_intent_is_live = workspaces.iter().any(|workspace| {
                workspace.workspace_id == tab.workspace_id
                    && tower_cli::dockcmd::tab_creation_intent_is_live(workspace, &tab.label)
            });
            let mut tab_panes = panes
                .iter()
                .filter(|pane| pane.workspace_id == tab.workspace_id && pane.tab_id == tab.tab_id);
            let only = tab_panes.next()?;
            (tab_panes.next().is_none()
                && !only.terminal_id.is_empty()
                && !only.has_agent()
                && (workspace_intent_is_live
                    || only.tokens.get(tower_cli::dockcmd::TAB_CREATING_TOKEN) == Some(&tab.label))
                && !only.tokens.contains_key(tower_cli::dockcmd::REUSABLE_TOKEN))
            .then(|| only.terminal_id.clone())
        })
        .collect()
}

/// Puts the tower in a workspace of its own, so a click in herdr's sidebar
/// opens it. See `tower_cli::dockcmd`.
pub fn run_dock(args: &[String]) -> Result<()> {
    use tower_cli::dockcmd::{self, Plan};

    if matches!(args, [arg] if arg == "--install-startup") {
        return install_dock_startup();
    }
    let (focus, require_active) = match args {
        [] => (false, false),
        [f] if f == "--focus" => (true, false),
        [f] if f == "--require-active" => (false, true),
        _ => return Err("Usage: herdr-tower dock [--focus | --install-startup]".into()),
    };
    let socket = herdrx::default_socket_path().ok_or("the herdr socket is unknown ($HOME)")?;
    // A shared socket-directory inode is a cheap local fast path. It is
    // deliberately optional: a client may have search-only access, or mount
    // only the socket file. The workspace-metadata gate below is authoritative.
    let _server_gate = dockcmd::exclusive_socket_parent(&socket).ok();
    // Absolute before anything uses them: the tower starts in the pane's
    // directory, where a relative path names a different store.
    let state_dir = std::path::absolute(tower_state::default_dir())?;
    let config = std::env::var_os("HERDR_TOWER_CONFIG")
        .filter(|c| !c.is_empty())
        .map(std::path::absolute)
        .transpose()?;
    let dirs = match tower_state::coordination_dir()
        .ok()
        .and_then(|path| std::path::absolute(path).ok())
    {
        Some(coordination) => dockcmd::server_dirs(&coordination, &state_dir, &socket),
        None => dockcmd::server_dirs_without_runtime(&state_dir, &socket),
    };
    // Every writable lock coordinates `dock` runs that share this filesystem.
    // The server-side lease below covers container/host namespace boundaries;
    // unavailable local roots (including read-only /tmp) are only lost fast
    // paths.
    let _one_at_a_time = match dockcmd::exclusive_available_in(&dirs) {
        dockcmd::LocalClaim::Acquired(claims) => claims,
        dockcmd::LocalClaim::Contended => {
            return dock_pending(
                require_active,
                "another dock invocation is coordinating a tower launch",
            );
        }
        dockcmd::LocalClaim::Unavailable => Vec::new(),
    };
    let h = tower_cli::muxsel::default_mux();
    // Only a new place needs a directory to start in, and without a home herdr
    // picks one itself.
    let home = || home().ok().map(|p| p.to_string_lossy().into_owned());

    let executable = self_path();
    let request_id = dockcmd::starting_lease_value();
    let workspace_request_label = dockcmd::workspace_request_label(&request_id);
    let tab_request_label = dockcmd::tab_request_label(&request_id);
    // Only IDs returned by this process's successful `workspace.create` call,
    // or proven by its request-unique label after a lost reply, enter this set.
    // A public label or an empty-looking pane alone is never ownership proof.
    let mut created_candidates = HashSet::new();
    let mut created_candidate_fence = None;
    // The restored dock shell is waited on at most once per run.
    let mut restored_settled = false;
    let (workspaces, tabs, panes, plan) = 'snapshot: loop {
        // Once this invocation has created a private workspace, returning on a
        // transient snapshot failure would discard the only in-memory proof
        // that it may be safely published or closed. Retain ownership and keep
        // reconciling until Herdr can report both halves of the snapshot.
        let workspaces = if created_candidates.is_empty() {
            h.workspaces()?
        } else {
            retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)?
        };
        let tabs = if created_candidates.is_empty() {
            h.tabs()?
        } else {
            retry_transient_dock_observation(|| h.tabs(), dock_observation_pause)?
        };
        let panes = if created_candidates.is_empty() {
            h.list_panes()?
        } else {
            retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)?
        };
        let dock_workspaces: HashSet<&str> = workspaces
            .iter()
            .filter(|workspace| workspace.label == dockcmd::LABEL)
            .map(|workspace| workspace.workspace_id.as_str())
            .collect();
        // The server owns this observation, so every client sees the same
        // answer even when its own /tmp lives in another mount namespace.
        let dock_panes: Vec<_> = panes
            .iter()
            .filter(|pane| dock_workspaces.contains(pane.workspace_id.as_str()))
            .collect();
        let mut server_running: HashSet<String> = HashSet::new();
        let mut server_idle_shells: HashSet<String> = HashSet::new();
        for pane in dock_panes {
            // 🚨 Only a pane in a workspace this invocation created waits. That
            // pane is seconds old and its shell may still be running startup
            // files, which would read as "in use" and send the plan off to
            // build a second tab beside the one just made. Every other dock
            // pane is asked once: the usual answer there is a running tower,
            // which is never an idle shell, and waiting would spend the budget
            // on every call. The one other pane worth waiting on — the dock's
            // own shell after a herdr restart — waits after planning, and only
            // when the plan would otherwise build a new tab.
            let observed = if created_candidates.contains(&pane.workspace_id) {
                observe_settled_shell(
                    || h.process_info(&pane.pane_id),
                    SHELL_SETTLE,
                    dock_observation_pause,
                )
            } else {
                h.process_info(&pane.pane_id)
            };
            match observed {
                Ok(info) => {
                    if dockcmd::is_tower_process(&info, Path::new(&executable)) {
                        server_running.insert(pane.terminal_id.clone());
                    }
                    if process_info_has_idle_foreground_shell(&info) {
                        server_idle_shells.insert(pane.terminal_id.clone());
                    }
                }
                Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {
                    // Enumeration and inspection are separate RPCs. Refresh
                    // both halves when a pane disappears between them.
                    continue 'snapshot;
                }
                Err(error) => return Err(error.into()),
            }
        }
        let server_starting: HashSet<&str> = panes
            .iter()
            .filter(|pane| {
                pane.tokens
                    .get(dockcmd::STARTING_TOKEN)
                    .is_some_and(|value| {
                        dockcmd::pane_starting_lease_is_held_for(value, &pane.terminal_id)
                    })
            })
            .map(|pane| pane.terminal_id.as_str())
            .collect();
        let mut server_unsubmitted: HashSet<String> = panes
            .iter()
            .filter(|pane| dockcmd::was_never_submitted(pane))
            .map(|pane| pane.terminal_id.clone())
            .collect();
        server_unsubmitted.extend(recoverable_request_tab_terminals(
            &workspaces,
            &tabs,
            &panes,
        ));

        // The dock's own shells back after a herdr restart. They have new
        // terminal IDs — herdr keeps no terminal IDs across a restart — so
        // neither the claim file nor the never-submitted token vouches for
        // them, and without this the plan built another tab beside them every
        // time.
        //
        // One of them, and only one, is offered to the plan
        // (`restored_to_offer`). One, because `plan` picks among reusable panes
        // by tab ID, and the order that matters is the one
        // `restored_dock_shells` gives — the tab a click on the dock shows
        // first.
        let restored = dockcmd::restored_to_offer(
            &dockcmd::restored_dock_shells(&workspaces, &tabs, &panes),
            |t| server_idle_shells.contains(t),
            restored_settled,
        )
        .map(|pane| (pane.pane_id.clone(), pane.terminal_id.clone()));
        let is_restored = |t: &str| restored.as_ref().is_some_and(|(_, terminal)| terminal == t);
        let plan = dockcmd::plan(
            &workspaces,
            &panes,
            |t| {
                dockcmd::tower_is_evident(server_running.contains(t), dockcmd::is_held_in(&dirs, t))
            },
            |t| server_starting.contains(t),
            // Only the explicit never-submitted state is safe by itself.
            // A submitted trace still requires this filesystem namespace to
            // have observed the tower's lifetime lock.
            //
            // 🚨 The restored shell belongs here as well as in `shell_ready`.
            // Only there, it was reused solely through `plan`'s lone-pane
            // fallback, so a dock with two restored tabs built a third (#41).
            |t| {
                server_unsubmitted.contains(t)
                    || dockcmd::was_claimed_in(&dirs, t)
                    || is_restored(t)
            },
            |t| {
                let owned_shell = panes.iter().any(|pane| {
                    pane.terminal_id == t && created_candidates.contains(&pane.workspace_id)
                });
                server_idle_shells.contains(t)
                    // Same rule as the running predicate above, and for the
                    // same reason. A dead tower's leftover token must not veto
                    // reuse of a pane the server reports as an idle shell: when
                    // it did, the pane was abandoned and a second, empty tab
                    // appeared beside it.
                    && !dockcmd::tower_is_evident(
                        server_running.contains(t),
                        dockcmd::is_held_in(&dirs, t),
                    )
                    && (owned_shell
                        || server_unsubmitted.contains(t)
                        || dockcmd::was_claimed_in(&dirs, t)
                        || is_restored(t))
            },
        );
        // A restored shell that is not idle yet is still in its startup files:
        // herdr starts it the moment it restores the workspace, and the startup
        // plugin asks within milliseconds. So when the plan would build a tab
        // beside the dock's restored shell, wait for that shell once, bounded,
        // and look again. Still busy after that — a slow `.zshrc`, or a program
        // left running there — and the tab is built as it always was: a tab too
        // many is recoverable, a dock that never comes back is not. That tab
        // holds a restored shell itself after the next restart, so the tabs
        // pile up only while every restored shell keeps taking longer than
        // the wait. An idle one is not waited on: the plan passed it over for
        // another reason, and looking again changes nothing.
        if let (Plan::NewTab { .. }, Some((pane_id, terminal_id))) = (&plan, &restored) {
            if !restored_settled && !server_idle_shells.contains(terminal_id) {
                restored_settled = true;
                settle_restored_shell(
                    || h.process_info(pane_id),
                    Path::new(&executable),
                    SHELL_SETTLE,
                    dock_observation_pause,
                );
                continue 'snapshot;
            }
        }
        if matches!(plan, Plan::Create) {
            // There is no server-lifetime metadata object on which to gate the
            // first workspace creation. Each request therefore creates under
            // a unique temporary label and reconciles only that label. The
            // refreshed plan elects one private candidate as the shared gate;
            // no caller publishes directly from this stale `Create` snapshot.
            // From the first possibly-delivered create until this invocation
            // relinquishes ownership, a kernel deadline bounds every retry
            // before the server-side creation lease can expire.
            if created_candidate_fence.is_none() {
                created_candidate_fence = Some(dockcmd::CoordinationFence::arm()?);
            }
            match h.new_workspace(&workspace_request_label, &home().unwrap_or_default(), false) {
                Ok((workspace, _, _)) => {
                    let workspace_id = workspace.workspace_id;
                    created_candidates.insert(workspace_id.clone());
                    establish_workspace_creation_claim(
                        &h,
                        &workspace_id,
                        &workspace_request_label,
                    )?;
                    continue 'snapshot;
                }
                Err(error) if dockcmd::delivery_is_uncertain(&error) => {
                    // The unique temporary label is part of the create itself,
                    // so it identifies this request's workspace even when the
                    // reply carrying its ID is lost.
                    let after = match retry_transient_dock_observation_bounded(
                        || h.workspaces(),
                        dock_observation_pause,
                    ) {
                        Ok(workspaces) => workspaces,
                        Err(_) => return Err(error.into()),
                    };
                    let Some(workspace) =
                        dockcmd::workspace_for_request(&after, &workspace_request_label)
                    else {
                        return Err(error.into());
                    };
                    let workspace_id = workspace.workspace_id.clone();
                    created_candidates.insert(workspace_id.clone());
                    establish_workspace_creation_claim(
                        &h,
                        &workspace_id,
                        &workspace_request_label,
                    )?;
                    continue 'snapshot;
                }
                Err(error) => return Err(error.into()),
            }
        }
        {
            let canonical = dockcmd::dock_workspace(&workspaces)
                .map(|workspace| workspace.workspace_id.as_str());
            created_candidates.retain(|candidate| {
                workspaces
                    .iter()
                    .any(|workspace| workspace.workspace_id == candidate.as_str())
            });
            let mut losers: Vec<(String, String)> = workspaces
                .iter()
                .filter(|workspace| {
                    workspace.label != dockcmd::LABEL
                        && (dockcmd::is_workspace_creation_candidate(workspace)
                            || dockcmd::is_unclaimed_workspace_creation_candidate(workspace))
                        && canonical != Some(workspace.workspace_id.as_str())
                })
                .map(|workspace| (workspace.workspace_id.clone(), workspace.label.clone()))
                .collect();
            // A candidate created here may already have been published by a
            // concurrent winner. Keep the public workspace, but retire this
            // invocation's create ownership from the in-memory cleanup set.
            for candidate in &created_candidates {
                if canonical != Some(candidate.as_str())
                    && !losers
                        .iter()
                        .any(|(workspace_id, _)| workspace_id == candidate)
                {
                    losers.push((candidate.clone(), workspace_request_label.clone()));
                }
            }
            for (candidate, candidate_label) in &losers {
                match reconcile_created_workspace_cleanup(
                    &h,
                    candidate,
                    candidate_label,
                    created_candidates.contains(candidate),
                    Path::new(&executable),
                )? {
                    CreatedWorkspaceCleanup::Busy => {
                        if created_candidates.contains(candidate) {
                            // This process still owns the only proof that it
                            // may close this candidate. A competing namespace
                            // can hold its gate briefly, but cannot safely do
                            // that cleanup on our behalf.
                            dock_observation_pause();
                            continue 'snapshot;
                        }
                        return dock_pending(
                            require_active,
                            "another dock invocation is coordinating a tower launch",
                        );
                    }
                    CreatedWorkspaceCleanup::Closed | CreatedWorkspaceCleanup::Adopted => {}
                }
                created_candidates.remove(candidate);
            }
            if created_candidates.is_empty() {
                created_candidate_fence.take();
            }
            if !losers.is_empty() {
                continue 'snapshot;
            }
        }
        if let Plan::Publish {
            workspace_id,
            request_label,
        }
        | Plan::Claim {
            workspace_id,
            request_label,
        } = &plan
        {
            let needs_claim = matches!(plan, Plan::Claim { .. });
            match publish_canonical_workspace(
                &h,
                workspace_id,
                request_label,
                !_one_at_a_time.is_empty(),
                created_candidates.contains(workspace_id),
                needs_claim,
                Path::new(&executable),
            )? {
                WorkspacePublication::Changed => continue 'snapshot,
                WorkspacePublication::Busy => {
                    return dock_pending(
                        require_active,
                        "another dock invocation is coordinating a tower launch",
                    );
                }
            }
        }
        break (workspaces, tabs, panes, plan);
    };
    match &plan {
        Plan::Running {
            pane_id,
            workspace_id,
        } => {
            pin_workspace(&h, workspace_id)?;
            hold_dock_pane(&h, pane_id);
            tower_cli::outln!(
                "the tower is already running in {pane_id} (workspace {})",
                dockcmd::LABEL
            )?;
            if focus {
                h.focus(pane_id)?;
            }
            return Ok(());
        }
        Plan::Starting {
            pane_id,
            workspace_id,
        } => {
            pin_workspace(&h, workspace_id)?;
            let message = format!(
                "the tower is still starting in {pane_id} (workspace {})",
                dockcmd::LABEL
            );
            if focus {
                h.focus(pane_id)?;
            }
            return dock_pending(require_active, message);
        }
        Plan::Create | Plan::NewTab { .. } | Plan::Launch { .. } => {}
        Plan::Publish { .. } | Plan::Claim { .. } => {
            unreachable!("workspace publication restarts planning")
        }
    }

    // Gate on the workspace every following mutation actually targets. If it
    // closes after an ownership check, its panes disappear too: tab creation,
    // metadata writes, and command submission all fail against the old IDs,
    // while a replacement dock can proceed without overlapping this run.
    let anchor = dockcmd::dock_workspace(&workspaces)
        .ok_or("herdr reports no dock workspace on which to coordinate tower launch")?;
    let anchor_workspace_id = anchor.workspace_id.clone();
    let coordinating_value = dockcmd::starting_lease_value();
    // Herdr 0.9 has no conditional workspace/tab mutation. The kernel fence
    // therefore terminates a stopped coordinator before another client may
    // let Herdr's TTL remove this token; no client wall-clock comparison can
    // safely fence the following RPC.
    let coordination_fence = Some(dockcmd::CoordinationFence::arm()?);
    let (coordination_generation, expected_coordination) =
        match acquire_coordination_gate(&h, &anchor_workspace_id, &coordinating_value)? {
            CoordinationGateClaim::Acquired {
                generation,
                expected,
            } => (generation, expected),
            CoordinationGateClaim::Busy => {
                return dock_pending(
                    require_active,
                    "another dock invocation is coordinating a tower launch",
                );
            }
        };

    let (mut pane_id, terminal_id, recovered_tab_request) = match plan {
        Plan::Create => unreachable!("workspace creation is resolved before coordination"),
        Plan::Publish { .. } | Plan::Claim { .. } => {
            unreachable!("workspace publication restarts planning")
        }
        Plan::NewTab { workspace_id } => {
            // Created focused, because that is what makes it the tab the
            // workspace shows when clicked — herdr has no way to set a
            // workspace's active tab without focusing it. The view then goes
            // back where it was unless `--focus` asked to stay.
            let was = match h.focused_workspace() {
                Ok(workspace) => workspace,
                Err(error) => {
                    clear_dock_coordination(
                        &h,
                        &anchor_workspace_id,
                        coordination_generation,
                        true,
                    );
                    return Err(error.into());
                }
            };
            let restore_focus = || {
                if let Some(workspace) = was.as_ref().filter(|_| !focus) {
                    h.focus_workspace(&workspace.workspace_id)?;
                }
                Ok::<(), herdrx::Error>(())
            };
            let opts = herdrx::TabOptions {
                workspace_id: Some(workspace_id.clone()),
                label: Some(tab_request_label.clone()),
                cwd: home(),
                focus: true,
                ..Default::default()
            };
            ensure_tab_creation_intent(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                &expected_coordination,
                &tab_request_label,
            )?;
            let created = coordinated_dock_call(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                &expected_coordination,
                || h.new_tab(&opts),
            );
            let pane = match created {
                Ok(created) => created.1,
                Err(error) if error.delivery_is_uncertain() => {
                    let reconciled = match reconcile_created_tab_after_uncertain(
                        &h,
                        &panes,
                        &workspace_id,
                        &tab_request_label,
                    ) {
                        Ok(pane) => pane,
                        Err(observation_error) => {
                            let _ = restore_focus();
                            return Err(observation_error.into());
                        }
                    };
                    let Some(pane) = reconciled else {
                        let _ = restore_focus();
                        return Err(error.into());
                    };
                    pane
                }
                Err(error) => {
                    let _ = restore_focus();
                    return Err(error.into());
                }
            };
            if let Err(error) = ensure_tab_creation_lease(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                &pane.pane_id,
                &tab_request_label,
            ) {
                let _ = restore_focus();
                return Err(error.into());
            }
            // From this point the shell is ours. Put down the local trace
            // before another RPC can fail, then restore the caller's view
            // before either request-label or server-trace reconciliation can
            // spend the hard-fenced interval waiting on a server outage.
            for dir in &dirs {
                let _ = dockcmd::remember_reusable(dir, &pane.terminal_id);
            }
            let focus_result = restore_focus();
            if let Err(error) = ensure_reusable_metadata(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                true,
                &pane.pane_id,
                &pane.terminal_id,
            ) {
                return Err(error.into());
            }
            finish_tab_request(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                &expected_coordination,
                &pane.tab_id,
                &tab_request_label,
            )?;
            if let Err(error) = focus_result {
                clear_dock_coordination(&h, &anchor_workspace_id, coordination_generation, true);
                return Err(error.into());
            }
            (pane.pane_id, pane.terminal_id, None)
        }
        Plan::Launch {
            pane_id,
            terminal_id,
        } => {
            let recovered_tab_request = panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .and_then(|pane| tabs.iter().find(|tab| tab.tab_id == pane.tab_id))
                .filter(|tab| dockcmd::tab_request_id(&tab.label).is_some())
                .map(|tab| (tab.tab_id.clone(), tab.label.clone()));
            (pane_id, terminal_id, recovered_tab_request)
        }
        Plan::Running { .. } | Plan::Starting { .. } => unreachable!(),
    };
    // Create and existing-shell paths reach this common trace point. NewTab
    // already wrote it before restoring the caller's focus; repeating it is
    // idempotent and keeps all paths identical from here on.
    for dir in &dirs {
        let _ = dockcmd::remember_reusable(dir, &terminal_id);
    }
    ensure_reusable_metadata(
        &h,
        &anchor_workspace_id,
        coordination_generation,
        true,
        &pane_id,
        &terminal_id,
    )?;
    if let Some((tab_id, request_label)) = recovered_tab_request {
        finish_tab_request(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            &expected_coordination,
            &tab_id,
            &request_label,
        )?;
    }
    // Derive ordering from the exact value retained by Herdr, never from this
    // client's wall clock. Keeping the workspace gate while reading prevents a
    // second dock invocation from choosing the same idle generation.
    let prelease_pane =
        match retry_transient_dock_observation(|| h.pane(&pane_id), dock_observation_pause) {
            Ok(pane) => pane,
            Err(error) => {
                // This was only an observation, so no target mutation can be
                // in flight. Release the workspace gate synchronously instead
                // of making a retry wait for its server TTL when the planned
                // pane moved or disappeared.
                clear_dock_coordination_confirmed(
                    &h,
                    &anchor_workspace_id,
                    coordination_generation,
                    &expected_coordination,
                )?;
                return Err(error.into());
            }
        };
    if !pane_retains_launch_target(&prelease_pane, &terminal_id)
        || dockcmd::active_token_blocks_launch(
            prelease_pane
                .tokens
                .get(dockcmd::ACTIVE_TOKEN)
                .map(String::as_str),
            // 🚨 Lazily. Rust evaluates arguments first, so passing the
            // observation by value made every launch pay a `process_info` round
            // trip — and its whole retry budget on a flaky socket — for a value
            // the common path (no token at all) never reads.
            || {
                observed_tower_on(
                    || h.process_info(&pane_id),
                    &executable,
                    dock_observation_pause,
                )
            },
        )
    {
        clear_dock_coordination_confirmed(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            &expected_coordination,
        )?;
        return Err(
            format!("dock pane {pane_id} changed identity or became active before launch").into(),
        );
    }
    let Some(lease_generation) = dockcmd::next_pane_starting_generation(
        prelease_pane
            .tokens
            .get(dockcmd::STARTING_GENERATION_TOKEN)
            .map(String::as_str),
        prelease_pane
            .tokens
            .get(dockcmd::STARTING_TOKEN)
            .map(String::as_str),
        &terminal_id,
    ) else {
        clear_dock_coordination_confirmed(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            &expected_coordination,
        )?;
        return dock_pending(
            require_active,
            format!(
                "another dock invocation is starting the tower in {pane_id} (workspace {})",
                dockcmd::LABEL
            ),
        );
    };
    let lease_owner = dockcmd::starting_lease_value();
    // Re-resolve the stable terminal immediately before each pane-addressed
    // write. The value itself is terminal-bound too, so a move in the final
    // call window is rejected during readback rather than authorizing a lease
    // on an unrelated pane.
    let generation_target =
        match retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause) {
            Ok(panes) => panes,
            Err(error) => {
                clear_dock_coordination_confirmed(
                    &h,
                    &anchor_workspace_id,
                    coordination_generation,
                    &expected_coordination,
                )?;
                return Err(error.into());
            }
        };
    let Some(generation_target) = pane_for_terminal(&generation_target, &terminal_id) else {
        clear_dock_coordination_confirmed(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            &expected_coordination,
        )?;
        return Err(format!("dock terminal {terminal_id} disappeared before launch").into());
    };
    if !pane_retains_launch_target(generation_target, &terminal_id) {
        clear_dock_coordination_confirmed(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            &expected_coordination,
        )?;
        return Err(format!("dock terminal {terminal_id} changed before launch").into());
    }
    pane_id = generation_target.pane_id.clone();
    let generation_value =
        dockcmd::pane_starting_generation_value(lease_generation, &lease_owner, &terminal_id);
    let generation_write = coordinated_dock_call(
        &h,
        &anchor_workspace_id,
        coordination_generation,
        &expected_coordination,
        || {
            h.report_metadata(&dockcmd::pane_starting_generation_claim(
                &pane_id,
                &generation_value,
                lease_generation,
            ))
        },
    );
    let uncertain_generation_error = match generation_write {
        Ok(()) => None,
        Err(error) if error.delivery_is_uncertain() => Some(error),
        Err(error) => {
            clear_dock_coordination_confirmed(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                &expected_coordination,
            )?;
            return Err(error.into());
        }
    };
    // Same-sequence writes are acknowledged even when another contender won.
    // Exact readback elects the owner of this persistent generation.
    let generation_pane =
        match retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause) {
            Ok(panes) => pane_for_terminal(&panes, &terminal_id).cloned(),
            Err(error) => {
                // The generation claim cannot submit a command. If its target
                // disappears during readback, release the workspace gate so a
                // fresh snapshot can immediately choose the moved terminal.
                clear_dock_coordination_confirmed(
                    &h,
                    &anchor_workspace_id,
                    coordination_generation,
                    &expected_coordination,
                )?;
                return Err(error.into());
            }
        };
    let generation_is_ours = generation_pane.as_ref().is_some_and(|pane| {
        pane_retains_launch_target(pane, &terminal_id)
            && pane.tokens.get(dockcmd::STARTING_GENERATION_TOKEN) == Some(&generation_value)
    });
    if !generation_is_ours {
        clear_dock_coordination_confirmed(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            &expected_coordination,
        )?;
        if let Some(error) = uncertain_generation_error {
            return Err(error.into());
        }
        return dock_pending(
            require_active,
            "another dock invocation advanced the tower startup generation",
        );
    }
    let generation_pane = generation_pane.expect("generation ownership was just confirmed");
    pane_id = generation_pane.pane_id;
    let lease_value =
        dockcmd::pane_starting_lease_value(lease_generation, &lease_owner, &terminal_id);
    let lease_write = coordinated_dock_call(
        &h,
        &anchor_workspace_id,
        coordination_generation,
        &expected_coordination,
        || {
            h.report_metadata(&dockcmd::starting_lease(
                &pane_id,
                &lease_value,
                lease_generation,
            ))
        },
    );
    if let Err(error) = lease_write {
        if error.delivery_is_uncertain() {
            // An I/O/decode failure may have followed an applied write. Read
            // the exact token back while the hard fence and workspace gate are
            // still held, and remove our unsubmitted lease before returning.
            let observed =
                retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)?;
            let ours = pane_for_terminal(&observed, &terminal_id)
                .and_then(|pane| pane.tokens.get(dockcmd::STARTING_TOKEN))
                == Some(&lease_value);
            if ours {
                clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value)?;
            }
            clear_dock_coordination_confirmed(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                &expected_coordination,
            )?;
        } else {
            clear_dock_coordination_confirmed(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                &expected_coordination,
            )?;
        }
        return Err(error.into());
    }
    // Read back the bounded target lease before submitting anything; the
    // server, rather than a client-private filesystem, owns this state.
    let leased_panes = match h.list_panes() {
        Ok(panes) => panes,
        Err(error) => {
            // The lease write above was acknowledged, while no launch has yet
            // been submitted. Remove that now-known-stale target lease so an
            // immediate retry can reuse the idle shell.
            let clear_result =
                clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value);
            clear_dock_coordination(&h, &anchor_workspace_id, coordination_generation, true);
            clear_result?;
            return Err(error.into());
        }
    };
    let Some(leased_pane) = leased_panes
        .into_iter()
        .find(|pane| pane.terminal_id == terminal_id)
    else {
        let clear_result =
            clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value);
        clear_dock_coordination(&h, &anchor_workspace_id, coordination_generation, true);
        clear_result?;
        return Err(format!("dock pane {pane_id} disappeared before launch").into());
    };
    pane_id = leased_pane.pane_id.clone();
    if leased_pane.tokens.get(dockcmd::STARTING_TOKEN) != Some(&lease_value) {
        clear_dock_coordination(&h, &anchor_workspace_id, coordination_generation, true);
        pin_dock(&h)?;
        let message = format!(
            "another dock invocation is starting the tower in {pane_id} (workspace {})",
            dockcmd::LABEL
        );
        if focus {
            h.focus(&pane_id)?;
        }
        return dock_pending(require_active, message);
    }
    if !pane_retains_launch_target(&leased_pane, &terminal_id)
        || dockcmd::active_token_blocks_launch(
            leased_pane
                .tokens
                .get(dockcmd::ACTIVE_TOKEN)
                .map(String::as_str),
            || {
                observed_tower_on(
                    || h.process_info(&pane_id),
                    &executable,
                    dock_observation_pause,
                )
            },
        )
    {
        let clear_result =
            clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value);
        clear_dock_coordination(&h, &anchor_workspace_id, coordination_generation, true);
        clear_result?;
        return Err(
            format!("dock pane {pane_id} changed identity or became active before launch").into(),
        );
    }
    // 🚨 Settled, not merely observed. This pane may be seconds old — the
    // workspace was created by this very invocation — and a shell still running
    // its startup files would read as taken, aborting the launch and leaving
    // that new workspace standing empty.
    let shell_still_idle = match observe_settled_shell(
        || h.process_info(&pane_id),
        SHELL_SETTLE,
        dock_observation_pause,
    ) {
        Ok(info) => {
            process_info_has_idle_foreground_shell(&info)
                && !dockcmd::is_held_in(&dirs, &terminal_id)
        }
        Err(error) => {
            let clear_result =
                clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value);
            clear_dock_coordination(&h, &anchor_workspace_id, coordination_generation, true);
            clear_result?;
            return Err(error.into());
        }
    };
    if !shell_still_idle {
        let clear_result =
            clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value);
        clear_dock_coordination(&h, &anchor_workspace_id, coordination_generation, true);
        clear_result?;
        return Err(format!("dock pane {pane_id} is no longer an idle shell").into());
    }
    // This durable state distinguishes a shell that no command has reached
    // from one whose tower might merely be stopped or hidden. Keep it
    // unsubmitted throughout every earlier failure path, and transition it
    // while the target lease still prevents another client from reusing it.
    if let Err(error) = ensure_reusable_submitted(
        &h,
        &anchor_workspace_id,
        coordination_generation,
        &pane_id,
        &terminal_id,
    ) {
        // The submitted write may have been applied even when neither its
        // reply nor bounded readback arrived. Restore and reconcile the exact
        // pre-submit state before releasing the target lease; otherwise this
        // known-idle shell becomes permanently ineligible for retry.
        let restore_result = ensure_reusable_metadata(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            true,
            &pane_id,
            &terminal_id,
        );
        let clear_result =
            clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value);
        let coordination_result = clear_dock_coordination_confirmed(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            &expected_coordination,
        );
        restore_result?;
        clear_result?;
        coordination_result?;
        return Err(error.into());
    }
    // The target-specific lease is now visible, so the global mutation gate can
    // admit later callers. Confirm that release before submission: otherwise a
    // definitively rejected command could leave both leases blocking its retry.
    // Keep the hard fence armed through command submission; if this process is
    // stopped here, it must die before the target lease can expire.
    if let Err(error) = clear_dock_coordination_confirmed(
        &h,
        &anchor_workspace_id,
        coordination_generation,
        &expected_coordination,
    ) {
        ensure_reusable_metadata(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            false,
            &pane_id,
            &terminal_id,
        )?;
        clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value)?;
        return Err(error.into());
    }
    // A foreground shell can still have an unsubmitted edit buffer. Cancel the
    // complete prompt (including multiline input) immediately before sending
    // the generated command, so pane.send_input cannot append it to user text.
    // An uncertain key delivery is harmless but not proof of a clear prompt;
    // abort without retrying it and restore the known no-command state.
    // Herdr pane IDs are stable and never reused. If this terminal moves after
    // the identity check, an external caller's old ID becomes not-found rather
    // than resolving to the replacement pane, so neither call can be retargeted
    // to another user's shell.
    // A new PowerShell tab is still starting when it is created, and a Ctrl+C
    // that reaches it before its prompt ends the shell — the tab closes and
    // the tower never arrives. Its prompt on screen is the sign it reads.
    #[cfg(windows)]
    wait_for_prompt(&h, &pane_id);
    if let Err(error) = h.send_keys(&pane_id, &["ctrl+c"]) {
        let restore_result = ensure_reusable_metadata(
            &h,
            &anchor_workspace_id,
            coordination_generation,
            false,
            &pane_id,
            &terminal_id,
        );
        let clear_result =
            clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value);
        drop(coordination_fence);
        restore_result?;
        clear_result?;
        return Err(error.into());
    }
    let submitted = h.run(
        &pane_id,
        &dockcmd::launch_command(
            &executable,
            &state_dir,
            config.as_deref(),
            &lease_value,
            &terminal_id,
        ),
    );
    if let Err(error) = submitted {
        // API/validation failures prove the command was rejected. Socket I/O
        // and decoding failures may have happened after acceptance, so those
        // retain only the bounded lease above.
        if !dockcmd::delivery_is_uncertain(&error) {
            ensure_reusable_metadata(
                &h,
                &anchor_workspace_id,
                coordination_generation,
                false,
                &pane_id,
                &terminal_id,
            )?;
            clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value)?;
        }
        drop(coordination_fence);
        return Err(error.into());
    }
    drop(coordination_fence);

    // Report success only once the tower has published its active token. The
    // local flock and process observer become visible before fallible active
    // initialization, so neither proves that the TUI can actually start. The
    // exact launch value also excludes a stale marker, and terminal lookup
    // follows a pane moved while initialization was in flight.
    let active_value = dockcmd::active_claim_value(&terminal_id, &lease_value);
    if let Some(ready_pane) = dockcmd::wait_for_ready(std::time::Duration::from_secs(5), || {
        let panes = h.list_panes().ok()?;
        ready_pane_for_launch(&panes, &terminal_id, &active_value).cloned()
    }) {
        clear_starting_lease_confirmed(&h, &terminal_id, lease_generation, &lease_value)?;
        pin_dock(&h)?;
        hold_dock_pane(&h, &ready_pane.pane_id);
        if focus {
            h.focus(&ready_pane.pane_id)?;
        }
        tower_cli::outln!(
            "the tower is running in {} — click workspace {} in the sidebar",
            ready_pane.pane_id,
            dockcmd::LABEL,
        )?;
        return Ok(());
    }
    Err(
        format!("sent the tower to {pane_id}, but it did not start within 5s — look at that pane")
            .into(),
    )
}

/// A coordination handoff is success for an interactive one-shot `dock`, but
/// only an active tower may stop the startup plugin's retry loop.
fn dock_pending(require_active: bool, message: impl Into<String>) -> Result<()> {
    let message = message.into();
    if require_active {
        return Err(message.into());
    }
    tower_cli::outln!("{message}")?;
    Ok(())
}

/// Installs and links the startup hook from bytes embedded in this binary.
fn install_dock_startup() -> Result<()> {
    let home = home()?;
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME");
    let dir = tower_cli::dockplugin::install_dir(&home, xdg_data_home.as_deref());
    let executable = invoked_self_path()?;
    tower_cli::dockplugin::install(&mut std::io::stdout().lock(), &dir, &executable)?;

    let local_herdr = home.join(".local/bin/herdr");
    let herdr = std::env::var_os("HERDR_BIN_PATH")
        .filter(|path| !path.is_empty())
        .or_else(|| local_herdr.is_file().then(|| local_herdr.into_os_string()))
        .unwrap_or_else(|| "herdr".into());
    let status = Command::new(&herdr)
        .args(["plugin", "link"])
        .arg(&dir)
        .status()
        .map_err(|error| format!("could not run {}: {error}", Path::new(&herdr).display()))?;
    if !status.success() {
        return Err(format!("herdr plugin link failed with {status}").into());
    }
    tower_cli::outln!("dock startup plugin linked: {}", dir.display())?;
    Ok(())
}

/// The stable spelling used to invoke this binary, preserving Homebrew's link.
fn invoked_self_path() -> Result<PathBuf> {
    Ok(tower_cli::selfpath::current()?)
}

/// Observes the result of a possibly delivered `tab.create`.
///
/// A failed observation says nothing about whether the preceding mutation was
/// applied. Keep the dock gate (and its hard lifetime fence) while retrying
/// transient socket/decoding failures; returning on one would lose the only
/// chance to put a durable reusable trace on the shell that may now exist.
fn reconcile_created_tab_after_uncertain(
    h: &herdrx::Herdr,
    before: &[herdrx::Pane],
    workspace_id: &str,
    request_label: &str,
) -> herdrx::Result<Option<herdrx::Pane>> {
    observe_created_tab_until_stable(
        before,
        workspace_id,
        || h.tabs(),
        || h.list_panes(),
        dock_observation_pause,
        request_label,
    )
}

fn observe_created_tab_until_stable(
    before: &[herdrx::Pane],
    workspace_id: &str,
    mut read_tabs: impl FnMut() -> herdrx::Result<Vec<herdrx::Tab>>,
    mut read_panes: impl FnMut() -> herdrx::Result<Vec<herdrx::Pane>>,
    mut pause: impl FnMut(),
    request_label: &str,
) -> herdrx::Result<Option<herdrx::Pane>> {
    let after_tabs = retry_transient_dock_observation(&mut read_tabs, &mut pause)?;
    let after_panes = retry_transient_dock_observation(&mut read_panes, &mut pause)?;
    Ok(tower_cli::dockcmd::reconcile_created_tab(
        before,
        &after_tabs,
        &after_panes,
        workspace_id,
        request_label,
    ))
}

/// Is a tower running in this pane right now, as far as the server can say?
///
/// `None` means the question could not be put — an observation that kept
/// failing. An unanswered question is not a "no", and callers must not round it
/// down to one.
fn observed_tower_on<T>(
    read: impl FnMut() -> herdrx::Result<T>,
    executable: &str,
    pause: impl FnMut(),
) -> Option<bool>
where
    T: std::borrow::Borrow<herdrx::ProcessInfo>,
{
    match retry_transient_dock_observation_bounded(read, pause) {
        Ok(info) => tower_cli::dockcmd::tower_observed(info.borrow(), Path::new(executable)),
        Err(_) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceCreationClaim {
    Stored,
}

fn ensure_workspace_creation_lease(
    h: &herdrx::Herdr,
    workspace_id: &str,
    request_label: &str,
    fenced: bool,
) -> std::result::Result<WorkspaceCreationClaim, CoordinatedDockError> {
    let lease = tower_cli::dockcmd::workspace_creation_lease(workspace_id, request_label);
    for attempt in 0..2 {
        match h.report_workspace_metadata(&lease) {
            Ok(()) => return Ok(WorkspaceCreationClaim::Stored),
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let observed = if fenced {
                    retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)
                } else {
                    retry_transient_dock_observation_bounded(
                        || h.workspaces(),
                        dock_observation_pause,
                    )
                };
                let workspaces = match observed {
                    Ok(workspaces) => workspaces,
                    Err(_) => return Err(CoordinatedDockError::Mutation(error)),
                };
                let Some(workspace) = workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                else {
                    return Err(CoordinatedDockError::Mutation(error));
                };
                if workspace.label == request_label
                    && tower_cli::dockcmd::workspace_creator_is_live(workspace, request_label)
                {
                    return Ok(WorkspaceCreationClaim::Stored);
                }
                if workspace.label == request_label && attempt == 0 {
                    continue;
                }
                return Err(CoordinatedDockError::Mutation(error));
            }
            Err(error) => return Err(CoordinatedDockError::Mutation(error)),
        }
    }
    unreachable!("two workspace-creation lease writes either return or continue once")
}

/// Makes a request-labeled workspace eligible for cross-invocation adoption.
fn ensure_workspace_creation_claim(
    h: &herdrx::Herdr,
    workspace_id: &str,
    request_label: &str,
    fenced: bool,
) -> std::result::Result<WorkspaceCreationClaim, CoordinatedDockError> {
    let claim = tower_cli::dockcmd::workspace_creation_claim(workspace_id, request_label);
    for attempt in 0..2 {
        match h.report_workspace_metadata(&claim) {
            Ok(()) => return Ok(WorkspaceCreationClaim::Stored),
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let observed = if fenced {
                    retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)
                } else {
                    retry_transient_dock_observation_bounded(
                        || h.workspaces(),
                        dock_observation_pause,
                    )
                };
                let workspaces = match observed {
                    Ok(workspaces) => workspaces,
                    Err(_) => return Err(CoordinatedDockError::Mutation(error)),
                };
                let Some(workspace) = workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                else {
                    return Err(CoordinatedDockError::Mutation(error));
                };
                if tower_cli::dockcmd::is_workspace_creation_candidate(workspace)
                    && workspace.label == request_label
                {
                    return Ok(WorkspaceCreationClaim::Stored);
                }
                if workspace.label == request_label && attempt == 0 {
                    continue;
                }
                return Err(CoordinatedDockError::Mutation(error));
            }
            Err(error) => return Err(CoordinatedDockError::Mutation(error)),
        }
    }
    unreachable!("two creation-claim writes either return or continue once")
}

/// Records durable ownership.
fn establish_workspace_creation_claim(
    h: &herdrx::Herdr,
    workspace_id: &str,
    request_label: &str,
) -> Result<()> {
    match ensure_workspace_creation_lease(h, workspace_id, request_label, false) {
        Ok(WorkspaceCreationClaim::Stored) => {}
        Err(error) => {
            if !error.delivery_is_uncertain() {
                let _ = close_unpublished_workspace_before_fence(h, workspace_id);
            }
            return Err(error.into());
        }
    }
    match ensure_workspace_creation_claim(h, workspace_id, request_label, false) {
        Ok(WorkspaceCreationClaim::Stored) => Ok(()),
        Err(error) => {
            if !error.delivery_is_uncertain() {
                let _ = close_unpublished_workspace_before_fence(h, workspace_id);
            }
            Err(error.into())
        }
    }
}

/// Retires a request-shaped candidate that is no longer safe to adopt.
fn retire_workspace_creation_claim_confirmed(
    h: &herdrx::Herdr,
    workspace_id: &str,
    request_label: &str,
    fenced: bool,
) -> herdrx::Result<()> {
    let retired = herdrx::WorkspaceMetadata {
        workspace_id: workspace_id.to_string(),
        source: tower_cli::dockcmd::CREATED_SOURCE.to_string(),
        tokens: [(
            tower_cli::dockcmd::CREATED_TOKEN.to_string(),
            tower_cli::dockcmd::CREATED_RETIRED.to_string(),
        )]
        .into_iter()
        .collect(),
        ..herdrx::WorkspaceMetadata::default()
    };
    for attempt in 0..2 {
        match h.report_workspace_metadata(&retired) {
            Ok(()) => return Ok(()),
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let observed = if fenced {
                    retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)
                } else {
                    retry_transient_dock_observation_bounded(
                        || h.workspaces(),
                        dock_observation_pause,
                    )
                }?;
                let Some(workspace) = observed
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                else {
                    return Ok(());
                };
                let actual = workspace
                    .tokens
                    .get(tower_cli::dockcmd::CREATED_TOKEN)
                    .map(String::as_str);
                if actual == Some(tower_cli::dockcmd::CREATED_RETIRED) {
                    return Ok(());
                }
                if (actual.is_none() || actual == Some(request_label)) && attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two creation-claim retirements either return or continue once")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspacePublication {
    Changed,
    Busy,
}

/// How long a just-created shell is given to finish starting up.
///
/// `path_helper` takes a few hundred milliseconds. This is the outer bound on
/// waiting for something that normally takes one observation, not a duration
/// anyone should reach.
const SHELL_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// Observes a pane's processes, letting a shell that is still starting finish.
///
/// 🚨 A shell running its startup files is not a shell somebody else is using.
/// A login zsh on macOS runs `/usr/libexec/path_helper` out of `/etc/zprofile`,
/// so for a moment after a pane is created its foreground list holds something
/// that is not the shell — and the question the callers ask, "are all the
/// foreground processes the shell itself", answers no.
///
/// Reading that as "this shell is taken" is what left an empty tab beside the
/// dock. The dock created its workspace, looked at the brand-new pane, decided
/// it was in use, and either abandoned the launch outright or planned a second
/// tab next to the one it had just made. It reproduced on every run of a
/// release build against a fresh workspace; a debug build was slow enough to
/// miss the window, which is why it only ever appeared in the installed binary.
///
/// Bounded, and only worth using on a pane this invocation created: a pane
/// holding a running tower is never an idle shell and waiting on one would
/// spend this budget on every call.
fn observe_settled_shell(
    read: impl FnMut() -> herdrx::Result<herdrx::ProcessInfo>,
    within: std::time::Duration,
    pause: impl FnMut(),
) -> herdrx::Result<herdrx::ProcessInfo> {
    observe_until(read, process_info_has_idle_foreground_shell, within, pause)
}

/// Observes a pane's processes until `done` holds of them or `within` runs
/// out, and answers the last observation. The one polling loop both waits use.
fn observe_until(
    mut read: impl FnMut() -> herdrx::Result<herdrx::ProcessInfo>,
    done: impl Fn(&herdrx::ProcessInfo) -> bool,
    within: std::time::Duration,
    mut pause: impl FnMut(),
) -> herdrx::Result<herdrx::ProcessInfo> {
    let deadline = std::time::Instant::now() + within;
    loop {
        // 🚨 The injected pause, not the hardcoded sleeper. Handing
        // `dock_observation_pause` to the inner retry meant a caller asking for
        // no back-off still slept 100ms per transient failure — and the tests
        // that pass a panicking pause to prove "this path must not pause" were
        // asserting against a closure the retry never called.
        let observed = retry_transient_dock_observation_bounded(&mut read, &mut pause)?;
        if done(&observed) || std::time::Instant::now() >= deadline {
            return Ok(observed);
        }
        pause();
    }
}

/// Waits, at most `within`, for a restored dock shell to be of use: until it
/// is an idle shell, or a tower has appeared in it (another `dock` got there
/// first), or herdr turns out unable to show its processes. Nothing is decided
/// here — the caller looks again either way — so a failed observation just
/// ends the wait.
fn settle_restored_shell(
    read: impl FnMut() -> herdrx::Result<herdrx::ProcessInfo>,
    executable: &Path,
    within: std::time::Duration,
    pause: impl FnMut(),
) {
    let _ = observe_until(
        read,
        |info| {
            process_info_has_idle_foreground_shell(info)
                || tower_cli::dockcmd::is_tower_process(info, executable)
                || process_info_is_unobservable(info)
        },
        within,
        pause,
    );
}

/// herdr answered but could not show the pane's processes: no shell and no
/// foreground to judge by.
fn process_info_is_unobservable(info: &herdrx::ProcessInfo) -> bool {
    info.shell_pid <= 0 && info.foreground_processes.is_empty()
}

fn process_info_has_idle_foreground_shell(info: &herdrx::ProcessInfo) -> bool {
    info.shell_pid > 0
        && !info.foreground_processes.is_empty()
        && info
            .foreground_processes
            .iter()
            .all(|process| process.pid == info.shell_pid)
}

fn process_info_has_non_shell_foreground(info: &herdrx::ProcessInfo) -> bool {
    !process_info_has_idle_foreground_shell(info)
}

fn pane_retains_launch_target(pane: &herdrx::Pane, terminal_id: &str) -> bool {
    pane.terminal_id == terminal_id && !pane.has_agent()
}

/// A durable creation marker permits adoption; this live check permits use.
fn workspace_creation_candidate_is_idle(
    h: &herdrx::Herdr,
    workspace_id: &str,
    owned_by_request: bool,
    allow_unobservable_owned_shell: bool,
    fenced: bool,
) -> herdrx::Result<bool> {
    let panes = if fenced {
        retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)
    } else {
        retry_transient_dock_observation_bounded(|| h.list_panes(), dock_observation_pause)
    }?;
    let mut candidate_panes = panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace_id);
    let Some(pane) = candidate_panes.next() else {
        return Ok(false);
    };
    if candidate_panes.next().is_some()
        || pane.has_agent()
        || pane.terminal_id.is_empty()
        || pane
            .tokens
            .get(tower_cli::dockcmd::STARTING_TOKEN)
            .is_some_and(|value| tower_cli::dockcmd::pane_starting_lease_is_held(value))
        || pane
            .tokens
            .get(tower_cli::dockcmd::REUSABLE_TOKEN)
            .is_some_and(|value| !value.is_empty())
        || pane
            .tokens
            .get(tower_cli::dockcmd::ACTIVE_TOKEN)
            .is_some_and(|value| !value.is_empty())
    {
        return Ok(false);
    }
    let process_info = if owned_by_request {
        observe_settled_shell(
            || h.process_info(&pane.pane_id),
            SHELL_SETTLE,
            dock_observation_pause,
        )
    } else if fenced {
        retry_transient_dock_observation(|| h.process_info(&pane.pane_id), dock_observation_pause)
    } else {
        retry_transient_dock_observation_bounded(
            || h.process_info(&pane.pane_id),
            dock_observation_pause,
        )
    };
    match process_info {
        Ok(info) if info.shell_pid > 0 => Ok(!process_info_has_non_shell_foreground(&info)),
        Ok(info) => Ok(allow_unobservable_owned_shell && info.foreground_processes.is_empty()),
        Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => Ok(false),
        Err(error) => Err(error),
    }
}

/// Publishes only the private candidate that owns the shared creation gate.
///
/// A caller may reach `workspace.create` from a snapshot taken before another
/// create was visible. Every create therefore stops at a request label. On the
/// refreshed snapshot all callers coordinate through Herdr's earliest private
/// candidate (or the already-public dock), and the winner revalidates that
/// shared anchor under its server lease immediately before renaming it.
fn publish_canonical_workspace(
    h: &herdrx::Herdr,
    workspace_id: &str,
    request_label: &str,
    local_gate_available: bool,
    owned_by_request: bool,
    needs_claim: bool,
    executable: &Path,
) -> Result<WorkspacePublication> {
    // A candidate created by this process must keep its in-memory ownership
    // across transient observation failures. Returning would turn it into a
    // foreign candidate on the next invocation, so only recovered candidates
    // use the user-facing bounded read here.
    let workspaces = if owned_by_request {
        retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)?
    } else {
        retry_transient_dock_observation_bounded(|| h.workspaces(), dock_observation_pause)?
    };
    let Some(candidate) = tower_cli::dockcmd::dock_workspace(&workspaces).filter(|workspace| {
        workspace.workspace_id == workspace_id && workspace.label == request_label
    }) else {
        return Ok(WorkspacePublication::Changed);
    };
    if !owned_by_request && tower_cli::dockcmd::workspace_creator_is_live(candidate, request_label)
    {
        // Herdr expires this separate creator contribution on its own clock.
        // A foreign caller waits while it is live; after expiry the persistent
        // request claim is conservatively retired rather than adopted.
        return Ok(WorkspacePublication::Busy);
    }
    let fence = Some(tower_cli::dockcmd::CoordinationFence::arm()?);
    let owner = tower_cli::dockcmd::starting_lease_value();
    let (generation, expected) = match acquire_coordination_gate(h, workspace_id, &owner)? {
        CoordinationGateClaim::Acquired {
            generation,
            expected,
        } => (generation, expected),
        CoordinationGateClaim::Busy => return Ok(WorkspacePublication::Busy),
    };
    // The hard fence bounds this coordinator before the server TTL. Do not
    // voluntarily unwind and discard create ownership merely because three
    // transient reads failed after the gate was acquired.
    let coordinated_workspaces =
        match retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause) {
            Ok(workspaces) => workspaces,
            Err(error) => {
                clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
                return Err(error.into());
            }
        };
    if !tower_cli::dockcmd::owns_coordination(&coordinated_workspaces, workspace_id, &expected) {
        return Ok(WorkspacePublication::Busy);
    }
    let still_canonical =
        tower_cli::dockcmd::dock_workspace(&coordinated_workspaces).is_some_and(|workspace| {
            workspace.workspace_id == workspace_id && workspace.label == request_label
        });
    if !still_canonical {
        clear_dock_coordination(h, workspace_id, generation, true);
        drop(fence);
        return Ok(WorkspacePublication::Changed);
    }
    // A candidate that survived a previous process may already have been
    // adopted by a person, including with background jobs invisible to
    // `foreground_processes`. Preserve it and retire the reserved label rather
    // than inferring exclusive ownership from apparent shell idleness.
    if !owned_by_request {
        retire_workspace_creation_claim_confirmed(h, workspace_id, request_label, true)?;
        clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
        drop(fence);
        return Ok(WorkspacePublication::Changed);
    }
    let idle = match workspace_creation_candidate_is_idle(
        h,
        workspace_id,
        owned_by_request,
        local_gate_available && owned_by_request,
        true,
    ) {
        Ok(idle) => idle,
        Err(error) => {
            clear_dock_coordination(h, workspace_id, generation, true);
            return Err(error.into());
        }
    };
    if !idle {
        if let Err(error) =
            retire_workspace_creation_claim_confirmed(h, workspace_id, request_label, true)
        {
            clear_dock_coordination(h, workspace_id, generation, true);
            return Err(error.into());
        }
        clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
        drop(fence);
        return Ok(WorkspacePublication::Changed);
    }
    if needs_claim {
        match ensure_workspace_creation_claim(h, workspace_id, request_label, true) {
            Ok(WorkspaceCreationClaim::Stored) => {}
            Err(error) => {
                clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
                return Err(error.into());
            }
        }
    }
    if let Err(error) =
        finish_workspace_request(h, workspace_id, request_label, generation, &expected)
    {
        drop(fence);
        let _ = reconcile_created_workspace_cleanup(
            h,
            workspace_id,
            request_label,
            owned_by_request,
            executable,
        );
        return Err(error.into());
    }
    clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
    drop(fence);
    Ok(WorkspacePublication::Changed)
}

/// Publishes the elected workspace while holding its shared creation gate.
fn finish_workspace_request(
    h: &herdrx::Herdr,
    workspace_id: &str,
    request_label: &str,
    generation: u64,
    expected_coordination: &str,
) -> std::result::Result<(), CoordinatedDockError> {
    for attempt in 0..2 {
        match coordinated_dock_call(h, workspace_id, generation, expected_coordination, || {
            h.rename_workspace(workspace_id, tower_cli::dockcmd::LABEL)
        }) {
            Ok(()) => return Ok(()),
            Err(error) if error.delivery_is_uncertain() => {
                let workspaces =
                    retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)
                        .map_err(CoordinatedDockError::Observation)?;
                let Some(workspace) = workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                else {
                    return Err(error);
                };
                if workspace.label == tower_cli::dockcmd::LABEL {
                    return Ok(());
                }
                if workspace.label == request_label && attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two workspace rename attempts either return or continue once")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatedWorkspaceCleanup {
    Closed,
    Busy,
    Adopted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatedWorkspaceCleanupScope {
    Busy,
    Coordinate,
    Adopted,
}

fn created_workspace_cleanup_scope(
    workspace: &herdrx::Workspace,
    request_label: &str,
) -> CreatedWorkspaceCleanupScope {
    if workspace.label == tower_cli::dockcmd::LABEL {
        return CreatedWorkspaceCleanupScope::Adopted;
    }
    if workspace.label != request_label {
        return CreatedWorkspaceCleanupScope::Adopted;
    }
    match tower_cli::dockcmd::next_coordination_generation(
        workspace
            .tokens
            .get(tower_cli::dockcmd::COORDINATING_GENERATION_TOKEN)
            .map(String::as_str),
        workspace
            .tokens
            .get(tower_cli::dockcmd::COORDINATING_TOKEN)
            .map(String::as_str),
    ) {
        Some(_) => CreatedWorkspaceCleanupScope::Coordinate,
        None => CreatedWorkspaceCleanupScope::Busy,
    }
}

/// Cleans up a candidate without extending create ownership past publication.
///
/// A request-only label cannot be discovered as a dock and remains safe to
/// close under its gate. Publication permanently ends create ownership: the
/// workspace may contain background or suspended user jobs that the foreground
/// process observer cannot see, so a public dock is never auto-closed here.
fn reconcile_created_workspace_cleanup(
    h: &herdrx::Herdr,
    workspace_id: &str,
    request_label: &str,
    owned_by_request: bool,
    executable: &Path,
) -> Result<CreatedWorkspaceCleanup> {
    let workspaces =
        retry_transient_dock_observation_bounded(|| h.workspaces(), dock_observation_pause)?;
    let Some(workspace) = workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
    else {
        return Ok(CreatedWorkspaceCleanup::Closed);
    };
    match created_workspace_cleanup_scope(workspace, request_label) {
        CreatedWorkspaceCleanupScope::Coordinate => {}
        CreatedWorkspaceCleanupScope::Busy => return Ok(CreatedWorkspaceCleanup::Busy),
        CreatedWorkspaceCleanupScope::Adopted => return Ok(CreatedWorkspaceCleanup::Adopted),
    }
    let fence = tower_cli::dockcmd::CoordinationFence::arm()?;
    let owner = tower_cli::dockcmd::starting_lease_value();
    let (generation, expected) = match acquire_coordination_gate(h, workspace_id, &owner)? {
        CoordinationGateClaim::Acquired {
            generation,
            expected,
        } => (generation, expected),
        CoordinationGateClaim::Busy => {
            drop(fence);
            return Ok(CreatedWorkspaceCleanup::Busy);
        }
    };

    // Cross-invocation ownership cannot prove the absence of background or
    // suspended jobs. Retire the reserved marker so future dock planning
    // ignores this visible workspace, but never inspect-and-close it.
    if !owned_by_request {
        retire_workspace_creation_claim_confirmed(h, workspace_id, request_label, true)?;
        clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
        drop(fence);
        return Ok(CreatedWorkspaceCleanup::Adopted);
    }

    let panes = retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)?;
    let candidate_panes: Vec<_> = panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace_id)
        .collect();
    let mut adopted = candidate_panes.is_empty();
    for pane in candidate_panes {
        adopted |= pane.has_agent()
            || pane.terminal_id.is_empty()
            || pane
                .tokens
                .get(tower_cli::dockcmd::STARTING_TOKEN)
                .is_some_and(|value| tower_cli::dockcmd::pane_starting_lease_is_held(value))
            || pane
                .tokens
                .get(tower_cli::dockcmd::REUSABLE_TOKEN)
                .is_some_and(|value| !value.is_empty());
        adopted |= pane
            .tokens
            .get(tower_cli::dockcmd::ACTIVE_TOKEN)
            .is_some_and(|value| !value.is_empty());
        if adopted {
            break;
        }
        let process_info = if owned_by_request {
            observe_settled_shell(
                || h.process_info(&pane.pane_id),
                SHELL_SETTLE,
                dock_observation_pause,
            )
        } else {
            retry_transient_dock_observation(
                || h.process_info(&pane.pane_id),
                dock_observation_pause,
            )
        };
        match process_info {
            Ok(info) => {
                adopted |= tower_cli::dockcmd::is_tower_process(&info, executable)
                    || process_info_has_non_shell_foreground(&info);
            }
            Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {
                adopted = true;
            }
            Err(error) => {
                clear_dock_coordination(h, workspace_id, generation, true);
                return Err(error.into());
            }
        }
    }
    if adopted {
        if let Err(error) =
            retire_workspace_creation_claim_confirmed(h, workspace_id, request_label, true)
        {
            clear_dock_coordination(h, workspace_id, generation, true);
            return Err(error.into());
        }
        clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
        drop(fence);
        return Ok(CreatedWorkspaceCleanup::Adopted);
    }

    for attempt in 0..2 {
        let current = retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)?;
        if !tower_cli::dockcmd::owns_coordination(&current, workspace_id, &expected) {
            drop(fence);
            return Ok(CreatedWorkspaceCleanup::Busy);
        }
        match h.close_workspace(workspace_id) {
            Ok(()) => return Ok(CreatedWorkspaceCleanup::Closed),
            Err(error) if error.is_code("workspace_not_found") || error.is_code("not_found") => {
                return Ok(CreatedWorkspaceCleanup::Closed);
            }
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let after =
                    retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)?;
                if !after
                    .iter()
                    .any(|workspace| workspace.workspace_id == workspace_id)
                {
                    return Ok(CreatedWorkspaceCleanup::Closed);
                }
                if attempt == 0 {
                    continue;
                }
                return Err(error.into());
            }
            Err(error) => {
                clear_dock_coordination(h, workspace_id, generation, true);
                return Err(error.into());
            }
        }
    }
    unreachable!("two coordinated workspace close attempts either return or continue once")
}

fn close_unpublished_workspace_before_fence(
    h: &herdrx::Herdr,
    workspace_id: &str,
) -> herdrx::Result<()> {
    for attempt in 0..2 {
        match h.close_workspace(workspace_id) {
            Ok(()) => return Ok(()),
            Err(error) if error.is_code("workspace_not_found") || error.is_code("not_found") => {
                return Ok(());
            }
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let workspaces = retry_transient_dock_observation_bounded(
                    || h.workspaces(),
                    dock_observation_pause,
                )?;
                if !workspaces
                    .iter()
                    .any(|workspace| workspace.workspace_id == workspace_id)
                {
                    return Ok(());
                }
                if attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two workspace close attempts either return or continue once")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinationReleaseObservation {
    Released,
    Retry,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoordinationGateClaim {
    Acquired { generation: u64, expected: String },
    Busy,
}

/// Acquires an expiring workspace gate and makes its winning sequence durable
/// before the caller may mutate anything.
///
/// Herdr remembers ordered sequences after a token's TTL expires. If a process
/// died before persisting its high-water mark, an acknowledged write can read
/// back as absent; advancing the persistent marker and retrying recovers that
/// hidden sequence. Concurrent contenders still submit the same next sequence,
/// so exact held-token readback elects one winner before its marker is stored.
fn acquire_coordination_gate(
    h: &herdrx::Herdr,
    workspace_id: &str,
    owner: &str,
) -> Result<CoordinationGateClaim> {
    loop {
        let workspaces =
            retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)?;
        let Some(workspace) = workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return Ok(CoordinationGateClaim::Busy);
        };
        let Some(generation) = tower_cli::dockcmd::next_coordination_generation(
            workspace
                .tokens
                .get(tower_cli::dockcmd::COORDINATING_GENERATION_TOKEN)
                .map(String::as_str),
            workspace
                .tokens
                .get(tower_cli::dockcmd::COORDINATING_TOKEN)
                .map(String::as_str),
        ) else {
            return Ok(CoordinationGateClaim::Busy);
        };
        let lease = tower_cli::dockcmd::coordinating_lease(workspace_id, generation, owner);
        let expected = lease.tokens[tower_cli::dockcmd::COORDINATING_TOKEN].clone();
        match h.report_workspace_metadata(&lease) {
            Ok(()) => {}
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {}
            Err(error) => return Err(error.into()),
        }
        let after = retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)?;
        let Some(workspace) = after
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return Ok(CoordinationGateClaim::Busy);
        };
        let actual_gate = workspace.tokens.get(tower_cli::dockcmd::COORDINATING_TOKEN);
        let marker =
            tower_cli::dockcmd::coordinating_generation_claim(workspace_id, generation, owner);
        let expected_marker =
            marker.tokens[tower_cli::dockcmd::COORDINATING_GENERATION_TOKEN].clone();
        if actual_gate == Some(&expected) {
            // Persist only after this exact held value won. The marker patch has
            // no TTL and does not touch the held token's existing expiry.
            match h.report_workspace_metadata(&marker) {
                Ok(()) => {}
                Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {}
                Err(error) => {
                    clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
                    return Err(error.into());
                }
            }
            let confirmed =
                match retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause) {
                    Ok(confirmed) => confirmed,
                    Err(error) => {
                        clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
                        return Err(error.into());
                    }
                };
            let confirmed_workspace = confirmed
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id);
            let gate_is_ours = confirmed_workspace.is_some_and(|workspace| {
                workspace.tokens.get(tower_cli::dockcmd::COORDINATING_TOKEN) == Some(&expected)
            });
            let marker_is_ours = confirmed_workspace.is_some_and(|workspace| {
                workspace
                    .tokens
                    .get(tower_cli::dockcmd::COORDINATING_GENERATION_TOKEN)
                    == Some(&expected_marker)
            });
            if gate_is_ours && !marker_is_ours {
                clear_dock_coordination_confirmed(h, workspace_id, generation, &expected)?;
            }
            return Ok(if gate_is_ours && marker_is_ours {
                CoordinationGateClaim::Acquired {
                    generation,
                    expected,
                }
            } else {
                CoordinationGateClaim::Busy
            });
        }
        if actual_gate.is_some_and(|value| value.starts_with("held:")) {
            return Ok(CoordinationGateClaim::Busy);
        }
        // The held write was rejected by a hidden post-TTL sequence. Advance a
        // visible high-water candidate only; no mutation has been authorized.
        match h.report_workspace_metadata(&marker) {
            Ok(()) => {}
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Clears a launch-specific pane lease, reconciling a reply lost after apply.
fn clear_starting_lease_confirmed(
    h: &herdrx::Herdr,
    terminal_id: &str,
    generation: u64,
    expected: &str,
) -> herdrx::Result<()> {
    for attempt in 0..2 {
        let panes =
            retry_transient_dock_observation_bounded(|| h.list_panes(), dock_observation_pause)?;
        let Some(pane) = pane_for_terminal(&panes, terminal_id) else {
            return Ok(());
        };
        if pane
            .tokens
            .get(tower_cli::dockcmd::STARTING_TOKEN)
            .map(String::as_str)
            != Some(expected)
        {
            return Ok(());
        }
        let clear = tower_cli::dockcmd::clear_starting_lease(&pane.pane_id, generation);
        match h.report_metadata(&clear) {
            Ok(()) => return Ok(()),
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let panes = retry_transient_dock_observation_bounded(
                    || h.list_panes(),
                    dock_observation_pause,
                )?;
                let actual = pane_for_terminal(&panes, terminal_id)
                    .and_then(|pane| pane.tokens.get(tower_cli::dockcmd::STARTING_TOKEN));
                if actual.map(String::as_str) != Some(expected) {
                    return Ok(());
                }
                if attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {
                if attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two startup-lease clears either return or continue once")
}

fn observe_coordination_release(
    workspaces: &[herdrx::Workspace],
    workspace_id: &str,
    expected_held: &str,
    expected_idle: &str,
) -> CoordinationReleaseObservation {
    let Some(workspace) = workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
    else {
        return CoordinationReleaseObservation::Released;
    };
    match workspace
        .tokens
        .get(tower_cli::dockcmd::COORDINATING_TOKEN)
        .map(String::as_str)
    {
        Some(actual) if actual == expected_idle => CoordinationReleaseObservation::Released,
        Some(actual) if actual == expected_held => CoordinationReleaseObservation::Retry,
        _ => CoordinationReleaseObservation::Changed,
    }
}

/// Releases an acquired workspace gate without replanning behind our own
/// possibly still-live token.
fn clear_dock_coordination_confirmed(
    h: &herdrx::Herdr,
    workspace_id: &str,
    generation: u64,
    expected_held: &str,
) -> herdrx::Result<()> {
    let released = tower_cli::dockcmd::clear_coordinating_lease(workspace_id, generation);
    let expected_idle = released.tokens[tower_cli::dockcmd::COORDINATING_TOKEN].clone();
    loop {
        match h.report_workspace_metadata(&released) {
            Ok(()) => return Ok(()),
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let workspaces =
                    retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)?;
                match observe_coordination_release(
                    &workspaces,
                    workspace_id,
                    expected_held,
                    &expected_idle,
                ) {
                    CoordinationReleaseObservation::Released => return Ok(()),
                    CoordinationReleaseObservation::Retry => {
                        dock_observation_pause();
                        continue;
                    }
                    CoordinationReleaseObservation::Changed => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Clears the recoverable server-side mutation gate after a definitive result.
fn clear_dock_coordination(
    h: &herdrx::Herdr,
    workspace_id: &str,
    generation: u64,
    lease_held: bool,
) {
    if !lease_held {
        return;
    }
    let _ = h.report_workspace_metadata(&tower_cli::dockcmd::clear_coordinating_lease(
        workspace_id,
        generation,
    ));
}

#[derive(Debug)]
enum CoordinatedDockError {
    Observation(herdrx::Error),
    LeaseChanged,
    Mutation(herdrx::Error),
}

impl CoordinatedDockError {
    fn delivery_is_uncertain(&self) -> bool {
        matches!(self, Self::Mutation(error) if tower_cli::dockcmd::delivery_is_uncertain(error))
    }
}

impl std::fmt::Display for CoordinatedDockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observation(error) | Self::Mutation(error) => write!(f, "{error}"),
            Self::LeaseChanged => {
                write!(f, "the dock coordination lease changed before its mutation")
            }
        }
    }
}

impl std::error::Error for CoordinatedDockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Observation(error) | Self::Mutation(error) => Some(error),
            Self::LeaseChanged => None,
        }
    }
}

impl From<CoordinatedDockError> for CliError {
    fn from(error: CoordinatedDockError) -> Self {
        CliError::Other(Box::new(error))
    }
}

/// Converts a request-owned temporary tab label into the public dock label.
/// The request label is part of `tab.create`, so reconciliation never adopts a
/// user or another client tab that merely happens to be named `tower`.
fn finish_tab_request(
    h: &herdrx::Herdr,
    anchor_workspace_id: &str,
    generation: u64,
    expected_coordination: &str,
    tab_id: &str,
    request_label: &str,
) -> std::result::Result<(), CoordinatedDockError> {
    for attempt in 0..2 {
        match coordinated_dock_call(
            h,
            anchor_workspace_id,
            generation,
            expected_coordination,
            || h.rename_tab(tab_id, tower_cli::dockcmd::TAB_LABEL),
        ) {
            Ok(()) => return Ok(()),
            Err(error) if error.delivery_is_uncertain() => {
                let observed =
                    retry_transient_dock_observation(|| h.tabs(), dock_observation_pause);
                let tabs = observed.map_err(CoordinatedDockError::Observation)?;
                let Some(tab) = tabs.iter().find(|tab| tab.tab_id == tab_id) else {
                    return Err(error);
                };
                if tab.label == tower_cli::dockcmd::TAB_LABEL {
                    return Ok(());
                }
                if tab.label == request_label && attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two tab rename attempts either return or continue once")
}

/// Writes the durable retry trace, reconciling a reply that was lost after the
/// server applied it. The coordination lease remains held on an uncertain
/// response, so this read cannot race another `dock` mutation.
fn ensure_reusable_metadata(
    h: &herdrx::Herdr,
    anchor_workspace_id: &str,
    generation: u64,
    lease_held: bool,
    pane_id: &str,
    terminal_id: &str,
) -> std::result::Result<(), CoordinatedDockError> {
    set_pane_metadata(
        h,
        anchor_workspace_id,
        generation,
        lease_held,
        tower_cli::dockcmd::remember_reusable_metadata(pane_id, terminal_id),
    )
}

fn ensure_reusable_submitted(
    h: &herdrx::Herdr,
    anchor_workspace_id: &str,
    generation: u64,
    pane_id: &str,
    terminal_id: &str,
) -> std::result::Result<(), CoordinatedDockError> {
    set_pane_metadata(
        h,
        anchor_workspace_id,
        generation,
        true,
        tower_cli::dockcmd::mark_reusable_submitted_metadata(pane_id, terminal_id),
    )
}

fn ensure_tab_creation_lease(
    h: &herdrx::Herdr,
    anchor_workspace_id: &str,
    generation: u64,
    pane_id: &str,
    request_label: &str,
) -> std::result::Result<(), CoordinatedDockError> {
    set_pane_metadata(
        h,
        anchor_workspace_id,
        generation,
        true,
        tower_cli::dockcmd::tab_creation_lease(pane_id, request_label),
    )
}

/// Writes and confirms the server-clocked request intent before `tab.create`.
/// This closes the otherwise unobservable crash window between creation and
/// the first pane-metadata write.
fn ensure_tab_creation_intent(
    h: &herdrx::Herdr,
    anchor_workspace_id: &str,
    generation: u64,
    expected_coordination: &str,
    request_label: &str,
) -> std::result::Result<(), CoordinatedDockError> {
    let intent =
        tower_cli::dockcmd::tab_creation_intent(anchor_workspace_id, request_label, generation);
    for attempt in 0..2 {
        match coordinated_dock_call(
            h,
            anchor_workspace_id,
            generation,
            expected_coordination,
            || h.report_workspace_metadata(&intent),
        ) {
            Ok(()) => return Ok(()),
            Err(error) if error.delivery_is_uncertain() => {
                let workspaces =
                    retry_transient_dock_observation(|| h.workspaces(), dock_observation_pause)
                        .map_err(CoordinatedDockError::Observation)?;
                let stored = workspaces.iter().any(|workspace| {
                    workspace.workspace_id == anchor_workspace_id
                        && tower_cli::dockcmd::tab_creation_intent_is_live(workspace, request_label)
                });
                if stored {
                    return Ok(());
                }
                if attempt == 0 {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two tab-creation intent writes either return or continue once")
}

fn set_pane_metadata(
    h: &herdrx::Herdr,
    anchor_workspace_id: &str,
    generation: u64,
    lease_held: bool,
    metadata: herdrx::Metadata,
) -> std::result::Result<(), CoordinatedDockError> {
    let pane_id = metadata.pane_id.clone();
    let Some((token_name, expected)) = metadata.tokens.iter().next() else {
        return Ok(());
    };
    let token_name = token_name.clone();
    let expected = expected.clone();
    for attempt in 0..2 {
        match h.report_metadata(&metadata) {
            Ok(()) => return Ok(()),
            Err(error) if tower_cli::dockcmd::delivery_is_uncertain(&error) => {
                let observed = if lease_held {
                    retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)
                } else {
                    retry_transient_dock_observation_bounded(
                        || h.list_panes(),
                        dock_observation_pause,
                    )
                };
                let panes = match observed {
                    Ok(panes) => panes,
                    Err(_) => return Err(CoordinatedDockError::Mutation(error)),
                };
                let Some(pane) = panes.iter().find(|pane| pane.pane_id == pane_id) else {
                    return Err(CoordinatedDockError::Mutation(error));
                };
                if pane
                    .tokens
                    .get(&token_name)
                    .is_some_and(|value| value == &expected)
                {
                    return Ok(());
                }
                if attempt == 0 {
                    // The read proved the first idempotent write was not
                    // applied. Retry while the dock gate/local lock is still
                    // held instead of stranding the known-created shell.
                    continue;
                }
                return Err(CoordinatedDockError::Mutation(error));
            }
            Err(error) => {
                clear_dock_coordination(h, anchor_workspace_id, generation, lease_held);
                return Err(CoordinatedDockError::Mutation(error));
            }
        }
    }
    unreachable!("two pane metadata attempts either return or continue once")
}

/// Runs one planning mutation and releases its gate when rejection is certain.
///
/// I/O and decoding errors may arrive after Herdr applied the request. Those
/// retain the server-expiring lease so an immediate retry cannot race an
/// operation whose reply alone was lost.
fn coordinated_dock_call<T>(
    h: &herdrx::Herdr,
    anchor_workspace_id: &str,
    generation: u64,
    expected: &str,
    call: impl FnOnce() -> herdrx::Result<T>,
) -> std::result::Result<T, CoordinatedDockError> {
    let workspaces = match h.workspaces() {
        Ok(workspaces) => workspaces,
        Err(error) => {
            // This was only a read, so no mutation can be in flight.
            clear_dock_coordination(h, anchor_workspace_id, generation, true);
            return Err(CoordinatedDockError::Observation(error));
        }
    };
    let still_owned =
        tower_cli::dockcmd::owns_coordination(&workspaces, anchor_workspace_id, expected);
    if !still_owned {
        return Err(CoordinatedDockError::LeaseChanged);
    }
    match call() {
        Ok(value) => Ok(value),
        Err(error) => {
            if !tower_cli::dockcmd::delivery_is_uncertain(&error) {
                clear_dock_coordination(h, anchor_workspace_id, generation, true);
            }
            Err(CoordinatedDockError::Mutation(error))
        }
    }
}

/// Moves the dock to the top of the sidebar. The tower running in it keeps it
/// there; this covers the moment before that tower is up.
fn pin_dock(h: &herdrx::Herdr) -> Result<()> {
    if let Some(id) = tower_cli::dockcmd::to_pin(&h.workspaces()?) {
        h.move_workspace(&id, 0)?;
    }
    Ok(())
}

/// Names the dock's tab after the dock, and takes herdr's right-click menu —
/// whose entries include *Close pane* — off the tower's pane.
///
/// Best effort on purpose. `dock` exists to make the tower reachable by a
/// click, and a server that refuses either call has still done that. The tower
/// repeats both on
/// its own clock once it is up ([`tower_ui::Tower::hold_dock`]).
///
/// A tab still wearing another invocation's request label is left alone: that
/// label is how the invocation finds its own half-created tab again.
fn hold_dock_pane(h: &herdrx::Herdr, pane_id: &str) {
    let Ok(pane) = h.pane(pane_id) else {
        return;
    };
    if let Ok(tabs) = h.tabs() {
        if tower_cli::dockcmd::tab_needs_dock_label(&tabs, &pane.tab_id) {
            let _ = h.rename_tab(&pane.tab_id, tower_cli::dockcmd::TAB_LABEL);
        }
    }
    let _ = h.set_right_click(pane_id, herdrx::RightClick::Pane);
}

fn pin_workspace(h: &herdrx::Herdr, workspace_id: &str) -> Result<()> {
    if let Some(id) = tower_cli::dockcmd::to_pin_workspace(&h.workspaces()?, workspace_id) {
        h.move_workspace(&id, 0)?;
    }
    Ok(())
}

/// Waits, a little, for `pane_id` to show anything at all — on Windows, the
/// shell's prompt. Past the wait the launch goes ahead as before.
#[cfg(windows)]
fn wait_for_prompt(h: &herdrx::Herdr, pane_id: &str) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < until {
        let shown = h
            .read(pane_id, herdrx::ReadSource::Visible, 50)
            .is_ok_and(|text| !text.trim().is_empty());
        if shown {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_active_rejects_every_coordination_handoff() {
        assert!(dock_pending(true, "another dock invocation is coordinating").is_err());
        assert!(dock_pending(false, "another dock invocation is coordinating").is_ok());
    }
    /// A launch targets the terminal, not the pane coordinate: a pane that now
    /// holds a replacement terminal is not the shell the plan chose.
    #[test]
    fn a_launch_target_is_its_terminal_not_its_pane() {
        let pane: herdrx::Pane = serde_json::from_value(serde_json::json!({
            "pane_id": "opaque:pane",
            "tab_id": "opaque:tab",
            "workspace_id": "opaque-workspace",
            "terminal_id": "opaque-terminal"
        }))
        .unwrap();
        assert!(pane_retains_launch_target(&pane, "opaque-terminal"));
        let mut replacement = pane.clone();
        replacement.terminal_id = "replacement-terminal".into();
        assert!(!pane_retains_launch_target(&replacement, "opaque-terminal"));
    }

    #[test]
    fn a_request_labeled_tab_survives_its_creator_and_is_reused() {
        use tower_cli::dockcmd::{self, Plan};

        let workspaces: Vec<herdrx::Workspace> = serde_json::from_value(serde_json::json!([{
            "workspace_id": "w9",
            "label": dockcmd::LABEL
        }]))
        .unwrap();
        let request_label = dockcmd::tab_request_label("100000000000-a");
        let tabs: Vec<herdrx::Tab> = serde_json::from_value(serde_json::json!([
            {"tab_id": "w9:t1", "workspace_id": "w9", "label": dockcmd::TAB_LABEL},
            {"tab_id": "w9:t2", "workspace_id": "w9", "label": request_label},
            {"tab_id": "w9:t3", "workspace_id": "w9", "label": "tower#notes"}
        ]))
        .unwrap();
        let mut panes: Vec<herdrx::Pane> = serde_json::from_value(serde_json::json!([
            {"pane_id": "w9:p1", "tab_id": "w9:t1", "workspace_id": "w9", "terminal_id": "term-old"},
            {"pane_id": "w9:p2", "tab_id": "w9:t2", "workspace_id": "w9", "terminal_id": "term-recover"},
            {"pane_id": "w9:p3", "tab_id": "w9:t3", "workspace_id": "w9", "terminal_id": "term-user"}
        ]))
        .unwrap();
        let mut intent_workspaces = workspaces.clone();
        intent_workspaces[0].tokens = dockcmd::tab_creation_intent("w9", &request_label, 8).tokens;
        let prelease = recoverable_request_tab_terminals(&intent_workspaces, &tabs, &panes);
        assert_eq!(
            prelease,
            HashSet::from(["term-recover".to_string()]),
            "the pre-create workspace intent bridges a crash before pane metadata"
        );

        panes[1].tokens = dockcmd::tab_creation_lease("w9:p2", &request_label).tokens;
        let recoverable = recoverable_request_tab_terminals(&workspaces, &tabs, &panes);

        assert_eq!(recoverable, HashSet::from(["term-recover".to_string()]));
        assert_eq!(
            dockcmd::plan(
                &workspaces,
                &panes,
                |_| false,
                |_| false,
                |terminal| recoverable.contains(terminal),
                |terminal| terminal == "term-recover",
            ),
            Plan::Launch {
                pane_id: "w9:p2".into(),
                terminal_id: "term-recover".into(),
            }
        );

        let mut already_submitted = panes.clone();
        already_submitted[1]
            .tokens
            .extend(dockcmd::mark_reusable_submitted_metadata("w9:p2", "term-recover").tokens);
        assert!(
            recoverable_request_tab_terminals(&workspaces, &tabs, &already_submitted).is_empty(),
            "a contradictory submitted marker is never downgraded by the label"
        );

        let mut expired = panes.clone();
        expired[1].tokens.remove(dockcmd::TAB_CREATING_TOKEN);
        assert!(
            recoverable_request_tab_terminals(&workspaces, &tabs, &expired).is_empty(),
            "an old request tab is preserved after its server-timed lease expires"
        );

        let mut split = panes.clone();
        split.push(
            serde_json::from_value(serde_json::json!({
                "pane_id": "w9:p4",
                "tab_id": "w9:t2",
                "workspace_id": "w9",
                "terminal_id": "term-split"
            }))
            .unwrap(),
        );
        assert!(recoverable_request_tab_terminals(&workspaces, &tabs, &split).is_empty());
    }

    #[test]
    fn a_ready_marker_is_followed_by_terminal_identity_across_pane_moves() {
        let pane = |pane_id: &str, active: &str| {
            serde_json::from_value::<herdrx::Pane>(serde_json::json!({
                "pane_id": pane_id,
                "tab_id": "opaque-tab",
                "workspace_id": "opaque-workspace",
                "terminal_id": "stable-terminal",
                "tokens": { "dock-active": active }
            }))
            .unwrap()
        };
        let active_value = tower_cli::dockcmd::active_claim_value("stable-terminal", "launch-7");
        let before = pane("old-pane", &active_value);
        let mut after = pane("moved-pane", &active_value);
        after
            .tokens
            .insert(tower_cli::dockcmd::STARTING_TOKEN.into(), "idle:8".into());

        assert_eq!(
            pane_with_token_for_terminal(
                std::slice::from_ref(&before),
                "stable-terminal",
                tower_cli::dockcmd::ACTIVE_TOKEN,
                &active_value,
            )
            .map(|pane| pane.pane_id.as_str()),
            Some("old-pane")
        );
        assert_eq!(
            pane_with_token_for_terminal(
                std::slice::from_ref(&after),
                "stable-terminal",
                tower_cli::dockcmd::ACTIVE_TOKEN,
                &active_value,
            )
            .map(|pane| pane.pane_id.as_str()),
            Some("moved-pane")
        );
        assert!(ready_pane_for_launch(
            std::slice::from_ref(&before),
            "stable-terminal",
            &active_value
        )
        .is_none());
        assert_eq!(
            ready_pane_for_launch(
                std::slice::from_ref(&after),
                "stable-terminal",
                &active_value,
            )
            .map(|pane| pane.pane_id.as_str()),
            Some("moved-pane"),
            "readiness requires the tower to clear its startup lease"
        );
        assert!(pane_with_token_for_terminal(
            &[pane(
                "moved-pane",
                &tower_cli::dockcmd::active_claim_value("stable-terminal", "another-launch"),
            )],
            "stable-terminal",
            tower_cli::dockcmd::ACTIVE_TOKEN,
            &active_value,
        )
        .is_none());
    }

    #[test]
    fn ambiguous_tab_creation_retries_each_transient_observation() {
        let request_label = tower_cli::dockcmd::tab_request_label("request-a");
        let before: Vec<herdrx::Pane> = serde_json::from_value(serde_json::json!([{
            "pane_id": "w9:p1",
            "tab_id": "w9:t1",
            "workspace_id": "w9",
            "terminal_id": "term-old"
        }]))
        .unwrap();
        let tabs: Vec<herdrx::Tab> = serde_json::from_value(serde_json::json!([{
            "tab_id": "w9:t2",
            "workspace_id": "w9",
            "label": request_label
        }]))
        .unwrap();
        let after: Vec<herdrx::Pane> = serde_json::from_value(serde_json::json!([
            {
                "pane_id": "w9:p1",
                "tab_id": "w9:t1",
                "workspace_id": "w9",
                "terminal_id": "term-old"
            },
            {
                "pane_id": "w9:p2",
                "tab_id": "w9:t2",
                "workspace_id": "w9",
                "terminal_id": "term-created"
            }
        ]))
        .unwrap();
        let mut tab_reads = 0;
        let mut pane_reads = 0;
        let mut pauses = 0;

        let found = observe_created_tab_until_stable(
            &before,
            "w9",
            || {
                tab_reads += 1;
                if tab_reads == 1 {
                    Err(herdrx::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "tab snapshot reply was lost",
                    )))
                } else {
                    Ok(tabs.clone())
                }
            },
            || {
                pane_reads += 1;
                if pane_reads == 1 {
                    Err(herdrx::Error::Decode(
                        serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
                    ))
                } else {
                    Ok(after.clone())
                }
            },
            || pauses += 1,
            &request_label,
        )
        .unwrap()
        .unwrap();

        assert_eq!(found.terminal_id, "term-created");
        assert_eq!(tab_reads, 2);
        assert_eq!(pane_reads, 2);
        assert_eq!(pauses, 2);
    }

    #[test]
    fn pre_fence_observation_retries_are_bounded() {
        let mut reads = 0;
        let mut pauses = 0;
        let error = retry_transient_dock_observation_bounded(
            || {
                reads += 1;
                Err::<(), _>(herdrx::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "server remains unavailable",
                )))
            },
            || pauses += 1,
        )
        .expect_err("persistent outage must return");

        assert!(matches!(error, herdrx::Error::Io(_)));
        assert_eq!(reads, 3);
        assert_eq!(pauses, 2);
    }

    #[test]
    fn published_workspace_is_never_cleanup_owned() {
        let request_label = tower_cli::dockcmd::workspace_request_label("100000000000-a");
        let unpublished: herdrx::Workspace = serde_json::from_value(serde_json::json!({
            "workspace_id": "opaque-owned",
            "label": request_label
        }))
        .unwrap();
        assert_eq!(
            created_workspace_cleanup_scope(&unpublished, &request_label),
            CreatedWorkspaceCleanupScope::Coordinate
        );

        let adopted: herdrx::Workspace = serde_json::from_value(serde_json::json!({
            "workspace_id": "opaque-published",
            "label": tower_cli::dockcmd::LABEL,
            "tokens": {
                tower_cli::dockcmd::COORDINATING_TOKEN: "held:4:200:other-client"
            }
        }))
        .unwrap();
        assert_eq!(
            created_workspace_cleanup_scope(&adopted, &request_label),
            CreatedWorkspaceCleanupScope::Adopted,
            "publishing the label ends exclusive create ownership"
        );

        let idle_public: herdrx::Workspace = serde_json::from_value(serde_json::json!({
            "workspace_id": "opaque-idle-public",
            "label": tower_cli::dockcmd::LABEL
        }))
        .unwrap();
        assert_eq!(
            created_workspace_cleanup_scope(&idle_public, &request_label),
            CreatedWorkspaceCleanupScope::Adopted,
            "foreground idleness is insufficient because background jobs are not observed"
        );
    }

    #[test]
    fn a_non_shell_foreground_process_blocks_candidate_adoption() {
        assert!(process_info_has_non_shell_foreground(
            &herdrx::ProcessInfo::default()
        ));
        let unobserved_shell: herdrx::ProcessInfo =
            serde_json::from_value(serde_json::json!({ "shell_pid": 41 })).unwrap();
        assert!(process_info_has_non_shell_foreground(&unobserved_shell));
        let idle: herdrx::ProcessInfo = serde_json::from_value(serde_json::json!({
            "shell_pid": 41,
            "foreground_processes": [{ "pid": 41, "name": "zsh" }]
        }))
        .unwrap();
        assert!(!process_info_has_non_shell_foreground(&idle));

        let vim: herdrx::ProcessInfo = serde_json::from_value(serde_json::json!({
            "shell_pid": 41,
            "foreground_processes": [{ "pid": 92, "name": "vim" }]
        }))
        .unwrap();
        assert!(process_info_has_non_shell_foreground(&vim));
    }

    #[test]
    fn an_ambiguous_coordination_release_is_read_back() {
        let workspace = |token: &str| {
            serde_json::from_value::<herdrx::Workspace>(serde_json::json!({
                "workspace_id": "opaque-dock",
                "label": tower_cli::dockcmd::LABEL,
                "tokens": { tower_cli::dockcmd::COORDINATING_TOKEN: token }
            }))
            .unwrap()
        };
        assert_eq!(
            observe_coordination_release(
                &[workspace("idle:6")],
                "opaque-dock",
                "held:5:900:owner",
                "idle:6",
            ),
            CoordinationReleaseObservation::Released
        );
        assert_eq!(
            observe_coordination_release(
                &[workspace("held:5:900:owner")],
                "opaque-dock",
                "held:5:900:owner",
                "idle:6",
            ),
            CoordinationReleaseObservation::Retry
        );
    }

    fn process(pid: i64, name: &str) -> herdrx::ForegroundProcess {
        herdrx::ForegroundProcess {
            pid,
            name: name.into(),
            argv0: name.into(),
            argv: vec![name.into()],
            cmdline: name.into(),
            cwd: String::new(),
        }
    }

    fn info(shell_pid: i64, foreground: Vec<herdrx::ForegroundProcess>) -> herdrx::ProcessInfo {
        herdrx::ProcessInfo {
            pane_id: "w1:p1".into(),
            shell_pid,
            foreground_process_group_id: shell_pid,
            foreground_processes: foreground,
        }
    }

    /// The one polling loop both waits use, asked directly.
    ///
    /// 🚨 Only its two wrappers were tested, and each of them fixes `done` — so
    /// the loop's own rules were reachable only through them: that an
    /// observation already `done` returns **without pausing**, that a pause
    /// falls only between observations, that the budget is checked *after*
    /// `done` (so the answer at the deadline is still the last observation, not
    /// an error), and that a hard failure ends it at once.
    #[test]
    fn the_polling_loop_pauses_between_observations_and_stops_on_the_budget() {
        let idle = || info(100, vec![process(100, "zsh")]);
        let starting = || info(100, vec![process(101, "path_helper"), process(100, "zsh")]);
        let is_idle = process_info_has_idle_foreground_shell;

        // Already done: one observation, no pause. A pause here is 100 ms a
        // `dock` invocation spends for nothing, on the path that runs whenever a
        // herdr restart leaves a shell behind.
        let mut reads = 0;
        let mut pauses = 0;
        let got = observe_until(
            || {
                reads += 1;
                Ok(idle())
            },
            is_idle,
            std::time::Duration::from_secs(60),
            || pauses += 1,
        )
        .expect("an observation");
        assert_eq!((reads, pauses), (1, 0));
        assert!(is_idle(&got), "the answer is the observation that was done");

        // Done on the third: a pause between each pair, and none after the last.
        let mut reads = 0;
        let mut pauses = 0;
        observe_until(
            || {
                reads += 1;
                Ok(if reads < 3 { starting() } else { idle() })
            },
            is_idle,
            std::time::Duration::from_secs(60),
            || pauses += 1,
        )
        .expect("an observation");
        assert_eq!((reads, pauses), (3, 2));

        // Never done, and no budget: one observation, and the answer is it. The
        // budget is read after `done`, so a caller asking for no wait still gets
        // an answer rather than an error.
        let mut reads = 0;
        let mut pauses = 0;
        let got = observe_until(
            || {
                reads += 1;
                Ok(starting())
            },
            is_idle,
            std::time::Duration::ZERO,
            || pauses += 1,
        )
        .expect("an observation");
        assert_eq!((reads, pauses), (1, 0));
        assert!(!is_idle(&got), "the last observation, done or not");
    }

    /// A failure that retrying cannot answer ends the loop, and one that it can
    /// is retried on the loop's own pause — the injected one.
    ///
    /// 🚨 That last part is the bug the loop's doc comment records: handing the
    /// hardcoded sleeper to the inner retry made a caller who asked for no
    /// back-off sleep anyway, and the tests that pass a panicking pause to prove
    /// "this path must not pause" were asserting against a closure nothing
    /// called. Counting the pauses here is what holds it.
    #[test]
    fn a_failed_observation_is_retried_on_the_loops_own_pause_or_ends_it() {
        let idle = || info(100, vec![process(100, "zsh")]);
        let is_idle = process_info_has_idle_foreground_shell;

        // A refused request is not transient: no retry, no pause, the error.
        let mut pauses = 0;
        let err = observe_until(
            || Err(herdrx::Error::Invalid("no such pane".into())),
            is_idle,
            std::time::Duration::from_secs(60),
            || pauses += 1,
        )
        .expect_err("a refusal ends it");
        assert!(matches!(err, herdrx::Error::Invalid(_)), "{err:?}");
        assert_eq!(pauses, 0);

        // A transient one is retried, on this pause, and the answer follows.
        let mut reads = 0;
        let mut pauses = 0;
        let got = observe_until(
            || {
                reads += 1;
                if reads == 1 {
                    Err(herdrx::Error::Io(std::io::Error::other("lost reply")))
                } else {
                    Ok(idle())
                }
            },
            is_idle,
            std::time::Duration::from_secs(60),
            || pauses += 1,
        )
        .expect("the retry answered");
        assert!(is_idle(&got));
        assert_eq!((reads, pauses), (2, 1), "the retry used the given pause");

        // A transient failure that keeps coming is bounded rather than endless.
        let mut reads = 0;
        let mut pauses = 0;
        let err = observe_until(
            || {
                reads += 1;
                Err::<herdrx::ProcessInfo, _>(herdrx::Error::Io(std::io::Error::other("gone")))
            },
            is_idle,
            std::time::Duration::from_secs(60),
            || pauses += 1,
        )
        .expect_err("a persistent outage ends it");
        assert!(matches!(err, herdrx::Error::Io(_)), "{err:?}");
        assert_eq!(reads, 3, "three deadlines, then the error");
        assert_eq!(pauses, 2, "a pause between the tries, none after the last");
    }

    /// Cross validation: the two named waits are the loop with their own `done`,
    /// so each must answer exactly what the loop does when asked the same way.
    /// A wrapper that drifted from the loop — a different budget, a `done` that
    /// is not the one it documents — would pass its own tests and still wait for
    /// the wrong thing.
    #[test]
    fn each_named_wait_is_the_loop_with_its_own_done() {
        let exe = Path::new("/Users/me/.local/bin/herdr-tower");
        let starting = || info(100, vec![process(101, "path_helper"), process(100, "zsh")]);
        let stages: [herdrx::ProcessInfo; 4] = [
            info(100, vec![process(100, "zsh")]),
            info(100, vec![process(102, "herdr-tower")]),
            herdrx::ProcessInfo::default(),
            info(100, vec![process(103, "vim")]),
        ];
        for settled in stages {
            // The shell wait: idle, and nothing else.
            let mut reads = 0;
            let by_hand = observe_until(
                || {
                    reads += 1;
                    Ok(if reads < 2 {
                        starting()
                    } else {
                        settled.clone()
                    })
                },
                process_info_has_idle_foreground_shell,
                std::time::Duration::ZERO,
                || {},
            );
            let mut reads = 0;
            let by_name = observe_settled_shell(
                || {
                    reads += 1;
                    Ok(if reads < 2 {
                        starting()
                    } else {
                        settled.clone()
                    })
                },
                std::time::Duration::ZERO,
                || {},
            );
            assert_eq!(
                by_hand.map(|i| i.foreground_processes.len()).ok(),
                by_name.map(|i| i.foreground_processes.len()).ok(),
                "the shell wait is the loop: {settled:?}"
            );

            // The restored-shell wait: idle, or a tower, or unobservable.
            //
            // 🚨 A budget of milliseconds, not the minute the caller uses. A
            // stage this wait does *not* accept — a pane running `vim` — is
            // waited on until the budget runs out, and with a no-op pause that
            // is a busy loop for as long as the budget says. Written with the
            // real 60 s first, which made this one test take a minute.
            let mut counted = 0;
            settle_restored_shell(
                || {
                    counted += 1;
                    Ok(if counted < 2 {
                        starting()
                    } else {
                        settled.clone()
                    })
                },
                exe,
                std::time::Duration::from_millis(20),
                || std::thread::sleep(std::time::Duration::from_millis(1)),
            );
            let wanted = process_info_has_idle_foreground_shell(&settled)
                || tower_cli::dockcmd::is_tower_process(&settled, exe)
                || process_info_is_unobservable(&settled);
            if wanted {
                assert_eq!(
                    counted, 2,
                    "the wait went past the observation it accepts: {settled:?}"
                );
            } else {
                assert!(
                    counted > 2,
                    "the wait stopped at something it does not accept: {settled:?}"
                );
            }
        }
    }

    /// herdr answered but showed nothing: no shell and no foreground. 🚨 Not the
    /// same as an error — the pane is there, its processes are not visible — and
    /// a shell of *any* kind, or a foreground of any kind, makes it observable.
    #[test]
    fn a_pane_with_no_shell_and_no_foreground_is_unobservable() {
        assert!(process_info_is_unobservable(&herdrx::ProcessInfo::default()));
        assert!(!process_info_is_unobservable(&info(
            100,
            vec![process(100, "zsh")]
        )));
        assert!(
            !process_info_is_unobservable(&info(100, vec![])),
            "a shell with nothing in front of it is still a shell"
        );
        assert!(
            !process_info_is_unobservable(&info(0, vec![process(9, "vim")])),
            "a foreground with no shell pid is still something to see"
        );
    }

    /// The wait for a restored shell ends as soon as there is something to
    /// plan from — an idle shell, or a tower another `dock` put there — and a
    /// failed observation ends it too, since the caller looks again anyway.
    /// Only a shell still in its startup files is waited on, and never past
    /// the budget.
    #[test]
    fn a_restored_shell_is_waited_on_until_there_is_something_to_plan_from() {
        let exe = Path::new("/Users/me/.local/bin/herdr-tower");
        let starting = || info(100, vec![process(101, "path_helper"), process(100, "zsh")]);
        let idle = || info(100, vec![process(100, "zsh")]);
        let tower = || info(100, vec![process(102, "herdr-tower")]);
        let unobservable = herdrx::ProcessInfo::default;
        for (settled, why) in [
            (idle(), "idle"),
            (tower(), "a tower"),
            (unobservable(), "an unobservable pane"),
        ] {
            let mut reads = 0;
            settle_restored_shell(
                || {
                    reads += 1;
                    Ok(if reads < 3 {
                        starting()
                    } else {
                        settled.clone()
                    })
                },
                exe,
                std::time::Duration::from_secs(60),
                || {},
            );
            assert_eq!(reads, 3, "stops at the first {why} reading");
        }

        let mut reads = 0;
        settle_restored_shell(
            || {
                reads += 1;
                Err(herdrx::Error::Invalid("gone".into()))
            },
            exe,
            std::time::Duration::from_secs(60),
            || {},
        );
        assert_eq!(reads, 1, "a failed observation ends the wait");

        let mut reads = 0;
        settle_restored_shell(
            || {
                reads += 1;
                Ok(starting())
            },
            exe,
            std::time::Duration::ZERO,
            || {},
        );
        assert_eq!(reads, 1, "an empty budget reads once and stops");
    }

    /// 🚨 The empty tab beside the dock. A login zsh runs `path_helper` out of
    /// `/etc/zprofile`, so a pane that is seconds old has a foreground process
    /// that is not the shell. Reading that as "taken" made the dock abandon the
    /// workspace it had itself just created.
    #[test]
    fn a_shell_still_running_its_startup_files_is_waited_for() {
        let busy = || info(100, vec![process(101, "path_helper"), process(100, "zsh")]);
        let idle = || info(100, vec![process(100, "zsh")]);
        assert!(!process_info_has_idle_foreground_shell(&busy()));

        let mut reads = 0;
        let observed = observe_settled_shell(
            || {
                reads += 1;
                Ok(if reads < 3 { busy() } else { idle() })
            },
            std::time::Duration::from_secs(5),
            || {},
        )
        .expect("observed");
        assert!(
            process_info_has_idle_foreground_shell(&observed),
            "it waited for the startup files to finish"
        );
        assert_eq!(reads, 3);
    }

    /// The wait is bounded. A pane genuinely occupied must not hold the dock.
    #[test]
    fn a_shell_that_never_settles_is_given_up_on() {
        let mut reads = 0;
        let observed = observe_settled_shell(
            || {
                reads += 1;
                Ok(info(100, vec![process(101, "vim"), process(100, "zsh")]))
            },
            std::time::Duration::from_millis(0),
            || panic!("a spent budget must not pause again"),
        )
        .expect("observed");
        assert!(!process_info_has_idle_foreground_shell(&observed));
        assert_eq!(reads, 1, "asked once, then the deadline decided");
    }

    /// 🚨 The retry path uses the caller's pause. It was handed the hardcoded
    /// 100ms sleeper instead, so a caller asking for no back-off still slept on
    /// every transient failure — and a test passing a panicking pause to prove
    /// "this must not pause" was asserting against a closure nothing called.
    /// `Io` is the transient kind, so this one actually takes that path.
    #[test]
    fn a_transient_failure_waits_on_the_callers_pause() {
        let mut reads = 0;
        let mut pauses = 0;
        let started = std::time::Instant::now();
        let observed = observe_settled_shell(
            || {
                reads += 1;
                if reads == 1 {
                    Err(herdrx::Error::Io(std::io::Error::other("socket blip")))
                } else {
                    Ok(info(100, vec![process(100, "zsh")]))
                }
            },
            std::time::Duration::from_secs(5),
            || pauses += 1,
        )
        .expect("observed after one retry");
        assert!(process_info_has_idle_foreground_shell(&observed));
        assert_eq!(pauses, 1, "the retry paused through the injected closure");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(80),
            "and never through the real 100ms sleeper"
        );
    }

    /// The realistic way a server "cannot answer": transient `Io` errors, one
    /// after another, until the retry budget is spent. That has to end as
    /// `None` too — the case above uses a non-transient error, which returns
    /// before any retry, and so would not notice a change that mapped an
    /// exhausted budget to `Some(false)`.
    #[test]
    fn a_spent_retry_budget_is_still_not_a_no() {
        let mut reads = 0;
        let mut pauses = 0;
        let got = observed_tower_on(
            || -> herdrx::Result<herdrx::ProcessInfo> {
                reads += 1;
                Err(herdrx::Error::Io(std::io::Error::other("socket blip")))
            },
            "/usr/local/bin/herdr-tower",
            || pauses += 1,
        );
        assert_eq!(got, None, "an exhausted budget is an unanswered question");
        assert!(reads > 1, "it did retry ({reads} reads)");
        assert_eq!(
            pauses,
            reads - 1,
            "pausing between tries, through the caller's pause"
        );
    }

    /// 🚨 An observation that could not be made is `None`, never `Some(false)`.
    /// The caller rounds `None` up to "the token decides", which is the only
    /// safe reading of an unanswered question; a `false` there would let a
    /// launch through on the strength of a server that never replied.
    #[test]
    fn an_unanswered_observation_is_not_a_no() {
        let exe = "/usr/local/bin/herdr-tower";
        let cases: [(&str, herdrx::Result<herdrx::ProcessInfo>, Option<bool>); 4] = [
            (
                "a tower in the pane",
                Ok(info(100, vec![process(200, "herdr-tower")])),
                Some(true),
            ),
            (
                "an idle shell",
                Ok(info(100, vec![process(100, "zsh")])),
                Some(false),
            ),
            (
                "herdr-tower with arguments is not the tower",
                Ok(herdrx::ProcessInfo {
                    foreground_processes: vec![herdrx::ForegroundProcess {
                        argv: vec!["herdr-tower".into(), "status".into()],
                        ..process(200, "herdr-tower")
                    }],
                    ..info(100, vec![])
                }),
                Some(false),
            ),
            (
                "a server that cannot answer",
                Err(herdrx::Error::NoSocket),
                None,
            ),
        ];
        for (name, reply, want) in cases {
            let mut reply = Some(reply);
            let got = observed_tower_on(
                || reply.take().expect("asked once"),
                exe,
                || panic!("a non-transient answer must not pause"),
            );
            assert_eq!(got, want, "{name}");
        }
    }

    /// An error is the caller's to classify, so it comes back rather than being
    /// retried into the budget.
    #[test]
    fn an_error_is_returned_rather_than_waited_out() {
        let observed = observe_settled_shell(
            || Err(herdrx::Error::NoSocket),
            std::time::Duration::from_secs(5),
            || panic!("an error must not pause"),
        );
        assert!(observed.is_err());
    }
}
