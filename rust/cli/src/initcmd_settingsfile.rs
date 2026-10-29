// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The file handling and quoting these installers depend on.
//!
//! Everything in this file exists because the *bytes* matter. `init` does not
//! print a report and stop — it rewrites `~/.claude/settings.json`,
//! `~/.codex/hooks.json` and `~/.codex/config.toml`, files that three other
//! vendors' programs parse.
//!
//! The JSON encoding is **not** implemented here. The two habits these files
//! rely on that `serde_json` does not share — sorted object keys and `<`, `>`,
//! `&`, U+2028, U+2029 escaped — are already settled once in
//! [`tower_discord::to_sorted_json`] and [`tower_discord::to_sorted_json_pretty`],
//! written for the Discord card and equally load-bearing here. Those are
//! imported rather than copied: a second implementation of the same rule is how
//! two encoders quietly drift apart, and the two would only be discovered to
//! have diverged by a user whose `~/.claude.json` grew an `&`.
//!
//! What is left is the file dance around the encoder — reading a settings file
//! nobody owns, the backup, the temporary file and the rename — plus one
//! formatting rule the card never needed: **[`quote`] is not Rust's `{:?}`**,
//! because a `command = "…"` line is written with it and that string lands in a
//! TOML file.

use std::fmt::Display;
use std::io;
use std::path::Path;

use serde_json::{Map, Value};

/// A JSON object, which is what every settings file here is at the top level.
pub type Object = Map<String, Value>;

/// The suffix of the temporary file that is renamed into place.
pub const TMP_SUFFIX: &str = ".herdr-tower.tmp";
/// The suffix of the copy left behind before anything is touched.
pub const BAK_SUFFIX: &str = ".herdr-tower.bak";

/// Reads a settings file into an object, returning the original bytes too.
///
/// The raw bytes come back because every caller needs them for the backup, and
/// re-reading the file after deciding to write it would open a window where the
/// backup no longer matches what was parsed.
///
/// `None` for the raw bytes means the file was absent — which is *not* the same
/// as empty. Only a file that exists is backed up. An empty file is a parse
/// error, so that distinction never actually reaches the backup, but the shape
/// is kept because each caller's `if let Some(raw)` then says exactly "back up
/// what was there".
///
/// **A file containing bare `null` is treated as an empty object.** It holds no
/// settings to lose, and every installer goes on to insert its own keys into
/// what this returns, so refusing it would block an install over nothing.
pub fn read_json_object(path: &Path) -> io::Result<(Option<Vec<u8>>, Object)> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((None, Object::new())),
        Err(e) => return Err(e),
    };
    let value: Value = serde_json::from_slice(&raw).map_err(|e| parse_failed(path, e))?;
    let obj = match value {
        Value::Object(m) => m,
        Value::Null => Object::new(),
        // Any other top level is refused like a parse error: there is no
        // object to merge into.
        other => {
            return Err(parse_failed(
                path,
                format!("the top level is not an object ({})", kind_of(&other)),
            ))
        }
    };
    Ok((Some(raw), obj))
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The refusal printed when a settings file would not parse.
///
/// It stops rather than overwriting on purpose: the file belongs to another
/// program, and a settings file that herdr-tower cannot read is exactly the one
/// it must not replace.
pub fn parse_failed(path: &Path, e: impl Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} would not parse — look at it yourself: {e}",
            path.display()
        ),
    )
}

/// The compact sorted encoding as a `String`, for comparing two values by their
/// encoded bytes.
///
/// Both sides of every such comparison go through this, which is the point: a
/// `timeout` that came back from a file as a JSON float and the integer literal
/// wanted for it encode to the same three characters, so the idempotency check
/// does not rewrite a file that already says what it should.
pub fn json_key(v: &Value) -> String {
    String::from_utf8(tower_discord::to_sorted_json(v).unwrap_or_default()).unwrap_or_default()
}

