// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower version`, and the judgement of what counts as a release.
//!
//! Two lines, because two different questions get asked of a version. The
//! first answers "which build is this" for a person filing a report; the
//! second answers "what was it built for" when a binary turns up on the wrong
//! machine.

/// Build stamps injected at compile time.
///
/// A release pipeline fills all three; a plain local build fills none, which is
/// why every field degrades on its own rather than the struct being optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionInfo {
    pub version: String,
    pub commit: String,
    pub date: String,
}

impl VersionInfo {
    /// What this binary was actually built from.
    ///
    /// Cargo records none of this in the binary, so `build.rs` gathers the
    /// facts at compile time and hands them over as these variables — see that
    /// file for the precedence between an injected value and git.
    ///
    /// Empty variables are the normal case for a build from a tarball or a
    /// sandbox with no `git`, and [`format_version`] already degrades to `dev`.
    pub fn stamped() -> VersionInfo {
        VersionInfo {
            version: env!("AL_VERSION").to_string(),
            commit: env!("AL_COMMIT").to_string(),
            date: env!("AL_DATE").to_string(),
        }
    }
}

/// `herdr-tower version` output:
///
/// ```text
/// herdr-tower v1.1.0 (commit 340e7fd, 2026-08-27)
/// rust1.96.1 macos/aarch64
/// ```
///
/// The second line names the toolchain and the platform because it answers a
/// different question: why a binary will not run, or misbehaves, on a
/// particular machine. Rust cannot learn its compiler version at run time, so
/// the compiler version arrives from `build.rs` and the line degrades to the
/// platform alone when it is empty — which is what a build from a tarball with
/// no `rustc` on `PATH` gets.
///
/// The platform names are Rust's own (`macos/aarch64`): the line says which
/// toolchain produced the binary, and spelling the platform in any other
/// toolchain's vocabulary would misreport exactly that.
pub fn format_version(v: &VersionInfo) -> String {
    let ver = version_label(v);

    let mut parts: Vec<String> = Vec::new();
    let commit = short_commit(&v.commit);
    if !commit.is_empty() {
        parts.push(format!("commit {commit}"));
    }
    let date = short_date(&v.date);
    if !date.is_empty() {
        parts.push(date);
    }

    let mut head = format!("herdr-tower {ver}");
    if !parts.is_empty() {
        head.push_str(&format!(" ({})", parts.join(", ")));
    }
    format!("{head}\n{}", toolchain_line())
}

/// Compact build label for places where the full diagnostic version is too
/// wide, such as the popup title.
pub fn version_label(v: &VersionInfo) -> String {
    let mut ver = v.version.trim().to_string();
    if ver.is_empty() {
        ver = "dev".into();
    }
    // `dev` is a word, not a version, so it does not get the `v`.
    if ver != "dev" && !ver.starts_with('v') {
        ver.insert(0, 'v');
    }

    ver
}

