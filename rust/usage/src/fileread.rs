// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Bounded reads of append-only session files.
//!
//! These files grow without limit — a long codex session is tens of megabytes —
//! and the tower polls them every couple of seconds. Both ends are read with a
//! fixed budget: the header identifies the session, the tail carries the most
//! recent turn, and everything between is of no interest.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The first `limit` bytes of a file, as lossy UTF-8.
///
/// Lossy on purpose: a session file caught mid-write can end inside a
/// multi-byte character, and a truncated glyph is a much better outcome than
/// refusing to identify the session.
pub(crate) fn read_head(path: &Path, limit: u64) -> Option<String> {
    read_head_from(&mut File::open(path).ok()?, limit)
}

/// The first `limit` bytes of an already-open file, leaving the cursor behind
/// them. Callers that go on to read the tail seek anyway.
pub(crate) fn read_head_from(f: &mut File, limit: u64) -> Option<String> {
    let mut buf = Vec::new();
    f.take(limit).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The last `limit` bytes of an open file, as lossy UTF-8.
///
/// Takes the handle rather than the path so a caller that already stat'd and
/// header-matched the file does not open it a second time.
pub(crate) fn read_tail(f: &mut File, size: u64, limit: u64) -> String {
    try_read_tail(f, size, limit).unwrap_or_default()
}

/// [`read_tail`], saying when the read failed rather than answering with an
/// empty string — for a caller that must not remember a failure as the file's
/// content.
pub(crate) fn try_read_tail(f: &mut File, size: u64, limit: u64) -> std::io::Result<String> {
    f.seek(SeekFrom::Start(size.saturating_sub(limit)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// How much of a file is held in memory at once by [`last_field_between`].
pub(crate) const SCAN_CHUNK: u64 = 64 * 1024;

/// How far each chunk reaches past its end into the one read before it, so a
/// `"key":"value"` cut by a chunk's edge is whole in one of them. Far longer
/// than any value these scans look for.
pub(crate) const SCAN_OVERLAP: u64 = 1024;

/// The last `"<key>":"<value>"` in the bytes from `from` to `to`.
///
/// Read backwards a chunk at a time, stopping at the first chunk that has one:
/// the latest value is usually just before `to`, so a file of any size costs a
/// chunk or two, and is never held in memory whole.
pub(crate) fn last_field_between(
    f: &mut File,
    from: u64,
    to: u64,
    key: &str,
) -> std::io::Result<Option<String>> {
    let mut end = to;
    let mut buf = Vec::new();
    while end > from {
        let start = end.saturating_sub(SCAN_CHUNK).max(from);
        let stop = (end + SCAN_OVERLAP).min(to);
        f.seek(SeekFrom::Start(start))?;
        buf.clear();
        f.by_ref().take(stop - start).read_to_end(&mut buf)?;
        // A match wholly inside the overlap would have been found in the chunk
        // after this one; one found here is the latest before `end`.
        if let Some(value) = crate::jsonscan::last_string_field(&String::from_utf8_lossy(&buf), key)
        {
            return Ok(Some(value.to_string()));
        }
        end = start;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_with(body: &[u8]) -> (tempfile::TempDir, File, u64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        File::create(&path).unwrap().write_all(body).unwrap();
        (dir, File::open(&path).unwrap(), body.len() as u64)
    }

    fn model_at(pos: usize, len: usize) -> Vec<u8> {
        let key = b"\"model\":\"gpt-edge\"";
        let mut body = vec![b'x'; len];
        body[pos..pos + key.len()].copy_from_slice(key);
        body
    }

    /// A key cut by a chunk's edge — anywhere across it — is found whole in
    /// one of the two chunks.
    #[test]
    fn a_key_across_a_chunk_edge_is_found() {
        let len = 3 * SCAN_CHUNK as usize;
        let edge = len - SCAN_CHUNK as usize;
        for pos in edge - 25..edge + 5 {
            let (_d, mut f, to) = file_with(&model_at(pos, len));
            assert_eq!(
                last_field_between(&mut f, 0, to, "model")
                    .unwrap()
                    .as_deref(),
                Some("gpt-edge"),
                "key at {pos}, edge at {edge}"
            );
        }
    }

    /// The latest wins, however far apart they are.
    #[test]
    fn the_last_value_across_chunks_wins() {
        let mut body = b"\"model\":\"gpt-one\"".to_vec();
        body.extend(vec![b'x'; SCAN_CHUNK as usize * 3]);
        body.extend_from_slice(b"\"model\":\"gpt-two\"");
        body.extend(vec![b'x'; SCAN_CHUNK as usize]);
        let (_d, mut f, len) = file_with(&body);
        assert_eq!(
            last_field_between(&mut f, 0, len, "model")
                .unwrap()
                .as_deref(),
            Some("gpt-two")
        );
    }

    /// Only the range asked for, at both ends.
    #[test]
    fn the_scan_stays_inside_its_range() {
        let mut body = b"\"model\":\"gpt-one\"".to_vec();
        let one_ends = body.len() as u64;
        body.extend(vec![b'x'; 100]);
        let two_starts = body.len() as u64;
        body.extend_from_slice(b"\"model\":\"gpt-two\"");
        let (_d, mut f, len) = file_with(&body);
        assert_eq!(
            last_field_between(&mut f, 0, two_starts, "model")
                .unwrap()
                .as_deref(),
            Some("gpt-one")
        );
        assert_eq!(
            last_field_between(&mut f, one_ends, len, "model")
                .unwrap()
                .as_deref(),
            Some("gpt-two")
        );
        assert_eq!(
            last_field_between(&mut f, one_ends, two_starts, "model").unwrap(),
            None
        );
    }

    // ── g4testadd: last_field_between, cross-checked and at its edges ─────

    /// A fixed-seed generator, so a failing case replays exactly.
    struct Seq(u64);

    impl Seq {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Writes `key` at `at` if it lands on plain filler — no quote in it or
    /// next to it. Keys that overlap make text no JSON writer produces, where
    /// the answer depends on where a scan happens to start.
    fn place(body: &mut [u8], at: usize, key: &str) -> bool {
        let (lo, hi) = (at.saturating_sub(1), (at + key.len() + 1).min(body.len()));
        if at + key.len() > body.len() || body[lo..hi].contains(&b'"') {
            return false;
        }
        body[at..at + key.len()].copy_from_slice(key.as_bytes());
        true
    }

    /// A body of filler with `"model":"m<i>"` keys dropped in at random
    /// offsets.
    fn random_body(seq: &mut Seq) -> Vec<u8> {
        let len = SCAN_CHUNK as usize * (1 + seq.below(4)) + seq.below(3000);
        let mut body: Vec<u8> = (0..len).map(|i| b"abcdefgh, {}:"[i % 13]).collect();
        for i in 0..seq.below(6) {
            let key = format!("\"model\":\"m{i}\"");
            let at = seq.below(len.saturating_sub(key.len()).max(1));
            place(&mut body, at, &key);
        }
        body
    }

    /// Cross validation: the chunked backward scan answers exactly what a
    /// plain search of the whole range answers, for random bodies and random
    /// ranges. Half the keys are placed across a chunk's edge — counted back
    /// from the range's end, where the scan cuts — since that is the one place
    /// a chunked read can go wrong and a random offset almost never lands.
    #[test]
    fn the_backward_scan_agrees_with_a_whole_range_search() {
        let mut seq = Seq(0x5EED_F11E);
        for _ in 0..300 {
            let mut body = random_body(&mut seq);
            let len = body.len();
            let from = seq.below(len / 2);
            let to = from + seq.below(len - from + 1);
            for i in 0..3 {
                let key = format!("\"model\":\"edge{i}\"");
                let chunks_back = 1 + seq.below(3) as u64;
                let Some(edge) = (to as u64).checked_sub(chunks_back * SCAN_CHUNK) else {
                    continue;
                };
                let at = (edge as usize).saturating_sub(seq.below(key.len()));
                if at >= from && at + key.len() <= to {
                    place(&mut body, at, &key);
                }
            }
            let (_d, mut f, _) = file_with(&body);
            let whole = String::from_utf8_lossy(&body[from..to]).into_owned();
            let want = crate::jsonscan::last_string_field(&whole, "model").map(str::to_string);
            let got = last_field_between(&mut f, from as u64, to as u64, "model").unwrap();
            assert_eq!(got, want, "range {from}..{to} of {len}");
        }
    }

    /// Edge cases: an empty range, a range reaching past the end of the file,
    /// an empty file, a key with an empty value (not a match), and a key cut
    /// off by the end of the range.
    #[test]
    fn the_backward_scan_at_its_edges() {
        let (_d, mut f, len) = file_with(br#"xx"model":"gpt-a"yy"#);
        assert_eq!(
            last_field_between(&mut f, 5, 5, "model").unwrap(),
            None,
            "empty range"
        );
        assert_eq!(
            last_field_between(&mut f, 0, len + 1000, "model")
                .unwrap()
                .as_deref(),
            Some("gpt-a"),
            "past the end reads to the end"
        );
        assert_eq!(
            last_field_between(&mut f, 0, len - 5, "model").unwrap(),
            None,
            "the closing quote is outside the range"
        );

        let (_d, mut empty, _) = file_with(b"");
        assert_eq!(last_field_between(&mut empty, 0, 0, "model").unwrap(), None);

        let (_d, mut blank, len) = file_with(br#""model":"" "model":"gpt-b""#);
        assert_eq!(
            last_field_between(&mut blank, 0, len, "model")
                .unwrap()
                .as_deref(),
            Some("gpt-b"),
            "an empty value is not a value"
        );
    }

    /// Property: whatever came before, a key written later wins — the scan's
    /// answer is always the newest value in the range.
    #[test]
    fn a_later_value_always_wins() {
        let mut seq = Seq(0xA11_1A7E);
        for i in 0..50 {
            let mut body = random_body(&mut seq);
            let newest = format!("\"model\":\"newest-{i}\"");
            body.extend(vec![b'z'; 1 + seq.below(3 * SCAN_CHUNK as usize)]);
            body.extend_from_slice(newest.as_bytes());
            body.extend(vec![b'z'; seq.below(100)]);
            let (_d, mut f, len) = file_with(&body);
            assert_eq!(
                last_field_between(&mut f, 0, len, "model").unwrap(),
                Some(format!("newest-{i}"))
            );
        }
    }
}
