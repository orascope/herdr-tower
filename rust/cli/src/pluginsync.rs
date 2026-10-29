// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Bringing the plugin files the tower owns up to this build, at supervisor start.
//!
//! An upgrade replaces the binary, and `daemon install` restarts the
//! supervisor on it — but the opencode plugin and the dock's startup plugin
//! are copies written by an earlier build, and they stayed that way until
//! someone ran `init` or `dock` again. `doctor` said so; nothing fixed it.
//!
//! Only files the tower owns whole are touched here, and only a copy that is
//! already installed **and names this binary**:
//!
//! - Owned whole: `~/.claude/settings.json` and `~/.codex/hooks.json` hold the
//!   user's own settings beside ours, and a daemon that rewrote them without
//!   being asked would be a daemon nobody should run. Those stay with `init`.
//! - Already installed: a machine without opencode is not given a plugin for
//!   it, and a dock nobody set up is not set up behind their back.
//! - Names this binary: a test build or a second checkout running a
//!   supervisor must not repoint the user's plugins at itself.

use std::path::Path;

/// What was refreshed, or could not be — one line each, for the supervisor log.
pub fn sync_owned(opencode_dir: &Path, dock_dir: &Path, exe: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let bin = exe.to_string_lossy();
    let opencode = opencode_dir.join("plugins").join("herdr-tower.js");
    if let Ok(have) = std::fs::read_to_string(&opencode) {
        let want = crate::opencodeinit::render(&bin);
        if have != want
            && crate::doctorcmd::binaries_in(&have)
                .iter()
                .any(|b| *b == bin)
        {
            out.push(
                match crate::opencodeinit::install_opencode_plugin(
                    &mut std::io::sink(),
                    opencode_dir,
                    &bin,
                    false,
                ) {
                    Ok(()) => format!("refreshed {} for this build", opencode.display()),
                    Err(e) => format!("could not refresh {}: {e}", opencode.display()),
                },
            );
        }
    }
    let named = std::fs::read_to_string(dock_dir.join("herdr-tower-path"));
    if named.is_ok_and(|named| named.trim_end() == bin) && dock_stale(dock_dir) {
        out.push(
            match crate::dockplugin::install(&mut std::io::sink(), dock_dir, exe) {
                Ok(()) => format!("refreshed {} for this build", dock_dir.display()),
                Err(e) => format!("could not refresh {}: {e}", dock_dir.display()),
            },
        );
    }
    out
}

