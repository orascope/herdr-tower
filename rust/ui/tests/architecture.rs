// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What the redraw path must never do, checked in the source.
//!
//! Every key press and every redraw runs on one thread (`crate::run`). A call
//! there that waits — a file, a child process, an ssh round trip — freezes the
//! whole tower for as long as it takes: every row, the preview, the clock.
//! `R`emote actions did exactly that for up to eight seconds on a machine that
//! had stopped answering, and no behaviour test could see it, because a stub
//! answers at once.
//!
//! So the rule is kept as a fact about the source. [`App`](tower_ui::App) and
//! the drawing modules decide; the work that waits belongs to the feed's
//! threads (`feed.rs`) or to the driver (`run.rs`), and comes back as an event.
//! A comment or a string that mentions one of these is not a call, and neither
//! is anything inside the file's `#[cfg(test)]` module.

use std::path::Path;

/// The modules on the redraw path: the state machine and what draws it.
const REDRAW_PATH: &[&str] = &["app.rs", "view.rs", "input.rs", "text.rs", "theme.rs"];

/// Calls that wait, and what to do instead.
const FORBIDDEN: &[(&str, &str)] = &[
    ("std::fs", "file I/O — read it on a feed thread"),
    ("fs::", "file I/O — read it on a feed thread"),
    (
        "Command::new",
        "a child process — run it from `run.rs` or a feed thread",
    ),
    (
        "std::process",
        "a child process — run it from `run.rs` or a feed thread",
    ),
    ("thread::sleep", "sleeping on the redraw path"),
    (
        "thread::spawn",
        "a thread of its own — return an `Action` for `run.rs`",
    ),
    ("TcpStream", "a network connection"),
    ("UnixStream", "a socket connection"),
    ("ureq::", "an HTTP request"),
    // The `Tower` methods only the feed's threads and the driver may call:
    // each is a round trip to herdr, git, gh or ssh.
    (
        ".remote_act(",
        "an ssh round trip — return `Action::Remote`",
    ),
    (
        ".remote_machines(",
        "an ssh round trip — the feed's remote thread reads it",
    ),
    (
        ".capture_pane(",
        "a pane read — the feed's preview thread does it",
    ),
    (
        ".open_prs(",
        "a gh round trip — the feed's PR thread does it",
    ),
    (
        ".checked_out_branches(",
        "git per folder — the feed does it",
    ),
    (
        ".hold_dock(",
        "herdr calls — the feed's dock thread does it",
    ),
];

#[test]
fn the_redraw_path_never_waits() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for name in REDRAW_PATH {
        let path = src.join(name);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for hit in violations(&production(&text)) {
            found.push(format!("{name}:{hit}"));
        }
    }
    assert!(
        found.is_empty(),
        "the redraw path must not wait:\n  {}",
        found.join("\n  ")
    );
}

/// `line: token — why` for every forbidden call in `code`.
fn violations(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (n, line) in code.lines().enumerate() {
        for (token, why) in FORBIDDEN {
            if contains_token(line, token) {
                out.push(format!("{}: `{token}` — {why}", n + 1));
            }
        }
    }
    out
}

