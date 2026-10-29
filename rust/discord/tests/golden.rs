// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The card `herdr-tower card --out` publishes, pinned byte for byte.
//!
//! `tower_discord`'s own tests call [`build_card`] directly with hand-built
//! inputs. This one is a different path on purpose: it pins what `herdr-tower
//! card --out` prints, so it goes through the same wiring, the store listing,
//! and the encoder the binary really uses.
//!
//! A library-level test cannot catch a card assembled from the wrong inputs.
//! This can, because the only thing reproduced here is the wiring — everything
//! else comes off disk.
//!
//! The payload compared, the store it is rendered from and the clock all live
//! in `expected/`. See `expected/README.md`.

use std::collections::HashMap;
use time::OffsetDateTime;
use tower_discord::{build_card, encode_sorted_json, CardData};
use tower_state::{Agent, Store};
use tower_usage::DefaultModels;

/// Where the expected payload and its inputs live.
///
/// The payload is an authored snapshot of this implementation, and
/// `UPDATE_EXPECTED=1 cargo test` rewrites it.
const EXPECTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../expected");

/// The instant the card was rendered.
///
/// One second after `NOW`, because `card --out` was a separate process run. It
/// is not guessed: it was read back out of the `<t:…:R>` stamp in the payload
/// itself, which is the clock at the moment the card was built.
fn card_now() -> OffsetDateTime {
    let raw = std::fs::read_to_string(format!("{EXPECTED}/card.NOW")).expect("card.NOW");
    OffsetDateTime::parse(raw.trim(), &time::format_description::well_known::Rfc3339)
        .expect("card.NOW is RFC 3339")
}

fn fixture_agents() -> Vec<Agent> {
    let d = tempfile::tempdir().unwrap();
    let agents = d.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    for e in std::fs::read_dir(format!("{EXPECTED}/fixture/agents")).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), agents.join(e.file_name())).unwrap();
    }
    Store::new(d.path()).unwrap().list().unwrap()
}

/// The inputs `publish_card` had during the capture.
///
/// Every one of them is what an empty `HOME=/Users/fixture` produces, which is
/// the point of capturing under it: no worktree metas, so no branches; no bot
/// wiring. What is left is the agent section alone, which is the part that
/// reads from the store and therefore the part a wiring mistake would corrupt.
fn card_data() -> CardData {
    CardData {
        agents: fixture_agents(),
        ctx: HashMap::new(),
        wired: HashMap::new(),
        branches: HashMap::new(),
        // `Some`, not `None`: `DefaultModels::read` returns a value even with
        // nothing configured, and the recorded payload carries the resulting
        // "auto · auto · auto" line. `None` would drop the line entirely.
        def_models: Some(DefaultModels::default()),
        home: "/Users/fixture".into(),
    }
}

#[test]
fn the_card_matches_the_recorded_payload() {
    let got = encode_sorted_json(&build_card(&card_data(), card_now())).expect("encode");
    let path = format!("{EXPECTED}/card.json");
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        std::fs::write(&path, &got).expect("write expected");
        return;
    }
    let want = std::fs::read(&path).expect("expected card");

    // Compared as raw bytes first, because the trailing newline is part of the
    // output: `encode_sorted_json` writes one and `card --out` is piped into
    // `jq` and into files. Papering over it here is how the missing newline
    // survived — the line-by-line loop below cannot see it at all.
    let want = String::from_utf8(want).expect("expected card is UTF-8");
    let got = String::from_utf8(got).expect("output is UTF-8");
    assert_eq!(
        got.ends_with('\n'),
        want.ends_with('\n'),
        "the trailing newline differs from the recorded payload"
    );

    for (i, (a, b)) in got.lines().zip(want.lines()).enumerate() {
        assert_eq!(
            a,
            b,
            "line {} differs from the recorded payload\n  got:  {a}\n  want: {b}",
            i + 1
        );
    }
    assert_eq!(
        got.lines().count(),
        want.lines().count(),
        "line count differs"
    );
}
