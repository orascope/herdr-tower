// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Registering herdr-tower in the Antigravity CLI's global hooks file.
//!
//! `~/.gemini/config/hooks.json` is a different format from everything else here
//! and from the stock Gemini CLI's `settings.json` (which
//! [`crate::initcmd::install_gemini_cli_hooks`] handles): the *top-level* keys
//! are hook **names**, and the events hang underneath. So ownership is a single
//! key — herdr-tower writes `hooks["herdr-tower"]` and never reads any other name.
//!
//! agy's own `/hooks` UI writes into this same file, which is why the rule is a
//! whole-key replace rather than a merge: there is no shared list to append to.

use std::io::{self, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::initcmd::settingsfile::{self, BAK_SUFFIX, TMP_SUFFIX};

/// Registers herdr-tower's hooks in `~/.gemini/config/hooks.json`.
///
/// Which events, and why only these three:
///
/// - `PostToolUse` and `PreInvocation` mean WORK. Their output contract is an
///   empty object, so a hook that says nothing cannot interfere with the agent.
/// - `Stop` means DONE. Declining to emit a `decision` leaves the shutdown alone.
/// - `PreToolUse` is **not** registered. Its output *is* the approval decision,
///   so a supervisor listening there would be answering permission prompts on the
///   user's behalf.
///
/// Idempotent by comparing the encoded form of what is there against what is
/// wanted, so a `timeout` that came back from the file as a JSON float still
/// equals the integer here — both sides go through the same encoder. When
/// `bin_path` changes, only the `herdr-tower` key moves.
pub fn install_gemini_hooks(
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
    // One quoted shell word, for the reason `initcmd::install_json_hooks`
    // gives: agy runs this through a shell.
    let quoted = tower_state::sh_quote(bin_path);
    let cmd = |event: &str| {
        json!({
            "type": "command",
            "command": format!("{quoted} hook gemini --event {event}"),
            "timeout": 10,
        })
    };
    // `PostToolUse` takes the matcher-group shape and the other two take a bare
    // command list. That asymmetry is agy's, not ours.
    let desired = json!({
        "PostToolUse": [{"matcher": "*", "hooks": [cmd("post-tool-use")]}],
        "PreInvocation": [cmd("pre-invocation")],
        "Stop": [cmd("stop")],
    });

    let (raw, mut hooks) = settingsfile::read_json_object(hooks_path)?;
    let current = hooks.get("herdr-tower").cloned().unwrap_or(Value::Null);
    if settingsfile::json_key(&current) == settingsfile::json_key(&desired) {
        writeln!(w, "  herdr-tower: already registered — skipped")?;
        return Ok(());
    }
    hooks.insert("herdr-tower".to_string(), desired);
    writeln!(
        w,
        "  herdr-tower: PostToolUse·PreInvocation·Stop → {bin_path} hook gemini"
    )?;

    if dry_run {
        writeln!(w, "(dry-run — no file was changed)")?;
        return Ok(());
    }
    if let Some(raw) = &raw {
        settingsfile::backup(hooks_path, BAK_SUFFIX, raw, 0o600)?;
    }
    let mut body = tower_discord::to_sorted_json_pretty(&Value::Object(hooks))?;
    body.push(b'\n');
    // A freshly installed agy has no `config/` folder yet (measured on WSL2,
    // 2026-09-07), so the directory is created rather than assumed.
    settingsfile::create_parent(hooks_path)?;
    settingsfile::write_atomic(hooks_path, TMP_SUFFIX, &body, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real path is `~/.gemini/config/hooks.json`; every test here writes
    /// under a `tempfile` root so nothing touches the machine's own agy setup.
    fn run(p: &Path, bin: &str) -> String {
        let mut out = Vec::new();
        install_gemini_hooks(&mut out, p, bin, false).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The bytes are a recorded capture, taken 2026-09-11. agy parses this
    /// file, so the layout is a contract with another program.
    #[test]
    fn a_fresh_hooks_file_gets_the_three_events_in_the_recorded_layout() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config").join("hooks.json");
        let out = run(&p, "/abs/herdr-tower");
        assert_eq!(
            out,
            "  herdr-tower: PostToolUse·PreInvocation·Stop → /abs/herdr-tower hook gemini\n"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            include_str!("geminiinit_test_fresh.json")
        );
    }

    /// The file is shared with agy's own `/hooks`, so anything under another
    /// top-level name has to survive untouched — including a second install and
    /// a change of binary path.
    #[test]
    fn another_tools_hook_survives_installing_reinstalling_and_moving_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        std::fs::write(
            &p,
            r#"{"my-linter":{"PostToolUse":[{"matcher":"run_command","hooks":[{"command":"./lint.sh"}]}]}}"#,
        )
        .unwrap();

        run(&p, "/abs/herdr-tower");
        let first = std::fs::read_to_string(&p).unwrap();
        assert_eq!(first, include_str!("geminiinit_test_seeded.json"));

        let out = run(&p, "/abs/herdr-tower");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            first,
            "second run is a no-op"
        );
        assert_eq!(out, "  herdr-tower: already registered — skipped\n");

        run(&p, "/new/herdr-tower");
        let third = std::fs::read_to_string(&p).unwrap();
        assert!(third.contains("'/new/herdr-tower' hook gemini"));
        assert!(!third.contains("/abs/herdr-tower"));
        assert!(
            third.contains("my-linter"),
            "the other tool's hook is ours to leave alone"
        );
        assert!(third.contains("./lint.sh"));
    }

    /// A hand-edited file where our key holds something else entirely: the key is
    /// ours, so it is replaced rather than merged with.
    #[test]
    fn our_own_key_is_replaced_whatever_it_held() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        std::fs::write(&p, r#"{"herdr-tower":"junk"}"#).unwrap();
        run(&p, "/abs/herdr-tower");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(!body.contains("junk"));
        assert!(body.contains("pre-invocation"));
    }

    #[test]
    fn a_dry_run_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        let mut out = Vec::new();
        install_gemini_hooks(&mut out, &p, "/abs/herdr-tower", true).unwrap();
        assert!(!p.exists());
        assert!(String::from_utf8(out)
            .unwrap()
            .ends_with("(dry-run — no file was changed)\n"));
    }

    #[test]
    fn a_hooks_file_that_will_not_parse_stops_the_install() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        std::fs::write(&p, "{oops").unwrap();
        let mut out = Vec::new();
        let e = install_gemini_hooks(&mut out, &p, "/abs/herdr-tower", false).unwrap_err();
        assert!(e.to_string().contains("would not parse"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{oops");
    }

    /// `PreToolUse` must never appear: its output is read as the approval
    /// decision, so registering it would put the supervisor in the middle of the
    /// user's permission prompts.
    #[test]
    fn the_approval_hook_is_never_registered() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        run(&p, "/abs/herdr-tower");
        assert!(!std::fs::read_to_string(&p).unwrap().contains("PreToolUse"));
    }
}
