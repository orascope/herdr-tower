// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Registering herdr-tower's hooks in the settings files Claude Code and the
//! stock Gemini CLI read.
//!
//! One rule orders everything here: **the file belongs to somebody else.** It is
//! read into an untyped object so unknown fields survive a round trip, a copy is
//! written to `.herdr-tower.bak` before anything changes, the write goes through
//! a temporary file and a rename, and an event that is already registered is
//! left exactly as it is. A settings file this tool cannot parse stops the
//! install rather than being replaced with a version it can.
//!
//! The commands are registered by **absolute path**. A session started by a
//! LaunchAgent has a minimal `PATH`, and a hook spelled `herdr-tower hook …`
//! there dies as "command not found" — silently, because a hook's failure must
//! never reach the agent. A bare-name entry of ours is replaced by the absolute
//! path on the next run.

use std::io::{self, Write};
use std::path::Path;

use serde_json::{json, Value};

#[path = "initcmd_settingsfile.rs"]
pub(crate) mod settingsfile;

use settingsfile::{Object, BAK_SUFFIX, TMP_SUFFIX};

/// One hook event: the key it lives under in the settings file, and the name
/// herdr-tower's own `hook` subcommand knows it by.
///
/// The two are not the same string and the pairing is not derivable — Claude
/// Code's `PostToolUse` is our `post-tool-use`, but the Gemini CLI's `AfterTool`
/// is also a `--event after-tool` of its own. Naming both halves is what stops
/// a future edit from "simplifying" one into a transformation of the other.
#[derive(Debug, Clone, Copy)]
pub struct HookEvent {
    pub settings_key: &'static str,
    pub event_arg: &'static str,
}

const fn ev(settings_key: &'static str, event_arg: &'static str) -> HookEvent {
    HookEvent {
        settings_key,
        event_arg,
    }
}

/// Claude Code's `settings.json` events.
pub const CLAUDE_EVENTS: [HookEvent; 5] = [
    ev("PostToolUse", "post-tool-use"),
    ev("Notification", "notification"),
    ev("Stop", "stop"),
    ev("SessionStart", "session-start"),
    ev("UserPromptSubmit", "user-prompt-submit"),
];

/// The stock Gemini CLI's `settings.json` events.
///
/// Antigravity's `hooks.json` is a different format and lives in
/// [`crate::geminiinit`]; these two must not be merged just because both say
/// "gemini".
pub const GEMINI_CLI_EVENTS: [HookEvent; 5] = [
    ev("SessionStart", "session-start"),
    ev("BeforeAgent", "before-agent"),
    ev("AfterTool", "after-tool"),
    ev("AfterAgent", "after-agent"),
    ev("Notification", "notification"),
];

/// Registers herdr-tower's hooks in Claude Code's `settings.json`.
pub fn install_claude_hooks(
    w: &mut impl Write,
    settings_path: &Path,
    bin_path: &str,
    dry_run: bool,
) -> io::Result<()> {
    install_json_hooks(
        w,
        settings_path,
        bin_path,
        "claude",
        &CLAUDE_EVENTS,
        dry_run,
    )
}

/// Registers herdr-tower's hooks in the stock Gemini CLI's `~/.gemini/settings.json`.
///
/// It shares an installer with Claude Code because the two files share a schema
/// — event key to a list of `{matcher?, hooks: [...]}` groups — not because the
/// two products are related.
pub fn install_gemini_cli_hooks(
    w: &mut impl Write,
    settings_path: &Path,
    bin_path: &str,
    dry_run: bool,
) -> io::Result<()> {
    install_json_hooks(
        w,
        settings_path,
        bin_path,
        "gemini",
        &GEMINI_CLI_EVENTS,
        dry_run,
    )
}

