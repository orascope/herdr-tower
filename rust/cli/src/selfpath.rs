// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The path to write down for this binary.
//!
//! Every registration `init` makes — the agents' hooks, the opencode plugin,
//! the supervisor's service definition — names this binary by path, and each
//! of them breaks silently when that path goes away: a hook never reports its
//! own failure. `std::env::current_exe` is the wrong path to write for a
//! package manager's install. On Linux it reads `/proc/self/exe`, which has
//! every symlink resolved (measured 2026-09-24): run through Homebrew's
//! `bin/herdr-tower` link, it answers `…/Cellar/herdr-tower/<version>/bin/…`,
//! which the next `brew upgrade` deletes. The spelling the binary was invoked
//! by keeps the link.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The invoked spelling of this binary when it is the same file as `exe`,
/// else `exe`.
///
/// `argv0` with a separator in it is a path already; a bare name is looked up
/// on `path_var` the way the shell that started it would have. The candidate is
/// only taken when it resolves to `exe` itself: `argv0` is whatever the parent
/// chose to pass, and naming some other `herdr-tower` would be worse than the
/// versioned path.
pub fn stable(argv0: Option<&OsStr>, path_var: Option<&OsStr>, exe: &Path) -> PathBuf {
    let Some(argv0) = argv0.filter(|a| !a.is_empty()) else {
        return exe.to_path_buf();
    };
    let invoked = Path::new(argv0);
    let candidate = if invoked.components().count() > 1 {
        std::path::absolute(invoked).ok()
    } else {
        path_var.and_then(|p| {
            std::env::split_paths(p)
                .map(|dir| dir.join(invoked))
                .find(|c| c.is_file())
                .and_then(|c| std::path::absolute(c).ok())
        })
    };
    match candidate {
        Some(c) if same_file(&c, exe) => c,
        _ => exe.to_path_buf(),
    }
}

/// This process's own [`stable`] path.
pub fn current() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(stable(
        std::env::args_os().next().as_deref(),
        std::env::var_os("PATH").as_deref(),
        &exe,
    ))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A Homebrew-shaped install: the real binary under a versioned folder,
    /// a link to it in `bin/`.
    #[cfg(unix)]
    fn brew() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("Cellar/herdr-tower/0.19.9/bin/herdr-tower");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "bin").unwrap();
        let link = d.path().join("bin/herdr-tower");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        (d, real, link)
    }

    #[cfg(unix)]
    #[test]
    fn a_bare_name_found_on_path_keeps_the_link() {
        let (d, real, link) = brew();
        let path = d.path().join("bin");
        assert_eq!(
            stable(
                Some(OsStr::new("herdr-tower")),
                Some(path.as_os_str()),
                &real
            ),
            link
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_invoked_path_keeps_the_link() {
        let (_d, real, link) = brew();
        assert_eq!(stable(Some(link.as_os_str()), None, &real), link);
    }

    /// argv0 naming a different file is not believed.
    #[cfg(unix)]
    #[test]
    fn another_binary_of_the_same_name_is_not_taken() {
        let (d, real, _link) = brew();
        let other = d.path().join("other/herdr-tower");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "different").unwrap();
        assert_eq!(stable(Some(other.as_os_str()), None, &real), real);
        let path = d.path().join("other");
        assert_eq!(
            stable(
                Some(OsStr::new("herdr-tower")),
                Some(path.as_os_str()),
                &real
            ),
            real
        );
    }

    #[cfg(unix)]
    #[test]
    fn no_argv0_answers_the_exe() {
        let (_d, real, _link) = brew();
        assert_eq!(stable(None, None, &real), real);
        assert_eq!(stable(Some(OsStr::new("")), None, &real), real);
    }
}
