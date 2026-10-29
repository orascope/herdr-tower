// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! How full each agent's context window is, and which model is answering.
//!
//! There is no single source for this. Each CLI records it somewhere else, in
//! its own shape, with its own idea of how precise the number is — so the job
//! is to attribute the right source to the right agent and to stay honest
//! about which figures are measured and which are guessed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use tower_state::Agent;

use crate::claude::claude_for_session_in;
use crate::claudecfg::claude_default_model_in;
use crate::codex::{codex_for_session_in, CtxCache, Rollouts};
use crate::gemini::{agy_ctx, gemini_for_session};

/// How much of a context window is used, carrying whether the number is
/// measured or estimated.
///
/// A bare number with an "approximate" flag beside it would leave every
/// renderer to remember to check the flag before printing. One that forgets
/// shows a guess with the authority of a measurement — a `~` missing from
/// `ctx 62%` is invisible until somebody makes a decision on it.
/// Here the two cannot be confused: reaching the number means naming which
/// kind it is, and [`UsedPct::label`] carries the marker for free.
///
/// Exact figures come from a CLI that wrote down both the token count and the
/// window size (codex rollouts). Approximate ones are
/// reconstructed against a window size we had to assume, or from transcript
/// bytes — see [`crate::GEMINI_WINDOW`], [`crate::CLAUDE_WINDOW`] and
/// [`crate::AGY_BASELINE_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsedPct {
    /// Measured: the source recorded both the tokens and the window.
    Exact(f64),
    /// Estimated. Displayed with a leading `~`.
    Approx(f64),
}

impl UsedPct {
    /// An estimate, capped at 100%.
    ///
    /// An estimate that lands above 100% only means the assumed window was too
    /// small; reporting "137% used" would be reporting the error, not the
    /// usage. Measured figures are deliberately not capped — if one ever
    /// exceeds 100% that is a fact about the source worth seeing.
    pub fn approx(pct: f64) -> Self {
        UsedPct::Approx(pct.min(100.0))
    }

    pub fn value(self) -> f64 {
        match self {
            UsedPct::Exact(v) | UsedPct::Approx(v) => v,
        }
    }

    pub fn is_approx(self) -> bool {
        matches!(self, UsedPct::Approx(_))
    }

    /// The `~` an estimate has to wear, empty for a measurement.
    pub fn marker(self) -> &'static str {
        if self.is_approx() {
            "~"
        } else {
            ""
        }
    }

    /// `"14%"` / `"~8%"`, truncating as the tower's text renderers do.
    pub fn label(self) -> String {
        format!("{}{}%", self.marker(), self.value() as i64)
    }
}

/// What is known about one agent's most recent session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtxInfo {
    /// A display name or a model ID, whichever the source happened to record.
    /// Empty means unknown.
    pub model: String,
    pub used_pct: Option<UsedPct>,
    /// When the underlying source was last written. `None` where no source
    /// spoke at all — a zero time would read as a real timestamp in year 1 to
    /// anything that forgets to check.
    pub ts: Option<OffsetDateTime>,
}

impl CtxInfo {
    /// Nothing worth attributing to an agent. Callers use this to decide
    /// whether a source answered at all, so it must stay in step with what a
    /// row can display: a model name, a percentage, or neither.
    pub fn is_empty(&self) -> bool {
        self.model.is_empty() && self.used_pct.is_none()
    }
}

/// The home directory, for callers assembling the real paths below.
///
/// Reading it here rather than inside each path helper is what keeps this
/// crate testable: nothing else in it consults the environment.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Where codex keeps its rollouts.
pub fn codex_sessions_root(home: &Path) -> PathBuf {
    home.join(".codex").join("sessions")
}

/// The root the Gemini CLI ecosystem shares (`~/.gemini`), stock and
/// Antigravity alike.
pub fn gemini_dir(home: &Path) -> PathBuf {
    home.join(".gemini")
}

/// Collects context information per agent ID, each kind from its own source.
///
/// Keying on the working folder alone is what this exists to avoid: a codex
/// and a gemini session can share one folder, and one would then overwrite the
/// other's row with its model and context share. Every kind digs in its own
/// place instead.
///
/// The TUI, the Discord cards and `info` all call this, so the attribution
/// rules live here once rather than being re-derived — and re-broken — in three
/// renderers.
pub fn agent_ctx(
    agents: &[Agent],
    codex_root: &Path,
    gemini_root: &Path,
    claude_dir: &Path,
) -> HashMap<String, CtxInfo> {
    agent_ctx_cached(
        agents,
        codex_root,
        gemini_root,
        claude_dir,
        &mut CtxCache::default(),
    )
}

/// [`agent_ctx`] for a caller that asks on a clock: codex rollouts are read
/// through `codex`, which it keeps between calls, and the rollout directory is
/// walked once for all the codex agents rather than once for each.
pub fn agent_ctx_cached(
    agents: &[Agent],
    codex_root: &Path,
    gemini_root: &Path,
    claude_dir: &Path,
    codex: &mut CtxCache,
) -> HashMap<String, CtxInfo> {
    let mut out = HashMap::new();
    let mut rollouts: Option<Rollouts> = None;
    let mut claude_default: Option<String> = None;
    codex.begin_pass();
    for a in agents {
        if a.cwd.is_empty() {
            continue;
        }
        match a.kind.as_str() {
            "claude" => {
                // The transcript is filed by session ID, so two sessions in
                // one folder never trade models.
                let default =
                    claude_default.get_or_insert_with(|| claude_default_model_in(claude_dir));
                let info = claude_for_session_in(claude_dir, &a.session_id, &a.cwd, default, codex);
                if !info.is_empty() {
                    out.insert(a.id.clone(), info);
                }
            }
            "codex" => {
                // Own session first, folder second. With several sessions in
                // one folder the folder key holds only the most recent, and the
                // older rows would wear its model.
                let rollouts = rollouts.get_or_insert_with(|| Rollouts::scan(codex_root));
                let info = codex_for_session_in(rollouts, &a.session_id, &a.cwd, codex);
                if !info.is_empty() {
                    out.insert(a.id.clone(), info);
                }
            }
            "gemini" => {
                // Two candidate sources for one kind: the stock CLI's session
                // file, and the estimate from an Antigravity transcript. The
                // fresher one wins.
                let mut info = gemini_for_session(gemini_root, &a.session_id, &a.cwd);
                let agy = agy_ctx(gemini_root, &a.session_id);
                if agy.used_pct.is_some() && (info.ts.is_none() || agy.ts > info.ts) {
                    info = agy;
                }
                if info.model.is_empty() && !a.model.is_empty() {
                    // agy never writes its model to disk; the hook recorded it.
                    info.model = a.model.clone();
                }
                if info.ts.is_none() {
                    info.ts = Some(a.updated_at);
                }
                if !info.is_empty() {
                    out.insert(a.id.clone(), info);
                }
            }
            "opencode" => {
                // opencode's plugin reports the prompt size and the model's
                // window through the hook, so both are on the record, and the
                // share is a measurement rather than an estimate.
                let used_pct = (a.ctx_tokens > 0 && a.ctx_window > 0)
                    .then(|| UsedPct::Exact(a.ctx_tokens as f64 / a.ctx_window as f64 * 100.0));
                let info = CtxInfo {
                    model: a.model.clone(),
                    used_pct,
                    ts: Some(a.updated_at),
                };
                if !info.is_empty() {
                    out.insert(a.id.clone(), info);
                }
            }
            _ => {}
        }
    }
    codex.end_pass();
    out
}
