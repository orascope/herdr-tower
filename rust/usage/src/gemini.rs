// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The two Gemini CLIs, which record almost nothing useful about themselves.
//!
//! Both live under `~/.gemini`, and neither writes down how big its context
//! window is. Everything this module reports as a percentage is therefore an
//! estimate — see [`UsedPct::approx`] for why that distinction is carried in
//! the type rather than in a comment.

use std::fs::File;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::time::SystemTime;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::ctx::{CtxInfo, UsedPct};
use crate::fileread::{read_head, read_tail};
use crate::jsonscan::{last_string_field, last_tokens_total};

/// The context window assumed for the Gemini 3 family: 1M tokens.
///
/// A constant because the session file does not record the real one. Every
/// percentage divided by it is approximate by construction — if Google ships a
/// model on a different window, the figures stay plausible and stay wrong,
/// which is exactly what the `~` warns about.
pub const GEMINI_WINDOW: f64 = 1_000_000.0;

/// Bytes of transcript per token, roughly.
pub const AGY_BYTES_PER_TOKEN: f64 = 4.0;

/// Fixed overhead of an agy model request: system prompt, skills, tool
/// definitions.
///
/// None of it appears in the transcript, all of it occupies the context.
/// Measured 2026-08: at a transcript size of 30KB the real request was 134KB,
/// so the invisible part is around 105KB. Left at 100KB — the estimate is
/// coarse enough that a rounder number is no less honest, and erring low keeps
/// the reported share from overstating.
pub const AGY_BASELINE_BYTES: f64 = 100_000.0;

/// How much of one gemini chat log to read back for the current turn.
const TAIL_BYTES: u64 = 64 * 1024;

/// Enough of a chat log's start to carry its `sessionId` line.
const HEAD_BYTES: u64 = 8 * 1024;

/// Which command starts a `gemini`-kind agent in this installation.
///
/// `agy` (the Antigravity CLI) if there is any trace of it, because the stock
/// CLI's free tier is no longer reachable: the Gemini Code Assist for
/// individuals auth path is closed and the CLI refuses with
/// `IneligibleTierError` (measured 2026-08). Without that trace, stock
/// `gemini` — users on an API key or Vertex auth are still fine there.
///
/// The same three signals `init` uses to decide whether to register agy's hooks,
/// and for the reason recorded there: a freshly installed `agy` has written no
/// `config/` yet (measured on WSL2, 2026-09-07), so its folders prove nothing on
/// their own. Choosing stock `gemini` for that installation starts a CLI that
/// refuses to run.
pub fn gemini_command(gemini_dir: &Path) -> &'static str {
    gemini_command_with(gemini_dir, &on_path)
}

/// The same decision with the `PATH` probe injected, so a test's answer does not
/// depend on whether the machine running it happens to have `agy`.
pub fn gemini_command_with(gemini_dir: &Path, on_path: &dyn Fn(&str) -> bool) -> &'static str {
    let traced = gemini_dir.join("antigravity-cli").is_dir()
        || gemini_dir.join("config").is_dir()
        || on_path("agy");
    if traced {
        "agy"
    } else {
        "gemini"
    }
}

/// Whether a command can be found the way a shell would find it.
fn on_path(bin: &str) -> bool {
    tower_os::on_path(bin)
}

#[derive(Deserialize, Default)]
struct Projects {
    #[serde(default)]
    projects: std::collections::HashMap<String, String>,
}

/// The stock Gemini CLI's temp-folder name for `workdir`, from
/// `~/.gemini/projects.json`.
///
/// Exact match first; failing that, the longest ancestor that is mapped — a bot
/// that `cd`s into a subdirectory of the project it was launched in must not
/// fall off its own session.
fn project_folder(gemini_dir: &Path, workdir: &str) -> Option<String> {
    let body = std::fs::read(gemini_dir.join("projects.json")).ok()?;
    let pj: Projects = serde_json::from_slice(&body).ok()?;

    let mut best: Option<(usize, &str)> = None;
    for (path, name) in &pj.projects {
        let is_ancestor =
            workdir == path || workdir.starts_with(&format!("{path}{MAIN_SEPARATOR}"));
        if !is_ancestor {
            continue;
        }
        if best.is_none_or(|(len, _)| path.len() > len) {
            best = Some((path.len(), name));
        }
    }
    best.map(|(_, name)| name.to_string())
}

/// The most recently written `session-*.jsonl` in `dir`.
///
/// Candidates are visited in path order and only a strictly newer mtime
/// displaces the incumbent, so two logs written in the same second resolve the
/// same way on every call instead of following whatever order the filesystem
/// handed back.
fn newest_chat_log(dir: &Path) -> Option<(PathBuf, SystemTime)> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for (p, mtime) in chat_logs(dir) {
        if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
            best = Some((p, mtime));
        }
    }
    best
}

