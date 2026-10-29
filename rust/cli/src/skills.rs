// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The skill documents `init` installs into `~/.claude/skills`.
//!
//! They ship **inside the binary**. A skill here is not general advice — it names
//! this binary's own subcommands (`wt new`, `herdr-tower status`, `board send`)
//! and their exact flags, so a copy distributed through any other channel
//! drifts out of step with the binary that has to answer those calls.
//! Embedding them and having `init` write them locks the two together: the skill a
//! user has is the skill the installed binary was built with.
//!
//! There is one: `orchestration`, for a coordinator putting N workers in
//! worktrees, dispatching, waiting, collecting.

use std::io::{self, Write};
use std::path::Path;

/// One embedded skill: the folder name it installs under, and its body.
pub struct Skill {
    pub name: &'static str,
    pub body: &'static str,
}

/// What [`install_skills`] writes. The folder name is also the slash command, so
/// renaming one changes how users invoke it.
pub const SKILLS: [Skill; 1] = [Skill {
    name: "orchestration",
    body: include_str!("skills/orchestration.md"),
}];

/// Installs every embedded skill under `skills_dir` (normally `~/.claude/skills`).
///
/// Stops at the first failure rather than carrying on, so a half-installed set is
/// reported instead of being hidden behind later successes.
pub fn install_skills(w: &mut impl Write, skills_dir: &Path, dry_run: bool) -> io::Result<()> {
    for s in &SKILLS {
        install_skill(w, skills_dir, s.name, s.body, dry_run)?;
    }
    Ok(())
}

