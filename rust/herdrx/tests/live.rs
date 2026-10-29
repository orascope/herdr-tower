// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Live checks against a running herdr server.
//!
//! They run only inside herdr and skip otherwise — a pass against a server that
//! is not there would mean nothing. Calls that move the user's view (`focus`)
//! or poke an agent (`prompt`) are deliberately excluded: a test must not shake
//! the user's screen.

use herdrx::{Herdr, ReadSource};

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

#[test]
fn ping_reports_a_compatible_server() {
    let Some(h) = herdr_or_skip() else { return };
    let info = h.ping().expect("ping");
    assert_eq!(info.r#type, "pong");
    assert!(!info.version.is_empty(), "version missing: {info:?}");
    if !info.compatible() {
        eprintln!(
            "note: server protocol {}, client {}",
            info.protocol,
            herdrx::PROTOCOL
        );
    }
}

#[test]
fn lists_panes_including_our_own() {
    let Some(h) = herdr_or_skip() else { return };
    let panes = h.list_panes().expect("pane.list");
    assert!(!panes.is_empty(), "no panes at all");

    let Some(self_id) = herdrx::current_pane_id() else {
        return;
    };
    assert!(
        panes.iter().any(|p| p.pane_id == self_id),
        "own pane {self_id} missing from the listing"
    );

    // The classification a bare multiplexer cannot do: kind and state arrive
    // with the listing rather than being guessed from a process name.
    let agents: Vec<_> = panes.iter().filter(|p| p.has_agent()).collect();
    for a in &agents {
        assert!(a.agent.as_deref().is_some_and(|k| !k.is_empty()));
        assert!(!a.cwd.is_empty(), "agent pane without a cwd: {a:?}");
    }
    eprintln!("{} panes, {} hosting an agent", panes.len(), agents.len());
}

#[test]
fn snapshot_roots_are_live_panes() {
    let Some(h) = herdr_or_skip() else { return };
    let snapshot = h.pane_snapshot().expect("session.snapshot");
    assert!(!snapshot.panes.is_empty(), "snapshot has no panes");
    assert!(!snapshot.tab_roots.is_empty(), "snapshot has no tab roots");
    for root in &snapshot.tab_roots {
        assert!(
            snapshot.panes.iter().any(|pane| &pane.pane_id == root),
            "layout root {root} is absent from the pane snapshot"
        );
    }
}

#[test]
fn reads_our_own_screen() {
    let Some(h) = herdr_or_skip() else { return };
    let Some(self_id) = herdrx::current_pane_id() else {
        return;
    };

    let text = h.capture_pane(&self_id, 3).expect("capture");
    assert!(!text.trim().is_empty(), "own screen came back empty");

    // All four sources coincide for an alternate-screen agent pane; asking for
    // one that is not `visible` must still succeed.
    let recent = h
        .read(&self_id, ReadSource::RecentUnwrapped, 3)
        .expect("recent read");
    assert!(!recent.is_empty());
}

#[test]
fn pane_get_matches_the_listing() {
    let Some(h) = herdr_or_skip() else { return };
    let Some(self_id) = herdrx::current_pane_id() else {
        return;
    };

    let one = h.pane(&self_id).expect("pane.get");
    assert_eq!(one.pane_id, self_id);
    assert!(!one.tab_id.is_empty(), "pane without a tab: {one:?}");
}

/// A structured error must arrive as one, so callers can branch on the code.
#[test]
fn missing_pane_reports_a_code() {
    let Some(h) = herdr_or_skip() else { return };
    let err = h.pane("w9:pZZ").expect_err("should not exist");
    assert!(err.is_code("pane_not_found"), "unexpected error: {err}");
}

/// A subscription really opens and is acknowledged. When an event arrives
/// depends on what the session is doing, so this does not wait for one.
#[test]
fn a_subscription_opens_against_the_real_server() {
    let Some(h) = herdr_or_skip() else { return };
    let (stream, panes) = h.watch_panes().expect("watch");
    assert!(!panes.is_empty(), "the baseline snapshot is empty");
    stream.close().expect("close");
}

/// The sidebar write, against the real schema.
///
/// The token name is deliberately outside [`herdrx::ALL_TOKENS`] and outside
/// anything the sidebar config renders, so a running control tower's own
/// tokens are untouched and nothing appears on the user's screen. The TTL is
/// the safety net: if this test dies before clearing, herdr expires it.
#[test]
fn metadata_round_trips_under_our_own_source() {
    let Some(h) = herdr_or_skip() else { return };
    let Some(self_id) = herdrx::current_pane_id() else {
        return;
    };

    h.report_metadata(&herdrx::Metadata {
        pane_id: self_id.clone(),
        tokens: [("altest".to_string(), "probe".to_string())]
            .into_iter()
            .collect(),
        ttl: Some(std::time::Duration::from_secs(5)),
        ..herdrx::Metadata::default()
    })
    .expect("report_metadata");

    let pane = h.pane(&self_id).expect("pane.get");
    assert_eq!(pane.tokens.get("altest").map(String::as_str), Some("probe"));

    h.clear_tokens(&self_id, &["altest"]).expect("clear");
    let pane = h.pane(&self_id).expect("pane.get");
    assert!(
        !pane.tokens.contains_key("altest"),
        "an empty value must delete the token, not plant a blank one"
    );
}
