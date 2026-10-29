// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The remote MCP server's tool contract, pinned byte for byte.
//!
//! `tools/list` is what a model on somebody's phone reads to decide what to
//! call and how. A changed name, schema, description or hint changes what that
//! model does, so it has to show up in a pull request as a diff in
//! `expected/mcp-tools.json`. `UPDATE_EXPECTED=1` records a deliberate change.

use tower_cli::mcp::{oauth, prompts, tools};

const EXPECTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../expected/mcp-tools.json");

#[test]
fn the_tool_list_matches_the_recorded_contract() {
    let every_scope = [oauth::SCOPE_READ.to_string(), oauth::SCOPE_ACT.to_string()];
    let got = serde_json::to_string_pretty(&tools::list(&every_scope)).unwrap() + "\n";
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        std::fs::write(EXPECTED, &got).expect("write expected");
        return;
    }
    let want = std::fs::read_to_string(EXPECTED).expect("expected/mcp-tools.json");
    for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
        assert_eq!(g, w, "line {} of the tool list moved", i + 1);
    }
    assert_eq!(
        got.lines().count(),
        want.lines().count(),
        "line count differs"
    );
}

const EXPECTED_PROMPTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../expected/mcp-prompts.json");

/// `prompts/list` is the other half of what a client shows its user before
/// anything runs: the ready-made requests and the arguments they ask for.
#[test]
fn the_prompt_list_matches_the_recorded_contract() {
    let every_scope = [oauth::SCOPE_READ.to_string(), oauth::SCOPE_ACT.to_string()];
    let got = serde_json::to_string_pretty(&prompts::list(&every_scope)).unwrap() + "\n";
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        std::fs::write(EXPECTED_PROMPTS, &got).expect("write expected");
        return;
    }
    let want = std::fs::read_to_string(EXPECTED_PROMPTS).expect("expected/mcp-prompts.json");
    assert_eq!(got, want, "the prompt list moved");
}