/// Writes `<skills_dir>/<name>/SKILL.md`.
///
/// Identical content is left alone, so a reinstall is silent. Different content
/// is *replaced*, but only after the previous file is copied to `.bak` — the
/// upgrade path has to assume the user edited theirs, because some of them do,
/// and losing that edit to a version bump would be the tool's fault.
///
/// A file that cannot be read for any reason other than being absent is treated
/// as absent, and is overwritten with no backup. That is the wrong shape, but
/// making it an error would mean `init` refusing to finish over a skill file
/// somebody chmod'd — so it stays, named here so it is not a surprise.
fn install_skill(
    w: &mut impl Write,
    skills_dir: &Path,
    name: &str,
    body: &str,
    dry_run: bool,
) -> io::Result<()> {
    let path = skills_dir.join(name).join("SKILL.md");
    let shown = path.display();
    let existing = std::fs::read(&path).ok();
    if existing.as_deref() == Some(body.as_bytes()) {
        return writeln!(w, "  {shown}: already up to date — skipped");
    }
    if dry_run {
        return writeln!(
            w,
            "  {shown}: would be installed (dry-run — no file was changed)"
        );
    }
    crate::initcmd::settingsfile::create_parent(&path)?;
    if let Some(existing) = &existing {
        let bak = crate::initcmd::settingsfile::with_suffix(&path, ".bak");
        crate::initcmd::settingsfile::write_mode(&bak, existing, 0o644).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("the backup failed, so nothing was installed: {e}"),
            )
        })?;
        writeln!(w, "  the existing file was backed up to {shown}.bak")?;
    }
    crate::initcmd::settingsfile::write_atomic(&path, ".tmp", body.as_bytes(), 0o644)?;
    // "으로" where "로" is correct after a vowel — kept because it is what
    // shipped, and the string is user-visible output rather than a format.
    writeln!(w, "  {shown}: installed (call it with /{name})")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real target is `~/.claude/skills`, and this function writes files
    /// there. Every test hands it a `tempfile` root instead; nothing in this
    /// module reads `$HOME`.
    fn install(dir: &Path, dry_run: bool) -> String {
        let mut out = Vec::new();
        install_skills(&mut out, dir, dry_run).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_fresh_directory_gets_every_skill_under_its_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        let out = install(dir.path(), false);
        for s in &SKILLS {
            let name = s.name;
            let body = std::fs::read_to_string(dir.path().join(name).join("SKILL.md")).unwrap();
            assert_eq!(body, s.body, "{name}");
            assert!(
                body.contains(&format!("name: {name}")),
                "{name} front matter"
            );
            assert!(
                out.contains(&format!(
                    "{}: installed (call it with /{name})",
                    Path::new(name).join("SKILL.md").display()
                )),
                "{out}"
            );
        }
    }

    #[test]
    fn reinstalling_the_same_content_writes_nothing_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), false);
        let out = install(dir.path(), false);
        assert_eq!(
            out.matches("already up to date — skipped").count(),
            SKILLS.len()
        );
        for s in &SKILLS {
            assert!(!dir.path().join(s.name).join("SKILL.md.bak").exists());
        }
    }

    /// Some users edit their copy. An upgrade must not be how they find out that
    /// the edit is gone.
    #[test]
    fn an_edited_skill_is_copied_aside_before_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchestration").join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "an older version").unwrap();

        let out = install(dir.path(), false);
        assert_eq!(
            std::fs::read_to_string(path.with_file_name("SKILL.md.bak")).unwrap(),
            "an older version"
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("name: orchestration"));
        assert!(out.contains("backed up to"), "{out}");
    }

    #[test]
    fn a_dry_run_creates_nothing_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let out = install(dir.path(), true);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert_eq!(
            out.matches("would be installed (dry-run — no file was changed)")
                .count(),
            SKILLS.len()
        );
    }

    /// A skill folder name occupied by a *file* cannot be created, and the
    /// install stops there rather than reporting success for the rest.
    #[test]
    fn a_directory_that_cannot_be_created_stops_the_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("orchestration"), "x").unwrap();
        let mut out = Vec::new();
        assert!(install_skills(&mut out, dir.path(), false).is_err());
    }

    fn skill(name: &str) -> &'static str {
        SKILLS.iter().find(|s| s.name == name).unwrap().body
    }

    #[test]
    fn coordination_detail_lives_only_in_the_orchestration_skill() {
        let orch = skill("orchestration");
        for want in [
            "wt new",
            // `herdr pane run` sends text and Enter in one request. Pinning
            // it keeps a two-step send with a sleep between the halves from
            // creeping back into the document.
            "herdr pane run",
            "herdr-tower status",
            "wt merge <name> --yes",
            "wt clean",
        ] {
            assert!(orch.contains(want), "orchestration lost {want:?}");
        }
    }

    /// `init` decides "already up to date" by comparing these bytes against
    /// what is on disk, so any edit here rewrites every user's skill file on
    /// their next `init` — the byte counts are here to make that an explicit
    /// decision rather than a side effect of reflowing a paragraph.
    ///
    /// The count moved with each rename of the tool: the document names the
    /// binary the coordinator runs, so it had to. Again when the rename reached
    /// the *prose*. And again when the
    /// body was translated to English — everything below the front matter. It
    /// moved once more when its `status --json` path was corrected to
    /// `.pane.pane_id` and the notes about a removed backend were dropped.
    /// It moved again when dispatch went through the board
    /// (`board send`), replacing the `herdr pane run` it used to teach.
    /// And again when a worker could be a pane (`board assign`, `send <pane>`).
    /// And again for `board next` and the ready-for-review notices.
    /// And again for `--message`/`--file`, priorities and PR links.
    /// And again for `board wake on`.
    /// And again when the board column read `✓ review`, not `DONE  review`.
    /// And again when the tower's states went lowercase (`[active]`, `[wait]`).
    /// And again for `tree link`.
    /// And again when the dev-server notes moved into gathering (§4).
    ///
    /// The front matter was left in Korean on purpose, and it is the one part
    /// that could not follow: `description` carries the phrases a person *says*
    /// to summon the skill, and an agent matches a request against them.
    /// Translating those would mean the skill stops answering what was asked.
    #[test]
    fn the_embedded_body_is_the_shipped_document_unchanged() {
        assert_eq!(skill("orchestration").len(), 12098);
        for s in &SKILLS {
            assert!(s.body.starts_with("---\nname: "), "{} front matter", s.name);
            assert!(s.body.ends_with('\n'), "{} trailing newline", s.name);
        }
    }
}
