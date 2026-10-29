// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Claude Code's session transcripts.
//!
//! Every claude session appends to `~/.claude/projects/<folder>/<session>.jsonl`,
//! and each assistant line in it carries the model that answered and the
//! request's token usage. What the transcript does not record is the size of
//! the window, so the share reported here is an estimate — see
//! [`CLAUDE_WINDOW`] and [`CLAUDE_WINDOW_1M`].

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::claudecfg::pretty_model;
use crate::ctx::{CtxInfo, UsedPct};
use crate::fileread::try_read_tail;

/// The window assumed for a claude session: 200K tokens.
pub const CLAUDE_WINDOW: f64 = 200_000.0;

/// The window of a session on a `[1m]` model: 1M tokens.
///
/// The transcript names the model without the `[1m]` suffix, so a session is
/// taken to be on it when the pinned default says so for the same family, or
/// when its context has already outgrown [`CLAUDE_WINDOW`] — which only a
/// 1M session can do.
pub const CLAUDE_WINDOW_1M: f64 = 1_000_000.0;

/// How much of a transcript's end is read first for the latest assistant turn.
const TAIL_BYTES: u64 = 256 * 1024;

/// The second, wider read, for a transcript whose last few lines are one huge
/// tool result.
const WIDE_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Where Claude Code keeps its settings and transcripts (`~/.claude`).
pub fn claude_dir(home: &Path) -> PathBuf {
    home.join(".claude")
}

