// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What `init` writes into codex's own configuration: the `notify` command and
//! the hooks file.
//!
//! Two different files, two different ownership rules, and they are what the
//! module is about:
//!
//! - **`config.toml`** is a TOML document with hand-written content. herdr-tower
//!   inserts one top-level key and never reformats. Someone else's `notify` wins.
//! - **`hooks.json`** is shared with codex's own `/hooks` UI. herdr-tower owns
//!   exactly the matcher groups whose command carries [`HOOK_MARK`], replaces
//!   those, and does not look at any other group.

use std::io::{self, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::initcmd::settingsfile::{self, BAK_SUFFIX, TMP_SUFFIX};
use crate::initcmd::HookEvent;

/// The `notify` line for this binary.
fn notify_line(bin_path: &str) -> String {
    let bin_path = if bin_path.is_empty() {
        "herdr-tower"
    } else {
        bin_path
    };
    // Escaped, not interpolated. A Unix path may contain `"` or `\` — both legal
    // in a path component — and either one written raw changes the value TOML
    // reads or makes the file invalid, which costs the user their whole codex
    // configuration rather than just this line.
    format!(
        "notify = [{}, \"hook\", \"codex\"]",
        crate::initcmd::settingsfile::quote(bin_path)
    )
}

/// Adds `notify` to `~/.codex/config.toml`.
///
/// The insertion point is the start of the first section header line, because a
/// TOML top-level key has to precede every `[table]` — put it after one and it
/// silently becomes a key *of that table*.
///
/// Recorded 2026-09-07 (Windows VM): inserting at the header's offset *minus*
/// the preceding newline produced `approvals_reviewer = "auto_review"notify =
/// [...]` and codex refused to load its config at all. The `idx > 0` bump below
/// is that fix — step past the newline so the insertion lands at the beginning
/// of the header line rather than the end of the line before it.
pub fn install_codex_notify(
    w: &mut impl Write,
    config_path: &Path,
    bin_path: &str,
    dry_run: bool,
) -> io::Result<()> {
    let line = notify_line(bin_path);
    let raw = match std::fs::read(config_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if dry_run {
                writeln!(w, "  {} will be created: {line}", config_path.display())?;
                return Ok(());
            }
            // No `create_dir_all` here: `init` only reaches this function when
            // `~/.codex` already exists, and creating the folder for a codex
            // that is not installed would leave litter.
            return settingsfile::write_mode(config_path, format!("{line}\n").as_bytes(), 0o600);
        }
        Err(e) => return Err(e),
    };
    // Scanned and spliced as **bytes**, not as a `String`. Every delimiter this
    // needs is ASCII, and the alternative costs a user's file: `from_utf8_lossy`
    // would substitute U+FFFD for any byte sequence that is not valid UTF-8 and
    // then write that back, where splicing bytes carries them through
    // untouched. TOML is UTF-8 by specification, so the case is unreachable in a
    // healthy file — but "unreachable" is not a reason to corrupt the file when
    // it happens.
    for l in raw.split(|&b| b == b'\n') {
        let trimmed = l.trim_ascii();
        if trimmed.starts_with(b"[") {
            break; // end of the top-level region — no notify here
        }
        if !trimmed.starts_with(b"notify") {
            continue;
        }
        // Prefix, not equality: this is what makes an unrelated top-level key
        // like `notify_on_error` read as "already configured" and skip the
        // install entirely (verified 2026-09-11). Left as it is because the
        // alternative — matching `notify` exactly — would have to decide what
        // `notify=[...]` without spaces means, and the conservative reading of
        // an ambiguous line in somebody else's file is "leave it alone".
        if trimmed == line.as_bytes() {
            writeln!(w, "  codex notify: already registered — skipped")?;
            return Ok(());
        }
        writeln!(
            w,
            "  codex notify: already configured — skipped (the existing setting wins)"
        )?;
        return Ok(());
    }

    writeln!(w, "  codex notify: {line}")?;
    if dry_run {
        writeln!(w, "  (dry-run — no file was changed)")?;
        return Ok(());
    }
    settingsfile::backup(config_path, BAK_SUFFIX, &raw, 0o600)?;
    let updated = insert_top_level(&raw, line.as_bytes());
    settingsfile::write_atomic(config_path, TMP_SUFFIX, &updated, 0o600)
}

