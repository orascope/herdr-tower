// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! codex rollouts: which session last worked in a folder, and how full it is.
//!
//! codex writes one JSON Lines rollout per session under
//! `<root>/YYYY/MM/DD/<id>.jsonl`. Nothing indexes them by folder, so finding
//! "the session for this directory" means walking the files newest-first and
//! reading each header until one claims the folder.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::ctx::{CtxInfo, UsedPct};
use crate::fileread::{last_field_between, read_head, try_read_tail, SCAN_OVERLAP};
use crate::jsonscan::{first_string_field, last_string_field};

/// Tokens excluded from the context percentage before it is scaled.
///
/// codex's own TUI subtracts the same fixed overhead from both the usage and
/// the window, so a fresh session reads as 0% rather than as "already 4% in".
/// Matching the number is the point — a tower that disagrees with the pane it
/// is watching is worse than one that says nothing.
pub const CODEX_BASELINE_TOKENS: f64 = 12_000.0;

/// How far back to look. Beyond a few hundred rollouts the file is older than
/// anything the tower cares about, and every extra candidate is another header
/// read on a two-second poll.
const MAX_ROLLOUTS: usize = 400;

/// Enough to cover the `session_meta` line that opens every rollout.
const HEAD_BYTES: u64 = 4096;

/// Enough to cover the last few turns.
const TAIL_BYTES: u64 = 128 * 1024;

