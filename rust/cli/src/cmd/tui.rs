// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! TUI launch command and pane claim / coordination handling.

use std::path::Path;

use super::common::*;
use super::Result;

pub fn run_tui() -> Result<()> {
    // A tower in a pane claims it, so `dock` can tell a running tower from the
    // shell it leaves behind. A popup has no pane and claims nothing. A tower
    // opened by hand can tolerate losing that hint; one injected by `dock`
    // cannot, because an unlocked retry marker would make its live TUI look
    // like a reusable shell.
    let _claims = claim_pane();
    let docked = tower_cli::dockcmd::is_dock_launch(|key| std::env::var(key).ok());
    // The process itself is visible through herdr's host-side observer and the
    // local claim is only a fast path. A socket-only client
    // may have launched this pane across a mount namespace where no local lock
    // path is shared, so absence of that optional claim is safe only when the
    // server confirms the process instead.
    if docked {
        match &_claims {
            tower_cli::dockcmd::LocalClaim::Contended => {
                return Err("another tower already owns this terminal's local claim".into());
            }
            tower_cli::dockcmd::LocalClaim::Unavailable if !server_observes_own_tower() => {
                return Err(
                    "a dock-launched tower could not establish either a local claim or a server-observed process"
                        .into(),
                );
            }
            tower_cli::dockcmd::LocalClaim::Acquired(_)
            | tower_cli::dockcmd::LocalClaim::Unavailable => {}
        }
    }
    let active_claim = if docked {
        let claim = establish_dock_active()?;
        if let Err(error) =
            clear_dock_starting(&claim.terminal_id, &claim.expected, claim.generation)
        {
            clear_dock_active(&claim.terminal_id, &claim.expected, claim.generation)?;
            return Err(error.into());
        }
        Some(claim)
    } else {
        // 🚨 A tower opened by hand in a pane that still carries a dock's
        // active claim has to disown it. The claim is bound to the terminal,
        // not to a process, so a dock tower killed abruptly leaves one behind
        // — and the supervisor, seeing both that claim and a live tower
        // process, would take this person's workspace for the dock and
        // suppress the restart of the one that actually died. The claim
        // belonged to a previous life of this terminal; this is not it.
        disown_stale_dock_active();
        None
    };
    // The usage board, under the header and on `u` — the tower is where it
    // is shown, not a tab of its own.
    let usage: Option<std::sync::Arc<dyn tower_ui::Usage>> = super::common::home().ok().map(|h| {
        std::sync::Arc::new(tower_cli::usagetui::TowerUsage::new(
            h,
            tower_state::default_dir(),
            tower_state::local_offset(),
        )) as std::sync::Arc<dyn tower_ui::Usage>
    });
    let ui_result = tower_ui::main(
        None,
        tower_cli::versioncmd::version_label(&tower_cli::versioncmd::VersionInfo::stamped()),
        usage,
    );
    let active_clear_result = active_claim
        .map(|claim| clear_dock_active(&claim.terminal_id, &claim.expected, claim.generation))
        .transpose();
    ui_result?;
    active_clear_result?;
    Ok(())
}

/// Establishes a non-expiring server-visible lifetime claim before the dock
/// tower clears its startup lease. A suspended process retains this marker;
/// an abrupt exit also fails closed by leaving the shell non-reusable.
struct DockActiveClaim {
    terminal_id: String,
    expected: String,
    generation: u64,
}

fn dock_terminal_identity(h: &herdrx::Herdr) -> Option<String> {
    let supplied = std::env::var(tower_cli::dockcmd::DOCK_TERMINAL_ENV)
        .ok()
        .filter(|terminal| !terminal.is_empty());
    if tower_cli::dockcmd::is_dock_launch(|key| std::env::var(key).ok()) {
        return supplied;
    }
    supplied.or_else(|| {
        let pane = tower_cli::dockcmd::own_pane(|key| std::env::var(key).ok())?;
        h.pane(&pane)
            .ok()
            .map(|pane| pane.terminal_id)
            .filter(|terminal| !terminal.is_empty())
    })
}

