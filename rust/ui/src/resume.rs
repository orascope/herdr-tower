// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The command that revives a dead session's conversation.
//!
//! The explicit `resume` and `restore --resume` commands use this. It stays in a
//! dependency below `tower_cli` because the UI's local-info pass needs the same
//! paths for context lookup; putting it in the CLI would create a cycle.
//!
//! The three CLIs disagree about where a conversation ID lives, and that
//! disagreement is the whole content of this file:
//!
//!   - **claude** records its `session_id` through the hook, so the record has
//!     it.
//!   - **codex**'s `notify` hook carries no session ID at all, so it has to be
//!     dug out of the rollout files by working directory.
//!   - **gemini** is two products behind one name. Only Antigravity
//!     conversations can be resumed; the stock CLI has no resume command, and
//!     the only way to tell them apart is whether Antigravity left a `brain`
//!     folder for that ID.

use std::path::{Path, PathBuf};

use tower_state::Agent;

/// Where the resume lookups read from.
///
/// Parameters rather than `$HOME` lookups so a test can point them at a temp
/// directory — the same rule `tower_usage` follows, and for the same reason: a test
/// that reads the developer's real `~/.codex` passes or fails according to what
/// they happened to be doing that afternoon.
#[derive(Debug, Clone, Default)]
pub struct ResumePaths {
    pub codex_root: PathBuf,
    pub gemini_dir: PathBuf,
    pub claude_dir: PathBuf,
}

impl ResumePaths {
    /// The real locations, from `$HOME`. Falls back to paths under the current
    /// directory when there is no home, which finds nothing — the same outcome
    /// as a machine where those CLIs are not installed.
    pub fn from_env() -> Self {
        let home = tower_usage::home_dir().unwrap_or_else(|| PathBuf::from("."));
        ResumePaths {
            codex_root: tower_usage::codex_sessions_root(&home),
            gemini_dir: tower_usage::gemini_dir(&home),
            claude_dir: tower_usage::claude_dir(&home),
        }
    }

    pub fn under(home: &Path) -> Self {
        ResumePaths {
            codex_root: tower_usage::codex_sessions_root(home),
            gemini_dir: tower_usage::gemini_dir(home),
            claude_dir: tower_usage::claude_dir(home),
        }
    }
}

/// The shell command that resumes this agent's conversation, or the reason
/// there is none.
///
/// The error is the sentence the recovery command prints, so it says what is
/// missing rather than naming a Rust type. `Err` is not a failure of this
/// function: most dead records genuinely cannot be revived, and the command has
/// to say so without pretending it could have.
///
/// The result is typed into a shell, so the ID in it is checked and quoted
/// here, at the last step, whatever the writers did. The hooks and the scanner
/// already drop an ID outside [`tower_state::valid_session_id`], but a record
/// written before they did — or by hand — reaches this function all the same,
/// and a session ID like `x; curl …|sh #` would otherwise run as a command in
/// the user's own shell the moment they resumed it. An invalid ID is refused
/// rather than cleaned: a mangled ID resumes nothing, or the wrong thing. The
/// quoting on top is the second lock, not the first.
pub fn resume_command(a: &Agent, paths: &ResumePaths) -> Result<String, String> {
    match a.kind.as_str() {
        "claude" => {
            if a.session_id.is_empty() {
                return Err("a claude session with no recorded session_id".into());
            }
            Ok(format!("claude --resume {}", checked(&a.session_id)?))
        }
        "codex" => {
            // The record's own ID first. The directory scan answers with the
            // *newest* rollout for that folder, so reviving an older dead row
            // resumed whatever conversation had run there most recently — a
            // different one. Both the codex hooks and the backend can fill this
            // field in, so it is usually there; the scan stays as the fallback for
            // the notify-only path, which carries no session ID at all.
            if !a.session_id.is_empty() {
                return Ok(format!("codex resume {}", checked(&a.session_id)?));
            }
            if a.cwd.is_empty() {
                return Err("a codex session with neither cwd nor session_id".into());
            }
            // A rollout file is as untrusted as a hook payload: it is only a
            // file under `~/.codex`, and whatever can write there chooses this
            // ID too.
            match tower_usage::codex_session_id(&paths.codex_root, &a.cwd) {
                Some(sid) => Ok(format!("codex resume {}", checked(&sid)?)),
                None => Err(format!(
                    "no session found in the codex rollouts for {}",
                    a.cwd
                )),
            }
        }
        "gemini" => {
            if a.session_id.is_empty() {
                return Err("a gemini session with no recorded conversation ID".into());
            }
            // Checked before the join as well as before the command: `../` in
            // the ID would point the existence test outside `brain`.
            let id = a.session_id.trim();
            let sid = checked(id)?;
            let brain = paths
                .gemini_dir
                .join("antigravity-cli")
                .join("brain")
                .join(id);
            if !brain.exists() {
                return Err(
                    "the stock Gemini CLI cannot resume from the CLI (agy conversations only)"
                        .into(),
                );
            }
            Ok(format!("agy --conversation {sid}"))
        }
        // The ID comes from the plugin `init` installs; without it there is
        // nothing to name.
        "opencode" => {
            if a.session_id.is_empty() {
                return Err(
                    "an opencode session with no recorded session ID (is the plugin installed? herdr-tower init)"
                        .into(),
                );
            }
            Ok(format!("opencode --session {}", checked(&a.session_id)?))
        }
        // The limit is this tool's, not the CLI's: it has never recorded a
        // conversation ID for such a kind to resume.
        other => Err(format!(
            "herdr-tower records no {other} conversation to resume"
        )),
    }
}

/// `id` as one shell word, or the sentence that says why it will not be typed.
///
/// Blanks around the ID are not part of it — a recorded trailing newline is
/// the case the info card already trims for — so they are dropped before the
/// check rather than failing it.
fn checked(id: &str) -> Result<String, String> {
    let id = id.trim();
    if tower_state::valid_session_id(id) {
        Ok(tower_state::sh_quote(id))
    } else {
        Err(format!(
            "the recorded session ID {:?} is not a session ID; refusing to type it into a shell",
            tower_state::clean_text(id)
        ))
    }
}