/// The shared installer for `settings.json`-shaped files.
fn install_json_hooks(
    w: &mut impl Write,
    settings_path: &Path,
    bin_path: &str,
    agent: &str,
    events: &[HookEvent],
    dry_run: bool,
) -> io::Result<()> {
    let bin_path = if bin_path.is_empty() {
        "herdr-tower"
    } else {
        bin_path
    };
    let (raw, mut settings) = settingsfile::read_json_object(settings_path)?;
    // A `hooks` key that is not an object is replaced rather than merged. It
    // loses whatever was there; the alternative is refusing to install against
    // a file no schema describes.
    let mut hooks = object_at(&settings, "hooks");

    // The agent CLIs run this string with `sh -c`, so the path is one quoted
    // word. Unquoted, a path with a space in it broke every hook silently — a
    // hook's failure is swallowed by design — and a path with shell
    // metacharacters in it ran them. Always quoted rather than only when
    // needed, so there is one shape for `ours_command` and `doctor` to read.
    let quoted = if cfg!(windows) {
        tower_os::windows_command_word(bin_path)
    } else {
        tower_state::sh_quote(bin_path)
    };
    let mut changed = false;
    for e in events {
        let cmd = format!("{quoted} hook {agent} --event {}", e.event_arg);
        let mut entries = array_at(&hooks, e.settings_key);
        // Remove *any* entry that runs this event's hook, whatever binary path
        // it names — not only the bare-name one.
        //
        // Matching the bare name alone left a stale absolute path behind
        // whenever the binary moved, so `Stop` ended with two groups and the
        // old path fired on every event. That is not hypothetical: installing
        // once from a scratch build and then from `~/.local/bin` left five dead
        // hooks in a real `settings.json`, failing on every Notification,
        // PostToolUse, SessionStart, Stop and UserPromptSubmit — silently,
        // because a hook's failure is swallowed by design.
        let mine = format!(" hook {agent} --event {}", e.event_arg);
        if remove_matching(&mut entries, &mine, &cmd) {
            changed = true;
            writeln!(w, "  {}: removed the old-path entry", e.settings_key)?;
        }
        if has_command(&entries, &cmd) {
            writeln!(w, "  {}: already registered — skipped", e.settings_key)?;
            hooks.insert(e.settings_key.to_string(), Value::Array(entries));
            continue;
        }
        entries.push(json!({"hooks": [{"type": "command", "command": cmd}]}));
        hooks.insert(e.settings_key.to_string(), Value::Array(entries));
        changed = true;
        writeln!(w, "  {}: {cmd}", e.settings_key)?;
    }
    settings.insert("hooks".to_string(), Value::Object(hooks));

    if dry_run {
        writeln!(w, "(dry-run — no file was changed)")?;
        return Ok(());
    }
    if !changed {
        return Ok(());
    }
    if let Some(raw) = &raw {
        settingsfile::backup(settings_path, BAK_SUFFIX, raw, 0o600)?;
    }
    let mut body = tower_discord::to_sorted_json_pretty(&Value::Object(settings))?;
    body.push(b'\n');
    settingsfile::create_parent(settings_path)?;
    settingsfile::write_atomic(settings_path, TMP_SUFFIX, &body, 0o600)
}

/// The object under `key`, or a fresh one when the key is missing or holds
/// something else.
fn object_at(m: &Object, key: &str) -> Object {
    match m.get(key) {
        Some(Value::Object(o)) => o.clone(),
        _ => Object::new(),
    }
}

/// The array under `key`, or an empty one when the key is missing or holds
/// something else.
fn array_at(m: &Object, key: &str) -> Vec<Value> {
    match m.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

/// Drops every entry whose hook list holds exactly this command, reporting
/// whether anything went.
/// Drops every entry that runs our hook for this event, except the one that is
/// already correct.
///
/// `suffix` is the part that identifies the hook regardless of where the binary
/// lives (`" hook claude --event stop"`); `keep` is the command we are about to
/// register, left alone so a reinstall reports "already registered" instead of
/// rewriting the file. Another tool's hook for the same event does not match,
/// because the suffix names this tool's own subcommand and argument shape.
///
/// **Commands are removed, not groups.** A user may add their own command to a
/// group that already holds one of ours — `settings.json` allows several per
/// matcher — and dropping the whole group took their hook with it. The first
/// version of this did exactly that. A group is only discarded once nothing is
/// left in it.
///
/// **And only our executable's commands.** The suffix is a shape, not a
/// signature: `/opt/supervisor hook claude --event stop` ends with it too, and
/// this function *deletes* what it matches. See [`ours_command`].
fn remove_matching(entries: &mut Vec<Value>, suffix: &str, keep: &str) -> bool {
    let mut changed = false;
    for e in entries.iter_mut() {
        let Some(hooks) = e.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = hooks.len();
        hooks.retain(|h| {
            let cmd = h.get("command").and_then(Value::as_str).unwrap_or_default();
            !ours_command(cmd, suffix) || cmd == keep
        });
        changed |= hooks.len() != before;
    }
    // A group whose every command was ours has nothing left to run.
    let before = entries.len();
    entries.retain(|e| {
        e.get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|hs| !hs.is_empty())
    });
    changed || entries.len() != before
}

