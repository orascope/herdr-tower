// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Init command handler.

use std::io::Write as _;
use std::path::Path;

use super::common::*;
use super::Result;

/// `herdr-tower init [--dry-run]`
///
/// Registers hooks with whichever CLIs are installed, and
/// prints the herdr key binding rather than editing herdr's config — that file
/// is the user's, and `herdr --default-config` already documents the shape.
///
/// Each vendor's block is guarded by the *directory* it would write into, so a
/// machine with only claude is never told about codex. The exception is `agy`,
/// whose config folder does not exist until first run.
pub fn run_init(args: &[String]) -> Result<()> {
    // Parsed, not scanned. A misspelled `--dry-rnu` read as "no flag" and went on
    // to rewrite every detected CLI's hooks and skills — and a
    // misspelling is *most* likely on the one flag whose whole purpose is to
    // change nothing.
    let fs = tower_cli::helpcmd::FlagSet::new("init").bool(
        "dry-run",
        false,
        "print what it would do and change nothing",
    );
    let dry_run = flags_only("init", &fs, args)?.bool("dry-run");
    let home = home()?;
    let bin = self_path();
    let mut out = std::io::stdout().lock();

    tower_cli::daemoncmd::install(
        &mut out,
        &home,
        Path::new(&bin),
        &tower_state::default_dir(),
        config_override()?.as_deref(),
        dry_run,
    )?;
    writeln!(out)?;

    let _ = out.flush();

    let claude_settings = home.join(".claude").join("settings.json");
    writeln!(out, "Claude Code hooks: {}", claude_settings.display())?;
    tower_cli::initcmd::install_claude_hooks(&mut out, &claude_settings, &bin, dry_run)?;
    writeln!(out)?;

    let codex_dir = home.join(".codex");
    let codex_config = codex_dir.join("config.toml");
    if codex_dir.is_dir() {
        writeln!(out, "Codex notify: {}", codex_config.display())?;
        tower_cli::codexinit::install_codex_notify(&mut out, &codex_config, &bin, dry_run)?;
        let hooks = codex_dir.join("hooks.json");
        writeln!(out, "Codex hooks: {}", hooks.display())?;
        tower_cli::codexinit::install_codex_hooks(&mut out, &hooks, &bin, dry_run)?;
        writeln!(out)?;
    }

    let gemini_dir = home.join(".gemini");

    // Only when `agy` is actually there. The file is shared — `agy`'s own
    // `/hooks` writes here too — so registering into it on a machine without
    // `agy` would leave a hook that nothing ever fires.
    let gemini_hooks = gemini_dir.join("config").join("hooks.json");
    if agy_installed(&home) {
        writeln!(out, "Gemini (agy) hooks: {}", gemini_hooks.display())?;
        tower_cli::geminiinit::install_gemini_hooks(&mut out, &gemini_hooks, &bin, dry_run)?;
        writeln!(out)?;
    }

    let gemini_settings = gemini_dir.join("settings.json");
    if gemini_dir.is_dir() {
        writeln!(out, "Gemini CLI hooks: {}", gemini_settings.display())?;
        tower_cli::initcmd::install_gemini_cli_hooks(&mut out, &gemini_settings, &bin, dry_run)?;
        writeln!(out)?;
    }

    // Guarded by the config directory like the others: a machine that never
    // ran opencode is not given a plugin for it.
    let opencode_dir =
        tower_cli::opencodeinit::config_dir(&home, std::env::var_os("XDG_CONFIG_HOME").as_deref());
    if opencode_dir.is_dir() {
        writeln!(
            out,
            "opencode plugin: {}",
            opencode_dir
                .join("plugins")
                .join("herdr-tower.js")
                .display()
        )?;
        tower_cli::opencodeinit::install_opencode_plugin(&mut out, &opencode_dir, &bin, dry_run)?;
        writeln!(out)?;
    }

    let skills_dir = home.join(".claude").join("skills");
    writeln!(out, "Skills: {}", skills_dir.display())?;
    tower_cli::skills::install_skills(&mut out, &skills_dir, dry_run)?;
    writeln!(out)?;

    tower_cli::initcmd::print_herdr_binding(&mut out, &bin)?;
    Ok(())
}