/// Splices `line` in just before the first `[section]`, or onto the end when
/// there is none.
fn insert_top_level(content: &[u8], line: &[u8]) -> Vec<u8> {
    let mut idx = content.len();
    let mut offset = 0;
    for l in content.split(|&b| b == b'\n') {
        if l.trim_ascii().starts_with(b"[") {
            idx = offset;
            break;
        }
        offset += l.len() + 1; // the newline `split` consumed
    }
    let mut out = Vec::with_capacity(content.len() + line.len() + 2);
    if idx >= content.len() {
        // No section at all: append, adding the newline the file is missing so
        // the new key does not land on the end of the last one.
        out.extend_from_slice(content);
        if !content.is_empty() && !content.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    } else {
        // `content[..idx]` ends with the newline before the header, so the new
        // key cannot land on the end of the line above it.
        out.extend_from_slice(&content[..idx]);
        out.extend_from_slice(line);
        out.push(b'\n');
        out.extend_from_slice(&content[idx..]);
    }
    out
}

/// The subcommand and argument shape our codex hooks use.
///
/// codex's own `/hooks` UI writes into the same file, so ownership has to be
/// legible from the group's contents alone — there is no field to put a name in.
///
/// Not sufficient on its own: this shape is generic enough that another tool
/// could write it, and `is_ours` drops a group it claims. The executable's own
/// name has to appear too — see [`is_ours`].
pub const HOOK_MARK: &str = " hook codex --event ";

/// The binary name a group of ours is registered under.
pub(crate) const OUR_NAME: &str = "herdr-tower";

/// The codex events herdr-tower registers.
///
/// `notify` (agent-turn-complete) is deliberately not among them: it fires only
/// at the end of a turn, so codex showed as DONE for the whole time it was
/// working (2026-09-03). `PreToolUse` is left out too — its output is read as an
/// approval decision, and a supervisor must not answer approval prompts.
pub const HOOK_EVENTS: [HookEvent; 5] = [
    HookEvent {
        settings_key: "SessionStart",
        event_arg: "session-start",
    },
    HookEvent {
        settings_key: "UserPromptSubmit",
        event_arg: "user-prompt-submit",
    },
    HookEvent {
        settings_key: "PostToolUse",
        event_arg: "post-tool-use",
    },
    HookEvent {
        settings_key: "PermissionRequest",
        event_arg: "permission-request",
    },
    HookEvent {
        settings_key: "Stop",
        event_arg: "stop",
    },
];

/// Registers herdr-tower's hooks in `~/.codex/hooks.json`.
///
/// File shape: `{"hooks": {"<Event>": [{"matcher"?, "hooks": [{type, command,
/// timeout}]}]}}`. Per event there is exactly one herdr-tower group; every group
/// that is not ours is copied through untouched, and duplicates of ours — from an
/// older binary path, or from running two builds — collapse to one.
///
/// The advice at the end is not decoration: codex refuses to run a hook it has
/// not been trusted with, so an install that says nothing looks like an install
/// that did not work.
pub fn install_codex_hooks(
    w: &mut impl Write,
    hooks_path: &Path,
    bin_path: &str,
    dry_run: bool,
) -> io::Result<()> {
    let bin_path = if bin_path.is_empty() {
        "herdr-tower"
    } else {
        bin_path
    };
    let (raw, mut root) = settingsfile::read_json_object(hooks_path)?;
    let mut hooks = match root.get("hooks") {
        Some(Value::Object(o)) => o.clone(),
        _ => settingsfile::Object::new(),
    };

    let mut changed = false;
    for e in HOOK_EVENTS {
        let want = json!({"hooks": [{
            "type": "command",
            "command": format!("{}{HOOK_MARK}{}", shell_word(bin_path), e.event_arg),
            "timeout": 10,
        }]});
        let groups = match hooks.get(e.settings_key) {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let mut kept: Vec<Value> = Vec::new();
        let mut found = false;
        for g in groups {
            if !is_ours(&g) {
                kept.push(g);
                continue;
            }
            // Compared as encoded bytes, so a `timeout` that came back from
            // the file as a float still equals the integer we want. Both sides
            // go through the same encoder.
            if !found && settingsfile::json_key(&g) == settingsfile::json_key(&want) {
                kept.push(g);
                found = true;
                continue;
            }
            changed = true; // an older path, or a duplicate — dropped
        }
        if !found {
            kept.push(want);
            changed = true;
        }
        hooks.insert(e.settings_key.to_string(), Value::Array(kept));
    }
    if !changed {
        writeln!(w, "  herdr-tower: already registered — skipped")?;
        return Ok(());
    }
    root.insert("hooks".to_string(), Value::Object(hooks));
    writeln!(
        w,
        "  herdr-tower: SessionStart·UserPromptSubmit·PostToolUse·PermissionRequest·Stop → {bin_path} hook codex"
    )?;
    if dry_run {
        writeln!(w, "(dry-run — no file was changed)")?;
        return Ok(());
    }
    if let Some(raw) = &raw {
        settingsfile::backup(hooks_path, BAK_SUFFIX, raw, 0o600)?;
    }
    let mut body = tower_discord::to_sorted_json_pretty(&Value::Object(root))?;
    body.push(b'\n');
    settingsfile::create_parent(hooks_path)?;
    settingsfile::write_atomic(hooks_path, TMP_SUFFIX, &body, 0o600)?;
    writeln!(
        w,
        "  ⚠ codex will not run a new hook until it is trusted — allow the herdr-tower entry once with /hooks inside codex"
    )
}

/// The binary's path as one word of the shell codex runs the hook in
/// (`$SHELL -lc "<command>"`, codex-rs `hooks/src/engine/command_runner.rs`).
///
/// Quoted only when it has to be: a path with a space or another shell
/// character would otherwise run something else, while an ordinary path is
/// written as before — codex asks for `/hooks` trust again for a command that
/// changed, and quoting every install would cost everyone that for nothing.
fn shell_word(path: &str) -> String {
    if cfg!(windows) {
        return tower_os::windows_command_word(path);
    }
    let plain = !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c));
    if plain {
        path.to_string()
    } else {
        tower_state::sh_quote(path)
    }
}

