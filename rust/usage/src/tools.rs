// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Reaching an external binary from a minimal environment.

use std::path::{Path, PathBuf};

use crate::ctx::home_dir;

/// Where to look for an external tool, and with what `PATH`.
///
/// Both are injected rather than read at the point of use. That is not only for
/// tests: the tower runs from places with a minimal environment — a popup, a
/// LaunchAgent, a systemd unit — where `PATH` is `/usr/bin:/bin` and none of
/// the usual install locations are on it.
#[derive(Debug, Clone, Default)]
pub struct ToolSearch {
    home: Option<PathBuf>,
    path: String,
    /// Leaves out the machine-wide install locations (`/opt/homebrew/bin`,
    /// `/usr/local/bin`). See [`ToolSearch::without_system_dirs`].
    no_system_dirs: bool,
}

impl ToolSearch {
    /// The real environment: `$HOME` and `$PATH`.
    pub fn from_env() -> Self {
        ToolSearch {
            home: home_dir(),
            path: std::env::var("PATH").unwrap_or_default(),
            no_system_dirs: false,
        }
    }

    /// A fixed environment, for tests and for callers that already know both.
    pub fn new(home: Option<PathBuf>, path: impl Into<String>) -> Self {
        ToolSearch {
            home,
            path: path.into(),
            no_system_dirs: false,
        }
    }

    /// Searches only the given `PATH` and the given home, never the machine's
    /// own install locations — so a test's answer does not change with what
    /// happens to be installed where it runs.
    pub fn without_system_dirs(mut self) -> Self {
        self.no_system_dirs = true;
        self
    }

    /// Install locations worth checking when `PATH` comes up empty.
    pub fn dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = &self.home {
            dirs.push(home.join(".local").join("bin"));
            if let Some(nvm) = nvm_latest_bin(home) {
                // An nvm-only user has no system node at all, so `npx` exists
                // nowhere else.
                dirs.push(nvm);
            }
        }
        if !self.no_system_dirs {
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
        }
        dirs
    }

    /// Finds `name` on `PATH`, then in the usual install locations.
    pub fn lookup(&self, name: &str) -> Option<PathBuf> {
        tower_os::split_path(&self.path)
            .chain(self.dirs())
            .flat_map(|d| tower_os::exe_candidates(&d, name))
            .find(|p| tower_os::is_executable_file(p))
    }

    /// `PATH` widened to cover [`ToolSearch::dirs`], for handing to a child.
    ///
    /// The child needs it as much as we do: `npx`'s shebang looks up `node` on
    /// its own `PATH`.
    pub fn extended_path(&self) -> String {
        // Entries, not string concatenation. Appending `":" + dir` to an *empty*
        // inherited `PATH` produced a value starting with `:`, and an empty
        // component means the current working directory — so a child launched from
        // an untrusted checkout would search that checkout first. The MCP server
        // passes this to its children, and npx's `/usr/bin/env` shebang would have
        // found a repository-provided `node`.
        let mut out: Vec<String> = self
            .path
            .split(tower_os::PATH_SEP)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        for d in self.dirs() {
            let d = d.to_string_lossy().into_owned();
            if !d.is_empty() && !out.contains(&d) {
                out.push(d);
            }
        }
        out.join(&tower_os::PATH_SEP.to_string())
    }
}

/// `~/.nvm/versions/node/<highest version>/bin`, if nvm is installed.
///
/// Highest rather than "the one nvm currently points at": the tower is not in
/// the user's shell, so `nvm use` has told it nothing, and the newest install
/// is the best available guess.
fn nvm_latest_bin(home: &Path) -> Option<PathBuf> {
    let root = home.join(".nvm").join("versions").join("node");
    let entries = std::fs::read_dir(&root).ok()?;
    let mut best: Option<((u64, u64, u64), String)> = None;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(v) = parse_node_version(&name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(bv, _)| *bv < v) {
            best = Some((v, name));
        }
    }
    best.map(|(_, name)| root.join(name).join("bin"))
}

/// `vMAJOR.MINOR.PATCH`, tolerating a suffix on the patch (`v22.3.0-rc1`): the
/// patch is the digits it starts with.
fn parse_node_version(name: &str) -> Option<(u64, u64, u64)> {
    let rest = name.strip_prefix('v')?;
    let mut parts = rest.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let tail = parts.next()?;
    let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let patch = tail[..digits].parse().ok()?;
    Some((major, minor, patch))
}
