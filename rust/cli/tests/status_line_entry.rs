// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `status --line` through the real binary: what herdr's tab bar runs every
//! few seconds. It reads the records and nothing else, so it answers with no
//! herdr server at all.

use std::path::Path;
use std::process::{Command, Stdio};

use time::OffsetDateTime;
use tower_state::{Agent, AgentState, Store};

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-tower"))
        .args(args)
        .env("HOME", dir)
        .env("HERDR_TOWER_STATE_DIR", dir.join("state"))
        .env("HERDR_TOWER_CONFIG", dir.join("config.json"))
        .env("HERDR_SOCKET_PATH", dir.join("no-herdr.sock"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs")
}

#[test]
fn the_line_counts_the_records_without_a_herdr_server() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["status", "--line"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "\n",
        "no agents, no entry"
    );

    let store = Store::new(dir.path().join("state")).unwrap();
    let now = OffsetDateTime::now_utc();
    for (id, state) in [
        ("a", AgentState::Waiting),
        ("b", AgentState::Working),
        ("c", AgentState::Waiting),
    ] {
        let mut a = Agent::new(id, "claude", now);
        a.transition(state, now);
        store.save(&a).unwrap();
    }
    let out = run(dir.path(), &["status", "--line"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "🗼 2 wait · 1 working\n"
    );
}

#[test]
fn line_and_json_together_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["status", "--line", "--json"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--line and --json"));
}