/// Whether the dock plugin's embedded files differ from this build's.
fn dock_stale(dir: &Path) -> bool {
    let differs = |name: &str, want: &str| {
        std::fs::read_to_string(dir.join(name)).map_or(true, |have| have != want)
    };
    differs("herdr-plugin.toml", crate::dockplugin::MANIFEST)
        || differs("startup.sh", crate::dockplugin::STARTUP)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/bin/herdr-tower";

    fn opencode_file(dir: &Path) -> std::path::PathBuf {
        dir.join("plugins").join("herdr-tower.js")
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn stale_opencode_plugin(bin: &str) -> String {
        format!("// an earlier build\nconst BIN = {:?};\n", bin)
    }

    /// A space in the ownership path must not leave an old plugin installed.
    #[test]
    fn a_stale_plugin_with_a_spaced_binary_becomes_current() {
        let bin = "/opt/my tools/herdr-tower";
        let d = tempfile::tempdir().unwrap();
        write(&opencode_file(d.path()), &stale_opencode_plugin(bin));
        assert!(
            !crate::doctorcmd::check_opencode_plugin(d.path(), bin)
                .unwrap()
                .ok
        );
        let lines = sync_owned(d.path(), &d.path().join("dock"), Path::new(bin));
        assert_eq!(lines.len(), 1, "{lines:?}");
        let finding = crate::doctorcmd::check_opencode_plugin(d.path(), bin).unwrap();
        assert!(
            finding.ok && finding.line.ends_with(": current"),
            "{finding:?}"
        );
    }

    #[test]
    fn a_stale_opencode_plugin_naming_this_binary_is_rewritten() {
        let d = tempfile::tempdir().unwrap();
        let file = opencode_file(d.path());
        write(&file, &stale_opencode_plugin(EXE));

        let lines = sync_owned(d.path(), &d.path().join("dock"), Path::new(EXE));

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            crate::opencodeinit::render(EXE)
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("refreshed "), "{lines:?}");
    }

    #[test]
    fn an_opencode_plugin_naming_another_binary_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let file = opencode_file(d.path());
        let theirs = stale_opencode_plugin("/usr/local/bin/herdr-tower");
        write(&file, &theirs);

        let lines = sync_owned(d.path(), &d.path().join("dock"), Path::new(EXE));

        assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs);
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_missing_opencode_plugin_is_not_installed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("plugins")).unwrap();

        let lines = sync_owned(d.path(), &d.path().join("dock"), Path::new(EXE));

        assert!(!opencode_file(d.path()).exists());
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_current_opencode_plugin_is_not_rewritten_or_logged() {
        let d = tempfile::tempdir().unwrap();
        write(&opencode_file(d.path()), &crate::opencodeinit::render(EXE));

        assert!(sync_owned(d.path(), &d.path().join("dock"), Path::new(EXE)).is_empty());
    }

    #[test]
    fn a_stale_dock_plugin_naming_this_binary_is_rewritten() {
        let d = tempfile::tempdir().unwrap();
        let dock = d.path().join("dock");
        write(&dock.join("herdr-plugin.toml"), "# an earlier manifest\n");
        write(&dock.join("startup.sh"), crate::dockplugin::STARTUP);
        write(&dock.join("herdr-tower-path"), &format!("{EXE}\n"));

        let lines = sync_owned(&d.path().join("opencode"), &dock, Path::new(EXE));

        assert_eq!(
            std::fs::read_to_string(dock.join("herdr-plugin.toml")).unwrap(),
            crate::dockplugin::MANIFEST
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
    }

    #[test]
    fn a_dock_plugin_for_another_binary_or_none_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let dock = d.path().join("dock");
        write(&dock.join("herdr-plugin.toml"), "# an earlier manifest\n");
        write(&dock.join("herdr-tower-path"), "/elsewhere/herdr-tower\n");

        assert!(sync_owned(&d.path().join("opencode"), &dock, Path::new(EXE)).is_empty());
        assert_eq!(
            std::fs::read_to_string(dock.join("herdr-plugin.toml")).unwrap(),
            "# an earlier manifest\n"
        );
        // No dock at all: nothing is created.
        let none = d.path().join("no-dock");
        assert!(sync_owned(&d.path().join("opencode"), &none, Path::new(EXE)).is_empty());
        assert!(!none.exists());
    }

    /// A stale opencode plugin and a stale dock, both naming `exe`.
    fn stale_everything(root: &Path, exe: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let opencode = root.join("opencode");
        let dock = root.join("dock");
        write(&opencode_file(&opencode), &stale_opencode_plugin(exe));
        write(&dock.join("herdr-plugin.toml"), "# an earlier manifest\n");
        write(&dock.join("startup.sh"), "# an earlier startup\n");
        write(&dock.join("herdr-tower-path"), &format!("{exe}\n"));
        (opencode, dock)
    }

    /// The identity of every file sync may touch: its bytes and its inode,
    /// so a rewrite of the same bytes (a rename over it) shows too.
    #[cfg(unix)]
    fn fingerprint(opencode: &Path, dock: &Path) -> Vec<(Vec<u8>, u64)> {
        use std::os::unix::fs::MetadataExt;
        [
            opencode_file(opencode),
            dock.join("herdr-plugin.toml"),
            dock.join("startup.sh"),
            dock.join("herdr-tower-path"),
        ]
        .iter()
        .map(|p| {
            (
                std::fs::read(p).unwrap(),
                std::fs::metadata(p).unwrap().ino(),
            )
        })
        .collect()
    }

    /// A generated binary path: unicode and a few punctuation characters.
    /// No whitespace and none of the doctor scan's bound characters: in the
    /// plugin's double-quoted `BIN` those end the path early, so sync's
    /// "names this binary" gate does not open for such a path — a known gap,
    /// left out of these generated cases.
    fn generated_exe(rng: &mut tower_testrng::Rng) -> String {
        const PIECES: &[&str] = &[
            "opt", "bin", "세션", "😀", "é", "$", "~", "%", "!", "-", ".",
        ];
        let mut exe = String::new();
        for _ in 0..1 + rng.below(3) {
            exe.push('/');
            for _ in 0..1 + rng.below(3) {
                exe.push_str(rng.pick(PIECES).unwrap());
            }
        }
        exe.push_str("/herdr-tower");
        exe
    }

    /// Idempotency, over generated binary paths (seeded): the first sync
    /// refreshes both plugins and says so, one line each; a second sync
    /// writes nothing — same bytes, same files — and returns no line.
    #[cfg(unix)]
    #[test]
    fn a_second_sync_writes_nothing_and_says_nothing() {
        let mut rng = tower_testrng::Rng::new(0x0005_1DE0_0707);
        for _ in 0..40 {
            let exe = generated_exe(&mut rng);
            let d = tempfile::tempdir().unwrap();
            let (opencode, dock) = stale_everything(d.path(), &exe);
            let first = sync_owned(&opencode, &dock, Path::new(&exe));
            assert_eq!(first.len(), 2, "{exe:?}: {first:?}");
            assert!(
                first.iter().all(|l| l.starts_with("refreshed ")),
                "{first:?}"
            );
            let after_first = fingerprint(&opencode, &dock);
            for _ in 0..2 {
                assert!(sync_owned(&opencode, &dock, Path::new(&exe)).is_empty());
                assert_eq!(fingerprint(&opencode, &dock), after_first, "{exe:?}");
            }
        }
    }

    /// Round trip with doctor, over generated binary paths (seeded): the
    /// plugin sync writes is the one doctor calls current for this binary
    /// (and not for another), and doctor's scan of it finds this binary —
    /// the gate the next build's sync opens with.
    #[test]
    fn after_a_sync_doctor_calls_the_plugin_current() {
        let mut rng = tower_testrng::Rng::new(0x000D_0C75_1DE0);
        for _ in 0..40 {
            let exe = generated_exe(&mut rng);
            let d = tempfile::tempdir().unwrap();
            let (opencode, dock) = stale_everything(d.path(), &exe);
            let before = crate::doctorcmd::check_opencode_plugin(&opencode, &exe).unwrap();
            assert!(!before.ok, "{}", before.line);
            sync_owned(&opencode, &dock, Path::new(&exe));
            let after = crate::doctorcmd::check_opencode_plugin(&opencode, &exe).unwrap();
            assert!(
                after.ok && after.line.ends_with(": current"),
                "{}",
                after.line
            );
            let other = crate::doctorcmd::check_opencode_plugin(&opencode, "/else/herdr-tower");
            assert!(!other.unwrap().ok);
            let body = std::fs::read_to_string(opencode_file(&opencode)).unwrap();
            assert_eq!(
                crate::doctorcmd::binaries_in(&body),
                [exe.as_str()],
                "{exe:?}"
            );
            assert!(!dock_stale(&dock));
        }
    }
}
