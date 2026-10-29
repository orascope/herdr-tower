// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower doctor`: whether what `init` wired up still works.
//!
//! Everything `init` registers fails silently by design — a hook that errors
//! must not disturb the agent that ran it — so a broken registration looks
//! exactly like an agent with nothing to say. The rows just stop moving. This
//! is the place that says so out loud.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use tower_state::{Agent, SUPERVISED_KINDS};

/// One `herdr-tower` path written into a file `init` manages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub file: PathBuf,
    pub bin: String,
}

/// The files `init` writes a binary path into.
pub fn registration_files(home: &Path, opencode_dir: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".claude").join("settings.json"),
        home.join(".codex").join("hooks.json"),
        home.join(".codex").join("config.toml"),
        home.join(".gemini").join("settings.json"),
        home.join(".gemini").join("config").join("hooks.json"),
        opencode_dir.join("plugins").join("herdr-tower.js"),
        crate::daemoncmd::service_path(home),
    ]
}

/// Every registered absolute binary path in `text`.
///
/// JSON and the plugin's BIN literal must be decoded before looking at paths:
/// their escaping belongs to the file format, not to the executable's name.
/// The other formats retain the literal scan used for older registrations.
pub fn binaries_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        json_binaries(&value, false, &mut out);
    } else {
        let mut raw = text;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let at = offset;
            offset += line.len();
            if let Some(literal) = line.trim_start().strip_prefix("const BIN = ") {
                let mut values = serde_json::Deserializer::from_str(literal).into_iter::<String>();
                if let Some(Ok(bin)) = values.next() {
                    let start = text.len() - raw.len();
                    out.extend(literal_binaries(&text[start..at]));
                    add_binary(&bin, &mut out);
                    raw = &text[at + line.len()..];
                }
            }
        }
        out.extend(literal_binaries(raw));
    }
    if cfg!(windows) {
        out.extend(windows_binaries(text));
    }
    let mut unique = Vec::new();
    for bin in out {
        if !unique.contains(&bin) {
            unique.push(bin);
        }
    }
    unique
}

/// Keys can name project directories; only values can name registrations.
/// A command containing just a path is an argv-style MCP registration.
fn json_binaries(value: &serde_json::Value, command: bool, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                json_binaries(value, key == "command", out);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                json_binaries(value, false, out);
            }
        }
        serde_json::Value::String(text) => {
            if command && names_us(text) {
                add_binary(text, out);
            } else if let Some((bin, after)) =
                // Only a quoted herdr-tower: `'/usr/bin/env' /x/herdr-tower …`
                // names it unquoted, further on.
                quoted_binary(text).filter(|(bin, _)| bin.ends_with("/herdr-tower"))
            {
                if invoked("", after) {
                    add_binary(&bin, out);
                }
            } else {
                out.extend(literal_binaries(text));
            }
        }
        _ => {}
    }
}

/// Undo the single shell word `sh_quote` writes, including its apostrophe
/// splice. Splitting on whitespace or the last quote loses part of the path.
fn quoted_binary(text: &str) -> Option<(String, &str)> {
    let mut rest = text.strip_prefix('\'')?;
    let mut bin = String::new();
    loop {
        let close = rest.find('\'')?;
        bin.push_str(&rest[..close]);
        rest = &rest[close..];
        if let Some(next) = rest.strip_prefix("'\\''") {
            bin.push('\'');
            rest = next;
        } else {
            return Some((bin, &rest[1..]));
        }
    }
}

fn add_binary(bin: &str, out: &mut Vec<String>) {
    if names_us(bin) {
        out.push(bin.to_string());
    }
}

/// An absolute path to this binary: `/…/herdr-tower`, or on Windows
/// `C:\…\herdr-tower.exe` with either slash.
fn names_us(bin: &str) -> bool {
    if bin.starts_with('/') && bin.ends_with("/herdr-tower") {
        return true;
    }
    cfg!(windows) && windows_absolute(bin) && windows_names_us(bin)
}