fn establish_dock_active() -> Result<DockActiveClaim> {
    use tower_cli::dockcmd;
    let expected = std::env::var(dockcmd::DOCK_LEASE_ENV)
        .map_err(|_| "a dock-launched tower is missing its startup lease")?;
    let generation = dockcmd::pane_starting_lease_generation(&expected)
        .ok_or("a dock-launched tower has an invalid startup lease")?;
    let h = tower_cli::muxsel::default_mux();
    let terminal_id = dock_terminal_identity(&h)
        .ok_or("a dock-launched tower has no stable Herdr terminal identity")?;
    let active_value = dockcmd::active_claim_value(&terminal_id, &expected);
    for attempt in 0..2 {
        let panes =
            retry_transient_dock_observation_bounded(|| h.list_panes(), dock_observation_pause)?;
        let pane_id = pane_for_terminal(&panes, &terminal_id)
            .ok_or("the dock tower's terminal disappeared during initialization")?
            .pane_id
            .clone();
        let metadata = dockcmd::active_metadata(&pane_id, &terminal_id, &expected, generation);
        let uncertain_error = match h.report_metadata(&metadata) {
            Ok(()) => None,
            Err(error) if dockcmd::delivery_is_uncertain(&error) => Some(error),
            Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {
                if attempt == 0 {
                    continue;
                }
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        let current = match retry_transient_dock_observation_bounded(
            || h.list_panes(),
            dock_observation_pause,
        ) {
            Ok(current) => current,
            Err(error) => {
                // The active write above may have been applied even when its
                // bounded confirmation read failed. Remove that non-expiring
                // claim before initialization is allowed to return an error.
                clear_dock_active_value(&h, &active_value, generation)?;
                return Err(error.into());
            }
        };
        let misplaced: Vec<String> = current
            .iter()
            .filter(|pane| {
                pane.terminal_id != terminal_id
                    && pane.tokens.get(dockcmd::ACTIVE_TOKEN) == Some(&active_value)
            })
            .map(|pane| pane.pane_id.clone())
            .collect();
        if !misplaced.is_empty() {
            clear_dock_active_value(&h, &active_value, generation)?;
            if attempt == 0 {
                continue;
            }
            return Err("the dock tower moved repeatedly during its lifetime claim".into());
        }
        if pane_with_token_for_terminal(
            &current,
            &terminal_id,
            dockcmd::ACTIVE_TOKEN,
            &active_value,
        )
        .is_some()
        {
            return Ok(DockActiveClaim {
                terminal_id,
                expected: active_value,
                generation,
            });
        }
        if attempt == 0 {
            continue;
        }
        if let Some(error) = uncertain_error {
            return Err(error.into());
        }
        return Err("Herdr did not retain the dock tower lifetime claim".into());
    }
    unreachable!("two active-claim writes either return or continue once")
}

/// Removes one exact active value from a pane coordinate that was recycled
/// between terminal lookup and metadata application. The value is terminal
/// bound, so this never clears another tower's claim.
fn clear_dock_active_from_pane(
    h: &herdrx::Herdr,
    pane_id: &str,
    expected: &str,
    generation: u64,
) -> herdrx::Result<()> {
    use tower_cli::dockcmd;
    loop {
        let pane =
            match retry_transient_dock_observation(|| h.pane(pane_id), dock_observation_pause) {
                Ok(pane) => pane,
                Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {
                    return Ok(())
                }
                Err(error) => return Err(error),
            };
        if pane.tokens.get(dockcmd::ACTIVE_TOKEN).map(String::as_str) != Some(expected) {
            return Ok(());
        }
        let clear = dockcmd::clear_active_metadata(pane_id, generation);
        match h.report_metadata(&clear) {
            Ok(()) => {}
            Err(error) if dockcmd::delivery_is_uncertain(&error) => {}
            Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {}
            Err(error) => return Err(error),
        }
        dock_observation_pause();
    }
}

fn clear_dock_active_value(
    h: &herdrx::Herdr,
    expected: &str,
    generation: u64,
) -> herdrx::Result<()> {
    loop {
        let panes = retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)?;
        let matching: Vec<String> = panes
            .iter()
            .filter(|pane| {
                pane.tokens
                    .get(tower_cli::dockcmd::ACTIVE_TOKEN)
                    .map(String::as_str)
                    == Some(expected)
            })
            .map(|pane| pane.pane_id.clone())
            .collect();
        if matching.is_empty() {
            return Ok(());
        }
        for pane_id in matching {
            clear_dock_active_from_pane(h, &pane_id, expected, generation)?;
        }
    }
}

/// Clears a lifetime claim only after the tower UI has returned, and confirms
/// the ordered removal before allowing this process to exit. Pane IDs change
/// when a terminal moves between tabs, so every attempt resolves the stable
/// terminal identity again.
fn clear_dock_active(terminal_id: &str, expected: &str, generation: u64) -> herdrx::Result<()> {
    use tower_cli::dockcmd;
    let h = tower_cli::muxsel::default_mux();
    loop {
        let panes = retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)?;
        let Some(pane) = pane_for_terminal(&panes, terminal_id) else {
            return Ok(());
        };
        if pane.tokens.get(dockcmd::ACTIVE_TOKEN).map(String::as_str) != Some(expected) {
            return Ok(());
        }
        let clear = dockcmd::clear_active_metadata(&pane.pane_id, generation);
        match h.report_metadata(&clear) {
            Ok(()) => {}
            Err(error) if dockcmd::delivery_is_uncertain(&error) => {}
            Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {}
            Err(error) => return Err(error),
        }
        dock_observation_pause();
    }
}

