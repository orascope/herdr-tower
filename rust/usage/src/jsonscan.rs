// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Literal scans over session files that are only incidentally JSON.
//!
//! The files these read — codex rollouts, gemini chat logs — are JSON Lines
//! measured in hundreds of kilobytes, and we want two or three fixed keys out
//! of the tail of one. Parsing every line would cost far more than it buys, so
//! these are pattern scans, written by hand rather than as regexes: with
//! literal anchors and no alternation there is nothing for an engine to
//! optimize, and the two semantics that actually matter — *last match wins*,
//! and *greedy backtracking picks the rightmost inner key* — are easier to
//! state directly than to re-derive from a pattern.
//!
//! Deliberately no escape handling, just as the pattern `[^"]+` has none, so a
//! value containing `\"` truncates. Model IDs and session IDs never contain
//! one.

/// Every `"<key>":"<value>"` in `hay`, in order, mirroring `"key":"([^"]+)"`.
///
/// `[^"]+` demands at least one character, so `"model":""` is not a match at
/// all and the scan moves past it rather than yielding an empty string.
fn string_field_values<'a>(hay: &'a str, key: &str) -> Vec<&'a str> {
    let needle = format!("\"{key}\":\"");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(&needle) {
        let start = from + rel + needle.len();
        match hay[start..].find('"') {
            Some(0) | None => from += rel + 1,
            Some(end) => {
                out.push(&hay[start..start + end]);
                // Matches do not overlap: resume after the closing quote.
                from = start + end + 1;
            }
        }
    }
    out
}

/// The last `"<key>":"<value>"` in `hay`.
///
/// Last rather than first because these files are append-only transcripts: the
/// most recent turn is the one that describes the session as it stands now.
pub(crate) fn last_string_field<'a>(hay: &'a str, key: &str) -> Option<&'a str> {
    string_field_values(hay, key).pop()
}

/// The first `"<key>":"<value>"` in `hay`, for keys written once in a header.
pub(crate) fn first_string_field<'a>(hay: &'a str, key: &str) -> Option<&'a str> {
    string_field_values(hay, key).into_iter().next()
}

/// The last `"tokens":{…"total":N…}` in `hay`, mirroring
/// `"tokens":\{[^}]*"total":(\d+)`.
///
/// Two subtleties are load-bearing. `[^}]*` cannot cross the object's closing
/// brace, so a `"total"` belonging to some later object is not eligible; and
/// because it is greedy and backtracks from the right, the *rightmost*
/// `"total"` inside one object wins, not the first.
pub(crate) fn last_tokens_total(hay: &str) -> Option<f64> {
    const OPEN: &str = "\"tokens\":{";
    const TOTAL: &str = "\"total\":";

    let mut last = None;
    let mut from = 0;
    while let Some(rel) = hay[from..].find(OPEN) {
        let body_start = from + rel + OPEN.len();
        let body_end = hay[body_start..]
            .find('}')
            .map_or(hay.len(), |i| body_start + i);
        let body = &hay[body_start..body_end];

        let mut hit = None;
        let mut at = 0;
        while let Some(r) = body[at..].find(TOTAL) {
            let s = at + r + TOTAL.len();
            let digits = body[s..].len()
                - body[s..]
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .len();
            if digits > 0 {
                hit = Some((s + digits, &body[s..s + digits]));
            }
            at += r + 1;
        }
        match hit {
            Some((end, digits)) => {
                last = digits.parse::<f64>().ok();
                from = body_start + end;
            }
            None => from += rel + 1,
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transcript grows by appending, so a model switch mid-session has to
    /// show the model in force now, not the one it started with.
    #[test]
    fn string_field_takes_the_last_occurrence() {
        let hay = r#"{"model":"gemini-3-flash"}{"model":"gemini-3-pro"}"#;
        assert_eq!(last_string_field(hay, "model"), Some("gemini-3-pro"));
    }

    /// A header key is written once, up front; taking the last would follow a
    /// mention of some other session's ID later in the file.
    #[test]
    fn string_field_can_take_the_first() {
        let hay = r#"{"session_id":"a","x":1}{"session_id":"b"}"#;
        assert_eq!(first_string_field(hay, "session_id"), Some("a"));
    }

    /// `[^"]+` needs a character. An empty value is no match, and must not be
    /// mistaken for "the model is the empty string" — that would blank a row.
    #[test]
    fn empty_values_are_not_matches() {
        assert_eq!(last_string_field(r#"{"model":""}"#, "model"), None);
        assert_eq!(
            last_string_field(r#"{"model":"","model":"opus"}"#, "model"),
            Some("opus")
        );
    }

    #[test]
    fn missing_key_is_none() {
        assert_eq!(last_string_field(r#"{"other":"x"}"#, "model"), None);
        assert_eq!(last_tokens_total(r#"{"total":5}"#), None);
    }

    /// The turn's own total is what sizes the context; an `input`/`output`
    /// breakdown sitting before it in the same object must not be picked up.
    #[test]
    fn tokens_total_is_the_rightmost_in_the_object() {
        let hay = r#"{"tokens":{"input":200,"total":7,"output":20,"total":220}}"#;
        assert_eq!(last_tokens_total(hay), Some(220.0));
    }

    /// A `"total"` after the object closed belongs to something else.
    #[test]
    fn tokens_total_stops_at_the_closing_brace() {
        assert_eq!(
            last_tokens_total(r#"{"tokens":{"input":1},"total":999}"#),
            None
        );
    }

    #[test]
    fn tokens_total_takes_the_last_turn() {
        let hay = r#"{"tokens":{"total":110}}
{"tokens":{"total":220}}"#;
        assert_eq!(last_tokens_total(hay), Some(220.0));
    }
}