fn windows_absolute(bin: &str) -> bool {
    let b = bin.as_bytes();
    (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
        || bin.starts_with("\\\\")
}

fn windows_names_us(bin: &str) -> bool {
    let lower = bin.to_ascii_lowercase();
    lower.ends_with("/herdr-tower.exe") || lower.ends_with("\\herdr-tower.exe")
}

/// The Windows spellings `init` writes and a person may: `C:/…/herdr-tower.exe`
/// bare or in double quotes, and `C:\\…` with each backslash doubled where
/// the file escapes it (TOML, JSON read as text).
fn windows_binaries(text: &str) -> Vec<String> {
    const NAME: &str = "herdr-tower.exe";
    let lower = text.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    for (at, _) in lower.match_indices(NAME) {
        let end = at + NAME.len();
        if !text[..at].ends_with(['/', '\\']) {
            continue;
        }
        if text[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || "-_.".contains(c))
        {
            continue;
        }
        // In double quotes the path may hold a space; bare, it ends at one.
        let start = if text[end..].starts_with('"') || text[end..].starts_with("\\\"") {
            text[..at].rfind('"').map(|i| i + 1)
        } else {
            text[..at]
                .rfind(|c: char| c.is_whitespace() || "\"'`,;=<>()[]{}".contains(c))
                .map(|i| i + 1)
        }
        .unwrap_or(0);
        // An escaped closing quote (`\"`) is still the quote.
        if !invoked(&text[..start], text[end..].trim_start_matches('\\')) {
            continue;
        }
        let path = text[start..end].replace("\\\\", "\\");
        if windows_absolute(&path) && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn literal_binaries(text: &str) -> Vec<String> {
    const NAME: &str = "/herdr-tower";
    let bounds = |c: char| c.is_whitespace() || "\"'`,;=<>()[]{}".contains(c);
    let mut out: Vec<String> = Vec::new();
    for (at, _) in text.match_indices(NAME) {
        let end = at + NAME.len();
        // A suffix names another file; a slash makes this a directory.
        if text[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || "-_./".contains(c))
        {
            continue;
        }
        // Past the bound, whose width is not always one byte (NBSP, U+3000).
        let start = quoted_start(text, at, end).unwrap_or_else(|| {
            text[..at].rfind(bounds).map_or(0, |i| {
                i + text[i..].chars().next().map_or(1, char::len_utf8)
            })
        });
        let path = &text[start..end];
        if !invoked(&text[..start], &text[end..]) {
            continue;
        }
        if path.starts_with('/') && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
}

/// Where a single-quoted path ending at `end` starts, if it is one.
///
/// `init` writes the hook commands with the path as one shell word —
/// `'/opt/my tools/herdr-tower' hook claude …` — so a path may hold a space,
/// which the plain scan takes for a bound and so would never see. A quote right
/// after the name, and an opening one before it with no other quote, double
/// quote or line in between, is that word; its inside is the path. An entry an
/// older build wrote unquoted has no closing quote and takes the plain scan,
/// so doctor reads both.
fn quoted_start(text: &str, at: usize, end: usize) -> Option<usize> {
    if !text[end..].starts_with('\'') {
        return None;
    }
    let open = text[..at].rfind('\'')?;
    let inside = &text[open + 1..at];
    (!inside.contains(['"', '\n'])).then_some(open + 1)
}

/// Whether a path sits where something runs it, rather than being a folder
/// that happens to share the name — a project folder in a settings file, the
/// state directory in the service definition.
///
/// Run means: a subcommand of ours follows it (`… hook claude`, the service's
/// `daemon run`, argv arrays), or it is the value of a `command` key (an MCP
/// entry, whose subcommand is in a separate `args`) or the plugin's `BIN`.
fn invoked(before: &str, after: &str) -> bool {
    let next = after
        .trim_start_matches(|c: char| c.is_whitespace() || "\"',".contains(c))
        .trim_start_matches("</string>")
        .trim_start_matches(char::is_whitespace)
        .trim_start_matches("<string>");
    if ["hook", "daemon"].iter().any(|w| next.starts_with(w)) {
        return true;
    }
    let prev = before.trim_end_matches(|c: char| c.is_whitespace() || "\"'".contains(c));
    prev.ends_with("\"command\":") || prev.ends_with("BIN =") || prev.ends_with("command =")
}

/// The registrations found in `files`; a file that cannot be read has none.
pub fn registrations(files: &[PathBuf]) -> Vec<Registration> {
    files
        .iter()
        .filter_map(|f| Some((f, std::fs::read_to_string(f).ok()?)))
        .flat_map(|(f, text)| {
            binaries_in(&text).into_iter().map(|bin| Registration {
                file: f.clone(),
                bin,
            })
        })
        .collect()
}

/// What doctor found about one thing, and whether it is a problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub ok: bool,
    pub line: String,
}

impl Finding {
    fn ok(line: String) -> Self {
        Finding { ok: true, line }
    }
    fn bad(line: String) -> Self {
        Finding { ok: false, line }
    }
}

/// Each registration against the binary running now.
///
/// A missing file is the failure this exists for: every event sent there is
/// lost. A path to some other, existing build is only worth a note — it works,
/// but it is not this version.
pub fn check_registrations(regs: &[Registration], this: &Path) -> Vec<Finding> {
    let this_real = std::fs::canonicalize(this).ok();
    regs.iter()
        .map(|r| {
            let bin = Path::new(&r.bin);
            let file = r.file.display();
            if !bin.is_file() {
                return Finding::bad(format!(
                    "{file}: {} does not exist — every event sent there is lost",
                    r.bin
                ));
            }
            let same = this_real.is_some() && std::fs::canonicalize(bin).ok() == this_real;
            if same {
                Finding::ok(format!("{file}: {}", r.bin))
            } else {
                Finding::ok(format!(
                    "{file}: {} (a different build from this one, {})",
                    r.bin,
                    this.display()
                ))
            }
        })
        .collect()
}

/// Whether the installed opencode plugin is the one this build would write.
pub fn check_opencode_plugin(opencode_dir: &Path, bin: &str) -> Option<Finding> {
    if !opencode_dir.is_dir() {
        return None;
    }
    let path = opencode_dir.join("plugins").join("herdr-tower.js");
    Some(match std::fs::read_to_string(&path) {
        Err(_) => Finding::bad(format!(
            "{}: not installed — opencode rows get no task, question or context",
            path.display()
        )),
        Ok(have) if have == crate::opencodeinit::render(bin) => {
            Finding::ok(format!("{}: current", path.display()))
        }
        Ok(_) => Finding::bad(format!("{}: from another build or version", path.display())),
    })
}

/// When a hook last reached each kind's live records.
///
/// Informational: a kind nobody ran recently has no hooks to show, which is
/// not a fault. The number that matters is a recent one that is missing.
pub fn last_hooks(agents: &[Agent], now: OffsetDateTime) -> Vec<String> {
    SUPERVISED_KINDS
        .iter()
        .filter_map(|kind| {
            let live: Vec<&Agent> = agents
                .iter()
                .filter(|a| a.kind == *kind && a.is_live())
                .collect();
            if live.is_empty() {
                return None;
            }
            let last = live
                .iter()
                .map(|a| a.last_hook)
                .max()
                .filter(|t| *t != OffsetDateTime::UNIX_EPOCH);
            Some(match last {
                Some(t) => format!(
                    "{kind}: {} live, last hook {} ago",
                    live.len(),
                    ago(now - t)
                ),
                None if *kind == "codex" => format!(
                    "{kind}: {} live, no hook ever — codex runs a hook only once trusted: /hooks, then t",
                    live.len()
                ),
                None => format!("{kind}: {} live, no hook ever", live.len()),
            })
        })
        .collect()
}

fn ago(d: time::Duration) -> String {
    let s = d.whole_seconds().max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Prints one section and says how many problems it held.
pub fn print_section(w: &mut impl Write, title: &str, findings: &[Finding]) -> io::Result<usize> {
    writeln!(w, "{title}")?;
    if findings.is_empty() {
        writeln!(w, "  (nothing registered)")?;
    }
    for f in findings {
        writeln!(w, "  {} {}", if f.ok { "ok " } else { "BAD" }, f.line)?;
    }
    Ok(findings.iter().filter(|f| !f.ok).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows spellings: what `init` writes there, and the escaped forms
    /// a TOML or JSON file holds.
    #[test]
    fn windows_paths_are_found_in_every_spelling() {
        let json =
            r#"{"command": "C:/Users/u/.local/bin/herdr-tower.exe hook claude --event stop"}"#;
        assert_eq!(
            windows_binaries(json),
            ["C:/Users/u/.local/bin/herdr-tower.exe"]
        );
        let quoted = r#""\"C:/Program Files/herdr-tower.exe\" hook claude""#;
        assert_eq!(
            windows_binaries(quoted),
            ["C:/Program Files/herdr-tower.exe"]
        );
        let toml = r#"notify = ["C:\\Users\\u\\herdr-tower.exe", "hook", "codex"]"#;
        assert_eq!(windows_binaries(toml), [r"C:\Users\u\herdr-tower.exe"]);
        assert!(windows_binaries("C:/x/herdr-tower.exe.bak hook").is_empty());
        assert!(windows_binaries("C:/proj/herdr-tower.exe/notes").is_empty());
    }

    /// Real writers exercise both layers of quoting; repeated hooks must
    /// still produce exactly one registration, even with mixed boundaries.
    #[cfg(unix)]
    #[test]
    fn written_paths_round_trip_through_the_registration_scan() {
        for bin in [
            "/a'b/herdr-tower",
            r#"/a"b/herdr-tower"#,
            r"/a\b/herdr-tower",
            "/<x>&y/herdr-tower",
            "/opt/my tools/herdr-tower",
            "/opt/herdr-tower/bin/herdr-tower",
            "/Users/q/herdr-tower/rust/target/release/herdr-tower",
            "/ü/한글/herdr-tower",
        ] {
            assert_written_path(bin);
        }
        let pieces = [
            "a",
            "'",
            "\"",
            "\\",
            "<",
            ">",
            "&",
            " ",
            "ü",
            "한글",
            "`",
            ",",
            ";",
            "=",
            "(",
            ")",
            "[",
            "]",
            "{",
            "}",
            "\n",
            "\t",
            "/herdr-tower/",
            "\u{3000}",
        ];
        let mut rng = tower_testrng::Rng::new(0xD0C70);
        for case in 0..256 {
            let mut bin = format!("/generated/{case}/");
            for _ in 0..(1 + rng.below(30)) {
                bin.push_str(pieces[rng.below(pieces.len())]);
            }
            bin.push_str("/herdr-tower");
            assert_written_path(&bin);
        }
    }

    #[cfg(unix)]
    fn assert_written_path(bin: &str) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        crate::initcmd::install_claude_hooks(&mut Vec::new(), &file, bin, false).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(binaries_in(&text), [bin], "hook path {bin:?}");
        assert_eq!(
            binaries_in(&crate::opencodeinit::render(bin)),
            [bin],
            "plugin path {bin:?}"
        );
        let argv = serde_json::json!({"command": bin, "args": ["daemon", "run"]});
        assert_eq!(binaries_in(&argv.to_string()), [bin], "argv path {bin:?}");
    }

    /// The command-key shortcut must also reject directories in raw formats.
    #[test]
    fn intermediate_binary_named_directories_are_skipped() {
        for bin in [
            "/opt/herdr-tower/bin/herdr-tower",
            "/Users/q/herdr-tower/rust/target/release/herdr-tower",
        ] {
            assert_eq!(binaries_in(&format!("command = \"{bin}\"")), [bin]);
        }
    }

    #[test]
    fn binaries_are_found_in_every_format_init_writes() {
        let json = r#"{"command": "/Users/q/.local/bin/herdr-tower hook claude --event stop"}"#;
        assert_eq!(binaries_in(json), ["/Users/q/.local/bin/herdr-tower"]);

        let toml = r#"notify = ["/opt/homebrew/bin/herdr-tower", "hook", "codex"]"#;
        assert_eq!(binaries_in(toml), ["/opt/homebrew/bin/herdr-tower"]);

        let plist =
            "<string>/home/linuxbrew/.linuxbrew/bin/herdr-tower</string><string>daemon</string>";
        assert_eq!(
            binaries_in(plist),
            ["/home/linuxbrew/.linuxbrew/bin/herdr-tower"]
        );

        let unit = "ExecStart=/usr/local/bin/herdr-tower daemon run";
        assert_eq!(binaries_in(unit), ["/usr/local/bin/herdr-tower"]);

        let js = r#"const BIN = "/opt/x/herdr-tower";"#;
        assert_eq!(binaries_in(js), ["/opt/x/herdr-tower"]);
    }

    /// The hook commands `init` writes now quote the path, and the ones an
    /// older build wrote do not: both are registrations, and a quoted path
    /// with a space in it is found whole.
    #[cfg(unix)]
    #[test]
    fn quoted_and_unquoted_hook_commands_are_both_found() {
        let old = r#"{"command": "/u/bin/herdr-tower hook claude --event stop"}"#;
        assert_eq!(binaries_in(old), ["/u/bin/herdr-tower"]);

        let new = r#"{"command": "'/u/bin/herdr-tower' hook claude --event stop"}"#;
        assert_eq!(binaries_in(new), ["/u/bin/herdr-tower"]);

        let spaced = r#"{"command": "'/opt/my tools/herdr-tower' hook gemini --event stop"}"#;
        assert_eq!(binaries_in(spaced), ["/opt/my tools/herdr-tower"]);

        // Another program quoted first: the herdr-tower after it still counts.
        let wrapped = r#"{"command": "'/usr/bin/env' /opt/bin/herdr-tower hook claude"}"#;
        assert_eq!(binaries_in(wrapped), ["/opt/bin/herdr-tower"]);
        let wrapped = r#"{"command": "'/usr/bin/env' '/opt/my tools/herdr-tower' hook"}"#;
        assert_eq!(binaries_in(wrapped), ["/opt/my tools/herdr-tower"]);

        let both = format!("{old}\n{spaced}");
        assert_eq!(
            binaries_in(&both),
            ["/u/bin/herdr-tower", "/opt/my tools/herdr-tower"]
        );

        // What `init` writes is what doctor reads, whatever the path holds.
        for bin in ["/abs/herdr-tower", "/opt/my tools/herdr-tower"] {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("settings.json");
            crate::initcmd::install_claude_hooks(&mut Vec::new(), &p, bin, false).unwrap();
            let text = std::fs::read_to_string(&p).unwrap();
            assert_eq!(binaries_in(&text), [bin], "{text}");
        }

        // A quoted word that is not followed by one of our subcommands is
        // still not a registration.
        assert!(binaries_in("'/Users/q/herdr-tower' is a folder").is_empty());
    }

    /// A folder named like the binary is not a registration: a project key in
    /// a settings file, the service's state directory.
    #[test]
    fn folders_that_share_the_name_are_not_registrations() {
        let json = r#"{"projects": {"/Users/q/herdr-tower": {"x": 1}},
            "hooks": {"command": "/u/bin/herdr-tower hook claude --event stop"}}"#;
        assert_eq!(binaries_in(json), ["/u/bin/herdr-tower"]);

        let plist = "<array><string>/u/bin/herdr-tower</string>\n<string>daemon</string></array>\n\
            <key>HERDR_TOWER_STATE_DIR</key><string>/u/.local/state/herdr-tower</string>";
        assert_eq!(binaries_in(plist), ["/u/bin/herdr-tower"]);
    }

    /// Paths that only share the prefix, relative mentions and repeats are
    /// not registrations of their own.
    #[test]
    fn near_misses_and_repeats_are_not_counted() {
        let text = r#"/x/herdr-tower-path /x/herdr-tower.js herdr-tower hook
            "/b/herdr-tower hook a" "/b/herdr-tower hook b" .herdr-tower/worktrees"#;
        assert_eq!(binaries_in(text), ["/b/herdr-tower"]);
    }

    #[test]
    fn a_missing_binary_is_a_problem_and_another_build_is_a_note() {
        let d = tempfile::tempdir().unwrap();
        let this = d.path().join("this/herdr-tower");
        let other = d.path().join("other/herdr-tower");
        for p in [&this, &other] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        let reg = |bin: &Path| Registration {
            file: d.path().join("settings.json"),
            bin: bin.display().to_string(),
        };
        let gone = d.path().join("Cellar/0.19.9/herdr-tower");
        let got = check_registrations(&[reg(&this), reg(&other), reg(&gone)], &this);
        assert!(got[0].ok && !got[0].line.contains("different"));
        assert!(got[1].ok && got[1].line.contains("a different build"));
        assert!(!got[2].ok && got[2].line.contains("does not exist"));
    }

    #[test]
    fn the_opencode_plugin_is_checked_against_this_build() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(check_opencode_plugin(&d.path().join("none"), "/b"), None);

        let got = check_opencode_plugin(d.path(), "/b").unwrap();
        assert!(!got.ok && got.line.contains("not installed"));

        let mut out = Vec::new();
        crate::opencodeinit::install_opencode_plugin(&mut out, d.path(), "/b", false).unwrap();
        assert!(check_opencode_plugin(d.path(), "/b").unwrap().ok);
        assert!(!check_opencode_plugin(d.path(), "/c").unwrap().ok);
    }

    #[test]
    fn last_hooks_are_per_kind_over_live_records() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut a = Agent::new("claude-1", "claude", now);
        a.mark_hook(now - time::Duration::minutes(3));
        let b = Agent::new("codex-1", "codex", now);
        let mut dead = Agent::new("gemini-1", "gemini", now);
        dead.state = tower_state::AgentState::Dead;
        assert_eq!(
            last_hooks(&[a, b, dead], now),
            [
                "claude: 1 live, last hook 3m ago",
                "codex: 1 live, no hook ever — codex runs a hook only once trusted: /hooks, then t"
            ]
        );
    }

    /// A bound wider than a byte before the path — NBSP, U+3000, both
    /// whitespace — is stepped over whole rather than sliced inside.
    #[test]
    fn a_multi_byte_space_before_the_path_is_stepped_over() {
        for sp in ['\u{a0}', '\u{3000}', ' '] {
            let text = format!("run{sp}/opt/bin/herdr-tower hook claude");
            assert_eq!(binaries_in(&text), ["/opt/bin/herdr-tower"], "{sp:?}");
        }
    }

    /// A generated absolute binary path: folders of letters, spaces, unicode
    /// (NBSP and U+3000 included) and the shell and markup characters the
    /// plain scan treats as bounds. Left out: the characters the settings
    /// file stores escaped (`'` is fine in the file but splits the quoted
    /// word; `"`, `\`, `<`, `>`, `&` are written as JSON escapes), which the
    /// raw-text scan does not decode; and a folder named `herdr-tower` on the
    /// way, which the scan reports as a registration of its own.
    #[cfg(unix)]
    fn generated_bin(rng: &mut tower_testrng::Rng) -> String {
        const PIECES: &[&str] = &[
            "a", "bin", " ", "my tools", "세션", "😀", "é", "\u{a0}", "\u{3000}", "$", ";", "(",
            ")", "[", "]", "{", "}", "=", ",", "`", "#", "!", "*", "%", "~", "-", ".",
        ];
        let mut bin = String::new();
        for _ in 0..1 + rng.below(3) {
            bin.push('/');
            for _ in 0..1 + rng.below(4) {
                bin.push_str(rng.pick(PIECES).unwrap());
            }
        }
        bin.push_str("/herdr-tower");
        bin
    }

    /// Round trip over generated paths (seeded): what `init` writes into a
    /// fresh settings file — the path as one single-quoted shell word — is
    /// what doctor reads back from that file, exactly and once.
    #[cfg(unix)]
    #[test]
    fn init_written_paths_read_back_through_doctor() {
        let mut rng = tower_testrng::Rng::new(0xD0C7_0417);
        let d = tempfile::tempdir().unwrap();
        for i in 0..150 {
            let bin = generated_bin(&mut rng);
            let p = d.path().join(format!("settings-{i}.json"));
            crate::initcmd::install_claude_hooks(&mut Vec::new(), &p, &bin, false).unwrap();
            let regs = registrations(std::slice::from_ref(&p));
            let bins: Vec<&str> = regs.iter().map(|r| r.bin.as_str()).collect();
            assert_eq!(bins, [bin.as_str()], "{bin:?}");
            // A second install of another path replaces it, and doctor sees
            // only the new one.
            let moved = generated_bin(&mut rng);
            crate::initcmd::install_claude_hooks(&mut Vec::new(), &p, &moved, false).unwrap();
            let text = std::fs::read_to_string(&p).unwrap();
            assert_eq!(binaries_in(&text), [moved.as_str()], "{moved:?}");
        }
    }

    /// Round trip for the legacy, unquoted spelling an older build wrote:
    /// a generated path with no bound character in it reads back whole, in
    /// every place `init` has written one (hook command, TOML argv, plist,
    /// unit, plugin `BIN`), and a quoted spelling of the same path does too.
    #[test]
    fn legacy_unquoted_and_quoted_spellings_read_back() {
        const PIECES: &[&str] = &[
            "a", "bin", "세션", "😀", "é", "-", ".", "_", "x1", "~", "%", "!",
        ];
        let mut rng = tower_testrng::Rng::new(0x001E_6AC7);
        for _ in 0..200 {
            let mut bin = String::new();
            for _ in 0..1 + rng.below(3) {
                bin.push('/');
                for _ in 0..1 + rng.below(3) {
                    bin.push_str(rng.pick(PIECES).unwrap());
                }
            }
            bin.push_str("/herdr-tower");
            for text in [
                format!(r#"{{"command": "{bin} hook claude --event stop"}}"#),
                format!(r#"{{"command": "'{bin}' hook claude --event stop"}}"#),
                format!(r#"notify = ["{bin}", "hook", "codex"]"#),
                format!("<string>{bin}</string><string>daemon</string>"),
                format!("ExecStart={bin} daemon run"),
                format!(r#"const BIN = "{bin}";"#),
            ] {
                assert_eq!(binaries_in(&text), [bin.as_str()], "{text}");
            }
        }
    }
}