/// Rollout paths, newest first, capped at [`MAX_ROLLOUTS`].
///
/// Each file is stat'd once and then sorted, rather than stat'd from inside
/// the comparator: that would be O(n log n) syscalls, and a file deleted
/// mid-sort would make the comparator answer "not less" in both directions,
/// which is not a valid ordering.
///
/// Ties break on the path *descending*, which is the same direction as the
/// mtime and not merely a reproducible one. codex writes rollouts under
/// `YYYY/MM/DD/` with a timestamp opening each filename, so the later path is
/// the later session — and ties are not rare. A filesystem with one-second
/// timestamps (ext4, and the overlay a container gets) gives two rollouts
/// written in the same second the same mtime, and ascending order then answered
/// with the *older* one: the folder walk handed every row that session's model
/// and context share. Seen on Linux, invisible on APFS, which stamps in
/// nanoseconds.
pub(crate) fn rollouts_by_recency(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    for year in child_dirs(root) {
        for month in child_dirs(&year) {
            for day in child_dirs(&month) {
                let Ok(entries) = std::fs::read_dir(&day) else {
                    continue;
                };
                for e in entries.flatten() {
                    if !e.file_name().to_string_lossy().ends_with(".jsonl") {
                        continue;
                    }
                    let Ok(meta) = e.metadata() else { continue };
                    let Ok(mtime) = meta.modified() else { continue };
                    found.push((mtime, e.path()));
                }
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    found.truncate(MAX_ROLLOUTS);
    found.into_iter().map(|(_, p)| p).collect()
}

fn child_dirs(p: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(p) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// How a rollout header names the folder it was started in.
///
/// Serialized, not interpolated. A path is allowed to contain a quote or a
/// backslash, and the rollout stores it JSON-escaped — so a raw needle could
/// never match one, and both context reporting and directory-based resume failed
/// silently for a repository whose path had either.
fn cwd_needle(workdir: &str) -> String {
    let quoted = serde_json::to_string(workdir).unwrap_or_else(|_| format!("\"{workdir}\""));
    format!("\"cwd\":{quoted}")
}

/// How a rollout header names the session it belongs to.
///
/// Serialized for the same reason as [`cwd_needle`]: the value is compared against
/// the file's own JSON, so it has to be spelled the way JSON spells it.
fn id_needle(session_id: &str) -> String {
    let quoted = serde_json::to_string(session_id).unwrap_or_else(|_| format!("\"{session_id}\""));
    format!("\"session_id\":{quoted}")
}

/// The `session_id` of the most recent codex rollout started in `workdir`.
///
/// codex's `notify` hook does not carry a session ID, so this file walk is the
/// only way to learn the one `resume` needs.
pub fn codex_session_id(root: &Path, workdir: &str) -> Option<String> {
    let needle = cwd_needle(workdir);
    for path in rollouts_by_recency(root) {
        let Some(head) = read_head(&path, HEAD_BYTES) else {
            continue;
        };
        if !head.contains(&needle) {
            continue;
        }
        if let Some(id) = first_string_field(&head, "session_id") {
            return Some(id.to_string());
        }
    }
    None
}

/// The model and context share of the most recently active codex session in
/// `workdir`, or an empty [`CtxInfo`] if there is none.
///
/// The first header that claims the folder wins and the walk stops there —
/// files are already ordered newest-first, so a later match is by definition an
/// older session.
pub fn codex_latest(root: &Path, workdir: &str) -> CtxInfo {
    matching_rollout(
        &Rollouts::scan(root),
        &cwd_needle(workdir),
        &mut CtxCache::default(),
    )
}

/// The same, for a session whose ID the record already knows.
///
/// The ID first, the folder second. Two codex sessions in one checkout share the
/// folder, so the folder walk answers with the newest of them for *every* row —
/// and each older agent then displayed the newest session's model and context
/// share as its own. Both the codex hooks and the backend fill `session_id` in,
/// so it is usually there; the folder remains the fallback for the notify-only
/// path, which carries no ID at all, and for a rollout that spells the field
/// differently than expected.
///
/// One-off: it walks the directory and reads the file afresh. A caller asking
/// on a clock uses [`codex_for_session_in`] with a [`Rollouts`] and a
/// [`CtxCache`] it keeps.
pub fn codex_for_session(root: &Path, session_id: &str, workdir: &str) -> CtxInfo {
    codex_for_session_in(
        &Rollouts::scan(root),
        session_id,
        workdir,
        &mut CtxCache::default(),
    )
}

/// [`codex_for_session`] against a walk already made and a cache kept across
/// calls.
///
/// The session's own rollout is found by its file name first: codex names each
/// one `rollout-<time>-<session id>.jsonl`, so that is a string comparison per
/// file instead of opening every header newest-first until one claims the ID.
/// The header scan stays behind it for a file named some other way.
pub fn codex_for_session_in(
    rollouts: &Rollouts,
    session_id: &str,
    workdir: &str,
    cache: &mut CtxCache,
) -> CtxInfo {
    if !session_id.is_empty() {
        let own = match rollouts.named(session_id) {
            Some(path) => File::open(path)
                .map(|mut f| read_ctx(path, &mut f, cache))
                .unwrap_or_default(),
            None => matching_rollout(rollouts, &id_needle(session_id), cache),
        };
        if !own.is_empty() {
            return own;
        }
    }
    matching_rollout(rollouts, &cwd_needle(workdir), cache)
}

/// The rollouts under a codex root, newest first: one walk, shared by every
/// lookup in a pass rather than repeated per agent.
#[derive(Debug, Clone, Default)]
pub struct Rollouts(Vec<PathBuf>);

impl Rollouts {
    /// Walks `root`. See [`rollouts_by_recency`] for the order.
    pub fn scan(root: &Path) -> Self {
        Rollouts(rollouts_by_recency(root))
    }

    /// The rollout codex named after `session_id`, if there is one.
    fn named(&self, session_id: &str) -> Option<&PathBuf> {
        self.0.iter().find(|path| {
            path.file_name()
                .is_some_and(|name| named_session(&name.to_string_lossy()) == Some(session_id))
        })
    }
}

/// The session a rollout's file name says it holds: codex names each one
/// `rollout-<YYYY-MM-DDTHH-MM-SS>-<session id>.jsonl`.
///
/// The whole name is parsed, not its end matched: a session ID recorded short,
/// or one that happens to end another's, must not claim that other session's
/// file. A name of any other shape says nothing, and the header decides.
fn named_session(name: &str) -> Option<&str> {
    const STAMP: usize = "YYYY-MM-DDTHH-MM-SS".len();
    let rest = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let (stamp, id) = (rest.get(..STAMP)?, rest.get(STAMP..)?.strip_prefix('-')?);
    let digits = stamp.bytes().filter(u8::is_ascii_digit).count();
    (digits == 14 && !id.is_empty()).then_some(id)
}

/// What each session file said the last time it was read, kept across passes:
/// codex rollouts here, claude transcripts in [`crate::claude`].
///
/// A rollout that has not changed since is not read again; one that has grown
/// is read only as far as it needs to be — see [`read_ctx`]. Without this the
/// tower read every codex rollout's tail on every pass, and read the whole of
/// any whose model had scrolled out of that tail: 44 MB into memory, twice
/// over, every fifteen seconds, for a session that had long since ended.
///
/// Bounded by what is on screen: a caller that runs passes brackets each with
/// [`CtxCache::begin_pass`] and [`CtxCache::end_pass`], and a rollout the
/// pass did not look at is forgotten.
#[derive(Debug, Default)]
pub struct CtxCache {
    seen: HashMap<PathBuf, Seen>,
    /// Each rollout's opening bytes, which a session writes once and never
    /// again: the header scan that finds an agent's rollout by its folder reads
    /// every file newer than the match, and it read them all again every pass.
    heads: HashMap<PathBuf, Head>,
    /// Which pass is running; each entry records the last one that used it.
    pass: u64,
    pub(crate) claude: crate::claude::ClaudeCache,
}

#[derive(Debug, Clone)]
struct Head {
    text: String,
    pass: u64,
    /// What the file was when these bytes were read. A head read to the full
    /// budget needs neither — it is what an uncached read would see, for good —
    /// but a shorter one is only still true while the file has not moved.
    len: u64,
    mtime: Option<SystemTime>,
    full: bool,
}

impl CtxCache {
    /// Starts a pass over the agents. What the pass does not look at is
    /// dropped by [`CtxCache::end_pass`].
    pub fn begin_pass(&mut self) {
        self.pass = self.pass.wrapping_add(1);
    }

    /// Forgets every rollout this pass did not look at — an ended session, a
    /// deleted file, a folder whose newest rollout has changed — so the cache
    /// holds what is on screen now, not everything the tower ever looked up.
    pub fn end_pass(&mut self) {
        let pass = self.pass;
        self.seen.retain(|_, seen| seen.pass == pass);
        self.heads.retain(|_, head| head.pass == pass);
        self.claude.end_pass(pass);
    }

    /// Which pass is running, for the per-source caches that share it.
    pub(crate) fn pass(&self) -> u64 {
        self.pass
    }

    /// Whether a rollout's opening bytes contain `needle`, reading them once.
    ///
    /// A head read to the full budget is kept for good: it is exactly what an
    /// uncached read would see, whatever the file does afterwards.
    ///
    /// 🚨 A *shorter* head is kept too, against the file's length and
    /// modification time. Only keeping full ones was the same bug the ctx read
    /// already had fixed for itself: a rollout smaller than the budget is most
    /// of them — a codex session of a few messages never reaches 4 KiB — so the
    /// folder walk, which opens every file newer than its match, re-opened and
    /// re-read those on every pass. A file that has not changed length and has
    /// the same mtime holds the same opening bytes, and one `metadata` is
    /// cheaper than an `open` and a 4 KiB read.
    ///
    /// A short head whose file *has* moved is read again, because its next lines
    /// may be the ones that name the session or the folder.
    fn head_contains(&mut self, path: &Path, needle: &str) -> bool {
        use std::io::Read;
        let pass = self.pass;
        let now = std::fs::metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        if let Some(head) = self.heads.get_mut(path) {
            let still =
                head.full || now.is_some_and(|(len, mtime)| head.len == len && head.mtime == mtime);
            if still {
                head.pass = pass;
                return head.text.contains(needle);
            }
        }
        let mut bytes = Vec::new();
        let Ok(file) = File::open(path) else {
            return false;
        };
        if file.take(HEAD_BYTES).read_to_end(&mut bytes).is_err() {
            return false;
        }
        let full = bytes.len() as u64 == HEAD_BYTES;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let hit = text.contains(needle);
        let (len, mtime) = now.unwrap_or((bytes.len() as u64, None));
        self.heads.insert(
            path.to_path_buf(),
            Head {
                text,
                pass,
                len,
                mtime,
                full,
            },
        );
        hit
    }

    /// How many rollouts are remembered.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[derive(Debug, Clone)]
struct Seen {
    len: u64,
    mtime: Option<SystemTime>,
    info: CtxInfo,
    pass: u64,
}

/// The newest rollout whose header contains `needle`, read for model and context.
fn matching_rollout(rollouts: &Rollouts, needle: &str, cache: &mut CtxCache) -> CtxInfo {
    for path in &rollouts.0 {
        if !cache.head_contains(path, needle) {
            continue;
        }
        let Ok(mut f) = File::open(path) else {
            continue;
        };
        return read_ctx(path, &mut f, cache);
    }
    CtxInfo::default()
}

/// A rollout's model and context share, reading no more of it than it takes.
///
/// Unchanged since the last read — same length, same mtime — it is not read at
/// all. Otherwise the tail is: it carries the latest `token_count`, and usually
/// the latest model too. When the model is not in it, it has scrolled out, and
/// what lies before the tail is searched backwards from the tail — only as far
/// back as no earlier read covered, when the file has merely grown since. codex
/// only ever appends, so the last model before that point is the one
/// remembered.
///
/// A read that fails is answered for but not remembered: the next pass tries
/// the file again rather than showing the failure until the file next changes.
fn read_ctx(path: &Path, f: &mut File, cache: &mut CtxCache) -> CtxInfo {
    let (len, mtime) = match f.metadata() {
        Ok(m) => (m.len(), m.modified().ok()),
        Err(_) => return CtxInfo::default(),
    };
    let pass = cache.pass;
    let before = cache.seen.get_mut(path);
    if let Some(seen) = before.filter(|s| s.len == len && s.mtime == mtime) {
        seen.pass = pass;
        return seen.info.clone();
    }
    // Grown, not rewritten: a file that is no longer than it was is a
    // different file as far as anything remembered about it goes.
    let before = cache.seen.get(path).filter(|s| s.len < len);
    let ts = mtime.map(OffsetDateTime::from);

    let Ok(tail) = try_read_tail(f, len, TAIL_BYTES) else {
        return CtxInfo {
            ts,
            ..CtxInfo::default()
        };
    };
    let tail_start = len.saturating_sub(TAIL_BYTES);
    let mut read_failed = false;
    let model = match last_string_field(&tail, "model") {
        Some(m) => m.to_string(),
        None => {
            // Back up a little into what was read before, so a key the last
            // read caught half-written is seen whole this time — and reach a
            // little into the tail, so a key the tail begins in the middle of
            // is too. The tail had no whole one, so anything found there is
            // exactly such a key.
            let from = before.map_or(0, |s| s.len.saturating_sub(SCAN_OVERLAP));
            let to = (tail_start + SCAN_OVERLAP).min(len);
            let found = if from < tail_start {
                last_field_between(f, from, to, "model").unwrap_or_else(|_| {
                    read_failed = true;
                    None
                })
            } else {
                None
            };
            found
                .or_else(|| before.map(|s| s.info.model.clone()))
                .unwrap_or_default()
        }
    };

    let used_pct = tail
        .lines()
        .rev()
        .filter(|l| l.contains("\"token_count\""))
        .find_map(parse_token_count)
        .map(UsedPct::Exact);

    let info = CtxInfo {
        model,
        used_pct,
        ts,
    };
    if !read_failed {
        cache.seen.insert(
            path.to_path_buf(),
            Seen {
                len,
                mtime,
                info: info.clone(),
                pass,
            },
        );
    }
    info
}

#[derive(Deserialize, Default)]
struct TokenCountLine {
    #[serde(default)]
    payload: TokenPayload,
}

#[derive(Deserialize, Default)]
struct TokenPayload {
    #[serde(default)]
    info: TokenInfo,
}

#[derive(Deserialize, Default)]
struct TokenInfo {
    #[serde(default)]
    last_token_usage: TokenUsage,
    #[serde(default)]
    model_context_window: Option<f64>,
}

#[derive(Deserialize, Default)]
struct TokenUsage {
    #[serde(default)]
    total_tokens: Option<f64>,
}

/// The context share from one `token_count` event line.
///
/// `last_token_usage` — the size of the most recent request — is the right
/// figure, not the session total: it *is* the current context, since every turn
/// resends it. A window at or below the baseline means codex has not reported a
/// real one yet, and dividing by it would produce a percentage out of nothing.
///
/// The line is scanned from its first `{` because rollout lines occasionally
/// carry a prefix before the JSON object.
fn parse_token_count(line: &str) -> Option<f64> {
    let start = line.find('{')?;
    let rec: TokenCountLine = serde_json::from_str(&line[start..]).ok()?;
    let window = rec.payload.info.model_context_window.unwrap_or(0.0);
    if window <= CODEX_BASELINE_TOKENS {
        return None;
    }
    let used = (rec
        .payload
        .info
        .last_token_usage
        .total_tokens
        .unwrap_or(0.0)
        - CODEX_BASELINE_TOKENS)
        .max(0.0);
    Some(used / (window - CODEX_BASELINE_TOKENS) * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rollouts_name_says_which_session_it_holds() {
        assert_eq!(
            named_session("rollout-2026-09-20T00-04-11-01a0bc20-17aa-7fc1-9f2d-bd0033fcd441.jsonl"),
            Some("01a0bc20-17aa-7fc1-9f2d-bd0033fcd441")
        );
        assert_eq!(
            named_session("rollout-2026-09-20T00-04-11-sid-a.jsonl"),
            Some("sid-a")
        );
    }

    /// Only a whole name of codex's shape says anything; the header decides the
    /// rest.
    #[test]
    fn a_name_of_another_shape_says_nothing() {
        for name in [
            "rollout.jsonl",
            "s1.jsonl",
            "rollout-2026-09-20T00-04-11.jsonl",
            "rollout-2026-09-20T00-04-11-.jsonl",
            "rollout-2026-09-20Txx-04-11-sid.jsonl",
            "rollout-2026-09-20T00-04-11-sid.json",
        ] {
            assert_eq!(named_session(name), None, "{name}");
        }
    }
}