/// A TOML basic string, not Rust's `{:?}`: `\"`, `\\`, the five control
/// characters TOML 1.0 has a short escape for (`\b \t \n \f \r`), and
/// `\uXXXX` for every other ASCII control character.
///
/// The one TOML string writer: the herdr binding `init` prints for
/// `~/.config/herdr/config.toml` and the codex configuration (`codexinit`)
/// both go through it, so the two cannot drift apart. Not the C
/// escapes it once wrote: `\a` and `\v` are in no TOML version and `\xHH`
/// only in 1.1, so a path holding one of those characters made the herdr
/// binding unparseable (#33).
///
/// Not the same rule as `tower_discord`'s own private `quote`, which escapes a
/// value going inside a quoted MIME header parameter (backslash and double
/// quote, nothing else). Merging the two would silently stop escaping control
/// characters here, and this output is TOML that herdr and codex parse.
///
/// Only ASCII control characters are escaped, so one case that never reaches
/// here is written down rather than papered over: a non-printable character
/// outside ASCII, U+00A0 included, is written raw rather than as `\u00a0`.
/// Escaping those needs the Unicode printability tables, and the only strings
/// quoted here are the running binary's path and a path from the caller. A
/// non-breaking space in the path of the running binary would be written raw;
/// raw and escaped are valid TOML for the same string, which is the property
/// that has to hold, since herdr and codex parse what this writes.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The first TOML basic string on a line, read back by the TOML 1.0 spec and
/// nothing else: the seven short escapes, `\uXXXX` and `\UXXXXXXXX`, and no raw
/// control character but a tab.
///
/// Written from the spec rather than from [`quote`], so what a caller checks
/// with it is that *codex* can read the line, not that two copies of one escape
/// table agree. It lives here, beside the writer it checks.
///
/// Not the same reader as the `toml_basic_string` in this file's own tests: that
/// one is handed a whole quoted string and refuses anything else, where this one
/// is handed a `key = "value"` line and stops at the closing quote.
#[cfg(test)]
pub(crate) fn toml_first_basic_string(line: &str) -> Result<String, String> {
    let start = line
        .find('"')
        .ok_or_else(|| format!("no basic string on {line:?}"))?;
    let mut chars = line[start + 1..].chars();
    let mut out = String::new();
    loop {
        let c = chars
            .next()
            .ok_or_else(|| format!("the string never ends on {line:?}"))?;
        match c {
            '"' => return Ok(out),
            '\\' => {
                let e = chars
                    .next()
                    .ok_or_else(|| "a trailing backslash".to_string())?;
                let hex = |n: usize, chars: &mut std::str::Chars| {
                    let h: String = chars.take(n).collect();
                    u32::from_str_radix(&h, 16)
                        .ok()
                        // `from_str_radix` takes a leading `+`; TOML does not.
                        .filter(|_| h.len() == n && h.bytes().all(|b| b.is_ascii_hexdigit()))
                        .and_then(char::from_u32)
                        .ok_or(format!("a bad \\{e} escape"))
                };
                out.push(match e {
                    'b' => '\u{8}',
                    't' => '\t',
                    'n' => '\n',
                    'f' => '\u{c}',
                    'r' => '\r',
                    '"' => '"',
                    '\\' => '\\',
                    'u' => hex(4, &mut chars)?,
                    'U' => hex(8, &mut chars)?,
                    other => return Err(format!("no such escape in TOML: \\{other}")),
                });
            }
            c if c != '\t' && ((c as u32) < 0x20 || c as u32 == 0x7f) => {
                return Err(format!("a raw control character {:#x}", c as u32))
            }
            c => out.push(c),
        }
    }
}

/// Writes a fresh file at `mode`, replacing whatever was there.
///
/// The mode is the point: these files hold nothing secret, but every settings
/// file is written 0600 and the markdown ones 0644, and quietly widening
/// `~/.codex/config.toml` to the umask default would be a change nobody asked
/// for. On a platform without Unix permissions the mode is simply not
/// expressible and the file is still written.
///
/// The mode applies only when the file is *created*. That is not the same as
/// "an existing settings file keeps its permissions", because [`write_atomic`]
/// renames a freshly created temporary over it: a 0644 `settings.json` comes out
/// 0600 and a 0600 `AGENTS.md` comes out 0644. The mode of the file herdr-tower
/// writes is the mode it asks for here, whatever the file it replaced had.
pub fn write_mode(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = opts.open(path)?;
    f.write_all(data)
}

