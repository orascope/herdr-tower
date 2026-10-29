// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Encoding with one exact byte layout: sorted keys, HTML-safe escapes.
//!
//! The card's payloads are pinned byte for byte against recorded ones, so a
//! diff of the output is the cheapest defect detector available. That only
//! works if the bytes are stable, and the layout has two rules `serde_json`
//! does not follow on its own:
//!
//! **Object keys come out sorted.** `serde_json` writes struct fields and
//! `serialize_entry` calls in the order it is given, so every object in this
//! crate is written in alphabetical key order by hand — see [`Component`]'s
//! `Serialize` impl and the body structs in `webhook.rs`. **Declaration order
//! there is wire order.** Reordering a field for readability is a wire change.
//!
//! **`<`, `>` and `&` are escaped** as `\u003c`, `\u003e` and `\u0026`. Not
//! hypothetical: the card's footer is `-# updated <t:1787626800:R>`, which goes
//! on the wire as `\u003ct:`, and the recorded payloads pin exactly that.
//! `U+2028`/`U+2029` are escaped too (they are legal JSON but break JSONP).

use std::io;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

/// Discord's Components V2 number for an accent-coloured block.
pub const TYPE_CONTAINER: u8 = 17;
/// Discord's Components V2 number for a text display.
pub const TYPE_TEXT: u8 = 10;

/// One Components V2 component.
///
/// A type rather than a bare map. A misspelled key in a map is not a compile
/// error and not a local test failure — it is a 400 from Discord every five
/// minutes, discovered whenever somebody next looks at the channel. The two
/// shapes the card actually uses are small enough to name.
#[derive(Debug, Clone, PartialEq)]
pub enum Component {
    /// A block with a coloured left edge. `accent_color` is Discord's 24-bit
    /// integer, not a CSS string — see `card::accent`.
    Container {
        accent_color: u32,
        components: Vec<Component>,
    },
    /// Markdown text. Discord rejects an empty `content` with a 400, so callers
    /// must not build one; the card's rule is "no row, no section".
    Text { content: String },
}

impl Component {
    pub fn text(content: impl Into<String>) -> Self {
        Component::Text {
            content: content.into(),
        }
    }
}

impl Serialize for Component {
    /// Keys are emitted in alphabetical order. Do not reorder.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Component::Container {
                accent_color,
                components,
            } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("accent_color", accent_color)?;
                m.serialize_entry("components", components)?;
                m.serialize_entry("type", &TYPE_CONTAINER)?;
                m.end()
            }
            Component::Text { content } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("content", content)?;
                m.serialize_entry("type", &TYPE_TEXT)?;
                m.end()
            }
        }
    }
}

/// Serializes compactly, with the key order and escapes described above.
pub fn to_sorted_json<T: Serialize + ?Sized>(v: &T) -> io::Result<Vec<u8>> {
    let raw = serde_json::to_vec(v).map_err(io::Error::other)?;
    Ok(escape_html(&raw))
}

/// [`to_sorted_json_pretty`], **newline-terminated**.
///
/// The newline is the whole difference from [`to_sorted_json_pretty`], which
/// correctly has none. It lives here rather than at the call site because `card
/// --out` is piped into `jq` and redirected into files, where a missing
/// terminator puts the shell prompt on top of the closing bracket — and because
/// one function is one place for the recorded-payload test to point at.
/// Reproducing it at the caller is how it went missing.
pub fn encode_sorted_json<T: Serialize + ?Sized>(v: &T) -> io::Result<Vec<u8>> {
    let mut out = to_sorted_json_pretty(v)?;
    out.push(b'\n');
    Ok(out)
}

/// Serializes with two-space indent — and no trailing newline, since the state
/// file is compared as a whole.
pub fn to_sorted_json_pretty<T: Serialize + ?Sized>(v: &T) -> io::Result<Vec<u8>> {
    let raw = serde_json::to_vec_pretty(v).map_err(io::Error::other)?;
    Ok(escape_html(&raw))
}