/// Whether one command line is ours — our argument shape *and* our executable.
///
/// Both halves are required, exactly as in [`crate::codexinit::ours_command`],
/// and for the same reason: the shape alone is generic, and the caller removes
/// what this claims. The *basename* is what is compared, not any part of the
/// path, so a binary that moved is still ours while a neighbour whose project
/// happens to live in a `herdr-tower/` folder is not.
///
/// The executable may be written either way: unquoted, as every entry before
/// the path was quoted, or as the single-quoted word `install_json_hooks` now
/// writes. Both have to be recognised, or re-running `init` after an upgrade
/// would add the quoted entry beside the old one and both would fire.
fn ours_command(cmd: &str, suffix: &str) -> bool {
    let Some(exe) = cmd.strip_suffix(suffix) else {
        return false;
    };
    tower_os::command_basename(exe) == crate::codexinit::OUR_NAME
}

fn has_command(entries: &[Value], cmd: &str) -> bool {
    entries.iter().any(|e| entry_runs(e, cmd))
}

/// Whether one `{matcher?, hooks: [...]}` group runs exactly this command.
///
/// Exact equality, not `contains`: the argument differs only in the `--event`
/// name, and a prefix match would make `stop` claim `stop-something`.
fn entry_runs(entry: &Value, cmd: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|inner| {
            inner
                .iter()
                .any(|h| h.get("command").and_then(Value::as_str) == Some(cmd))
        })
}