/// Writes through a temporary file and renames it into place.
///
/// Not belt-and-braces: the file being replaced is read at startup by codex,
/// Claude Code or the Gemini CLI, and a half-written `settings.json` is a CLI
/// that will not start. The rename is the only step those readers can observe.
pub fn write_atomic(path: &Path, tmp_suffix: &str, data: &[u8], mode: u32) -> io::Result<()> {
    // A name of this rewrite's own, not one per destination. Two `init` runs
    // overlapping shared `<file>.tmp`: one renamed that inode into place while
    // the other was still writing through its open descriptor, so the
    // destination — a settings file this function exists to publish atomically —
    // could be observed partial or mixed, and the loser's rename then failed on
    // a name that had been carried away.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = with_suffix(path, &format!("{tmp_suffix}.{}.{n}", std::process::id()));
    write_mode(&tmp, data, mode)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Copies the current contents aside before anything is changed.
///
/// The failure is fatal on purpose, and the message says so: if the backup
/// cannot be written there is no safe way to continue, because the next step
/// overwrites the only copy.
pub fn backup(path: &Path, suffix: &str, raw: &[u8], mode: u32) -> io::Result<()> {
    write_mode(&with_suffix(path, suffix), raw, mode).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("the backup failed, so nothing was installed: {e}"),
        )
    })
}

