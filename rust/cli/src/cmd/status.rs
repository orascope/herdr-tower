// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Status command handler.

use std::collections::HashMap;
use time::OffsetDateTime;

use super::common::*;
use super::Result;

pub fn run_status(args: &[String]) -> Result<()> {
    // Parsed rather than scanned for one exact string: a mistyped `--jsonn` used
    // to be ignored, so a script asking for JSON got the table back with a zero
    // exit status. The flag set refuses it, and prints its defaults before
    // returning — `diagnostic` is that block, so the reader learns what the
    // flag should have been.
    let fs = tower_cli::helpcmd::FlagSet::new("status")
        .bool("json", false, "print JSON")
        .bool("line", false, "print one summary line, for herdr's tab bar");
    let parsed = flags_only("status", &fs, args)?;
    let json_out = parsed.bool("json");
    let store = store()?;
    if parsed.bool("line") {
        if json_out {
            return Err("status: --line and --json are separate outputs; pick one".into());
        }
        // herdr runs this every few seconds, so it only reads the records.
        // No herdr call and no pane sync: the resident supervisor keeps the
        // records current, and a closed pane shows up at its next pass.
        // Parallax's handoffs the same way: the supervisor's snapshot, never
        // Parallax itself, and only while the snapshot is fresh.
        let parallax = tower_state::ParallaxStore::new(store.dir()).read();
        let waiting =
            tower_state::parallax_waiting(parallax.as_ref(), OffsetDateTime::now_utc()).len();
        tower_cli::outln!("{}", tower_cli::status_line_with(&store.list()?, waiting))?;
        return Ok(());
    }
    let now = OffsetDateTime::now_utc();
    sync_panes(&store, now)?;

    // The bot marks are per session and cost a few file stats to resolve. The
    // JSON form carries the records themselves and has no column to put a mark
    // in, so it does not pay for them.
    let wired = if json_out {
        HashMap::new()
    } else {
        let agents = store.list()?;
        let paths =
            tower_wiring::Paths::detect().unwrap_or_else(|| tower_wiring::Paths::for_home("."));
        tower_ui::wired_sessions(&agents, &paths, &tower_config::load().channel_labels)
    };
    let mut out = std::io::stdout().lock();
    tower_cli::status(&mut out, &store, json_out, now, &wired)?;
    if !json_out {
        let parallax = tower_state::ParallaxStore::new(store.dir()).read();
        tower_cli::parallax_section(
            &mut out,
            &tower_state::parallax_waiting(parallax.as_ref(), now),
        )?;
    }
    Ok(())
}
