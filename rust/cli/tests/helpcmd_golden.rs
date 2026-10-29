// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Three help outputs, pinned byte for byte.
//!
//! `herdr-tower help`, `herdr-tower wt` with no subcommand, and
//! `herdr-tower restore --help`, each compared against a recorded snapshot in
//! `expected/`, because nothing else answers "did this change what the user
//! reads?".
//!
//! What makes `restore-help.txt` worth pinning is the *shape*: the flag set's
//! defaults block down to its `\n    \t` indentation, and the trailing
//! `herdr-tower: flag: help requested` line, both halves, because both halves
//! are what a user sees.
//!
//! A diff means the text moved. `UPDATE_EXPECTED=1` records a deliberate change,
//! to be read in the diff like any other.

use tower_cli::helpcmd::{help_text, FlagError};
use tower_cli::restorecmd::{parse_restore_args, restore_flag_set};
use tower_cli::wtcmd::WT_USAGE;

/// Where the *expected* text lives.
///
/// These are authored snapshots of this implementation, and
/// `UPDATE_EXPECTED=1 cargo test` rewrites them — a deliberate act, to be read
/// in the diff like any other change.
const EXPECTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../expected");

/// Line by line, so a failure names the line that moved instead of dumping two
/// screens of text and leaving the reader to find the difference.
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
    // `lines()` cannot see a missing or extra trailing newline, and a help text
    // that runs into the shell prompt is exactly the kind of change nothing else
    // here would catch.
    assert_eq!(
        got.as_bytes(),
        want.as_bytes(),
        "the text matches line for line but the bytes do not — check the trailing newline"
    );
}

#[test]
fn help_prints_exactly_the_recorded_output() {
    assert_matches_golden(&help_text(), "help.txt");
}

#[test]
fn wt_with_no_subcommand_prints_the_recorded_help() {
    assert_matches_golden(WT_USAGE, "wt-help.txt");
}

/// Both halves of what `restore --help` puts on the terminal: the flag set's
/// own defaults block, and the error line `main` prints afterwards.
///
/// Composed here rather than run through `run_restore`, because the second half
/// is not the library's to print — `main` writes `herdr-tower: <error>` — and a
/// test that needed a record store and a backend to check a help string would be
/// asserting the wrong thing.
#[test]
fn restore_help_prints_exactly_the_recorded_help() {
    let err = parse_restore_args(&["--help"]).expect_err("--help is a refusal");
    assert_eq!(err, FlagError::Help);
    let got = format!(
        "{}herdr-tower: {err}\n",
        restore_flag_set().diagnostic(&err)
    );
    assert_matches_golden(&got, "restore-help.txt");
}

/// `-h` and `-help` reach the same place as `--help`, and scripts use all
/// three.
#[test]
fn every_spelling_of_help_reaches_the_same_output() {
    let block = restore_flag_set().diagnostic(&FlagError::Help);
    for arg in ["-h", "-help", "--help"] {
        let err = parse_restore_args(&[arg]).expect_err(arg);
        assert_eq!(err, FlagError::Help, "{arg}");
        assert_eq!(restore_flag_set().diagnostic(&err), block, "{arg}");
    }
}