/// `rust1.96.1 macos/aarch64`, or `rust macos/aarch64` where the compiler
/// version could not be learned at build time.
///
/// No separator between `rust` and the number: it reads as one token naming a
/// toolchain, and that is what it is.
fn toolchain_line() -> String {
    format!(
        "rust{} {}/{}",
        env!("AL_RUSTC"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// A commit hash cut to seven characters, keeping any dirty marker.
///
/// The marker survives on purpose: `340e7fd+dirty` is a different build from
/// `340e7fd`, and truncating the suffix away would report the second when it
/// was the first — the one case where the version string lies about which
/// code is running.
fn short_commit(c: &str) -> String {
    let c = c.trim();
    if c.is_empty() {
        return String::new();
    }
    let (body, suffix) = match c.strip_suffix("+dirty") {
        Some(rest) => (rest, "+dirty"),
        None => (c, ""),
    };
    // Counted in characters, not bytes. A commit hash is hex, so the two
    // agree; a hand-set `commit` that is not is merely cut short here rather
    // than split in the middle of a character.
    let short: String = body.chars().take(7).collect();
    format!("{short}{suffix}")
}

/// The date out of an RFC 3339 stamp, or the input untouched if it is not one.
///
/// Byte positions rather than a parse: the goal is to drop a time nobody
/// reads, not to validate a timestamp, and a stamp that fails the shape test
/// is shown as-is rather than swallowed.
///
/// The cut goes through `get` rather than an index, because slicing a `str`
/// panics on a cut inside a character. The shape test can pass on
/// a string whose tenth byte is mid-character (`abcd-fg-i` then a Korean
/// syllable), and `herdr-tower version` taking the process down over a
/// hand-injected build date would be absurd — so a cut that would land there
/// falls through to showing the value whole.
fn short_date(d: &str) -> String {
    let d = d.trim();
    let b = d.as_bytes();
    if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        if let Some(day) = d.get(..10) {
            return day.to_string();
        }
    }
    d.to_string()
}

/// Whether a version string is a tag a human chose, rather than something a
/// tool made up.
///
/// The callers use this to decide whether to show a version at all, so the
/// bar is "would a person recognize this as a release". A tool-generated
/// pseudo-version for an untagged commit (`v1.0.1-0.20260827055951-9d76f9ff8e0a`) is not one,
/// and neither is a dirty tree — both mean "somebody's working copy".
pub fn is_release_version(v: &str) -> bool {
    let v = v.trim();
    if v.is_empty() || v == "(devel)" {
        return false;
    }
    !v.contains("+dirty") && !has_pseudo_version(v)
}

/// `[-.]\d{14}-[0-9a-f]{12}` searched anywhere in the string.
///
/// Hand-scanned rather than compiled, because a regex crate is not among this
/// crate's dependencies and the pattern is fixed. Every class here is ASCII,
/// so scanning bytes is exact: a multi-byte character can never satisfy any of
/// them, and continuation bytes are all `>= 0x80`.
fn has_pseudo_version(v: &str) -> bool {
    let b = v.as_bytes();
    // 1 separator + 14 digits + 1 dash + 12 hex characters.
    const LEN: usize = 28;
    if b.len() < LEN {
        return false;
    }
    (0..=b.len() - LEN).any(|i| {
        (b[i] == b'-' || b[i] == b'.')
            && b[i + 1..i + 15].iter().all(u8::is_ascii_digit)
            && b[i + 15] == b'-'
            && b[i + 16..i + 28]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_line(s: &str) -> &str {
        s.lines().next().unwrap_or("")
    }

    /// 🚨 A locally built binary says which build it is. `build.rs` asks git
    /// for `describe` when no version was injected, so this is only `dev` when
    /// there is no git to ask — a tarball, a vendored source tree. Anywhere
    /// else, the tower header and a bug report name the commit's ancestry.
    ///
    /// Gated on this build having had git, not on it knowing its commit: the
    /// three stamps degrade independently, and a downstream build that injects
    /// `HERDR_TOWER_COMMIT` into a tree with no git metadata legitimately
    /// keeps `dev` as its version.
    #[test]
    fn a_local_build_is_not_nameless() {
        let stamped = VersionInfo::stamped();
        let had_git = std::process::Command::new("git")
            .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !had_git {
            return;
        }
        assert_ne!(
            version_label(&stamped),
            "dev",
            "a build made inside a repository can name its version"
        );
    }

    #[test]
    fn a_release_build_shows_its_tag_commit_and_day() {
        let got = format_version(&VersionInfo {
            version: "1.1.0".into(),
            commit: "340e7fdabc1234567890".into(),
            date: "2026-08-27T10:11:12Z".into(),
        });
        assert_eq!(
            first_line(&got),
            "herdr-tower v1.1.0 (commit 340e7fd, 2026-08-27)"
        );
        // The toolchain line is what explains a binary misbehaving on one
        // machine, so it has to name the platform even when `build.rs` could not
        // learn the compiler version.
        let second = got.lines().nth(1).unwrap_or_default();
        assert!(second.starts_with("rust"), "{second:?}");
        assert!(
            second.contains(std::env::consts::ARCH),
            "the toolchain line has to name the platform: {second:?}"
        );
    }

    /// A local `cargo build` injects nothing, and must still print something a
    /// person can quote.
    #[test]
    fn a_build_with_no_stamps_calls_itself_dev() {
        assert_eq!(
            first_line(&format_version(&VersionInfo::default())),
            "herdr-tower dev"
        );
    }

    #[test]
    fn a_tag_that_already_starts_with_v_does_not_get_a_second_one() {
        let v = VersionInfo {
            version: "v1.1.0".into(),
            ..VersionInfo::default()
        };
        assert_eq!(first_line(&format_version(&v)), "herdr-tower v1.1.0");
        assert_eq!(version_label(&v), "v1.1.0");
    }

    #[test]
    fn a_commit_without_a_date_stands_on_its_own() {
        let v = VersionInfo {
            version: "dev".into(),
            commit: "340e7fdabc".into(),
            date: String::new(),
        };
        assert_eq!(
            first_line(&format_version(&v)),
            "herdr-tower dev (commit 340e7fd)"
        );
    }

    /// A stamp that passes the shape test but would be cut mid-character comes
    /// back whole. A plain slice would panic, and `version` is not a command
    /// that may take the process down.
    #[test]
    fn a_date_that_cannot_be_cut_cleanly_is_shown_whole() {
        assert_eq!(short_date("2026-08-27T10:11:12Z"), "2026-08-27");
        assert_eq!(short_date("abcd-fg-i가jk"), "abcd-fg-i가jk");
        assert_eq!(short_date("not a date"), "not a date");
        assert_eq!(short_date(""), "");
    }

    /// A dirty build must not be reportable as the clean tag it was cut from.
    #[test]
    fn a_dirty_marker_survives_the_truncation() {
        assert_eq!(short_commit("340e7fdabc1234+dirty"), "340e7fd+dirty");
    }

    #[test]
    fn only_a_tag_a_person_chose_counts_as_a_release() {
        for (v, want) in [
            ("v1.1.0", true),
            ("1.1.0", true),
            ("v1.2.0-rc1", true),
            ("", false),
            ("(devel)", false),
            ("v1.0.1-0.20260827055951-9d76f9ff8e0a", false),
            ("v1.0.1-0.20260827055951-9d76f9ff8e0a+dirty", false),
        ] {
            assert_eq!(is_release_version(v), want, "{v:?}");
        }
    }

    /// The pattern is a search, not an anchor, and it must not fire on a tag
    /// that merely contains long digits.
    #[test]
    fn a_long_numeric_tag_is_not_mistaken_for_a_pseudo_version() {
        assert!(is_release_version("v2026.08.27-1"));
        assert!(is_release_version("v1.0.1-0.2026082705995-9d76f9ff8e0a"));
    }
}
