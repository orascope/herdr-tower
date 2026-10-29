// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The embedded startup plugin is a shell script; it is installed on Unix.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[cfg(unix)]
#[test]
fn the_binary_installs_and_links_its_embedded_startup_plugin() {
    let root = tempfile::tempdir().unwrap();
    let fake_herdr = root.path().join("herdr");
    let args_file = root.path().join("herdr-args");
    std::fs::write(
        &fake_herdr,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HERDR_ARGS_FILE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_herdr, std::fs::Permissions::from_mode(0o755)).unwrap();

    let data_home = root.path().join("data");
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-tower"))
        .args(["dock", "--install-startup"])
        .env("HOME", root.path().join("home"))
        .env("XDG_DATA_HOME", &data_home)
        .env("HERDR_BIN_PATH", &fake_herdr)
        .env("HERDR_ARGS_FILE", &args_file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dir = data_home.join("herdr-tower/plugins/dock-autostart");
    assert_eq!(
        std::fs::read_to_string(dir.join("herdr-plugin.toml")).unwrap(),
        include_str!("../../../plugins/dock-autostart/herdr-plugin.toml")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("startup.sh")).unwrap(),
        include_str!("../../../plugins/dock-autostart/startup.sh")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("herdr-tower-path")).unwrap(),
        format!("{}\n", env!("CARGO_BIN_EXE_herdr-tower"))
    );
    assert_eq!(
        std::fs::read_to_string(args_file).unwrap(),
        format!("plugin\nlink\n{}\n", dir.display())
    );
}