/// `path + suffix` as a path.
///
/// Built on `OsString` rather than `Path::with_extension`, which would eat the
/// existing extension: `settings.json` + `.herdr-tower.bak` has to stay
/// `settings.json.herdr-tower.bak`, not become `settings.herdr-tower.bak`.
pub fn with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// Creates the directory `path` lives in, and any missing ancestors, as 0755.
///
/// The mode is not passed through: `create_dir_all` asks for 0777 and lets the
/// umask narrow it, which on any normal umask lands on 0755. Spelling it out
/// would need a second `DirBuilder` path per platform to change nothing
/// observable.
pub fn create_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        // `Path::parent` of a bare filename is `Some("")`, which
        // `create_dir_all` would refuse; the parent is then the current
        // directory, which already exists.
        Some(d) if d.as_os_str().is_empty() => Ok(()),
        Some(d) => std::fs::create_dir_all(d),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two rewrites of one file must not share a temp path.
    ///
    /// They did: one renamed that inode into place while the other was still
    /// writing through its open descriptor, so a settings file this function
    /// exists to publish atomically could be observed partial or mixed — and the
    /// loser's rename failed on a name that had been carried away.
    #[test]
    fn concurrent_rewrites_do_not_share_a_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("settings.json");
        let a = "a".repeat(64 * 1024);
        let b = "b".repeat(64 * 1024);
        std::thread::scope(|s| {
            for body in [&a, &b] {
                let p = path.clone();
                s.spawn(move || {
                    for _ in 0..20 {
                        write_atomic(&p, ".tmp", body.as_bytes(), 0o600).unwrap();
                    }
                });
            }
        });

        // Whole, and one of the two — never a mixture.
        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got == a || got == b, "{} bytes, mixed", got.len());

        // And nothing is left beside it.
        let strays: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "settings.json")
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }
    use serde_json::json;

    /// Not a test of `tower_discord`'s encoder — that crate tests its own escaping.
    /// This asserts that *these installers* go through it, because the tempting
    /// swap (`serde_json::to_vec_pretty`, right there and one call shorter) is
    /// silent: it produces valid JSON that every parser accepts, and the only
    /// visible symptom would be a user's `~/.claude.json` reformatting an
    /// unrelated `&` on every `init`.
    #[test]
    fn the_installers_encode_through_tower_discords_escaping_not_serdes() {
        let v = json!({"cmd": "a<b>&c", "sep": "x\u{2028}y"});
        // Every backslash on the expected side is one the encoder has to put on
        // the wire.
        assert_eq!(
            String::from_utf8(tower_discord::to_sorted_json(&v).unwrap()).unwrap(),
            "{\"cmd\":\"a\\u003cb\\u003e\\u0026c\",\"sep\":\"x\\u2028y\"}"
        );
    }

    /// Sorted, because the recorded files have their keys sorted. Every expected
    /// file in this crate's tests depends on it, and it holds only as long as
    /// `serde_json`'s `preserve_order` feature stays off — which is a property of
    /// a manifest, not of this code.
    #[test]
    fn object_keys_come_out_sorted() {
        let v = json!({"z": 1, "a": {"y": [], "b": {}}});
        assert_eq!(
            String::from_utf8(tower_discord::to_sorted_json_pretty(&v).unwrap()).unwrap(),
            "{\n  \"a\": {\n    \"b\": {},\n    \"y\": []\n  },\n  \"z\": 1\n}"
        );
    }

    #[test]
    fn quoting_escapes_what_the_paths_that_reach_it_need() {
        assert_eq!(
            quote("/usr/local/bin/herdr-tower"),
            "\"/usr/local/bin/herdr-tower\""
        );
        assert_eq!(quote("/한/글 경로\"x\\y"), "\"/한/글 경로\\\"x\\\\y\"");
        assert_eq!(quote("a\tb\nc\u{1}"), "\"a\\tb\\nc\\u0001\"");
    }

    /// A small deterministic generator (an LCG), so the property tests walk the
    /// same inputs on every run and a failure can be replayed.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    /// Any character a path could hold, weighted toward the ones `quote`
    /// escapes: every ASCII control character (DEL included), the quote and
    /// the backslash, and non-ASCII — U+00A0 and U+2028 among it, since the
    /// doc comment promises those go out raw.
    fn any_char(rng: &mut Lcg) -> char {
        const SPECIAL: [char; 7] = ['"', '\\', '\u{7f}', 'é', '한', '\u{a0}', '\u{2028}'];
        match rng.below(4) {
            0 => char::from(rng.below(0x20) as u8),
            1 => SPECIAL[rng.below(SPECIAL.len() as u64) as usize],
            2 => '😀',
            _ => char::from(0x20 + rng.below(0x7f - 0x20) as u8),
        }
    }

    /// A TOML 1.0 basic string read back, written from the spec and nothing
    /// else: the only escapes are `\b \t \n \f \r \" \\ \uXXXX \UXXXXXXXX`, and
    /// a raw control character other than tab is an error.
    fn toml_basic_string(s: &str) -> Result<String, String> {
        let inner = s
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .ok_or_else(|| format!("not a quoted string: {s}"))?;
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    let e = chars.next().ok_or("a trailing backslash")?;
                    let hex = |n: usize, chars: &mut std::str::Chars| {
                        let h: String = chars.take(n).collect();
                        u32::from_str_radix(&h, 16)
                            .ok()
                            // `from_str_radix` takes a leading `+`; TOML does not.
                            .filter(|_| h.len() == n && h.bytes().all(|b| b.is_ascii_hexdigit()))
                            .and_then(char::from_u32)
                            .ok_or(format!("a bad \\{e} escape"))
                    };
                    out.push(match e {
                        'b' => '\u{8}',
                        't' => '\t',
                        'n' => '\n',
                        'f' => '\u{c}',
                        'r' => '\r',
                        '"' => '"',
                        '\\' => '\\',
                        'u' => hex(4, &mut chars)?,
                        'U' => hex(8, &mut chars)?,
                        other => return Err(format!("no such escape in TOML: \\{other}")),
                    });
                }
                '"' => return Err("a raw quote inside".into()),
                c if c != '\t' && ((c as u32) < 0x20 || c as u32 == 0x7f) => {
                    return Err(format!("a raw control character {:#x}", c as u32))
                }
                c => out.push(c),
            }
        }
        Ok(out)
    }

    /// The edge cases one by one: nothing at all, a string that is only
    /// escapes, the control characters TOML has no short escape for (which
    /// once went out as `\a`, `\v` and `\xNN`), and the non-ASCII the doc
    /// comment says is written raw rather than escaped.
    #[test]
    fn quoting_the_empty_string_escapes_and_raw_unicode() {
        let cases = [
            ("", "\"\""),
            ("\"\\", "\"\\\"\\\\\""),
            ("\u{8}\u{c}\r", "\"\\b\\f\\r\""),
            (
                "\u{7}\u{b}\0\u{1f}\u{7f}",
                "\"\\u0007\\u000b\\u0000\\u001f\\u007f\"",
            ),
            (
                "/opt/bin\u{7}/herdr-tower",
                "\"/opt/bin\\u0007/herdr-tower\"",
            ),
            ("a\u{a0}b\u{2028}c😀", "\"a\u{a0}b\u{2028}c😀\""),
        ];
        for (input, want) in cases {
            assert_eq!(quote(input), want, "{input:?}");
        }
    }

    /// Round trip and minimality over any characters (see [`any_char`]): what
    /// [`quote`] writes reads back through a spec-only TOML string reader as
    /// the input, and it escapes no more than it must — checked without
    /// restating `quote`. Every `\u` in the output decodes to an ASCII control
    /// character TOML has no short escape for; everything else in the input
    /// shows up raw or as one of the seven short escapes.
    #[test]
    fn quoted_strings_read_back_as_themselves() {
        const SHORT: [(char, char); 7] = [
            ('"', '"'),
            ('\\', '\\'),
            ('\u{8}', 'b'),
            ('\t', 't'),
            ('\n', 'n'),
            ('\u{c}', 'f'),
            ('\r', 'r'),
        ];
        let mut rng = Lcg(0x70_4d1);
        for _ in 0..2000 {
            let s: String = (0..rng.below(24)).map(|_| any_char(&mut rng)).collect();
            let q = quote(&s);
            assert_eq!(toml_basic_string(&q).as_deref(), Ok(s.as_str()), "{q}");

            let mut rest = q[1..q.len() - 1].chars();
            while let Some(c) = rest.next() {
                if c != '\\' {
                    assert!(!c.is_ascii_control(), "a raw control character in {q:?}");
                    continue;
                }
                match rest.next() {
                    Some('u') => {
                        let hex: String = rest.by_ref().take(4).collect();
                        let n = u32::from_str_radix(&hex, 16).expect("four hex digits");
                        let escaped = char::from_u32(n).expect("a character");
                        assert!(escaped.is_ascii_control(), "{escaped:?} escaped in {q:?}");
                        assert!(
                            SHORT.iter().all(|&(raw, _)| raw != escaped),
                            "{escaped:?} has a short escape, in {q:?}"
                        );
                    }
                    Some(e) => assert!(
                        SHORT.iter().any(|&(_, short)| short == e),
                        "\\{e} is not a TOML escape, in {q:?}"
                    ),
                    None => panic!("a trailing backslash in {q:?}"),
                }
            }
        }
    }

    /// Property and round trip for [`json_key`], which the installers use to
    /// decide whether a file already says what it should: two values get the
    /// same key exactly when they are equal, whatever order an object's keys
    /// were inserted in, and the key parses back as the value it came from.
    /// Numbers are integers only — `1` and `1.0` are different values to
    /// `serde_json` and are kept out of what is generated.
    #[test]
    fn json_keys_are_equal_exactly_when_the_values_are() {
        fn value(rng: &mut Lcg, depth: u32) -> Value {
            let pick = rng.below(if depth == 0 { 4 } else { 6 });
            match pick {
                0 => Value::Null,
                1 => Value::Bool(rng.below(2) == 0),
                2 => json!(rng.below(5) as i64 - 2),
                3 => Value::String(
                    ["", "a", "<b>", "&", "\u{2028}", "é"][rng.below(6) as usize].into(),
                ),
                4 => Value::Array((0..rng.below(3)).map(|_| value(rng, depth - 1)).collect()),
                _ => {
                    let mut entries: Vec<(String, Value)> = (0..rng.below(4))
                        .map(|_| {
                            (
                                ["x", "y", "<", "z"][rng.below(4) as usize].to_string(),
                                value(rng, depth - 1),
                            )
                        })
                        .collect();
                    if rng.below(2) == 0 {
                        entries.reverse();
                    }
                    Value::Object(entries.into_iter().collect())
                }
            }
        }
        let mut rng = Lcg(0x0b1ec7);
        let values: Vec<Value> = (0..300).map(|_| value(&mut rng, 3)).collect();
        for a in &values {
            let key = json_key(a);
            assert_eq!(serde_json::from_str::<Value>(&key).unwrap(), *a, "{key}");
            assert!(!key.contains(['<', '>', '&', '\u{2028}']), "{key}");
            for b in values.iter().take(60) {
                assert_eq!(key == json_key(b), a == b, "{a} vs {b}");
            }
        }

        // The same object with its keys inserted the other way round.
        let mut forward = Object::new();
        let mut backward = Object::new();
        for k in ["b", "a", "c"] {
            forward.insert(k.into(), json!(k));
        }
        for k in ["c", "a", "b"] {
            backward.insert(k.into(), json!(k));
        }
        assert_eq!(
            json_key(&Value::Object(forward)),
            json_key(&Value::Object(backward))
        );
    }

    /// There is no settings object to preserve in a bare `null` — only a
    /// choice, and the empty object is the one that lets every installer
    /// proceed with nothing lost.
    #[test]
    fn a_settings_file_holding_bare_null_reads_as_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, b"null").unwrap();
        let (raw, obj) = read_json_object(&p).unwrap();
        assert_eq!(raw.as_deref(), Some(&b"null"[..]));
        assert!(obj.is_empty());
    }

    #[test]
    fn a_settings_file_that_will_not_parse_stops_the_install() {
        let dir = tempfile::tempdir().unwrap();
        for body in [&b"{"[..], b"", b"[1,2]", b"7"] {
            let p = dir.path().join("settings.json");
            std::fs::write(&p, body).unwrap();
            let e = read_json_object(&p).expect_err("must refuse to guess");
            assert!(e.to_string().contains("would not parse"), "{e}");
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_object_with_nothing_to_back_up() {
        let dir = tempfile::tempdir().unwrap();
        let (raw, obj) = read_json_object(&dir.path().join("nope.json")).unwrap();
        assert!(raw.is_none());
        assert!(obj.is_empty());
    }

    /// The mode herdr-tower asks for wins over the mode the file had, because the
    /// rename puts a freshly created temporary in its place: a 0644 settings
    /// file comes back 0600. Pinned because the tempting "preserve the user's
    /// permissions" refactor would silently make `~/.claude.json` world-readable
    /// on a 0644 umask.
    #[cfg(unix)]
    #[test]
    fn the_written_mode_replaces_whatever_the_old_file_had() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        write_mode(&p, b"old", 0o644).unwrap();
        write_atomic(&p, TMP_SUFFIX, b"new", 0o600).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            !with_suffix(&p, TMP_SUFFIX).exists(),
            "the temporary is renamed, not left behind"
        );
    }

    #[test]
    fn a_backup_suffix_is_appended_rather_than_replacing_the_extension() {
        assert_eq!(
            with_suffix(Path::new("/a/settings.json"), BAK_SUFFIX),
            Path::new("/a/settings.json.herdr-tower.bak")
        );
    }

    /// `from_str_radix` takes a sign; a TOML `\u` escape does not.
    #[test]
    fn a_signed_unicode_escape_is_refused() {
        assert!(toml_first_basic_string(r#"k = "\u+041""#).is_err());
        assert!(toml_first_basic_string(r#"k = "\U+0000041""#).is_err());
        assert_eq!(toml_first_basic_string(r#"k = "\u0041""#).unwrap(), "A");
    }
}
