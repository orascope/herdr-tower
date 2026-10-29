// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Folding a burst of publish triggers into one flight.
//!
//! Every hook transition wants the card refreshed. Four agents finishing inside
//! the same second means four publishers, four `PATCH`es, and a rate limit — so
//! the triggers are collapsed instead: a flag file says "something changed" and
//! a lock says "somebody is already dealing with it".
//!
//! The order matters and is the whole design. **The flag is written before the
//! lock is attempted.** A caller that cannot take the lock has already left
//! evidence, and the holder re-checks the flag after each publish, so the work
//! is normally done by whoever is already running rather than dropped. Writing
//! the flag only after failing to lock would widen that hole enormously.
//!
//! It does not close it. There is a window between the holder's last look at the
//! flag and the moment it releases the lock: a caller that writes the flag and
//! fails to lock inside that window is told "somebody has it" by a holder that
//! has already decided to stop. Demonstrated, not theorised — a run with a hook
//! in exactly that spot left the flag on disk with nobody publishing it. Kept
//! as-is, because the damage is bounded: the flag survives, so the next trigger
//! — a hook transition, or the five-minute timer — takes the lock, sees it, and
//! publishes. The card is one cycle stale, never wrong.

use std::fs;
use std::io;
use std::path::Path;

use crate::open_private;

/// Publishes the card, coalescing concurrent triggers.
///
/// Returns as soon as another publisher holds the lock — its job now. Otherwise
/// it publishes until the flag stops reappearing, which mops up every trigger
/// that arrived while it was busy.
///
/// A publish error stops the loop and propagates. The flag has already been
/// consumed at that point, so the failed refresh is not retried until the next
/// trigger. That is the right trade — a publisher that retried on its own would
/// hammer a webhook that is rate-limiting it.
pub fn run_coalesced<F>(dir: &Path, mut publish: F) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
{
    let dirty = dir.join("card.dirty");
    fs::write(&dirty, [])?;

    let lock = open_private(&dir.join("card.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        // Somebody else is publishing. The flag we just wrote is what tells them
        // to go around again.
        Err(fs::TryLockError::WouldBlock) => return Ok(()),
        Err(fs::TryLockError::Error(e)) => return Err(e),
    }
    // The lock is released when `lock` is dropped, on every path out of here
    // including a panic — `flock` goes away with the file description.

    loop {
        if fs::metadata(&dirty).is_err() {
            return Ok(());
        }
        // Cleared *before* publishing, so a trigger that arrives during the
        // publish sets the flag again and earns another pass. Clearing it after
        // would swallow that trigger and leave the card a refresh behind.
        if let Err(e) = fs::remove_file(&dirty) {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(e);
            }
        }
        publish()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn dirty(dir: &Path) -> bool {
        dir.join("card.dirty").exists()
    }

    #[test]
    fn a_lone_caller_publishes_exactly_once() {
        let d = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        run_coalesced(d.path(), || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(!dirty(d.path()), "the flag is consumed");
    }

    /// The second caller does no work and does not wait. It leaves the flag,
    /// which is the holder's instruction to publish again.
    #[test]
    fn a_second_caller_leaves_the_flag_and_yields_to_the_holder() {
        let d = tempfile::tempdir().unwrap();
        let held = open_private(&d.path().join("card.lock")).unwrap();
        held.lock().unwrap();

        let calls = Cell::new(0);
        run_coalesced(d.path(), || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 0, "the holder publishes, not us");
        assert!(dirty(d.path()), "the flag must survive for the holder");
    }

    /// The mop-up pass. Without it the last trigger of a burst is the one that
    /// gets lost, which is exactly the one carrying the newest state.
    #[test]
    fn a_trigger_that_arrives_mid_publish_earns_one_more_publish() {
        let d = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let dir = d.path().to_path_buf();
        run_coalesced(d.path(), || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                fs::write(dir.join("card.dirty"), [])?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 2);
    }

    /// The flag is already spent when a publish fails, so the error surfaces
    /// rather than spinning against a webhook that is refusing us.
    #[test]
    fn a_failing_publish_stops_the_loop_and_reports() {
        let d = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let err = run_coalesced(d.path(), || {
            calls.set(calls.get() + 1);
            Err(io::Error::other("nope"))
        })
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert_eq!(err.to_string(), "nope");
    }

    /// A directory that does not exist is a caller bug, not something to
    /// swallow: silently not publishing looks identical to a healthy tower.
    #[test]
    fn a_missing_directory_is_an_error() {
        let calls = Cell::new(0);
        assert!(run_coalesced(Path::new("/nonexistent/herdr-tower"), || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .is_err());
        assert_eq!(calls.get(), 0);
    }
}