/// Prints the herdr key binding to add, and does not add it.
///
/// `~/.config/herdr/config.toml` is herdr's file, edited by hand, and a tool
/// that rewrites it would fight whoever owns it. Printing costs the user one
/// paste and leaves the file theirs.
///
/// herdr ships popups (`type = "popup"`, a session modal that leaves the tab
/// layout alone), so there is nothing to build and nothing to work around.
///
/// Returns `io::Result` rather than ignoring the write error, which would be
/// defensible for stdout and is indefensible for the arbitrary writer this
/// takes.
pub fn print_herdr_binding(w: &mut impl Write, bin_path: &str) -> io::Result<()> {
    let bin_path = if bin_path.is_empty() {
        "herdr-tower"
    } else {
        bin_path
    };
    // Where herdr reads it: `%APPDATA%\herdr` on Windows.
    let config = if cfg!(windows) {
        r"%APPDATA%\herdr\config.toml"
    } else {
        "~/.config/herdr/config.toml"
    };
    writeln!(w, "For the herdr popup, add this to {config} (prefix+a):")?;
    writeln!(w, "  [[keys.command]]")?;
    writeln!(w, "  key = \"prefix+a\"")?;
    writeln!(w, "  type = \"popup\"")?;
    writeln!(w, "  command = {}", settingsfile::quote(bin_path))?;
    writeln!(w, "  width = \"90%\"")?;
    writeln!(w, "  height = \"85%\"")?;
    writeln!(w, "Apply it with: herdr server reload-config")?;
    writeln!(
        w,
        "(to keep it open, add a second key in the same block with type = \"pane\")"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXISTING_SETTINGS: &str = r#"{
  "model": "opus",
  "hooks": {
    "Notification": [
      {"hooks": [{"type": "command", "command": "notify-me --hook"}]}
    ],
    "PostToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "my-logger"}]}
    ]
  },
  "permissions": {"allow": ["Bash(ls:*)"]}
}"#;

    /// Every test writes under a `tempfile` root and never reads `$HOME`. These
    /// functions rewrite the real `~/.claude/settings.json` when the caller
    /// hands them that path, so a test that fell back to a default would edit
    /// the machine it runs on.
    fn seeded(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    fn install(p: &Path, bin: &str) -> String {
        let mut out = Vec::new();
        install_claude_hooks(&mut out, p, bin, false).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The bytes below are the recorded layout, pinned byte for byte. Claude
    /// Code parses this file at startup, so the shape is a contract with another
    /// program, not a formatting preference.
    /// On Windows the hook's first word is the path with forward slashes and
    /// no quotes, which Git Bash, `cmd` and PowerShell all run.
    #[cfg(windows)]
    #[test]
    fn a_windows_hook_command_runs_in_every_shell() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        install_claude_hooks(
            &mut Vec::new(),
            &p,
            r"C:\Users\u\.local\bin\herdr-tower.exe",
            false,
        )
        .unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            body.contains("\"C:/Users/u/.local/bin/herdr-tower.exe hook claude --event stop\""),
            "{body}"
        );
        // And re-running finds it as ours: nothing is added beside it.
        install_claude_hooks(
            &mut Vec::new(),
            &p,
            r"C:\Users\u\.local\bin\herdr-tower.exe",
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), body);
    }

    #[cfg(unix)]
    #[test]
    fn a_fresh_settings_file_gets_every_claude_event_in_the_recorded_layout() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let out = install(&p, "/abs/herdr-tower");
        assert_eq!(
            out,
            "  PostToolUse: '/abs/herdr-tower' hook claude --event post-tool-use\n\
             \x20 Notification: '/abs/herdr-tower' hook claude --event notification\n\
             \x20 Stop: '/abs/herdr-tower' hook claude --event stop\n\
             \x20 SessionStart: '/abs/herdr-tower' hook claude --event session-start\n\
             \x20 UserPromptSubmit: '/abs/herdr-tower' hook claude --event user-prompt-submit\n"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            include_str!("initcmd_test_claude_fresh.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_settings_file_keeps_every_key_and_every_foreign_hook() {
        let (_d, p) = seeded(EXISTING_SETTINGS);
        install(&p, "/abs/herdr-tower");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            include_str!("initcmd_test_claude_existing.json")
        );
        assert_eq!(
            std::fs::read_to_string(settingsfile::with_suffix(&p, BAK_SUFFIX)).unwrap(),
            EXISTING_SETTINGS,
            "the original is copied aside before anything is written"
        );
    }

    #[test]
    fn installing_twice_registers_nothing_twice() {
        let (_d, p) = seeded(EXISTING_SETTINGS);
        install(&p, "/abs/herdr-tower");
        let first = std::fs::read(&p).unwrap();
        let out = install(&p, "/abs/herdr-tower");
        assert_eq!(std::fs::read(&p).unwrap(), first);
        assert_eq!(out.matches("already registered — skipped").count(), 5);
    }

    /// A LaunchAgent session has a minimal `PATH`, so a bare-name entry
    /// resolves to nothing and the hook dies silently. It is replaced rather
    /// than left beside the new one.
    #[test]
    fn a_bare_name_entry_from_an_older_build_is_replaced_not_duplicated() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"herdr-tower hook claude --event stop"}]}]}}"#,
        );
        let out = install(&p, "/abs/herdr-tower");
        assert!(
            out.contains("  Stop: removed the old-path entry\n"),
            "{out}"
        );
        let body = std::fs::read_to_string(&p).unwrap();
        assert_eq!(body.matches("hook claude --event stop").count(), 1);
        assert!(!body.contains("\"herdr-tower hook claude"));
    }

    /// The state an interrupted run leaves: the bare entry *and* the
    /// absolute one. The bare one goes, the absolute one is recognized, and
    /// nothing is appended — so the event ends up registered exactly once.
    #[cfg(unix)]
    #[test]
    fn a_bare_and_an_absolute_entry_together_collapse_to_the_absolute_one() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"herdr-tower hook claude --event stop"}]},{"hooks":[{"type":"command","command":"'/abs/herdr-tower' hook claude --event stop"}]}]}}"#,
        );
        let out = install(&p, "/abs/herdr-tower");
        assert!(
            out.contains(
                "  Stop: removed the old-path entry\n  Stop: already registered — skipped\n"
            ),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(&p)
                .unwrap()
                .matches("hook claude --event stop")
                .count(),
            1
        );
    }

    /// The upgrade path: an entry an older build wrote with the path unquoted
    /// is recognised as ours and replaced by the quoted one — the event ends up
    /// registered once, not once in each spelling.
    #[cfg(unix)]
    #[test]
    fn an_unquoted_entry_from_an_older_build_becomes_the_quoted_one() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/abs/herdr-tower hook claude --event stop"}]}]}}"#,
        );
        let out = install(&p, "/abs/herdr-tower");
        assert!(
            out.contains("  Stop: removed the old-path entry\n"),
            "{out}"
        );
        let body = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            body.matches("hook claude --event stop").count(),
            1,
            "{body}"
        );
        assert!(body.contains("\"'/abs/herdr-tower' hook claude --event stop\""));
    }

    /// A path with a space and a quote in it is one word to `sh`: the hook runs
    /// the binary it names, where unquoted it ran `/opt/my` with the rest as
    /// arguments. Read back through a real shell, and recognised as ours on
    /// the next install.
    #[cfg(unix)]
    #[test]
    fn a_path_with_a_space_and_a_quote_is_one_word_to_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let bin = "/opt/my tools/it's/herdr-tower";
        install(&p, bin);
        let settings: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let cmd = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string();
        let exe = cmd.strip_suffix(" hook claude --event stop").unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {exe}"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), bin);

        let again = install(&p, bin);
        assert_eq!(
            again.matches("already registered — skipped").count(),
            5,
            "{again}"
        );
    }

    /// `ours_command` reads both spellings of the executable, and still only
    /// ours.
    #[test]
    fn our_executable_is_recognised_quoted_or_not() {
        let suffix = " hook claude --event stop";
        for cmd in [
            "/abs/herdr-tower hook claude --event stop",
            "'/abs/herdr-tower' hook claude --event stop",
            "herdr-tower hook claude --event stop",
            "'herdr-tower' hook claude --event stop",
            "'/a b/herdr-tower' hook claude --event stop",
        ] {
            assert!(ours_command(cmd, suffix), "{cmd}");
        }
        for cmd in [
            "'/opt/supervisor' hook claude --event stop",
            "/herdr-tower/other hook claude --event stop",
            "'/abs/herdr-tower-dev' hook claude --event stop",
        ] {
            assert!(!ours_command(cmd, suffix), "{cmd}");
        }
    }

    /// Moving the binary leaves nothing behind.
    ///
    /// This test used to assert the opposite, frozen as a known hole: only the
    /// bare-name entry was removed, so a moved binary left its old absolute
    /// path registered beside the new one and both fired. It stopped
    /// being hypothetical — installing once from a scratch build and then from
    /// `~/.local/bin` left five dead hooks in a real `settings.json`, failing on
    /// every event and silently, because a hook's failure is swallowed by design.
    #[cfg(unix)]
    #[test]
    fn moving_the_binary_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        install(&p, "/old/herdr-tower");
        install(&p, "/new/herdr-tower");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            !body.contains("/old/herdr-tower"),
            "the old path is still registered: {body}"
        );
        assert!(body.contains("'/new/herdr-tower' hook claude --event stop"));
        assert_eq!(
            body.matches("hook claude --event stop").count(),
            1,
            "the event must be registered exactly once"
        );
    }

    /// Another tool's hook for the same event is not ours to remove.
    ///
    /// The match is on this tool's own subcommand and argument shape, so a
    /// neighbour that happens to run on `Stop` survives every reinstall.
    #[cfg(unix)]
    #[test]
    fn a_foreign_hook_on_the_same_event_survives() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"bash ~/.claude/hooks/other.sh stop"}]}]}}"#,
        );
        install(&p, "/abs/herdr-tower");
        install(&p, "/other/herdr-tower");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            body.contains("other.sh stop"),
            "somebody else's hook was removed: {body}"
        );
        assert_eq!(
            body.matches("'/other/herdr-tower' hook claude --event stop")
                .count(),
            1
        );
        assert!(!body.contains("/abs/herdr-tower"));
    }

    /// Another program invoked with the same generic argument shape is not ours,
    /// and `init` must not delete it.
    ///
    /// The suffix — ` hook claude --event stop` — says nothing about who wrote
    /// the command. Only the executable's own name does.
    #[cfg(unix)]
    #[test]
    fn a_foreign_program_with_our_argument_shape_survives() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/opt/supervisor hook claude --event stop"}]}]}}"#,
        );
        install(&p, "/abs/herdr-tower");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            body.contains("/opt/supervisor hook claude --event stop"),
            "somebody else's command was removed: {body}"
        );
        assert!(body.contains("'/abs/herdr-tower' hook claude --event stop"));
    }

    /// A binary of ours that moved is still ours.
    #[test]
    fn our_own_stale_paths_are_still_removed() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[
                {"hooks":[{"type":"command","command":"/old/herdr-tower hook claude --event stop"}]}
            ]}}"#,
        );
        install(&p, "/new/herdr-tower");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(!body.contains("/old/herdr-tower"), "{body}");
        assert_eq!(
            body.matches("hook claude --event stop").count(),
            1,
            "one live hook: {body}"
        );
    }

    /// `settings.json` and its backup come out 0600, whatever the file was
    /// before — these sit next to credentials in `~/.claude`, so every one of
    /// them is written 0600.
    #[cfg(unix)]
    #[test]
    fn the_settings_file_and_its_backup_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, p) = seeded(EXISTING_SETTINGS);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        install(&p, "/abs/herdr-tower");
        for f in [p.clone(), settingsfile::with_suffix(&p, BAK_SUFFIX)] {
            assert_eq!(
                std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o600,
                "{}",
                f.display()
            );
        }
    }

    #[test]
    fn a_dry_run_says_what_it_would_do_and_touches_nothing() {
        let (_d, p) = seeded(EXISTING_SETTINGS);
        let mut out = Vec::new();
        install_claude_hooks(&mut out, &p, "/abs/herdr-tower", true).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), EXISTING_SETTINGS);
        assert!(!settingsfile::with_suffix(&p, BAK_SUFFIX).exists());
        assert!(String::from_utf8(out)
            .unwrap()
            .ends_with("(dry-run — no file was changed)\n"));
    }

    #[test]
    fn a_settings_file_that_will_not_parse_stops_the_install() {
        let (_d, p) = seeded("{ not json");
        let mut out = Vec::new();
        let e = install_claude_hooks(&mut out, &p, "/abs/herdr-tower", false).unwrap_err();
        assert!(e.to_string().contains("would not parse"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json");
    }

    #[cfg(unix)]
    #[test]
    fn the_gemini_cli_gets_its_own_event_names_in_the_same_shape() {
        let (_d, p) = seeded(r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#);
        let mut out = Vec::new();
        install_gemini_cli_hooks(&mut out, &p, "/abs/herdr-tower", false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            include_str!("initcmd_test_geminicli.json")
        );
    }

    /// The advice is pasted straight into herdr's config, so it is pinned byte
    /// for byte: the keys, the config file and the reload command are all
    /// herdr's, and a person following a wrong line edits a file nothing reads.
    #[cfg(unix)]
    #[test]
    fn the_herdr_advice_is_pasteable() {
        let mut out = Vec::new();
        print_herdr_binding(&mut out, "/usr/local/bin/herdr-tower").unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "For the herdr popup, add this to ~/.config/herdr/config.toml (prefix+a):\n\
             \x20 [[keys.command]]\n\
             \x20 key = \"prefix+a\"\n\
             \x20 type = \"popup\"\n\
             \x20 command = \"/usr/local/bin/herdr-tower\"\n\
             \x20 width = \"90%\"\n\
             \x20 height = \"85%\"\n\
             Apply it with: herdr server reload-config\n\
             (to keep it open, add a second key in the same block with type = \"pane\")\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_binary_path_falls_back_to_the_bare_name() {
        let mut out = Vec::new();
        print_herdr_binding(&mut out, "").unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("  command = \"herdr-tower\"\n"));

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let mut out = Vec::new();
        install_claude_hooks(&mut out, &p, "", false).unwrap();
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .contains("\"'herdr-tower' hook claude --event stop\""));
    }

    /// The settings file lives under `~/.claude`, which `init` may be creating
    /// for the first time.
    #[test]
    fn a_missing_parent_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deep").join("er").join("settings.json");
        let mut out = Vec::new();
        install_claude_hooks(&mut out, &p, "/abs/herdr-tower", false).unwrap();
        assert!(p.is_file());
    }

    /// Somebody else's command in *our* group must survive.
    ///
    /// `settings.json` allows several commands under one matcher, so a user can
    /// add theirs beside ours. The first version of the stale-path fix dropped
    /// the whole group, which silently deleted that command the next time `init`
    /// ran after the binary moved.
    #[test]
    fn a_sibling_command_in_our_group_survives_a_migration() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[{"hooks":[
                 {"type":"command","command":"/old/herdr-tower hook claude --event stop"},
                 {"type":"command","command":"bash ~/.claude/hooks/mine.sh stop"}
               ]}]}}"#,
        );
        install(&p, "/new/herdr-tower");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            body.contains("mine.sh stop"),
            "the user's own command was deleted: {body}"
        );
        assert!(!body.contains("/old/herdr-tower"), "the stale path stayed");
        assert_eq!(
            body.matches("hook claude --event stop").count(),
            1,
            "registered exactly once"
        );
    }

    /// A group that held only ours is dropped, not left empty.
    #[test]
    fn a_group_with_nothing_left_is_removed() {
        let (_d, p) = seeded(
            r#"{"hooks":{"Stop":[{"hooks":[
                 {"type":"command","command":"/old/herdr-tower hook claude --event stop"}
               ]}]}}"#,
        );
        install(&p, "/new/herdr-tower");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let groups = v["hooks"]["Stop"].as_array().unwrap();
        assert!(
            groups
                .iter()
                .all(|g| !g["hooks"].as_array().unwrap().is_empty()),
            "an empty group was left behind: {v}"
        );
    }

    /// Round trip over generated paths (seeded) — spaces, single and double
    /// quotes, backslashes, `$`, unicode: every command `init` writes is
    /// recognised as ours by `ours_command`, quoted as written and in the
    /// legacy unquoted spelling; the same path under another basename is not;
    /// and moving the binary to another generated path leaves each event
    /// registered exactly once, under the new path.
    #[cfg(unix)]
    #[test]
    fn written_commands_are_ours_for_generated_paths() {
        const PIECES: &[&str] = &[
            "a", " ", "'", "\"", "\\", "$", "`", ";", "세션", "😀", "é", "my tools", "-", ".",
        ];
        let mut rng = tower_testrng::Rng::new(0x0035_C0DE);
        let mut gen = || {
            let mut dir = String::new();
            for _ in 0..1 + rng.below(3) {
                dir.push('/');
                for _ in 0..1 + rng.below(4) {
                    dir.push_str(rng.pick(PIECES).unwrap());
                }
            }
            dir
        };
        let suffix = " hook claude --event stop";
        let d = tempfile::tempdir().unwrap();
        for i in 0..120 {
            let dir = gen();
            let bin = format!("{dir}/herdr-tower");
            let quoted = format!("{}{suffix}", tower_state::sh_quote(&bin));
            assert!(ours_command(&quoted, suffix), "{quoted}");
            if !bin.contains('\'') {
                assert!(ours_command(&format!("{bin}{suffix}"), suffix), "{bin}");
            }
            let other = format!(
                "{}{suffix}",
                tower_state::sh_quote(&format!("{dir}/herdr-tower-dev"))
            );
            assert!(!ours_command(&other, suffix), "{other}");

            let p = d.path().join(format!("s{i}.json"));
            install(&p, &bin);
            let moved = format!("{}/herdr-tower", gen());
            install(&p, &moved);
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            for e in CLAUDE_EVENTS {
                let cmds: Vec<&str> = v["hooks"][e.settings_key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|g| g["hooks"].as_array().unwrap())
                    .filter_map(|h| h["command"].as_str())
                    .collect();
                let want = format!(
                    "{} hook claude --event {}",
                    tower_state::sh_quote(&moved),
                    e.event_arg
                );
                assert_eq!(cmds, [want.as_str()], "{bin:?} -> {moved:?}");
            }
        }
    }
}
