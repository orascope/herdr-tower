// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What a watch must do to a firehose, and how it ends.
// The fake herdr server in `common` listens on a Unix socket.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use tower_mux::{Wait, Watch, COALESCE_WINDOW};

/// The reason this type exists.
///
/// herdr pushes `pane.updated` on every pane revision — several per second
/// while an agent works. Handing each one to a redraw loop is worse than the
/// polling it replaces, so a burst has to collapse into a single signal, and
/// two signals can never arrive closer together than the window.
///
/// The second assertion is the load-bearing one: if the burst leaked through as
/// many signals, the second `next_change` would return `true` at once instead
/// of waiting for the stop.
#[test]
fn a_burst_of_revisions_becomes_one_signal() {
    let f = Fake::new(standard);
    let w = Watch::open(&f.herdr()).expect("watch");

    for _ in 0..20 {
        f.push(PANE_UPDATED);
    }

    let start = Instant::now();
    assert!(w.next_change(), "the first change never arrived");
    assert!(
        start.elapsed() >= COALESCE_WINDOW,
        "returned before the coalescing window closed, so a burst would redraw {} times",
        20
    );

    // Nothing more is pushed. A second signal would mean the burst leaked.
    let stopper = w.stopper();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        stopper.stop();
    });
    let start = Instant::now();
    assert!(!w.next_change(), "the burst produced more than one signal");
    assert!(
        start.elapsed() >= Duration::from_millis(140),
        "the second signal did not wait"
    );
}

/// A change arriving after the window is a separate signal, not swallowed by
/// the previous one. A watch that folds forever stops reporting.
#[test]
fn a_later_change_is_a_new_signal() {
    let f = Fake::new(standard);
    let w = Watch::open(&f.herdr()).expect("watch");

    f.push(PANE_UPDATED);
    assert!(w.next_change());

    f.push(PANE_UPDATED);
    assert!(w.next_change(), "a change after the window was swallowed");
}

#[test]
fn a_scheduler_can_wake_on_its_own_deadline() {
    let f = Fake::new(standard);
    let w = Watch::open(&f.herdr()).expect("watch");
    let start = Instant::now();
    assert_eq!(w.wait(Duration::from_millis(40)), Wait::Timeout);
    assert!(start.elapsed() >= Duration::from_millis(35));
}

/// The thread that decides to stop is not the one parked in `next_change` — in
/// the TUI it is the teardown path, after the program has already exited. If
/// stopping could not reach across, a quiet session would be unstoppable.
#[test]
fn a_stopper_wakes_a_waiting_reader() {
    let f = Fake::new(standard);
    let w = Watch::open(&f.herdr()).expect("watch");
    let stopper = w.stopper();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        stopper.stop();
    });

    assert!(!w.next_change(), "a stopped watch reported a change");
    // And it stays ended, rather than blocking forever on the next call.
    assert!(!w.next_change());
}

/// The end must not eat the change that was already in hand. Picking the
/// shutdown while a signal sat buffered would lose it; draining first cannot.
#[test]
fn the_last_change_is_delivered_before_the_end() {
    let f = Fake::new(standard);
    let w = Watch::open(&f.herdr()).expect("watch");

    f.push(PANE_UPDATED);
    // Give the reader thread time to take the event off the socket, then end
    // the stream while the signal is pending.
    std::thread::sleep(Duration::from_millis(50));
    w.stopper().stop();

    assert!(w.next_change(), "the pending change was lost at shutdown");
    assert!(!w.next_change(), "the end was not reported afterwards");
}

/// Forgetting to close must not leak a socket per watch. Dropping has to close
/// it, and it has to return rather than wait forever on a reader parked in a
/// blocking read.
#[test]
fn dropping_a_watch_ends_the_subscription() {
    let f = Fake::new(standard);
    let w = Watch::open(&f.herdr()).expect("watch");
    let stopper = w.stopper();

    let start = Instant::now();
    drop(w);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "drop waited on a blocked reader"
    );

    // Stopping an already-closed watch is harmless.
    stopper.stop();
}

/// The baseline listing is deliberately discarded: the caller re-reads its own
/// store, and two paths carrying the same facts is somewhere for them to
/// disagree. Opening the subscription before the snapshot is still the point,
/// since herdr replays no history.
#[test]
fn opening_a_watch_subscribes_before_it_snapshots() {
    let f = Fake::new(standard);
    let w = Watch::open(&f.herdr()).expect("watch");
    assert_eq!(f.methods(), vec!["events.subscribe", "pane.list"]);
    drop(w);
}
