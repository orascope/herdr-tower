// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Change signals a redraw loop can live with.
//!
//! herdr pushes `pane.updated` every time a pane record changes — several per
//! second while an agent is working, because the revision counter moves with
//! the screen. Redrawing on each one would be worse than the polling it
//! replaces. So the first signal is taken immediately (that is the
//! responsiveness worth having) and everything arriving in the window right
//! behind it is folded into that same signal.
//!
//! A signal says *that* something changed, never what. The control tower
//! re-reads its own store anyway, and an event payload would be a second source
//! of truth to keep in sync with the first.

use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use herdrx::{Closer, Herdr, Stream};

/// How long one signal absorbs the events behind it.
///
/// Long enough that a working agent's burst collapses to one redraw, short
/// enough that a person cannot see the delay. Measured against a live server:
/// signals never arrive closer together than this.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(200);

/// An open watch on the backend's changes.
///
/// Reading is the only thing this type does, and it is not `Sync`, so exactly
/// one thread can be waiting on it — a second reader would silently steal
/// signals from the first. Ending a watch is the separate [`Stopper`], which
/// *is* shareable, because the thread that decides to stop is generally not the
/// one parked in [`Watch::next_change`].
///
/// Dropping it closes the subscription and joins the reader, so there is no
/// explicit close to forget and no socket leaked per forgotten call.
pub struct Watch {
    rx: mpsc::Receiver<()>,
    stopper: Stopper,
    reader: Option<JoinHandle<()>>,
}

/// Why a timed watch returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Changed,
    Timeout,
    Ended,
}

/// Ends a watch from anywhere.
///
/// Cloneable and `Send`, so a signal handler, a UI teardown, or a timeout can
/// hold one while another thread blocks in [`Watch::next_change`]. Stopping
/// shuts the subscription's socket down under the reader, which is what wakes
/// it: a quiet session pushes nothing, and a reader waiting for an event that
/// will never come has no other way out.
#[derive(Debug, Clone)]
pub struct Stopper(Arc<Closer>);

impl Stopper {
    /// Ends the watch. Safe to call more than once, and from any thread.
    pub fn stop(&self) {
        let _ = self.0.close();
    }
}

impl Watch {
    /// Opens a watch on pane changes.
    ///
    /// The baseline listing `watch_panes` returns is deliberately discarded.
    /// The caller re-reads its own store when a signal arrives, and two paths
    /// carrying the same facts is just somewhere for them to disagree. Opening
    /// the subscription *before* the snapshot is still the point of going
    /// through `watch_panes` — since herdr 0.9.0 a subscription replays no
    /// history, so anything that happened before it opened is gone for good.
    pub fn open(h: &Herdr) -> herdrx::Result<Watch> {
        let (stream, _baseline) = h.watch_panes()?;
        let stopper = Stopper(Arc::new(stream.closer()?));

        // Capacity one, and a send that gives up when it is full: "something
        // changed" only needs to be true once. An unbounded queue would let a
        // busy agent build a backlog of redraws the caller then has to work
        // through long after the changes stopped.
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = std::thread::Builder::new()
            .name("tower_mux-watch".into())
            .spawn(move || pump(stream, tx))?;

        Ok(Watch {
            rx,
            stopper,
            reader: Some(reader),
        })
    }

    /// Waits for the next change.
    ///
    /// Returns `false` when the stream has ended — the caller's cue to fall
    /// back to polling rather than to sit there believing nothing is happening.
    /// It never returns sooner than [`COALESCE_WINDOW`] after a change, which
    /// is the whole reason this type exists.
    #[must_use = "false means the stream ended; a caller that ignores it stops seeing changes"]
    pub fn next_change(&self) -> bool {
        matches!(self.next_change_within(None), Wait::Changed)
    }

    /// Waits for a change, a deadline, or the subscription ending.
    ///
    /// The tower uses the unbounded form above; a scheduler has its own next
    /// deadline and must be able to wake for it even when every pane is quiet.
    pub fn wait(&self, timeout: Duration) -> Wait {
        self.next_change_within(Some(timeout))
    }

    fn next_change_within(&self, timeout: Option<Duration>) -> Wait {
        let first = match timeout {
            Some(t) => self.rx.recv_timeout(t),
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match first {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return Wait::Timeout,
            Err(RecvTimeoutError::Disconnected) => return Wait::Ended,
        }
        // Absorb the burst behind the first signal. The window is measured from
        // that first signal and is never reopened, so a continuously working
        // agent cannot postpone the redraw indefinitely.
        let deadline = Instant::now() + COALESCE_WINDOW;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => return Wait::Changed,
                // The stream ended mid-window. Deliver this change anyway; the
                // next call reports the end. A disconnect here cannot jump the
                // queue: everything already sent is drained first, so no change
                // is lost at shutdown.
                Err(RecvTimeoutError::Disconnected) => return Wait::Changed,
            }
        }
    }

    /// A handle that can end this watch from another thread.
    pub fn stopper(&self) -> Stopper {
        self.stopper.clone()
    }

    /// Ends the watch and waits for the reader to finish.
    ///
    /// Calling it is optional — dropping does the same — but an explicit close
    /// is worth having where the shutdown order matters, such as a TUI that
    /// must not leave a socket open behind a program that has already exited.
    pub fn close(&mut self) {
        self.stopper.stop();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.close();
    }
}

/// Reads the stream forever, turning every event into at most one pending
/// signal.
///
/// It runs on its own thread because the read is a blocking syscall and the
/// caller has a screen to keep drawing. It ends when the stream does — which a
/// [`Stopper`] can cause on purpose — or when the receiver goes away.
fn pump(mut stream: Stream, tx: SyncSender<()>) {
    while stream.next_event().is_ok() {
        match tx.try_send(()) {
            // A signal is already pending: nothing to add, since one is all the
            // caller can act on.
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => return,
        }
    }
}