/// The model and context share of the most recent stock Gemini CLI session in
/// `workdir`.
///
/// Each model turn in the session file carries a `model` key, so the last one
/// is the model answering now. The turn's `tokens.total` is the size of the
/// context at that point — which, against an assumed window, gives an
/// approximate share.
///
/// agy sessions do not write their model to these files at all; the hook's
/// `modelName` covers that case, applied by [`crate::agent_ctx`].
pub fn gemini_latest(gemini_dir: &Path, workdir: &str) -> CtxInfo {
    let Some(chats) = chats_dir(gemini_dir, workdir) else {
        return CtxInfo::default();
    };
    let Some((path, mtime)) = newest_chat_log(&chats) else {
        return CtxInfo::default();
    };
    chat_info(&path, mtime)
}

/// The same, for a session whose ID the record already knows.
///
/// The ID first, the folder second — the rule the claude and codex branches
/// follow. Two stock Gemini sessions in one checkout share the folder, so the
/// newest-log answer was handed to *both* rows and the older agent displayed the
/// newer session's model and share. The log names itself in its first line
/// (`"sessionId":…`), which is what makes the older one findable.
pub fn gemini_for_session(gemini_dir: &Path, session_id: &str, workdir: &str) -> CtxInfo {
    if !session_id.is_empty() {
        if let Some(chats) = chats_dir(gemini_dir, workdir) {
            if let Some((path, mtime)) = chat_log_for(&chats, session_id) {
                return chat_info(&path, mtime);
            }
        }
    }
    gemini_latest(gemini_dir, workdir)
}

/// Where this working directory's chat logs live.
fn chats_dir(gemini_dir: &Path, workdir: &str) -> Option<PathBuf> {
    let name = project_folder(gemini_dir, workdir)?;
    Some(gemini_dir.join("tmp").join(name).join("chats"))
}

/// Every chat log in the folder with its modification time, name order.
///
/// Sorted by name so the walk is reproducible; the caller decides what "best"
/// means.
fn chat_logs(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("session-") && n.ends_with(".jsonl")
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((p, mtime))
        })
        .collect()
}

/// The chat log whose header names this session.
fn chat_log_for(dir: &Path, session_id: &str) -> Option<(PathBuf, SystemTime)> {
    let needle = serde_json::to_string(session_id)
        .map(|q| format!("\"sessionId\":{q}"))
        .ok()?;
    for (path, mtime) in chat_logs(dir) {
        if read_head(&path, HEAD_BYTES).is_some_and(|h| h.contains(&needle)) {
            return Some((path, mtime));
        }
    }
    None
}

/// Model and context share from one chat log.
fn chat_info(path: &Path, mtime: SystemTime) -> CtxInfo {
    let Ok(mut f) = File::open(path) else {
        return CtxInfo::default();
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let tail = read_tail(&mut f, size, TAIL_BYTES);

    CtxInfo {
        model: last_string_field(&tail, "model")
            .unwrap_or_default()
            .to_string(),
        used_pct: last_tokens_total(&tail)
            .filter(|t| *t > 0.0)
            .map(|total| UsedPct::approx(total / GEMINI_WINDOW * 100.0)),
        ts: Some(OffsetDateTime::from(mtime)),
    }
}

/// Estimates how full an agy (Antigravity CLI) conversation's context is.
///
/// agy writes no token counts to disk, so the only handle is the size of the
/// brain transcript: bytes over [`AGY_BYTES_PER_TOKEN`], plus the fixed
/// [`AGY_BASELINE_BYTES`] the transcript never shows, over the assumed window.
/// Estimating an agy session from outside is the only option available — the
/// session cannot be asked, and it does not volunteer.
///
/// Both transcript files are considered because which one exists varies by agy
/// version; the larger is the fuller record, and the later mtime is the more
/// recent activity.
pub fn agy_ctx(gemini_dir: &Path, conversation_id: &str) -> CtxInfo {
    if conversation_id.is_empty() {
        return CtxInfo::default();
    }
    let logs = gemini_dir
        .join("antigravity-cli")
        .join("brain")
        .join(conversation_id)
        .join(".system_generated")
        .join("logs");

    let mut size: Option<u64> = None;
    let mut ts: Option<SystemTime> = None;
    for name in ["transcript_full.jsonl", "transcript.jsonl"] {
        let Ok(meta) = std::fs::metadata(logs.join(name)) else {
            continue;
        };
        size = Some(size.map_or(meta.len(), |s| s.max(meta.len())));
        if let Ok(m) = meta.modified() {
            ts = Some(ts.map_or(m, |t| t.max(m)));
        }
    }
    let Some(size) = size else {
        return CtxInfo::default();
    };

    let tokens = (AGY_BASELINE_BYTES + size as f64) / AGY_BYTES_PER_TOKEN;
    CtxInfo {
        model: String::new(),
        used_pct: Some(UsedPct::approx(tokens / GEMINI_WINDOW * 100.0)),
        ts: ts.map(OffsetDateTime::from),
    }
}
