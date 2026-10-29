// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What `herdr-tower info` prints, pinned byte for byte.
//!
//! The text compared is `expected/info.txt`: the card for five records out of
//! `expected/fixture`, recorded from this implementation against the clock in
//! `expected/info.NOW`. See `expected/README.md`.
//!
//! A diff means the card moved. `UPDATE_EXPECTED=1` records a deliberate change.
//!
//! The five are not a sample. Each is a shape the card renders differently:
//!
//! - `claude-w2:pA` — WAITING with a pending question, so the `asking` line
//!   appears and the headline is the question rather than the task.
//! - `codex-w2:pF` — nothing but the essentials, which is what a record with no
//!   wiring, no model and no task has to look like.
//! - `gemini-w2:pI` — DEAD, and days old, so both age buckets are in day units.
//! - `claude-w2:pD` — WORKING with no fresh hook, which is what earns the `?`
//!   on the state, plus a path long enough that any truncation would show.
//! - `eta` — looked up by *session name* rather than record ID, and the folder
//!   is Cyrillic.
//!
//! The last stanza is the failure mode: `herdr-tower info` with no argument. It
//! is part of the snapshot because the usage line is output too.

use std::collections::HashMap;

use time::OffsetDateTime;
use tower_cli::infocmd::{find_agent, render_info, InfoData, INFO_USAGE};
use tower_state::Store;
use tower_wiring::Info as Wiring;

/// Where the *expected* text lives, along with the fixture and clock it is
/// rendered from.
///
/// These are authored snapshots of this implementation, and
/// `UPDATE_EXPECTED=1 cargo test` rewrites them — a deliberate act, to be read
/// in the diff like any other change.
const EXPECTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../expected");

/// The recorded clock. Every age on the card is relative to it, so the
/// snapshot only reproduces against this exact value.
fn pinned_now() -> OffsetDateTime {
    let raw = std::fs::read_to_string(format!("{EXPECTED}/info.NOW")).expect("info.NOW");
    OffsetDateTime::parse(raw.trim(), &time::format_description::well_known::Rfc3339)
        .expect("info.NOW is RFC 3339")
}

/// Copies the fixture somewhere writable.
///
/// `Store::new` creates directories, and a test has no business creating them in
/// the source tree — a fixture a test run can modify is not a fixture.
fn fixture_store() -> (tempfile::TempDir, Store) {
    let d = tempfile::tempdir().unwrap();
    let agents = d.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    for e in std::fs::read_dir(format!("{EXPECTED}/fixture/agents")).unwrap() {
        let e = e.unwrap();
        // Under the name the store gives the record's ID, so a save in the
        // test replaces the copy rather than landing beside it. The files in
        // the tree carry no `:`, which Windows refuses in a name.
        let body = std::fs::read(e.path()).unwrap();
        let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        std::fs::write(
            agents.join(format!("{}.json", tower_os::file_name(&id))),
            body,
        )
        .unwrap();
    }
    let s = Store::new(d.path()).unwrap();
    (d, s)
}

/// The snapshot was recorded with this `HOME`, and `shorten_home` reads the
/// variable at call time, so the `~` paths only reproduce under it.
///
/// Set per test rather than once, because integration tests share a process and
/// run in parallel; every test in this file and in `golden.rs` wants the same
/// value, so repeating the write is harmless where coordinating would not be.
fn use_fixture_home() {
    std::env::set_var("HOME", "/Users/fixture");
}

/// Line by line, so a failure names the line that moved instead of dumping two
/// screens of Korean.
fn assert_matches_golden(got: &str, golden_file: &str) {
    let path = format!("{EXPECTED}/{golden_file}");
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        std::fs::write(&path, got).expect("write expected");
        return;
    }
    let want = std::fs::read_to_string(&path).expect("expected");
    let (g, w): (Vec<_>, Vec<_>) = (got.lines().collect(), want.lines().collect());

    for (i, (a, b)) in g.iter().zip(w.iter()).enumerate() {
        assert_eq!(
            a,
            b,
            "line {} differs from the recorded output\n  got:  {a:?}\n  want: {b:?}",
            i + 1
        );
    }
    assert_eq!(
        g.len(),
        w.len(),
        "line count differs: got {} vs want {}",
        g.len(),
        w.len()
    );
    assert_eq!(
        got.as_bytes(),
        want.as_bytes(),
        "the text matches line for line but the bytes do not — check the trailing newline"
    );
}

/// The snapshot is rendered from a fixture store with no bots, no Discord state
/// and no startup units, so the wiring and context are empty here too. That is
/// what puts `not connected` and `manual` on every card in the snapshot.
#[test]
fn info_prints_exactly_the_recorded_output() {
    use_fixture_home();
    let (_d, store) = fixture_store();
    let agents = store.list().unwrap();
    let now = pinned_now();
    let labels = HashMap::new();
    let wiring = Wiring::default();

    let mut out = String::new();
    for key in [
        "claude-w2:pA",
        "codex-w2:pF",
        "gemini-w2:pI",
        "claude-w2:pD",
        "eta",
    ] {
        let a = find_agent(&agents, key).unwrap_or_else(|| panic!("no fixture record for {key}"));
        out.push_str(&format!("### herdr-tower info {key}\n"));
        out.push_str(&render_info(
            &InfoData {
                agent: a,
                wiring: &wiring,
                ctx: None,
                branch: None,
                labels: &labels,
                // The platform the snapshot was recorded on. Read from the host
                // instead, the card said "systemd unit" on Linux and this whole
                // comparison failed there — on the one line where the two
                // platforms differ, and on no CI runner. The word itself has its
                // own test in `tower_ui`; this one is about the card.
                unit: tower_cli::infocmd::unit_word_for("macos"),
            },
            now,
        ));
        out.push('\n');
    }
    out.push_str("### herdr-tower info (no argument)\n");
    out.push_str(&format!("herdr-tower: {INFO_USAGE}\n"));

    assert_matches_golden(&out, "info.txt");
}

/// `eta` is in the snapshot as a session name, not a record ID, which is the whole
/// reason it is one of the five: a lookup that only matched IDs would find
/// nothing and the stanza would be empty rather than wrong.
#[test]
fn a_record_is_reachable_by_its_session_name_as_well_as_its_id() {
    let (_d, store) = fixture_store();
    let agents = store.list().unwrap();
    let by_name = find_agent(&agents, "eta").expect("session name");
    let by_id = find_agent(&agents, &by_name.id).expect("record id");
    assert_eq!(by_name.id, by_id.id);
}
