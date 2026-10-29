// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Read-only consumers of data other tools own: the session files the
//! agent CLIs leave behind, and their default-model settings.
//!
//! Two rules shape everything here.
//!
//! **A missing source is an empty value, never an error.** The control tower
//! has to keep supervising agents when a CLI quietly changes its on-disk
//! layout, or when a session file is caught half-written. A blank context
//! figure is a far smaller loss than a tower that stops, so every reader
//! answers "I don't know" rather than failing.
//!
//! **Nothing reaches for the real home directory or spawns a process on its
//! own.** Home directories, session roots and tool lookups are all
//! parameters, so a test never depends on the machine it runs on.

mod claude;
mod claudecfg;
mod codex;
mod ctx;
mod fileread;
mod gemini;
mod jsonscan;
mod limits;
mod tools;

pub use claude::{
    claude_dir, claude_for_session, claude_for_session_in, CLAUDE_WINDOW, CLAUDE_WINDOW_1M,
};
pub use claudecfg::{
    claude_default_model, codex_default_model, gemini_default_model, is_fable, pretty_model,
    CodexDefault, DefaultModels,
};
pub use codex::{
    codex_for_session, codex_for_session_in, codex_latest, codex_session_id, CtxCache, Rollouts,
    CODEX_BASELINE_TOKENS,
};
pub use ctx::{
    agent_ctx, agent_ctx_cached, codex_sessions_root, gemini_dir, home_dir, CtxInfo, UsedPct,
};
pub use gemini::{
    agy_ctx, gemini_command, gemini_command_with, gemini_for_session, gemini_latest,
    AGY_BASELINE_BYTES, AGY_BYTES_PER_TOKEN, GEMINI_WINDOW,
};
pub use limits::{
    claude_plan, claude_token, codex_limits, codex_limits_newer, codex_limits_scan,
    parse_claude_usage, parse_codex_line, parse_codex_rate_limits, CodexScan, Limits, Window,
};
pub use tools::ToolSearch;