/// Whether a matcher group's commands carry [`HOOK_MARK`].
///
/// `contains`, not equality: the point is to recognize *our* group whatever
/// binary path it was written with.
fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(ours_command)
            })
        })
}

/// Whether one command line is one of ours.
///
/// Both halves are required. `HOOK_MARK` alone is a generic shape — a command
/// ending in ` hook codex --event stop` could belong to anybody — and `is_ours`
/// *drops* what it claims, so matching on the shape alone could delete a
/// neighbour's hook during `init`. The binary's own name narrows it to us,
/// whatever path it was installed at.
fn ours_command(c: &str) -> bool {
    let Some(exe) = c.split(HOOK_MARK).next().filter(|e| *e != c) else {
        return false;
    };
    // The name has to be the executable's own, not any part of the path: a user
    // whose project lives in `~/src/herdr-tower/` must not have their unrelated
    // hook claimed.
    // Quoted or not: `'/opt/my tools/herdr-tower'` is ours too.
    tower_os::command_basename(exe) == OUR_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    // The one spec-only TOML string reader, written from the spec rather than
    // from the writer, so this checks that codex can read the line.
    use crate::initcmd::settingsfile::toml_first_basic_string;

    const CONFIG_FIXTURE: &str = "model = \"gpt-5.6\"\napproval_policy = \"on-request\"\n\n[mcp_servers.context7]\ncommand = \"npx\"\n";

    /// Nothing in this module may read `$HOME`: the paths it is given are the
    /// user's real `~/.codex` files, and a test that defaulted to one would
    /// rewrite the machine running it.
    fn seeded(name: &str, body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    fn notify(p: &Path) -> String {
        let mut out = Vec::new();
        install_codex_notify(&mut out, p, "/abs/herdr-tower", false).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn notify_goes_above_the_first_section_because_toml_puts_top_level_keys_there() {
        let (_d, p) = seeded("config.toml", CONFIG_FIXTURE);
        notify(&p);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "model = \"gpt-5.6\"\n\
             approval_policy = \"on-request\"\n\
             \n\
             notify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n\
             [mcp_servers.context7]\n\
             command = \"npx\"\n"
        );
        assert!(settingsfile::with_suffix(&p, BAK_SUFFIX).exists());
    }

    /// 2026-09-07, Windows VM: with no blank line before the header, the
    /// insertion landed at the end of the previous key and produced
    /// `approvals_reviewer = "auto_review"notify = [...]`. codex then refused to
    /// load its config at all, so this is the test that stands for a CLI that
    /// would not start.
    #[test]
    fn notify_lands_on_its_own_line_when_a_section_header_follows_immediately() {
        let (_d, p) = seeded(
            "config.toml",
            "model = \"x\"\napprovals_reviewer = \"auto_review\"\n[projects.\"/home/u\"]\ntrust_level = \"trusted\"\n",
        );
        notify(&p);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "model = \"x\"\n\
             approvals_reviewer = \"auto_review\"\n\
             notify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n\
             [projects.\"/home/u\"]\n\
             trust_level = \"trusted\"\n"
        );
    }

    #[test]
    fn notify_goes_first_when_the_file_opens_with_a_section_header() {
        let (_d, p) = seeded("config.toml", "[a]\nb = 1\n");
        notify(&p);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "notify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n[a]\nb = 1\n"
        );
    }

    #[test]
    fn notify_is_appended_when_the_file_has_no_sections_at_all() {
        for (before, after) in [
            (
                "model = \"x\"\n",
                "model = \"x\"\nnotify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n",
            ),
            (
                "model = \"x\"",
                "model = \"x\"\nnotify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n",
            ),
        ] {
            let (_d, p) = seeded("config.toml", before);
            notify(&p);
            assert_eq!(std::fs::read_to_string(&p).unwrap(), after);
        }
    }

    #[test]
    fn a_missing_config_is_created_holding_only_the_notify_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let out = notify(&p);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "notify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n"
        );
        assert_eq!(out, "", "creating the file from nothing says nothing");
    }

    #[test]
    fn somebody_elses_notify_wins_and_the_file_is_left_alone() {
        let (_d, p) = seeded("config.toml", "notify = [\"my-notifier\"]\n");
        let out = notify(&p);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "notify = [\"my-notifier\"]\n"
        );
        assert_eq!(
            out,
            "  codex notify: already configured — skipped (the existing setting wins)\n"
        );
    }

    #[test]
    fn installing_notify_twice_changes_nothing() {
        let (_d, p) = seeded("config.toml", CONFIG_FIXTURE);
        notify(&p);
        let first = std::fs::read(&p).unwrap();
        let out = notify(&p);
        assert_eq!(std::fs::read(&p).unwrap(), first);
        assert_eq!(out, "  codex notify: already registered — skipped\n");
    }

    /// The path codex is told to run has to come back out of the file as the path
    /// herdr-tower was installed at, for every character a Unix path may hold.
    ///
    /// The line is built by quoting the path, and `~/.codex/config.toml` is read
    /// whole: a spelling TOML does not define costs the user their entire codex
    /// configuration rather than this one key, and that is not hypothetical —
    /// the escapes written for a control character were `\a`, `\v` and `\xNN`
    /// until #33, and TOML 1.0 has none of the three.
    ///
    /// Three things are asked of each path: that an independent reader gets it
    /// back, that the line stays one line, and that a second install still
    /// recognizes its own work.
    ///
    /// "One line" is asserted on the notify line itself — it has to hold the
    /// whole of what [`notify_line`] wrote, closing quote and trailing
    /// `, "hook", "codex"]` included. The `[mcp_servers.context7]` header below
    /// it proves nothing: [`insert_top_level`] puts the new key *above* that
    /// header and a newline after it, so the header is on its own line in the
    /// broken file exactly as in the healthy one.
    #[test]
    fn a_notify_path_that_needs_escaping_reads_back_out_of_the_config() {
        for bin in [
            "/abs/herdr-tower",
            r#"/abs/a"b"#,
            r"/abs/a\b",
            "/abs/a\tb\u{8}c\u{c}d",
            "/abs/a\nb\rc",
            // No TOML short escape: these go out as `\uXXXX` or not at all.
            "/abs/a\u{7}b\u{b}c\u{0}d\u{1f}e\u{7f}",
            "/abs/한글 경로/herdr-tower",
            // Written raw on purpose — raw and escaped are the same string to a
            // TOML reader, which is the property that has to hold.
            "/abs/a\u{a0}b\u{2028}c😀",
            // Empty is the one input with a branch of its own: `notify_line`
            // substitutes the name on `PATH`, so the value read back is
            // `herdr-tower` rather than "".
            "",
        ] {
            let (_d, p) = seeded("config.toml", CONFIG_FIXTURE);
            let mut out = Vec::new();
            install_codex_notify(&mut out, &p, bin, false).unwrap();
            let body = std::fs::read_to_string(&p).unwrap();
            let line = body
                .lines()
                .find(|l| l.starts_with("notify = "))
                .unwrap_or_else(|| panic!("no notify line in {body:?}"));
            let want = if bin.is_empty() { "herdr-tower" } else { bin };
            assert_eq!(
                toml_first_basic_string(line).as_deref(),
                Ok(want),
                "{line:?}"
            );
            assert!(
                line.ends_with("\", \"hook\", \"codex\"]"),
                "the file was split open by {bin:?}: {body:?}"
            );
            assert_eq!(
                body.lines()
                    .filter(|l| l.contains("\"hook\", \"codex\"]"))
                    .count(),
                1,
                "the file was split open by {bin:?}: {body:?}"
            );

            let before = std::fs::read(&p).unwrap();
            let mut out = Vec::new();
            install_codex_notify(&mut out, &p, bin, false).unwrap();
            assert_eq!(
                std::fs::read(&p).unwrap(),
                before,
                "{bin:?} was not its own"
            );
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "  codex notify: already registered — skipped\n",
                "{bin:?}"
            );
        }
    }

    /// The comparison is against the *trimmed* line, so an indented `notify` is
    /// still recognized as ours — but one written without the spaces around `=`
    /// is a different string and is treated as somebody else's.
    #[test]
    fn a_notify_line_is_matched_after_trimming_but_not_after_reformatting() {
        let (_d, indented) = seeded(
            "config.toml",
            "   notify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n",
        );
        assert_eq!(
            notify(&indented),
            "  codex notify: already registered — skipped\n"
        );

        let (_d, tight) = seeded(
            "config.toml",
            "notify=[\"/abs/herdr-tower\",\"hook\",\"codex\"]\n",
        );
        assert_eq!(
            notify(&tight),
            "  codex notify: already configured — skipped (the existing setting wins)\n"
        );
    }

    /// A user's file is written back whole, so a byte herdr-tower cannot read is a
    /// byte herdr-tower must not change. Decoding lossily here would have
    /// replaced them with U+FFFD in somebody's own text, which is silent
    /// corruption of a file this tool was only asked to add a line to.
    #[test]
    fn a_byte_that_is_not_valid_utf8_survives() {
        let bad = [0xffu8, 0xfe];
        let dir = tempfile::tempdir().unwrap();

        let toml = dir.path().join("config.toml");
        let before = [&b"model = \"x"[..], &bad, b"\"\n[a]\n"].concat();
        std::fs::write(&toml, &before).unwrap();
        notify(&toml);
        assert_eq!(
            std::fs::read(&toml).unwrap(),
            [
                &b"model = \"x"[..],
                &bad,
                b"\"\nnotify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n[a]\n"
            ]
            .concat()
        );
    }

    /// Frozen because it is wrong: an unrelated top-level key whose name merely
    /// starts with `notify` is read as an existing configuration, and
    /// herdr-tower's `notify` is then never installed at all — silently, since
    /// the message says "skipped, yours wins".
    #[test]
    fn a_top_level_key_that_merely_starts_with_notify_blocks_the_install() {
        let (_d, p) = seeded("config.toml", "notify_on_error = true\nmodel = \"x\"\n");
        let out = notify(&p);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "notify_on_error = true\nmodel = \"x\"\n"
        );
        assert!(out.contains("the existing setting wins"), "{out}");
    }

    #[test]
    fn a_notify_dry_run_reports_and_writes_nothing() {
        let (_d, p) = seeded("config.toml", CONFIG_FIXTURE);
        let mut out = Vec::new();
        install_codex_notify(&mut out, &p, "/abs/herdr-tower", true).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), CONFIG_FIXTURE);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "  codex notify: notify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n  (dry-run — no file was changed)\n"
        );

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("config.toml");
        let mut out = Vec::new();
        install_codex_notify(&mut out, &missing, "/abs/herdr-tower", true).unwrap();
        assert!(!missing.exists());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "  {} will be created: notify = [\"/abs/herdr-tower\", \"hook\", \"codex\"]\n",
                missing.display()
            )
        );
    }

    /// The bytes are a recorded capture, taken 2026-09-11. codex reads this
    /// file, so the layout is a contract.
    #[test]
    fn codex_hooks_merge_beside_foreign_groups_and_replace_our_stale_one() {
        let (_d, p) = seeded(
            "hooks.json",
            r#"{"hooks":{"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"python -m py_compile \"$FILEPATH\""}]}],"Stop":[{"hooks":[{"type":"command","command":"/old/herdr-tower hook codex --event stop","timeout":10}]}]}}"#,
        );
        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/bin/herdr-tower", false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            include_str!("codexinit_test_hooks_merge.json")
        );
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("/hooks"),
            "the trust step has to be named: {out}"
        );
        assert!(settingsfile::with_suffix(&p, BAK_SUFFIX).exists());

        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/bin/herdr-tower", false).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "  herdr-tower: already registered — skipped\n"
        );
    }

    /// Unlike the Claude installer, this one recognizes its own group by the
    /// marker in the command, so moving the binary replaces the entry instead of
    /// adding a second one.
    #[test]
    fn moving_the_binary_replaces_our_codex_group_rather_than_duplicating_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/old/herdr-tower", false).unwrap();
        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/new/herdr-tower", false).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(!body.contains("/old/herdr-tower"));
        assert_eq!(body.matches("hook codex --event stop").count(), 1);
    }

    /// codex runs the hook through a shell: a path with a space is quoted, an
    /// ordinary one is written as before (a changed command needs `/hooks`
    /// trust again), and re-running `init` finds the quoted one as ours.
    #[cfg(unix)]
    #[test]
    fn a_path_with_a_space_is_one_shell_word_and_still_ours() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/opt/my tools/herdr-tower", false).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            body.contains("'/opt/my tools/herdr-tower' hook codex --event stop"),
            "{body}"
        );
        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/opt/my tools/herdr-tower", false).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("already registered"));
        assert_eq!(
            std::fs::read_to_string(&p)
                .unwrap()
                .matches("--event stop")
                .count(),
            1
        );

        for (path, word) in [
            (
                "/Users/q/.local/bin/herdr-tower",
                "/Users/q/.local/bin/herdr-tower",
            ),
            ("/opt/my tools/herdr-tower", "'/opt/my tools/herdr-tower'"),
            ("/a/it's/herdr-tower", r"'/a/it'\''s/herdr-tower'"),
            ("/a/$HOME/herdr-tower", "'/a/$HOME/herdr-tower'"),
            ("/a/b;rm/herdr-tower", "'/a/b;rm/herdr-tower'"),
        ] {
            assert_eq!(shell_word(path), word, "{path}");
        }
        assert!(ours_command(
            "'/opt/my tools/herdr-tower' hook codex --event stop"
        ));
        assert!(!ours_command(
            "'/opt/my tools/other' hook codex --event stop"
        ));
    }

    /// Two identical copies of our group — what running two builds against the
    /// same file produces — collapse to one, because only the first match is
    /// kept.
    #[test]
    fn a_duplicated_group_of_ours_collapses_to_one() {
        let group = r#"{"hooks":[{"type":"command","command":"/bin/herdr-tower hook codex --event stop","timeout":10}]}"#;
        let (_d, p) = seeded(
            "hooks.json",
            &format!(r#"{{"hooks":{{"Stop":[{group},{group}]}}}}"#),
        );
        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/bin/herdr-tower", false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p)
                .unwrap()
                .matches("hook codex --event stop")
                .count(),
            1
        );
    }

    #[test]
    fn a_codex_hooks_dry_run_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        let mut out = Vec::new();
        install_codex_hooks(&mut out, &p, "/bin/herdr-tower", true).unwrap();
        assert!(!p.exists());
        assert!(String::from_utf8(out)
            .unwrap()
            .ends_with("(dry-run — no file was changed)\n"));
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    /// `is_ours` *drops* the groups it claims, so a shape another tool could
    /// write must not be enough to claim them.
    ///
    /// ` hook codex --event stop` is generic — any executable can be invoked
    /// that way — and matching on it alone deleted an unrelated user's hook
    /// during `init`.
    #[test]
    fn ownership_needs_the_executable_name_not_just_the_shape() {
        for ours in [
            "herdr-tower hook codex --event stop",
            "/Users/x/.local/bin/herdr-tower hook codex --event stop",
        ] {
            assert!(ours_command(ours), "{ours:?} is ours");
        }
        for theirs in [
            "/opt/other/watcher hook codex --event stop",
            "python3 /opt/tool.py hook codex --event stop",
            // A project folder that happens to share the name is not the
            // executable.
            "/Users/x/src/herdr-tower/scripts/run.sh hook codex --event stop",
            "herdr-tower hook claude --event stop",
            "herdr-tower status",
            // A binary whose name only starts the same is not this one.
            "/usr/local/bin/herdr-tower-old hook codex --event stop",
        ] {
            assert!(!ours_command(theirs), "{theirs:?} is not ours");
        }
    }
}
