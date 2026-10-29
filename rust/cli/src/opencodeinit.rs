// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Installing the opencode plugin that serves as its hook.
//!
//! opencode reads no hook configuration; it loads every script in
//! `~/.config/opencode/plugins/`. So registering with it is writing one file,
//! and owning that file whole: `herdr-tower.js` is ours, and is rewritten
//! rather than merged. The script ships inside the binary, as the dock plugin
//! does, because an installed binary has no repository beside it.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::initcmd::settingsfile::{write_atomic, TMP_SUFFIX};

/// The plugin, with `__HERDR_TOWER_BIN__` where the binary's path goes.
pub const PLUGIN: &str = include_str!("../../../plugins/opencode/herdr-tower.js");

const BIN_SLOT: &str = "__HERDR_TOWER_BIN__";

/// opencode's config directory: `$XDG_CONFIG_HOME/opencode`, else
/// `~/.config/opencode` — where opencode itself looks.
pub fn config_dir(home: &Path, xdg_config_home: Option<&OsStr>) -> PathBuf {
    xdg_config_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("opencode")
}

/// The plugin as it is written for `bin_path`.
///
/// The path goes in as a JSON string, which is a valid JavaScript string
/// literal, so a path with a quote or a backslash in it cannot break the
/// script.
pub fn render(bin_path: &str) -> String {
    let bin_path = if bin_path.is_empty() {
        "herdr-tower"
    } else {
        bin_path
    };
    let quoted = serde_json::to_string(bin_path).unwrap_or_else(|_| "\"herdr-tower\"".into());
    PLUGIN.replace(BIN_SLOT, &quoted)
}

/// Writes `plugins/herdr-tower.js` under opencode's config directory, or says
/// it is already current.
pub fn install_opencode_plugin(
    w: &mut impl Write,
    config_dir: &Path,
    bin_path: &str,
    dry_run: bool,
) -> io::Result<()> {
    let path = config_dir.join("plugins").join("herdr-tower.js");
    let want = render(bin_path);
    if std::fs::read_to_string(&path).is_ok_and(|have| have == want) {
        return writeln!(w, "  already installed");
    }
    if dry_run {
        return writeln!(w, "  would write {}", path.display());
    }
    std::fs::create_dir_all(path.parent().unwrap_or(config_dir))?;
    write_atomic(&path, TMP_SUFFIX, want.as_bytes(), 0o644)?;
    writeln!(
        w,
        "  wrote {} (takes effect in opencode sessions started from now on)",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_config_home_wins_and_an_empty_one_falls_back() {
        let home = Path::new("/home/p");
        assert_eq!(
            config_dir(home, Some(OsStr::new("/cfg"))),
            Path::new("/cfg/opencode")
        );
        assert_eq!(
            config_dir(home, Some(OsStr::new(""))),
            Path::new("/home/p/.config/opencode")
        );
        assert_eq!(
            config_dir(home, None),
            Path::new("/home/p/.config/opencode")
        );
    }

    /// The path lands as a string literal, whatever it contains, and the slot
    /// is gone.
    #[test]
    fn the_binary_path_is_written_as_a_string_literal() {
        let js = render(r#"/opt/we"ird\bin/herdr-tower"#);
        assert!(js.contains(r#"const BIN = "/opt/we\"ird\\bin/herdr-tower";"#));
        assert!(!js.contains(BIN_SLOT));
        assert!(render("").contains(r#"const BIN = "herdr-tower";"#));
    }

    /// Installed once, then left alone; a moved binary rewrites it; a dry run
    /// writes nothing.
    #[test]
    fn installation_is_idempotent_and_follows_the_binary() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("plugins").join("herdr-tower.js");

        let mut out = Vec::new();
        install_opencode_plugin(&mut out, d.path(), "/a/herdr-tower", true).unwrap();
        assert!(!file.exists(), "a dry run writes nothing");

        install_opencode_plugin(&mut out, d.path(), "/a/herdr-tower", false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            render("/a/herdr-tower")
        );

        let mut again = Vec::new();
        install_opencode_plugin(&mut again, d.path(), "/a/herdr-tower", false).unwrap();
        assert_eq!(String::from_utf8(again).unwrap(), "  already installed\n");

        install_opencode_plugin(&mut out, d.path(), "/b/herdr-tower", false).unwrap();
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains(r#""/b/herdr-tower""#));
    }
}
