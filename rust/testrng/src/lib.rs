// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! One deterministic generator, for tests that want many inputs rather than a
//! few.
//!
//! # Why this is a crate
//!
//! Twenty test files each carried their own copy, and not the same copy: some
//! were this xorshift, some a plain LCG keeping the high bits, with seeds and
//! shifts that drifted apart. A generated test is only as good as its inputs,
//! and nobody could say whether one of those twenty covered its space evenly —
//! the answer had to be re-derived per file. One definition, measured once.
//!
//! # Why not `rand`
//!
//! A test must fail the same way twice. `rand`'s generators are free to change
//! their algorithm between versions, which would silently change every
//! generated corpus in the workspace; this one cannot change without this file
//! changing. It is also a dev-dependency only, so nothing shipped depends on it.
//!
//! 🚨 Not for anything but tests. It is fast and reproducible, not
//! unpredictable: the sequence is fully determined by the seed, so a secret,
//! a token or an ID must never come from here.

/// A seeded xorshift64\* generator.
///
/// The shifts (12, 25, 27) and the multiply are Marsaglia's xorshift64star,
/// which passes the tests a generated corpus needs: a full period over the
/// 64-bit state (bar zero) and no visible structure in the low bits, which is
/// where [`Self::below`] takes its answer from.
///
/// 🚨 Any seed is safe, zero included: [`Self::new`] scrambles it away from
/// xorshift's fixed point.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator that will produce the same sequence for the same `seed`,
    /// today and after any refactor of the code under test.
    pub fn new(seed: u64) -> Self {
        // The seed is scrambled by the golden-ratio odd constant and then forced
        // odd. 🚨 Two reasons, both load-bearing: zero is xorshift's fixed point
        // (it would stay there forever, so a test seeded from a counter starting
        // at zero would generate one input a thousand times without saying so),
        // and nearby seeds — 0, 1, 2 for three rounds of the same test — would
        // otherwise start with nearly the same state and walk nearly the same
        // corpus. This is the spelling the workspace's copies already used, so
        // consolidating them changed no corpus.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// The next 64 bits.
    ///
    /// Named `draw`, not `next`: this is not an iterator, and a `next` that is
    /// not `Iterator::next` reads like one at every call site.
    pub fn draw(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number under `n`, or zero when `n` is zero.
    ///
    /// Modulo, which is not perfectly uniform for an `n` that does not divide
    /// 2^64 — the first `2^64 % n` values are one draw more likely. For the
    /// sizes tests use (a handful of pieces, a few hundred rounds) that bias is
    /// smaller than the sampling noise of the test itself, and rejection
    /// sampling would make the sequence depend on how many draws were rejected,
    /// which is harder to reason about when a corpus changes.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.draw() % n as u64) as usize
    }

    /// One of `choices`, or `None` when there are none.
    pub fn pick<'a, T>(&mut self, choices: &'a [T]) -> Option<&'a T> {
        choices.get(self.below(choices.len()))
    }

    /// `true` about one time in `n`.
    pub fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same seed is the same sequence — the whole contract.
    #[test]
    fn a_seed_names_a_sequence() {
        let seq: Vec<u64> = (0..8).scan(Rng::new(7), |r, _| Some(r.draw())).collect();
        let again: Vec<u64> = (0..8).scan(Rng::new(7), |r, _| Some(r.draw())).collect();
        assert_eq!(seq, again);
        let other: Vec<u64> = (0..8).scan(Rng::new(8), |r, _| Some(r.draw())).collect();
        assert_ne!(seq, other, "a different seed is a different sequence");
    }

    /// 🚨 Zero is xorshift's fixed point. A generator that sat there would hand
    /// out one value forever and every test seeded from a counter starting at
    /// zero would be testing a single input.
    #[test]
    fn a_zero_seed_still_moves() {
        let mut r = Rng::new(0);
        let first = r.draw();
        assert_ne!(first, 0);
        assert_ne!(first, r.draw(), "and it keeps moving");
    }

    /// `below` stays inside its bound, covers it, and answers zero for zero
    /// rather than dividing by it.
    #[test]
    fn below_stays_under_its_bound_and_covers_it() {
        let mut r = Rng::new(0x5eed);
        assert_eq!(r.below(0), 0, "no bound, no panic");
        assert_eq!(r.below(1), 0);
        let mut seen = [false; 6];
        for _ in 0..600 {
            let n = r.below(6);
            assert!(n < 6);
            seen[n] = true;
        }
        assert!(
            seen.iter().all(|&s| s),
            "every value of a small bound: {seen:?}"
        );
    }

    /// Roughly even over a small bound: a chi-square over six buckets and 6000
    /// draws, against the 0.1% critical value for five degrees of freedom
    /// (20.5). A generator that leaned on one bucket would make a generated
    /// corpus quietly narrow.
    #[test]
    fn the_low_bits_do_not_lean() {
        let mut r = Rng::new(0xC0FFEE);
        let mut counts = [0u32; 6];
        for _ in 0..6000 {
            counts[r.below(6)] += 1;
        }
        let expected = 1000.0;
        let chi: f64 = counts
            .iter()
            .map(|&c| {
                let d = f64::from(c) - expected;
                d * d / expected
            })
            .sum();
        assert!(chi < 20.5, "chi-square {chi:.2} over {counts:?}");
    }

    /// `pick` answers from the slice it was given, and `None` for an empty one.
    #[test]
    fn pick_answers_from_the_slice() {
        let mut r = Rng::new(3);
        assert_eq!(r.pick::<u8>(&[]), None);
        let choices = ["a", "b", "c"];
        for _ in 0..50 {
            assert!(choices.contains(r.pick(&choices).expect("not empty")));
        }
    }
}
