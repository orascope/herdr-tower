// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! How often one grant may call tools: a sliding ten-minute window per grant
//! and per scope.
//!
//! In memory. A restart forgets the counts, which is fine — the limit is there
//! to stop a runaway loop on the other end from typing into an agent all
//! night, not to account for anything.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::oauth::{SCOPE_ACT, SCOPE_READ};

pub const WINDOW: Duration = Duration::from_secs(10 * 60);
pub const READ_MAX: usize = 300;
pub const ACT_MAX: usize = 30;

#[derive(Default)]
pub struct Limiter {
    hits: Mutex<HashMap<(String, &'static str), VecDeque<Instant>>>,
}

impl Limiter {
    /// Counts one call and says whether it is within the limit. A refused
    /// call is not counted, so a client that backs off recovers.
    pub fn allow(&self, grant: &str, scope: &'static str, now: Instant) -> bool {
        let max = match scope {
            SCOPE_ACT => ACT_MAX,
            SCOPE_READ => READ_MAX,
            _ => return false,
        };
        let Ok(mut hits) = self.hits.lock() else {
            return false;
        };
        // Forget every grant that has been quiet for a whole window. A grant
        // is revoked, expires or is replaced at every reconnect, and the
        // server runs for weeks; keys kept for them would only ever grow (#106).
        hits.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) < WINDOW));
        let q = hits.entry((grant.to_string(), scope)).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= WINDOW) {
            q.pop_front();
        }
        if q.len() >= max {
            return false;
        }
        q.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acts_are_limited_per_grant_and_recover_with_the_window() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..ACT_MAX {
            assert!(l.allow("g1", SCOPE_ACT, t0));
        }
        assert!(!l.allow("g1", SCOPE_ACT, t0));
        assert!(
            l.allow("g2", SCOPE_ACT, t0),
            "another grant has its own count"
        );
        assert!(l.allow("g1", SCOPE_READ, t0), "reads have their own count");
        assert!(!l.allow("g1", SCOPE_ACT, t0 + WINDOW - Duration::from_secs(1)));
        assert!(l.allow("g1", SCOPE_ACT, t0 + WINDOW));
    }

    #[test]
    fn an_unknown_scope_is_never_allowed() {
        assert!(!Limiter::default().allow("g1", "tower.admin", Instant::now()));
    }

    /// Over any window, however calls are spread, no more than the limit are
    /// allowed — and a quiet grant always gets its full budget back.
    #[test]
    fn no_window_ever_holds_more_than_the_limit() {
        let l = Limiter::default();
        let t0 = Instant::now();
        let mut allowed: Vec<Instant> = Vec::new();
        let mut seed: u64 = 7;
        let mut t = t0;
        for _ in 0..2000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            t += Duration::from_secs((seed >> 58) % 40); // 0..39s apart
            if l.allow("g", SCOPE_ACT, t) {
                allowed.push(t);
            }
        }
        for (i, start) in allowed.iter().enumerate() {
            let within = allowed[i..]
                .iter()
                .take_while(|x| x.duration_since(*start) < WINDOW)
                .count();
            assert!(within <= ACT_MAX, "{within} calls in one window");
        }
        let later = t + WINDOW;
        for _ in 0..ACT_MAX {
            assert!(l.allow("g", SCOPE_ACT, later));
        }
    }

    /// #106: a grant nobody has heard from for a window costs nothing.
    #[test]
    fn quiet_grants_are_forgotten() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for g in 0..500 {
            assert!(l.allow(&format!("g{g}"), SCOPE_READ, t0));
        }
        assert_eq!(l.hits.lock().unwrap().len(), 500);
        assert!(l.allow("latest", SCOPE_ACT, t0 + WINDOW));
        assert_eq!(
            l.hits.lock().unwrap().len(),
            1,
            "only the grant just heard from"
        );
        // Still within its window, a busy grant keeps its count.
        let l = Limiter::default();
        for _ in 0..ACT_MAX {
            assert!(l.allow("busy", SCOPE_ACT, t0));
        }
        assert!(l.allow("other", SCOPE_ACT, t0 + WINDOW / 2));
        assert!(!l.allow("busy", SCOPE_ACT, t0 + WINDOW / 2));
    }
}