/// The folder name Claude Code files a working directory's transcripts under:
/// every character that is not an ASCII letter or digit becomes `-`.
fn project_folder(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The transcript of `session_id`.
///
/// The folder derived from `cwd` first; failing that, every project folder.
/// The fallback is what finds a session whose agent has since `cd`ed
/// elsewhere — the transcript stays filed under the folder it started in — and
/// one whose path was too long for Claude Code to file under its plain name.
fn transcript(claude_dir: &Path, session_id: &str, cwd: &str) -> Option<PathBuf> {
    if let Some(direct) = direct_transcript(claude_dir, session_id, cwd) {
        return Some(direct);
    }
    let projects = claude_dir.join("projects");
    let name = format!("{session_id}.jsonl");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

/// The transcript in the folder `cwd` files under, if it is there.
fn direct_transcript(claude_dir: &Path, session_id: &str, cwd: &str) -> Option<PathBuf> {
    let direct = claude_dir
        .join("projects")
        .join(project_folder(cwd))
        .join(format!("{session_id}.jsonl"));
    direct.is_file().then_some(direct)
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(rename = "isSidechain", default)]
    sidechain: bool,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// The model and the prompt size of the last main-chain assistant turn in
/// `tail`.
///
/// The prompt — fresh input plus both kinds of cached input — is what occupied
/// the window on that request; output tokens only join it on the next one.
/// Sidechain turns are a subagent's own window, and `<synthetic>` turns are
/// Claude Code's own notes with no request behind them, so neither says
/// anything about this session's context.
fn last_turn(tail: &str) -> Option<(String, u64)> {
    for raw in tail.lines().rev() {
        if !raw.contains("\"assistant\"") {
            continue;
        }
        // The first line of a tail is usually cut; it fails to parse and is
        // passed over like any other line that is not a turn.
        let Ok(line) = serde_json::from_str::<Line>(raw) else {
            continue;
        };
        if line.kind != "assistant" || line.sidechain {
            continue;
        }
        let Some(msg) = line.message else { continue };
        if msg.model.is_empty() || msg.model.starts_with('<') {
            continue;
        }
        let Some(u) = msg.usage else { continue };
        let prompt = u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
        return Some((msg.model, prompt));
    }
    None
}

/// Whether a pinned default such as `opus[1m]` or `claude-opus-5-5[1m]` puts
/// `model` on the 1M window.
///
/// Only for the same family: a default of `opus[1m]` says nothing about a
/// session someone switched to sonnet.
fn pinned_1m(default_model: &str, model: &str) -> bool {
    let Some(pinned) = default_model.strip_suffix("[1m]") else {
        return false;
    };
    let family = pinned
        .strip_prefix("claude-")
        .unwrap_or(pinned)
        .split('-')
        .next()
        .unwrap_or_default();
    !family.is_empty() && model.contains(family)
}

/// What was learned about claude transcripts, kept across passes by
/// [`crate::CtxCache`].
///
/// Two things are remembered. Which file a session lives in: the fallback that
/// finds it walks every project folder, and did that every pass for an agent
/// that had `cd`ed away. And each transcript's last turn against its length
/// and modification time: an idle session's file does not move, and was read
/// — up to 4 MiB of it — on every pass all the same.
#[derive(Debug, Default)]
pub(crate) struct ClaudeCache {
    paths: HashMap<String, (PathBuf, u64)>,
    turns: HashMap<PathBuf, Turn>,
    /// Sessions whose transcript the full walk did not find: the pass it
    /// last walked for each, and the last pass that asked.
    misses: HashMap<String, (u64, u64)>,
}

/// How many passes a session whose transcript was not found waits before the
/// full walk of every project folder is done for it again.
///
/// A new session has no transcript until its first turn, and a session id
/// from a record can name one that is gone; either walked all of
/// `~/.claude/projects` — a hundred folders and more — on every pass. The
/// folder its working directory files under is still checked every pass, and
/// that is where a new session's transcript appears.
const WALK_AGAIN_AFTER: u64 = 8;

#[derive(Debug, Clone)]
struct Turn {
    len: u64,
    mtime: Option<SystemTime>,
    last: Option<(String, u64)>,
    pass: u64,
}

impl ClaudeCache {
    /// Forgets what the pass did not use.
    pub(crate) fn end_pass(&mut self, pass: u64) {
        self.paths.retain(|_, (_, p)| *p == pass);
        self.turns.retain(|_, t| t.pass == pass);
        self.misses.retain(|_, (_, asked)| *asked == pass);
    }

    fn transcript(
        &mut self,
        claude_dir: &Path,
        session_id: &str,
        cwd: &str,
        pass: u64,
    ) -> Option<PathBuf> {
        if let Some((path, seen)) = self.paths.get_mut(session_id) {
            if path.is_file() {
                *seen = pass;
                return Some(path.clone());
            }
        }
        let recent_miss = self.misses.get_mut(session_id).and_then(|(walked, asked)| {
            *asked = pass;
            (pass.wrapping_sub(*walked) < WALK_AGAIN_AFTER).then_some(())
        });
        let found = if recent_miss.is_some() {
            direct_transcript(claude_dir, session_id, cwd)
        } else {
            transcript(claude_dir, session_id, cwd)
        };
        let Some(path) = found else {
            if recent_miss.is_none() {
                self.misses.insert(session_id.to_string(), (pass, pass));
            }
            return None;
        };
        self.misses.remove(session_id);
        self.paths
            .insert(session_id.to_string(), (path.clone(), pass));
        Some(path)
    }

    fn last_turn(&mut self, path: &Path, pass: u64) -> Option<(String, u64, SystemTime)> {
        let meta = std::fs::metadata(path).ok()?;
        let (len, mtime) = (meta.len(), meta.modified().ok());
        if let Some(t) = self.turns.get_mut(path) {
            if t.len == len && t.mtime == mtime {
                t.pass = pass;
                let (model, prompt) = t.last.clone()?;
                return Some((model, prompt, mtime?));
            }
        }
        // A read that failed is answered for but not remembered, as with
        // codex: the file has not changed, so nothing else would ask again.
        let Ok(last) = read_last_turn(path, len) else {
            return None;
        };
        self.turns.insert(
            path.to_path_buf(),
            Turn {
                len,
                mtime,
                last: last.clone(),
                pass,
            },
        );
        let (model, prompt) = last?;
        Some((model, prompt, mtime?))
    }
}

/// The last main-chain turn of the transcript at `path`, `len` bytes long.
/// `Err` when the file could not be read, so no answer is worth keeping.
fn read_last_turn(path: &Path, len: u64) -> std::io::Result<Option<(String, u64)>> {
    let mut f = File::open(path)?;
    let turn = last_turn(&try_read_tail(&mut f, len, TAIL_BYTES)?);
    if turn.is_none() && len > TAIL_BYTES {
        return Ok(last_turn(&try_read_tail(&mut f, len, WIDE_TAIL_BYTES)?));
    }
    Ok(turn)
}

/// The model and context share of one claude session.
///
/// `default_model` is the pinned default ([`crate::claude_default_model`]),
/// consulted only to decide the window. The model is reported as its display
/// name ([`crate::pretty_model`]) — `Opus 5.5`, or `Opus 5.5 (1M)` on the 1M
/// window — since every renderer prints it as it comes.
pub fn claude_for_session(
    claude_dir: &Path,
    session_id: &str,
    cwd: &str,
    default_model: &str,
) -> CtxInfo {
    claude_for_session_in(
        claude_dir,
        session_id,
        cwd,
        default_model,
        &mut crate::CtxCache::default(),
    )
}

/// [`claude_for_session`] through a cache kept across passes.
pub fn claude_for_session_in(
    claude_dir: &Path,
    session_id: &str,
    cwd: &str,
    default_model: &str,
    cache: &mut crate::CtxCache,
) -> CtxInfo {
    if session_id.is_empty() {
        return CtxInfo::default();
    }
    let pass = cache.pass();
    let Some(path) = cache.claude.transcript(claude_dir, session_id, cwd, pass) else {
        return CtxInfo::default();
    };
    let Some((model, prompt, mtime)) = cache.claude.last_turn(&path, pass) else {
        return CtxInfo::default();
    };

    let one_m = prompt as f64 > CLAUDE_WINDOW || pinned_1m(default_model, &model);
    let window = if one_m {
        CLAUDE_WINDOW_1M
    } else {
        CLAUDE_WINDOW
    };
    CtxInfo {
        model: pretty_model(&if one_m { format!("{model}[1m]") } else { model }),
        used_pct: (prompt > 0).then(|| UsedPct::approx(prompt as f64 / window * 100.0)),
        ts: Some(OffsetDateTime::from(mtime)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folder_dashes_everything_but_letters_and_digits() {
        assert_eq!(
            project_folder("/Users/q/dbstore_go_v2/.claude/wt"),
            "-Users-q-dbstore-go-v2--claude-wt"
        );
    }

    /// A transcript not found is walked for again only every
    /// [`WALK_AGAIN_AFTER`] passes — but the folder its working directory files
    /// under is checked on every one, which is where a new session's first turn
    /// lands.
    #[test]
    fn a_missing_transcript_is_walked_for_again_only_now_and_then() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path();
        let cwd = "/w/app";
        let own = claude.join("projects").join(project_folder(cwd));
        let elsewhere = claude.join("projects").join("-w-started-here");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let mut cache = ClaudeCache::default();

        // Pass 1 walks and misses.
        assert_eq!(cache.transcript(claude, "s1", cwd, 1), None);
        // Filed under another folder: only a walk finds it, and the next walk
        // is WALK_AGAIN_AFTER passes on.
        std::fs::write(elsewhere.join("s1.jsonl"), "{}\n").unwrap();
        for pass in 2..(1 + WALK_AGAIN_AFTER) {
            assert_eq!(
                cache.transcript(claude, "s1", cwd, pass),
                None,
                "pass {pass}"
            );
            cache.end_pass(pass);
        }
        assert_eq!(
            cache.transcript(claude, "s1", cwd, 1 + WALK_AGAIN_AFTER),
            Some(elsewhere.join("s1.jsonl"))
        );

        // A new session's transcript in its own folder is found at once.
        assert_eq!(cache.transcript(claude, "s2", cwd, 20), None);
        std::fs::write(own.join("s2.jsonl"), "{}\n").unwrap();
        assert_eq!(
            cache.transcript(claude, "s2", cwd, 21),
            Some(own.join("s2.jsonl"))
        );
    }

    /// A miss a pass no longer asks about is forgotten with the pass, like
    /// every other entry.
    #[test]
    fn a_miss_is_forgotten_when_no_pass_asks() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ClaudeCache::default();
        assert_eq!(cache.transcript(dir.path(), "gone", "/w", 1), None);
        assert_eq!(cache.misses.len(), 1);
        cache.end_pass(2);
        assert!(cache.misses.is_empty());
    }

    /// A pinned `[1m]` speaks for its own family only.
    #[test]
    fn a_pinned_1m_default_covers_its_family_only() {
        assert!(pinned_1m("opus[1m]", "claude-opus-5-5"));
        assert!(pinned_1m("claude-opus-5-5[1m]", "claude-opus-5-5"));
        assert!(!pinned_1m("opus[1m]", "claude-sonnet-5"));
        assert!(!pinned_1m("opus", "claude-opus-5-5"));
        assert!(!pinned_1m("[1m]", "claude-opus-5-5"));
    }

    /// The last main-chain turn wins; subagent turns, synthetic notes and a
    /// cut first line are passed over.
    #[test]
    fn last_turn_skips_what_is_not_this_sessions_request() {
        let tail = concat!(
            "ge\":{\"input_tokens\":9}}}\n",
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":50}}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"assistant"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"model":"claude-haiku-4-5","usage":{"input_tokens":5000}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0}}}"#,
            "\n",
        );
        assert_eq!(last_turn(tail), Some(("claude-opus-5-5".into(), 1002)));
    }

    /// One main-chain turn, as a transcript line.
    const TURN: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":50}}}"#;

    /// A read that fails is not remembered: the file does not change, so a
    /// cached "no turn" would stand until the session's next turn. Once the
    /// file can be read again, the same unchanged file gives its turn.
    #[cfg(unix)]
    #[test]
    fn a_failed_read_is_not_cached_as_no_turn() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        std::fs::write(&p, format!("{TURN}\n")).unwrap();
        let mut cache = ClaudeCache::default();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&p).is_ok() {
            return; // running as root: nothing here can fail to read
        }
        assert_eq!(cache.last_turn(&p, 1), None);
        assert!(cache.turns.is_empty(), "the failure was remembered");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (model, prompt, _) = cache.last_turn(&p, 2).expect("read once it can be");
        assert_eq!((model.as_str(), prompt), ("claude-opus-5-5", 1002));
    }

    /// A read that succeeds is kept, and answers again without the file:
    /// the cache and a fresh read agree for an unchanged file.
    #[test]
    fn a_good_read_is_cached_and_agrees_with_a_fresh_one() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        std::fs::write(&p, format!("{TURN}\n")).unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        let fresh = read_last_turn(&p, len).unwrap();
        let mut cache = ClaudeCache::default();
        let first = cache.last_turn(&p, 1).map(|(m, n, _)| (m, n));
        assert_eq!(first, fresh);
        assert_eq!(cache.turns.len(), 1);
        let second = cache.last_turn(&p, 2).map(|(m, n, _)| (m, n));
        assert_eq!(second, first);
        // A file with no turn in it is an answer too, and is kept.
        let empty = d.path().join("e.jsonl");
        std::fs::write(&empty, "{}\n").unwrap();
        assert_eq!(cache.last_turn(&empty, 3), None);
        assert_eq!(cache.turns.len(), 2);
    }

    /// A missing file is no turn, and nothing is kept for it.
    #[test]
    fn a_missing_transcript_is_no_turn() {
        let d = tempfile::tempdir().unwrap();
        let mut cache = ClaudeCache::default();
        assert_eq!(cache.last_turn(&d.path().join("gone.jsonl"), 1), None);
        assert!(cache.turns.is_empty());
    }
}
