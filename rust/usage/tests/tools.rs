// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Reaching a binary a minimal PATH cannot see.

use std::path::PathBuf;

use tower_usage::ToolSearch;

/// A popup or a LaunchAgent starts with `/usr/bin:/bin` and none of the places
/// a user's tools actually live.
#[test]
fn tool_search_covers_the_usual_install_locations() {
    let home = PathBuf::from("/home/x");
    let dirs = ToolSearch::new(Some(home.clone()), "/usr/bin:/bin").dirs();
    assert!(dirs.contains(&home.join(".local/bin")));
    assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    assert!(dirs.contains(&PathBuf::from("/usr/local/bin")));
}

/// A test's search leaves the machine's own install locations out, so what
/// happens to be installed where it runs cannot change its answer — and keeps
/// the home it was given.
#[test]
fn without_system_dirs_keeps_only_the_given_home() {
    let home = PathBuf::from("/home/x");
    let dirs = ToolSearch::new(Some(home.clone()), "")
        .without_system_dirs()
        .dirs();
    assert_eq!(dirs, [home.join(".local/bin")]);
}

/// Leaving the system locations out only ever removes: what remains is a
/// subset of the full search, in the same order — and a PATH that already
/// names a system location still reaches it.
#[test]
fn without_system_dirs_only_removes() {
    for home in [None, Some(PathBuf::from("/home/x"))] {
        let full = ToolSearch::new(home.clone(), "").dirs();
        let narrow = ToolSearch::new(home.clone(), "")
            .without_system_dirs()
            .dirs();
        let kept: Vec<_> = full
            .iter()
            .filter(|d| narrow.contains(d))
            .cloned()
            .collect();
        assert_eq!(kept, narrow, "{home:?}");
        assert!(!narrow.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(!narrow.contains(&PathBuf::from("/usr/local/bin")));
    }
    let ts = ToolSearch::new(None, "/usr/bin:/opt/homebrew/bin").without_system_dirs();
    assert_eq!(ts.extended_path(), "/usr/bin:/opt/homebrew/bin");
    assert!(!ToolSearch::new(None, "/usr/bin")
        .without_system_dirs()
        .extended_path()
        .contains("/usr/local/bin"));
}

/// An nvm-only user has no system node at all, so `npx` exists nowhere but
/// under the version nvm installed.
#[test]
fn tool_search_finds_the_newest_nvm_node() {
    let d = tempfile::tempdir().unwrap();
    for v in ["v20.1.0", "v22.3.0", "v9.9.9", "not-a-version"] {
        std::fs::create_dir_all(d.path().join(".nvm/versions/node").join(v).join("bin")).unwrap();
    }
    let dirs = ToolSearch::new(Some(d.path().into()), "").dirs();
    assert!(
        dirs.contains(&d.path().join(".nvm/versions/node/v22.3.0/bin")),
        "9.9.9 must not beat 22.3.0 on a string compare: {dirs:?}"
    );
}

/// The child needs the widened PATH too — `npx`'s shebang looks up `node`.
#[cfg(unix)]
#[test]
fn extended_path_appends_without_duplicating() {
    let ts = ToolSearch::new(None, "/usr/bin:/usr/local/bin");
    let path = ts.extended_path();
    assert!(path.starts_with("/usr/bin:/usr/local/bin"));
    assert!(path.contains(":/opt/homebrew/bin"));
    assert_eq!(
        path.matches("/usr/local/bin").count(),
        1,
        "already present, do not re-append"
    );
}

/// PATH is searched first; the fallback locations exist for when it is bare.
#[cfg(unix)]
#[test]
fn lookup_falls_back_off_path() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let d = tempfile::tempdir().unwrap();
    let bin = d.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("some-tool");
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();

    let ts = ToolSearch::new(Some(d.path().into()), "/nonexistent");
    assert_eq!(
        ts.lookup("some-tool"),
        None,
        "not executable yet: not a tool"
    );

    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(ts.lookup("some-tool").as_deref(), Some(tool.as_path()));
    assert_eq!(ts.lookup("definitely-not-installed"), None);
}