/// Clears the short server-side lease planted before `dock` submitted us.
///
/// The cleared startup token is the tower's readiness handshake: a launcher
/// requires both this ordered idle state and the active token before it reports
/// success.
fn clear_dock_starting(terminal_id: &str, expected: &str, generation: u64) -> herdrx::Result<()> {
    use tower_cli::dockcmd;
    let h = tower_cli::muxsel::default_mux();
    loop {
        let panes = retry_transient_dock_observation(|| h.list_panes(), dock_observation_pause)?;
        let Some(current) = pane_for_terminal(&panes, terminal_id) else {
            return Ok(());
        };
        if current
            .tokens
            .get(dockcmd::STARTING_TOKEN)
            .map(String::as_str)
            != Some(expected)
        {
            return Ok(());
        }
        let clear = dockcmd::clear_starting_lease(&current.pane_id, generation);
        match h.report_metadata(&clear) {
            Ok(()) => {}
            Err(error) if dockcmd::delivery_is_uncertain(&error) => {}
            Err(error) if error.is_code("pane_not_found") || error.is_code("not_found") => {}
            Err(error) => return Err(error),
        }
        dock_observation_pause();
    }
}

/// Claims the herdr pane this tower runs in, so `dock` can tell a running tower
/// from the shell it leaves behind. See `tower_cli::dockcmd`.
///
/// Keyed by the pane's *terminal*, which costs one call to the server before
/// the screen opens: pane IDs are handed on to later panes, terminal IDs are
/// not.
fn claim_pane() -> tower_cli::dockcmd::LocalClaim {
    use tower_cli::dockcmd;
    let Some(socket) = herdrx::default_socket_path() else {
        return dockcmd::LocalClaim::Unavailable;
    };
    let h = tower_cli::muxsel::default_mux();
    let Some(terminal) = dock_terminal_identity(&h) else {
        return dockcmd::LocalClaim::Unavailable;
    };
    let Ok(selected) = std::path::absolute(tower_state::default_dir()) else {
        return dockcmd::LocalClaim::Unavailable;
    };
    let dirs = match tower_state::coordination_dir()
        .ok()
        .and_then(|root| std::path::absolute(root).ok())
    {
        Some(root) => dockcmd::server_dirs(&root, &selected, &socket),
        None => dockcmd::server_dirs_without_runtime(&selected, &socket),
    };
    dockcmd::hold_available_in(&dirs, &terminal)
}

/// Whether the server itself confirms this dock-launched tower's process, so the
/// missing local claim does not matter.
fn server_observes_own_tower() -> bool {
    use tower_cli::dockcmd;
    let h = tower_cli::muxsel::default_mux();
    let Some(terminal) = dock_terminal_identity(&h) else {
        return false;
    };
    let executable = self_path();
    if executable.is_empty() {
        return false;
    }
    let Ok(panes) =
        retry_transient_dock_observation_bounded(|| h.list_panes(), dock_observation_pause)
    else {
        return false;
    };
    let Some(pane) = pane_for_terminal(&panes, &terminal) else {
        return false;
    };
    retry_transient_dock_observation_bounded(
        || h.process_info(&pane.pane_id),
        dock_observation_pause,
    )
    .is_ok_and(|info| dockcmd::is_tower_process(&info, Path::new(&executable)))
}

/// Clears a `dock-active` claim left on this pane by a tower that is gone.
///
/// Best effort in every direction: without a pane, a server, or a claim there
/// is nothing to do, and a failure leaves the situation exactly as it was.
fn disown_stale_dock_active() {
    let Some(pane_id) = herdrx::current_pane_id() else {
        return;
    };
    let h = tower_cli::muxsel::default_mux();
    let Ok(pane) = h.pane(&pane_id) else {
        return;
    };
    if !tower_cli::dockcmd::pane_has_bound_active(&pane) {
        return;
    }
    // 🚨 From the active claim itself, which is what has to be superseded.
    // The high-water token next to it reads `<generation>:<owner>@@terminal=…`
    // and the lease parser wants `held:<generation>:<owner>`, so asking it
    // about the wrong one silently answers zero — and a clear written at
    // sequence 1 cannot displace a claim made at the real generation, which
    // starts at 2. The disown would look like it worked and change nothing.
    let generation = pane
        .tokens
        .get(tower_cli::dockcmd::ACTIVE_TOKEN)
        .and_then(|value| tower_cli::dockcmd::pane_starting_lease_generation(value))
        .unwrap_or_default();
    let _ = h.report_metadata(&tower_cli::dockcmd::clear_active_metadata(
        &pane.pane_id,
        generation,
    ));
}
