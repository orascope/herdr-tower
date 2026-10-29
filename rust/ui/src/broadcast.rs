// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Sending one instruction to every session at once (`W`, `C`, `B`).
//!
//! Like [`crate::resume`], this lives below the command line rather than in
//! it: the three keys are tower behaviour, and the tests specify what they send
//! and to whom.
//!
//! The confirmation is deliberately *not* here. Whether to ask before writing
//! to every agent on the machine is the caller's decision (the TUI's `y/N`
//! prompt, or a CLI flag), and burying it in the send would make the safe
//! version the one nobody remembers to reach for.

use tower_state::Agent;

/// The wake and close instructions, for folders that keep a session handoff
/// (`SESSION.md`). The agents read these as prose, so the exact strings are
/// the interface. In English, like everything the tower sends an agent: they
/// name the file, so they work whatever words a folder's own rules use for
/// "resume" and "wrap up".
pub const WAKE_MESSAGE: &str =
    "Resume the session: read SESSION.md, then carry on with its next step.";
pub const CLOSE_MESSAGE: &str =
    "Wrap up the session: write where you stopped and what comes next into SESSION.md.";

/// The close instruction as it was worded before it was English. Never sent;
/// only recognised, in a schedule stored back then, so that sending it still
/// ends the session's idle loop.
const LEGACY_CLOSE_MESSAGE: &str = "\u{C138}\u{C158} \u{B9C8}\u{AC10}\u{D558}\u{C790}";
/// The wake instruction as it was, likewise only recognised.
const LEGACY_WAKE_MESSAGE: &str = "\u{C138}\u{C158} \u{C774}\u{C5B4}\u{C11C}\u{D558}\u{C790}";

/// Whether a message is the close instruction, as worded now or before.
pub fn is_close_message(message: &str) -> bool {
    let m = message.trim();
    m == CLOSE_MESSAGE || m == LEGACY_CLOSE_MESSAGE
}

/// What to call an instruction when telling a person it went out.
///
/// The two above are the agents' interface and stay in the words the agents
/// were taught, which are not the words the person typed — they pressed `W` or
/// `C`. Echoing those bytes back puts a phrase nobody wrote into a line that is
/// otherwise theirs, and in a screen that is now English it reads as a bug. So
/// the built-ins are named; anything else is the person's own text and is shown
/// exactly as they wrote it, quoted.
pub fn shown(message: &str) -> String {
    match message {
        WAKE_MESSAGE | LEGACY_WAKE_MESSAGE => "the wake instruction".into(),
        CLOSE_MESSAGE | LEGACY_CLOSE_MESSAGE => "the close instruction".into(),
        other => format!("{other:?}"),
    }
}

/// How a broadcast went. Two counts, because "3 sent" means nothing without
/// knowing whether there were three targets or thirty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sent {
    pub sent: usize,
    pub total: usize,
}

/// Who a broadcast goes to.
///
/// Every supervised kind is a target: the handoff discipline runs in each of
/// them alike, so singling one out would leave sessions behind on a
/// close-everything.
///
/// `self_pane` is the pane the tower itself is running in, and it is excluded.
/// It has to be asked of the backend rather than read from the environment: a
/// pane variable the running backend did not set names a coordinate in a
/// vocabulary that backend may not use, and the tower then broadcasts an
/// instruction to itself.
pub fn targets<'a>(agents: &'a [Agent], self_pane: Option<&str>) -> Vec<&'a Agent> {
    agents
        .iter()
        .filter(|a| tower_state::SUPERVISED_KINDS.contains(&a.kind.as_str()))
        .filter(|a| a.is_live())
        .filter(|a| !a.pane.pane_id.is_empty())
        .filter(|a| {
            self_pane
                .is_none_or(|pane| pane != tower_mux::target_key(&a.pane.backend, &a.pane.pane_id))
        })
        .collect()
}

/// Whether a folder has the session-handoff discipline set up.
///
/// `SESSION.md` present, or a `CLAUDE.md`/`AGENTS.md` that mentions the
/// discipline. Wake and close are instructions *about* that discipline — telling
/// an agent to "resume the session" in a folder with no `SESSION.md` asks it to
/// act on a file that does not exist, and it will invent one.
pub fn has_session_handoff(cwd: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let dir = std::path::Path::new(cwd);
    if dir.join("SESSION.md").exists() {
        return true;
    }
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let Ok(body) = std::fs::read_to_string(dir.join(name)) else {
            continue;
        };
        if body.contains("store:session-handoff") || body.contains("세션 이어가기") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A close sent from a schedule stored before the instruction was English
    /// still ends the session's idle loop, and is still named, not quoted.
    #[test]
    fn the_old_close_wording_is_still_recognised() {
        assert!(is_close_message(CLOSE_MESSAGE));
        assert!(is_close_message(LEGACY_CLOSE_MESSAGE));
        assert!(is_close_message(&format!("  {LEGACY_CLOSE_MESSAGE}\n")));
        assert!(!is_close_message(WAKE_MESSAGE));
        assert!(!is_close_message(LEGACY_WAKE_MESSAGE));
        assert_eq!(shown(LEGACY_CLOSE_MESSAGE), "the close instruction");
        assert_eq!(shown(LEGACY_WAKE_MESSAGE), "the wake instruction");
        assert_eq!(shown(CLOSE_MESSAGE), "the close instruction");
    }
}
