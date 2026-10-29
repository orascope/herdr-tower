// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Answering the "do you trust the files in this folder?" question by reading
//! the screen.
//!
//! **Nothing in this crate calls this any more.** The launch path used to create
//! a shell pane, type the agent's command, and then watch the screen for a
//! startup dialog; herdr starts the agent through `agent.start`, waits for its
//! own integration to report ready, and surfaces a startup question as *blocked*
//! instead. That deleted both the guesswork and the incident behind it — Claude
//! Code changed its trust dialog's default button to "No, exit" and the blind
//! Enter this file sends began pressing *decline* (2026-09-11).
//!
//! It is kept anyway, and only for one reason: the CLI still exposes
//! `herdr-tower wt accept-prompts <pane>` as an internal subcommand, and a person
//! who has a worker stuck on a startup dialog can still run it by hand. When
//! that subcommand goes, this file goes with it.
//!
//! It was always the deliberate exception to the rule that screen text never
//! decides anything (2026-08-25): this is a launch handshake rather than a state
//! judgement, and it looks for a few seconds right after a pane is created.
//! Pre-registering trust in config files was tried and is not portable — codex
//! keys it per repository root, agy's location is undocumented, and every new
//! repository asks again.

use std::time::{Duration, Instant};

/// Vocabulary for "do you trust", in the three CLIs' phrasings plus Korean.
const TRUST_WORDS: [&str; 3] = ["trust", "신뢰", "승인"];

/// Vocabulary for what is being trusted. `director` covers both "directory" and
/// "directories".
const FOLDER_WORDS: [&str; 7] = [
    "folder",
    "director",
    "files",
    "workspace",
    "폴더",
    "파일",
    "디렉터리",
];

/// Whether the pane's last few dozen lines look like a folder-trust question.
///
/// Both vocabularies must appear, in either order — English asks trust→folder,
/// Korean folder→trust — and requiring both is what keeps "Reading files in
/// folder src/" and "trust me it's fine" from being mistaken for a dialog.
///
/// Matching is ASCII-case-insensitive rather than Unicode-case-insensitive. The
/// only inputs that tell the two apart are ones like `WOR<U+212A>SPACE` (a Kelvin
/// sign for the `k`), which no CLI emits; the Korean words have no case to fold.
pub fn is_trust_prompt(screen: &str) -> bool {
    let lower = screen.to_ascii_lowercase();
    TRUST_WORDS.iter().any(|w| lower.contains(w)) && FOLDER_WORDS.iter().any(|w| lower.contains(w))
}

/// How many lines of the pane to look at.
const SCREEN_LINES: u32 = 40;

/// Enters at a trust dialog until it goes away, and gives up if it will not.
///
/// Returns how many times Enter was pressed. Enter and not a chosen option: each
/// CLI's dialog defaults to yes and each labels the choices differently, so
/// accepting the default is the only gesture all three share.
///
/// The seam is two closures rather than two fallible calls because the algorithm
/// only ever asks two things — "is the pane still there" (`capture` yielding
/// `None`) and "did the Enter land" (`enter` yielding `false`). Threading error
/// types through would mean naming values that get discarded on the next line.
///
/// Three presses is the giving-up point: a dialog still on screen after three
/// Enters is not one of the shapes we know, and pressing a fourth time is typing
/// into somebody's session for no reason.
pub fn accept_startup_prompts<C, E, S>(
    capture: C,
    enter: E,
    pane_id: &str,
    timeout: Duration,
    interval: Duration,
    sleep: S,
) -> u32
where
    C: Fn(&str, u32) -> Option<String>,
    E: Fn(&str) -> bool,
    S: Fn(Duration),
{
    let deadline = Instant::now() + timeout;
    let mut pressed = 0;
    while Instant::now() < deadline {
        let Some(screen) = capture(pane_id, SCREEN_LINES) else {
            return pressed; // the pane is gone
        };
        if is_trust_prompt(&screen) {
            if enter(pane_id) {
                pressed += 1;
            }
            if pressed >= 3 {
                return pressed;
            }
            sleep(Duration::from_millis(1500)); // let the screen catch up
            continue;
        }
        if pressed > 0 {
            return pressed; // the question is gone, and we are what moved it
        }
        sleep(interval);
    }
    pressed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The four phrasings that have actually been seen on screen.
    #[test]
    fn the_dialog_is_recognized_in_all_three_clis_and_in_korean() {
        for screen in [
            "Do you trust the files in this folder?\n\n> 1. Yes, continue\n  2. No, quit", // codex
            "Do you trust the files in this folder?\n/Users/x/repo\n❯ Yes, proceed\n  No, exit", // claude
            "Do you trust this folder?\nTrusting a folder allows Gemini CLI to execute commands\n● Trust folder", // gemini
            "이 폴더를 신뢰하시겠습니까?\n[확인]",
        ] {
            assert!(is_trust_prompt(screen), "not recognized:\n{screen}");
        }
    }

    /// Half the vocabulary is not a dialog. Pressing Enter into a working agent
    /// submits an empty prompt at best and confirms something else at worst.
    #[test]
    fn one_vocabulary_alone_is_not_a_dialog() {
        for screen in [
            "› 무엇을 도와드릴까요?",
            "Reading files in folder src/",
            "git status: trust me it's fine",
        ] {
            assert!(!is_trust_prompt(screen), "false positive:\n{screen}");
        }
    }

    #[test]
    fn one_enter_is_enough_when_the_dialog_goes_away() {
        let screens = [
            "booting…",
            "Do you trust the files in this folder?\n> 1. Yes",
            "› ready",
        ];
        let seen = Cell::new(0usize);
        let pressed = Cell::new(0u32);
        let n = accept_startup_prompts(
            |_, _| {
                let i = seen.get();
                seen.set(i + 1);
                Some(screens[i.min(screens.len() - 1)].to_string())
            },
            |_| {
                pressed.set(pressed.get() + 1);
                true
            },
            "w2:p1",
            Duration::from_secs(5),
            Duration::from_millis(1),
            |_| {},
        );
        assert_eq!((n, pressed.get()), (1, 1));
    }

    /// A dialog that survives three Enters is a shape we do not know, and the
    /// right move is to stop typing into it.
    #[test]
    fn an_unfamiliar_dialog_is_abandoned_after_three_presses() {
        let pressed = Cell::new(0u32);
        let n = accept_startup_prompts(
            |_, _| Some("Do you trust this folder?".to_string()),
            |_| {
                pressed.set(pressed.get() + 1);
                true
            },
            "w2:p1",
            Duration::from_secs(5),
            Duration::from_millis(1),
            |_| {},
        );
        assert_eq!((n, pressed.get()), (3, 3));
    }

    #[test]
    fn a_pane_that_has_gone_ends_the_watch_immediately() {
        let n = accept_startup_prompts(
            |_, _| None,
            |_| panic!("nothing to press at a pane that is gone"),
            "w2:p1",
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| {},
        );
        assert_eq!(n, 0);
    }

    /// The pane is asked for its last few dozen lines, not its whole scrollback:
    /// a dialog that scrolled off is a dialog somebody already answered.
    #[test]
    fn the_watch_only_looks_at_the_visible_screen() {
        let asked = Cell::new(0u32);
        accept_startup_prompts(
            |_, lines| {
                asked.set(lines);
                None
            },
            |_| true,
            "w2:p1",
            Duration::from_millis(50),
            Duration::from_millis(1),
            |_| {},
        );
        assert_eq!(asked.get(), 40);
    }
}
