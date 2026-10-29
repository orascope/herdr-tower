// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The watch, against a running herdr server.
//!
//! It runs only inside herdr and skips otherwise — a pass against a server that
//! is not there would mean nothing. Nothing here moves the user's view
//! (`focus`), writes to an agent (`prompt`), or creates a workspace or tab: a
//! test must not shake the screen somebody is working on.

use std::time::{Duration, Instant};

use herdrx::Herdr;
use tower_mux::{Watch, COALESCE_WINDOW};

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

/// The claim this design rests on: against a real server, signals never arrive
/// closer together than the coalescing window.
///
/// A live session is the only place the claim can be checked, because the
/// firehose is real — `pane.updated` fires on every revision of every pane, and
/// an agent at work produces several per second. A quiet session yields no
/// signals and the gap assertion is vacuous; what still gets checked then is
/// that the watch opens, blocks, and is stoppable.
#[test]
fn signals_never_arrive_closer_together_than_the_window() {
    let Some(h) = herdr_or_skip() else { return };
    let w = Watch::open(&h).expect("watch");

    // The reader below blocks until something changes, which on a quiet session
    // is never. The stopper is what guarantees this test ends.
    let stopper = w.stopper();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        stopper.stop();
    });

    let started = Instant::now();
    let mut signals = 0usize;
    while w.next_change() {
        signals += 1;
    }
    let elapsed = started.elapsed();

    // Counted, not timed pairwise. The gap between two `next_change` *returns* is
    // the reader's gap, not the stream's: this thread can be descheduled, and two
    // properly folded signals then come back to back. Under a busy session that
    // failed the assertion for a property that had not been violated. What the
    // window actually promises is a rate, and a rate is what a count over a known
    // interval measures.
    //
    // The allowance is generous on purpose: one signal can already be in flight
    // when the watch opens, and the last window can close early. A firehose
    // without folding is hundreds of `pane.updated` in two seconds, so this still
    // fails loudly for the thing it is watching for.
    let ceiling = (elapsed.as_millis() / COALESCE_WINDOW.as_millis()) as usize + 3;
    eprintln!("{signals} signals in {elapsed:?} (ceiling {ceiling})");
    assert!(
        signals <= ceiling,
        "{signals} signals in {elapsed:?} — the burst was not folded"
    );
}

/// Opening a watch must not disturb anything: it subscribes and takes one
/// listing, and the panes it reports are the same ones a plain listing does.
#[test]
fn a_live_watch_opens_without_moving_anything() {
    let Some(h) = herdr_or_skip() else { return };
    let before = h.list_panes().expect("list").len();

    let w = Watch::open(&h).expect("watch");
    drop(w);

    assert_eq!(h.list_panes().expect("list").len(), before);
    assert!(
        h.focused_workspace().expect("workspaces").is_some(),
        "the focused workspace must still be focused"
    );
}
