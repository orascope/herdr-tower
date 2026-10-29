// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The Herdr startup plugin that restores the dock after a server restart.
//!
//! The plugin ships inside the binary rather than beside it. Homebrew installs
//! only the binary, and the release installer extracts only the binary, so a
//! repository-relative plugin path would work only for source checkouts.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Kept as `plugin.toml` in the repository: the herdr.dev index lists every
/// `herdr-plugin.toml` it finds, and the repository's one listing is the root
/// manifest. [`install`] writes it under herdr's name.
pub const MANIFEST: &str = include_str!("../../../plugins/dock-autostart/plugin.toml");
pub const STARTUP: &str = include_str!("../../../plugins/dock-autostart/startup.sh");

/// The stable user-owned directory linked into Herdr's plugin registry.
pub fn install_dir(home: &Path, xdg_data_home: Option<&OsStr>) -> PathBuf {
    let data_home = xdg_data_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    data_home
        .join("herdr-tower")
        .join("plugins")
        .join("dock-autostart")
}

/// Writes the embedded plugin atomically and with executable startup code.
pub fn install(w: &mut impl Write, dir: &Path, executable: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    crate::initcmd::settingsfile::write_atomic(
        &dir.join("herdr-plugin.toml"),
        ".tmp",
        MANIFEST.as_bytes(),
        0o644,
    )?;
    crate::initcmd::settingsfile::write_atomic(
        &dir.join("startup.sh"),
        ".tmp",
        STARTUP.as_bytes(),
        0o755,
    )?;
    let executable = format!("{}\n", executable.display());
    crate::initcmd::settingsfile::write_atomic(
        &dir.join("herdr-tower-path"),
        ".tmp",
        executable.as_bytes(),
        0o644,
    )?;
    writeln!(w, "dock startup plugin files: {}", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_data_home_wins_and_an_empty_one_falls_back() {
        let home = Path::new("/home/person");
        assert_eq!(
            install_dir(home, Some(OsStr::new("/data"))),
            Path::new("/data/herdr-tower/plugins/dock-autostart")
        );
        assert_eq!(
            install_dir(home, Some(OsStr::new(""))),
            Path::new("/home/person/.local/share/herdr-tower/plugins/dock-autostart")
        );
    }

    #[cfg(unix)]
    #[test]
    fn installation_writes_the_embedded_plugin_with_safe_modes() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plugin");
        let mut out = Vec::new();
        install(&mut out, &dir, Path::new("/opt/homebrew/bin/herdr-tower")).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("herdr-plugin.toml")).unwrap(),
            MANIFEST
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("startup.sh")).unwrap(),
            STARTUP
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("herdr-tower-path")).unwrap(),
            "/opt/homebrew/bin/herdr-tower\n"
        );
        assert_eq!(
            std::fs::metadata(dir.join("herdr-plugin.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(dir.join("startup.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(String::from_utf8(out)
            .unwrap()
            .contains(&dir.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn startup_uses_the_persisted_binary_outside_path_and_requires_active() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("homebrew/herdr-tower");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$TOWER_ARGS_FILE\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plugin = root.path().join("plugin");
        install(&mut Vec::new(), &plugin, &binary).unwrap();
        let args_file = root.path().join("tower-args");
        let status = Command::new("sh")
            .arg(plugin.join("startup.sh"))
            .current_dir(&plugin)
            .env("PATH", "/usr/bin:/bin")
            .env("TOWER_ARGS_FILE", &args_file)
            .status()
            .unwrap();

        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(args_file).unwrap(),
            "dock\n--require-active\n"
        );
    }
}
