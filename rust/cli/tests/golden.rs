// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What `herdr-tower status` prints, pinned byte for byte.
//!
//! The text compared is in `expected/`: `status.txt` and `status.json`,
//! recorded from this implementation and rendered from the inputs beside them —
//! `expected/fixture/` as the store and `expected/NOW` as the clock. See
//! `expected/README.md` for what may be done to a recorded snapshot.
//!
//! A diff here means the rendering moved. `UPDATE_EXPECTED=1` is how a
//! deliberate change is recorded, and the diff is to be read like any other.
//!
//! The fixture is not a sample; every row is a case that once rendered wrong
//! somewhere:
//!
//! - Two `claude` WAITING rows whose only difference is `state_since`. Their
//!   order is the bug this harness was built to catch — the listing sorted
//!   newest-first and buried the agent that had been blocked for five hours.
//! - CJK text long enough to truncate, which is where a renderer that counts
//!   `char`s instead of display columns tears the table apart.
//! - `~` alone, `~/path`, a path outside `$HOME`, and Cyrillic.
//! - All six states, including the `?` suffix a record with no fresh hook gets.
//! - An unknown kind (`aider`), which has to land in the fallback bucket
//!   after the supervised vendors rather than sorting alphabetically ahead of
//!   them.

use std::collections::HashMap;
use time::OffsetDateTime;
use tower_cli::status;
use tower_state::Store;

/// Where the *expected* text lives, along with the fixture and clock it is
/// rendered from.
///
/// These are authored snapshots of this implementation, and
/// `UPDATE_EXPECTED=1 cargo test` rewrites them — a deliberate act, to be read
/// in the diff like any other change.
const EXPECTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../expected");

/// The recorded clock. Every `SINCE` cell is relative to it, so the snapshot
/// is only reproducible with this exact value.
fn pinned_now() -> OffsetDateTime {
    let raw = std::fs::read_to_string(format!("{EXPECTED}/NOW")).expect("NOW");
    OffsetDateTime::parse(raw.trim(), &time::format_description::well_known::Rfc3339)
        .expect("NOW is RFC 3339")
}

/// Copies the fixture somewhere writable.
///
/// `Store::new` creates directories, and a test has no business creating them
/// inside the source tree — a fixture that a test run can modify is not a
/// fixture.
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
/// variable at call time, so the `~` rows only reproduce under it.
///
/// Set per test rather than once, because integration tests share a process and
/// run in parallel; every test in this file wants the same value, so repeating
/// the write is harmless where coordinating would not be.
fn use_fixture_home() {
    std::env::set_var("HOME", "/Users/fixture");
}

fn render(json_out: bool) -> String {
    use_fixture_home();
    let (_d, s) = fixture_store();
    let mut buf = Vec::new();
    status(&mut buf, &s, json_out, pinned_now(), &HashMap::new()).unwrap();
    String::from_utf8(buf).expect("output is UTF-8")
}

/// Line-by-line, so a failure names the row that moved instead of dumping two
/// tables and leaving the reader to find the difference.
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
}

#[test]
fn status_renders_exactly_the_recorded_output() {
    assert_matches_golden(&render(false), "status.txt");
}

#[test]
fn status_json_matches_the_recording_byte_for_byte() {
    assert_matches_golden(&render(true), "status.json");
}

/// The ordering case, asserted directly as well as through the snapshot.
///
/// Reading it out of the table would depend on the column layout; this pins the
/// property itself, so it survives a future change to how rows are drawn.
#[test]
fn the_agent_blocked_longest_is_the_first_row() {
    let (_d, s) = fixture_store();
    let ids: Vec<String> = s.list().unwrap().into_iter().map(|a| a.id).collect();
    let (a, b) = (
        ids.iter().position(|i| i == "claude-w2:pA").unwrap(),
        ids.iter().position(|i| i == "claude-w2:pB").unwrap(),
    );
    assert!(
        a < b,
        "pA has been WAITING five hours and pB ninety seconds; pA belongs first, got {ids:?}"
    );
}

/// An unknown vendor sorts after the three known ones, not among them.
///
/// Inside one workspace, which is where vendor order lives now: the outer group
/// is the workspace, so a vendor no longer forms one run down the whole table.
/// The fixture's records are folded into a single workspace and a single state
/// first, leaving the vendor as the only thing left to order them by.
#[test]
fn an_unknown_kind_lands_in_the_fallback_bucket() {
    use tower_state::AgentState;
    let (_d, s) = fixture_store();
    for mut a in s.list().unwrap() {
        a.pane.session = "one".into();
        a.state = AgentState::Idle;
        a.state_since = pinned_now();
        s.save(&a).unwrap();
    }
    let kinds: Vec<String> = s.list().unwrap().into_iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("aider"),
        "'aider' sorts alphabetically before every supervised kind, so a rank \
         that fell through to a string compare would put it first: {kinds:?}"
    );
}

/// `status` is read in a terminal and over ssh, with no detail view behind it
/// and no key to press — so a pending question takes the TASK cell even from a
/// pane somebody renamed, because it is the one thing a reader there cannot get
/// anywhere else.
#[test]
fn a_renamed_pane_that_is_waiting_still_says_what_it_wants() {
    use tower_state::AgentState;
    use_fixture_home();
    let (_d, store) = fixture_store();
    let mut a = store.list().unwrap().into_iter().next().unwrap();
    a.label = "dbeaver".into();
    a.ask = "may I drop the column?".into();
    a.state = AgentState::Waiting;
    store.save(&a).unwrap();

    let table = |s: &tower_state::Store| {
        let mut buf = Vec::new();
        status(&mut buf, s, false, pinned_now(), &HashMap::new()).unwrap();
        String::from_utf8(buf).unwrap()
    };

    let out = table(&store);
    assert!(out.contains("may I drop the column?"), "{out}");

    // With nothing being asked, the name a person chose is what the cell says.
    a.ask.clear();
    store.save(&a).unwrap();
    let out = table(&store);
    assert!(out.contains("dbeaver"), "{out}");
}
