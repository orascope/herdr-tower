// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `restore`'s occupancy check, against a running herdr server.
//!
//! It runs only inside herdr and skips otherwise: a pass against a server that
//! is not there would mean nothing. Everything here is a read — `pane.list` and
//! `workspace.list`. Nothing moves the view, writes to an agent, or creates a
//! workspace or a tab, because a test must not shake the screen somebody is
//! working on, and `restore` in particular is a command whose whole job is
//! making things.
//!
//! The property under test is the one that cannot be faked. `restore` refuses to
//! act on a slot that already holds an agent, and herdr restores its own layout
//! across a reboot — so after exactly the event `restore` exists for, the
//! workspaces and tabs are back and full of empty shells. If a shell counted as
//! occupancy, `restore` would skip every session it was written to bring back
//! and print nothing but skip reasons. That needs a real server with real panes
//! in it, some of them shells.

use std::collections::HashSet;

use herdrx::Herdr;
use tower_cli::restorecmd::occupied_slots;

fn herdr_or_skip() -> Option<Herdr> {
    if !herdrx::inside() {
        eprintln!("skip: not inside herdr");
        return None;
    }
    let h = Herdr::default();
    match h.ping() {
        Ok(_) => Some(h),
        Err(e) => {
            eprintln!("skip: no herdr server: {e}");
            None
        }
    }
}

/// A shell pane is not an occupied slot.
///
/// Asserted two ways, because the counting alone would pass on a session that
/// happens to have no shells: the slot count can never exceed the number of
/// panes with an agent, and a folder reachable *only* through a shell pane must
/// have no slot at all.
#[test]
fn an_empty_shell_pane_does_not_look_like_an_occupied_slot() {
    let Some(h) = herdr_or_skip() else { return };
    let panes = h.list_panes().expect("pane.list");
    let labels = h.workspace_labels().unwrap_or_default();
    let slots = occupied_slots(&h);

    let session_of = |workspace_id: &String| {
        labels
            .get(workspace_id)
            .cloned()
            .unwrap_or_else(|| workspace_id.clone())
    };

    let with_agent: HashSet<String> = panes
        .iter()
        .filter(|p| p.has_agent())
        .map(|p| format!("{}|{}", session_of(&p.workspace_id), p.cwd))
        .collect();

    // Two agents in one folder collapse to one slot, so this is a bound rather
    // than an equality.
    assert!(
        slots.len() <= with_agent.len(),
        "{} slots from {} agent panes — a shell got counted",
        slots.len(),
        with_agent.len()
    );

    let mut shells = 0;
    for p in panes.iter().filter(|p| !p.has_agent()) {
        shells += 1;
        let key = format!("{}|{}", session_of(&p.workspace_id), p.cwd);
        if with_agent.contains(&key) {
            continue; // an agent lives in that folder too, so the slot is real
        }
        // The stored key has its symlinks resolved, so compare on the folder
        // rather than on the raw string.
        let canon = std::fs::canonicalize(&p.cwd)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.cwd.clone());
        let occupied = format!("{}|{canon}", session_of(&p.workspace_id));
        assert!(
            !slots.contains(&occupied),
            "shell pane {} made {occupied:?} look occupied — restore would skip \
             every session it exists to bring back",
            p.pane_id
        );
    }
    eprintln!(
        "{} panes: {} with an agent, {shells} shells, {} slots",
        panes.len(),
        with_agent.len(),
        slots.len()
    );
}

/// With no server there is nothing occupied, and the answer is empty rather than
/// a failure.
///
/// That is not a corner case: it is what a reboot looks like, and it is the
/// state `restore` is normally run in. A version that errored here would refuse
/// to work at exactly the moment it is needed.
#[test]
fn an_unreachable_server_reports_nothing_occupied_rather_than_failing() {
    let h = Herdr::at("/nonexistent/tower_cli-restore-live/herdr.sock");
    assert!(occupied_slots(&h).is_empty());
}