/// `token` in `line`, not as the tail of a longer name: `fs::` matches
/// `std::fs::read` and `fs::write`, not `prefs::`.
fn contains_token(line: &str, token: &str) -> bool {
    let starts_word = token.starts_with(|c: char| c.is_alphanumeric() || c == '_');
    line.match_indices(token).any(|(at, _)| {
        !starts_word
            || !line[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// The source with comments and literals blanked and the test module cut off.
/// Line numbers are kept: everything blanked keeps its newlines.
fn production(text: &str) -> String {
    let masked = mask(text);
    match masked.find("#[cfg(test)]\nmod ") {
        Some(at) => masked[..at].to_string(),
        None => masked,
    }
}

/// Blanks comments, string literals and char literals, leaving newlines.
fn mask(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let blank = |out: &mut String, c: char| out.push(if c == '\n' { '\n' } else { ' ' });
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '/' if chars.get(i + 1).copied() == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    blank(&mut out, chars[i]);
                    i += 1;
                }
            }
            '/' if chars.get(i + 1).copied() == Some('*') => {
                let mut depth = 0;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1).copied() == Some('*') {
                        depth += 1;
                        out.push_str("  ");
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1).copied() == Some('/') {
                        depth -= 1;
                        out.push_str("  ");
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        blank(&mut out, chars[i]);
                        i += 1;
                    }
                }
            }
            // A raw string: r"…", r#"…"#, and the b/br forms.
            'r' if is_raw_start(&chars, i) => {
                let mut j = i + 1;
                while chars[j] == '#' {
                    j += 1;
                }
                let hashes = j - i - 1;
                // Through the opening quote.
                for &c in &chars[i..=j] {
                    blank(&mut out, c);
                }
                i = j + 1;
                while i < chars.len() {
                    let closes =
                        chars[i] == '"' && (1..=hashes).all(|k| chars.get(i + k) == Some(&'#'));
                    if closes {
                        for _ in 0..=hashes {
                            out.push(' ');
                        }
                        i += hashes + 1;
                        break;
                    }
                    blank(&mut out, chars[i]);
                    i += 1;
                }
            }
            '"' => {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        out.push(' ');
                        i += 1;
                        if i < chars.len() {
                            blank(&mut out, chars[i]);
                            i += 1;
                        }
                        continue;
                    }
                    blank(&mut out, chars[i]);
                    i += 1;
                }
                out.push(' ');
                i += 1;
            }
            // A char literal, not a lifetime: `'x'` or `'\n'`.
            '\'' if is_char_literal(&chars, i) => {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    if chars[i] == '\\' {
                        out.push(' ');
                        i += 1;
                    }
                    if i < chars.len() {
                        blank(&mut out, chars[i]);
                        i += 1;
                    }
                }
                out.push(' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `r` opening a raw string rather than ending a name like `bar"`: nothing
/// but a lone `b` (for `br"…"`) may stand right before it.
fn is_raw_start(chars: &[char], i: usize) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    if i > 0 && ident(chars[i - 1]) {
        let lone_b = chars[i - 1] == 'b' && (i < 2 || !ident(chars[i - 2]));
        if !lone_b {
            return false;
        }
    }
    let mut j = i + 1;
    while chars.get(j) == Some(&'#') {
        j += 1;
    }
    chars.get(j) == Some(&'"')
}

fn is_char_literal(chars: &[char], i: usize) -> bool {
    matches!(
        (chars.get(i + 1), chars.get(i + 2)),
        (Some('\\'), _) | (Some(_), Some('\''))
    )
}

#[test]
fn the_scanner_finds_a_call_in_code() {
    let code = production("fn f() {\n    let _ = std::fs::read(\"x\");\n}\n");
    assert_eq!(
        violations(&code),
        [
            "2: `std::fs` — file I/O — read it on a feed thread",
            "2: `fs::` — file I/O — read it on a feed thread"
        ]
    );
    let code = production("fn g(&self) {\n    self.tower.remote_act(&m, &a, &x);\n}\n");
    assert_eq!(violations(&code).len(), 1);
}

#[test]
fn the_scanner_ignores_what_is_not_a_call() {
    let code = production(concat!(
        "// std::fs::read is fine in a comment\n",
        "/* Command::new /* nested */ thread::sleep */\n",
        "let s = \"std::process::exit\";\n",
        "let r = r#\"fs::write \"quoted\" \"#;\n",
        "let c = '\"'; let d = '\\'';\n",
        "fn life<'a>(x: &'a str) -> &'a str { x }\n",
        "let prefs::x = 1;\n",
        "#[cfg(test)]\nmod tests {\n    fn t() { std::fs::read(\"y\"); }\n}\n",
    ));
    assert!(violations(&code).is_empty(), "{:?}", violations(&code));
}

#[test]
fn the_scanner_reads_on_after_a_lifetime_and_a_literal() {
    let code = production("fn f<'a>(x: &'a str) { let _ = '\"'; std::thread::sleep(d); }\n");
    assert_eq!(violations(&code).len(), 1, "{:?}", violations(&code));
}