/// Escapes `<`, `>`, `&`, U+2028 and U+2029 in an already-encoded document.
///
/// A second pass rather than a custom `serde_json::ser::Formatter` because the
/// rewrite is provably equivalent and needs no parser: in a JSON document `<`,
/// `>`, `&`, `U+2028` and `U+2029` can only occur inside a string literal —
/// none of them is JSON syntax — and `serde_json` never emits them as part of
/// an escape sequence it wrote itself. Their bytes are also unambiguous:
/// `<`, `>` and `&` are ASCII, which UTF-8 never uses as a continuation byte.
fn escape_html(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let (esc, width) = match src[i] {
            b'<' => (&b"\\u003c"[..], 1),
            b'>' => (&b"\\u003e"[..], 1),
            b'&' => (&b"\\u0026"[..], 1),
            0xE2 if src[i..].starts_with("\u{2028}".as_bytes()) => (&b"\\u2028"[..], 3),
            0xE2 if src[i..].starts_with("\u{2029}".as_bytes()) => (&b"\\u2029"[..], 3),
            b => {
                out.push(b);
                i += 1;
                continue;
            }
        };
        out.extend_from_slice(esc);
        i += width;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(v: &impl Serialize) -> String {
        String::from_utf8(to_sorted_json(v).unwrap()).unwrap()
    }

    /// Discord's relative-timestamp syntax is `<t:…:R>`, so every card carries
    /// characters that get escaped. A card that renders the escape wrong shows
    /// the user a literal `<t:1787626800:R>` instead of "3 minutes ago".
    #[test]
    fn angle_brackets_and_ampersands_are_escaped() {
        // The expected side is a raw string, so each escape there is the six
        // literal characters that have to appear on the wire.
        assert_eq!(
            json(&Component::text("-# updated <t:1787626800:R> & <b>")),
            r#"{"content":"-# updated \u003ct:1787626800:R\u003e \u0026 \u003cb\u003e","type":10}"#
        );
    }

    /// Escaped always, not only alongside the HTML characters: there is no
    /// switch for these two — only the bytes.
    #[test]
    fn line_separators_are_escaped_unconditionally() {
        assert_eq!(
            json(&Component::text("a\u{2028}b\u{2029}c")),
            r#"{"content":"a\u2028b\u2029c","type":10}"#
        );
    }

    /// `serde_json` keeps the order it is given; these have to be written
    /// sorted by hand.
    #[test]
    fn object_keys_come_out_sorted() {
        let c = Component::Container {
            accent_color: 15_054_183,
            components: vec![Component::text("hi")],
        };
        assert_eq!(
            json(&c),
            r#"{"accent_color":15054183,"components":[{"content":"hi","type":10}],"type":17}"#
        );
    }

    /// An empty list is `[]`, not `null` — Discord rejects `null` components.
    #[test]
    fn an_empty_component_list_is_an_empty_array() {
        let empty: Vec<Component> = Vec::new();
        assert_eq!(json(&empty), "[]");
    }

    /// The empty shapes keep their keys. A container whose rows all dropped out
    /// still writes `"components":[]`, and an empty string still writes
    /// `"content":""` — a missing key is a 400 from Discord, where an empty one
    /// is merely an empty block.
    #[test]
    fn the_empty_shapes_keep_their_keys() {
        assert_eq!(json(&Component::text("")), r#"{"content":"","type":10}"#);
        let bare = Component::Container {
            accent_color: 0,
            components: Vec::new(),
        };
        assert_eq!(
            json(&bare),
            r#"{"accent_color":0,"components":[],"type":17}"#
        );
        assert_eq!(
            json(&Component::Container {
                accent_color: 0,
                components: vec![bare],
            }),
            r#"{"accent_color":0,"components":[{"accent_color":0,"components":[],"type":17}],"type":17}"#,
            "nesting an empty container changes nothing about either of them"
        );
    }

    /// The escape walks bytes, and U+2028/U+2029 share their lead byte `0xE2`
    /// with every other character in that range — `…`, `→`, `☃` and the whole
    /// of CJK punctuation. A card is full of them, and one mis-measured branch
    /// would either corrupt them or cut a character in half.
    #[test]
    fn other_characters_sharing_the_lead_byte_are_left_alone() {
        let text = "… → ☃ 세션 \u{2027}\u{202a}\u{2030} 🗼";
        assert_eq!(
            json(&Component::text(text)),
            format!("{{\"content\":\"{text}\",\"type\":10}}")
        );
    }

    /// `encode_sorted_json` is the indented form plus the newline that keeps a
    /// redirected `card --out` off the shell prompt — and nothing else. The
    /// escapes have to survive the indented encoder too, which is a separate
    /// `serde_json` call and therefore a separate chance to lose them.
    #[test]
    fn the_encoded_form_is_the_indented_one_with_a_newline() {
        let c = Component::Container {
            accent_color: 15_054_183,
            components: vec![Component::text("-# updated <t:1787626800:R>")],
        };
        let pretty = to_sorted_json_pretty(&c).unwrap();
        assert_ne!(
            pretty.last(),
            Some(&b'\n'),
            "the indented form ends on the closing bracket"
        );
        let mut want = pretty.clone();
        want.push(b'\n');
        assert_eq!(encode_sorted_json(&c).unwrap(), want);
        assert!(
            String::from_utf8(pretty)
                .unwrap()
                .contains(r"\u003ct:1787626800:R\u003e"),
            "indenting must not cost the escapes"
        );
    }
}

/// Generated documents, checked against what a second encoder makes of them.
///
/// Hand-rolled rather than a property framework: the generator is seeded, so a
/// failure replays exactly and nothing is added to the build.
#[cfg(test)]
mod generated_tests {
    use super::*;

    use serde_json::Value;

    /// xorshift64*: small, fast, and the same sequence on every machine.
    struct Rng(tower_testrng::Rng);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(tower_testrng::Rng::new(seed))
        }

        fn next(&mut self) -> u64 {
            self.0.draw()
        }

        fn below(&mut self, n: usize) -> usize {
            self.0.below(n)
        }

        /// Text out of the pieces a card really carries, plus every character
        /// the encoder has an opinion about.
        fn text(&mut self, max: usize) -> String {
            const PIECES: &[&str] = &[
                "a",
                "Z",
                "fix the card",
                " ",
                "·",
                "…",
                "→",
                "세션",
                "🗼",
                "é",
                "<",
                ">",
                "&",
                "<t:1787626800:R>",
                "\"",
                "\\",
                "/",
                "\n",
                "\t",
                "\u{2028}",
                "\u{2029}",
                "\u{0}",
                "\u{1b}",
                "\u{8}",
                "\u{c}",
            ];
            (0..self.below(max))
                .map(|_| PIECES[self.below(PIECES.len())])
                .collect()
        }

        /// A component tree of at most `depth` levels of container.
        fn component(&mut self, depth: usize) -> Component {
            if depth == 0 || self.below(100) < 60 {
                return Component::text(self.text(8));
            }
            Component::Container {
                accent_color: self.next() as u32,
                components: (0..self.below(4))
                    .map(|_| self.component(depth - 1))
                    .collect(),
            }
        }
    }

    /// The obvious encoder, written a second time: keys spelled out in sorted
    /// order, escapes applied character by character. It agrees with
    /// [`to_sorted_json`] or one of the two is wrong, and only one of them is
    /// the one the webhook sees.
    fn naive(c: &Component) -> String {
        match c {
            Component::Container {
                accent_color,
                components,
            } => {
                let inner: Vec<String> = components.iter().map(naive).collect();
                format!(
                    "{{\"accent_color\":{accent_color},\"components\":[{}],\"type\":{TYPE_CONTAINER}}}",
                    inner.join(",")
                )
            }
            Component::Text { content } => format!(
                "{{\"content\":\"{}\",\"type\":{TYPE_TEXT}}}",
                naive_string(content)
            ),
        }
    }

    /// One JSON string body, escaped the long way round.
    fn naive_string(s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{8}' => out.push_str("\\b"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\u{c}' => out.push_str("\\f"),
                '\r' => out.push_str("\\r"),
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    /// Every generated card against the second encoder, key for key and escape
    /// for escape.
    #[test]
    fn a_generated_component_encodes_the_way_a_hand_written_encoder_would() {
        let mut rng = Rng::new(0xCA5D);
        for _ in 0..5_000 {
            let c = rng.component(3);
            let got = String::from_utf8(to_sorted_json(&c).unwrap()).unwrap();
            assert_eq!(got, naive(&c), "{c:?}");
        }
    }

    /// Whatever the text, the five characters that must not reach the wire do
    /// not, and the text still comes back out of the document unchanged — an
    /// escape that dropped or doubled a character would corrupt a card without
    /// ever failing to encode.
    ///
    /// The last assertion is the round trip closing: re-encoding what was
    /// parsed gives back the same bytes, so the document is a fixed point of
    /// the encoder and could be compared against a recorded one twice over.
    #[test]
    fn a_generated_text_survives_the_escape_and_comes_back_unchanged() {
        let mut rng = Rng::new(0xBEEF);
        for _ in 0..5_000 {
            let content = rng.text(12);
            let out = to_sorted_json(&Component::text(content.clone())).unwrap();
            let text = String::from_utf8(out.clone()).unwrap();
            for raw in ['<', '>', '&', '\u{2028}', '\u{2029}'] {
                assert!(!text.contains(raw), "{raw:?} left unescaped in {text}");
            }
            let back: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(back["content"], Value::String(content));
            assert_eq!(
                to_sorted_json(&back).unwrap(),
                out,
                "re-encoding what was parsed gives the same bytes"
            );
        }
    }

    /// The indented encoder is the same document with whitespace in it: parsing
    /// both has to give the same value, and the compact one has to be the
    /// shorter of the two.
    #[test]
    fn the_indented_form_carries_the_same_document() {
        let mut rng = Rng::new(0x1DEA);
        for _ in 0..2_000 {
            let c = rng.component(3);
            let compact = to_sorted_json(&c).unwrap();
            let pretty = to_sorted_json_pretty(&c).unwrap();
            let a: Value = serde_json::from_slice(&compact).unwrap();
            let b: Value = serde_json::from_slice(&pretty).unwrap();
            assert_eq!(a, b, "{c:?}");
            assert!(pretty.len() >= compact.len());
            assert_eq!(encode_sorted_json(&c).unwrap().last(), Some(&b'\n'));
        }
    }
}
