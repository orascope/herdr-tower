// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Rendering, against fixed records and a fixed clock.

use std::collections::HashMap;

use time::{Duration, OffsetDateTime};
use tower_cli::*;
use tower_state::{Agent, AgentState, Store};
use unicode_width::UnicodeWidthStr;

fn at(mins: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::minutes(mins)
}

/// The column shear this guards against: CJK characters occupy two terminal
/// columns, so padding by character count leaves the table crooked exactly when
/// a task description is in Korean.
#[test]
fn pads_by_display_width_not_character_count() {
    assert_eq!("승인 대기".width(), 9, "4 CJK chars + a space");
    assert_eq!("abcdefghi".width(), 9);
    assert_eq!(pad_right("승인 대기", 12).width(), 12);
    assert_eq!(pad_right("abcdefghi", 12).width(), 12);
    // Already wider than the target: left alone rather than truncated.
    assert_eq!(pad_right("긴글자열입니다", 4), "긴글자열입니다");
}

/// The ellipsis is paid for out of the budget, not added on top of it.
///
/// A cut cell is at most `max - 1` columns wide *including* the `…`. The
/// bound used to be asserted as
/// `<= 40`, which a version that produced exactly 40 satisfied — so the test
/// passed while every truncated cell was two columns too wide and the whole
/// table shifted. Assert the exact width, not a ceiling.
#[test]
fn truncates_by_display_width() {
    let long = "가".repeat(30); // 60 columns
    let cut = truncate_width(&long, 40);
    assert_eq!(
        cut.width(),
        39,
        "19 double-width chars (38) + the ellipsis; a 20th would overshoot: {cut:?}"
    );
    assert!(cut.ends_with('…'));

    // ASCII lands on the budget exactly, which is where an off-by-one hides.
    assert_eq!(truncate_width(&"a".repeat(60), 40).width(), 39);

    assert_eq!(
        truncate_width("짧다", 40),
        "짧다",
        "under the limit is untouched"
    );
    // Exactly at the limit is not truncated, so it may be one column wider
    // than any cut cell. The asymmetry is kept on purpose, and the table is
    // sized from the cells themselves, so it stays aligned either way.
    let exact = "가".repeat(20); // 40 columns
    assert_eq!(truncate_width(&exact, 40), exact);
}

#[test]
fn elapsed_time_is_coarse() {
    let t = at(0);
    assert_eq!(since(t, t), "just now");
    assert_eq!(since(t, t + Duration::seconds(59)), "just now");
    assert_eq!(since(t, t + Duration::minutes(5)), "5m");
    assert_eq!(since(t, t + Duration::hours(2)), "2h");
    assert_eq!(since(t, t + Duration::days(3)), "3d");
}

/// WORKING that has gone quiet is marked, because the state is only what a hook
/// last said and a lost hook looks identical to work in progress.
#[test]
fn stale_working_is_marked_with_a_question() {
    let mut a = Agent::new("claude-1", "claude", at(0));
    a.transition(AgentState::Working, at(0));
    assert_eq!(state_label(&a, at(1)), "[working]");
    assert_eq!(state_label(&a, at(60)), "[working?]");
}

#[cfg(unix)]
#[test]
fn home_is_shortened() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(shorten_home(&format!("{home}/proj")), "~/proj");
    assert_eq!(shorten_home(&home), "~");
    assert_eq!(shorten_home("/opt/elsewhere"), "/opt/elsewhere");
    // A path that merely starts with the same letters is not under home.
    assert_eq!(
        shorten_home(&format!("{home}x/proj")),
        format!("{home}x/proj")
    );
}

fn store_with(agents: Vec<Agent>) -> (tempfile::TempDir, Store) {
    let d = tempfile::tempdir().unwrap();
    let s = Store::new(d.path()).unwrap();
    for a in &agents {
        s.save(a).unwrap();
    }
    (d, s)
}

#[test]
fn table_columns_line_up_with_cjk_content() {
    let mut a = Agent::new("claude-w2:p1", "claude", at(0));
    a.pane.session = "작업공간".into();
    a.task = "핸드오프 문서 확인".into();
    a.cwd = "/proj".into();
    a.transition(AgentState::Waiting, at(0));

    let mut b = Agent::new("codex-w2:p2", "codex", at(0));
    b.pane.session = "dev".into();
    b.cwd = "/other".into();

    let (_d, s) = store_with(vec![a, b]);
    let mut out = Vec::new();
    status(&mut out, &s, false, at(1), &HashMap::new()).unwrap();
    let text = String::from_utf8(out).unwrap();

    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("STATE"));
    // Every row must start its DIR column at the same display offset.
    let dir_col = |line: &str| line.find('/').or_else(|| line.find('~'));
    let offsets: Vec<usize> = lines[1..]
        .iter()
        .filter_map(|l| dir_col(l).map(|i| l[..i].width()))
        .collect();
    assert!(
        offsets.windows(2).all(|w| w[0] == w[1]),
        "DIR column sheared: {offsets:?}\n{text}"
    );
}

/// A record written before the hooks cleaned their text — or by hand — still
/// never puts a terminal control sequence on the screen: no ESC, no BEL, no C1
/// introducer reaches the output, from the question or from any other cell.
#[test]
fn the_table_never_prints_a_control_character() {
    let mut a = Agent::new("claude-w2:p1", "claude", at(0));
    a.pane.session = "dev\u{1b}[2J".into();
    a.ask = "allow\u{1b}]52;c;ZXZpbA==\u{7} rm?\u{9b}".into();
    a.cwd = "/proj\u{1b}]0;pwned\u{7}".into();
    a.transition(AgentState::Waiting, at(0));

    let (_d, s) = store_with(vec![a]);
    let mut out = Vec::new();
    status(&mut out, &s, false, at(1), &HashMap::new()).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text
            .chars()
            .any(|c| c != '\n' && (c.is_control() || ('\u{80}'..='\u{9f}').contains(&c))),
        "{text:?}"
    );
    assert!(text.contains("allow]52;c;ZXZpbA== rm?"), "{text}");
}

#[test]
fn empty_store_says_so() {
    let (_d, s) = store_with(vec![]);
    let mut out = Vec::new();
    status(&mut out, &s, false, at(0), &HashMap::new()).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("no agents"));
}

/// The Discord wiring mark is decided per session, so a working session in a
/// bot's folder does not get one.
#[test]
fn wired_mark_follows_the_session() {
    let mut a = Agent::new("claude-1", "claude", at(0));
    a.pane.session = "collab-bot".into();
    let mut b = Agent::new("claude-2", "claude", at(0));
    b.pane.session = "dev".into();

    let (_d, s) = store_with(vec![a, b]);
    let wired = HashMap::from([("collab-bot".to_string(), "⌁테스트방".to_string())]);
    let mut out = Vec::new();
    status(&mut out, &s, false, at(0), &wired).unwrap();
    let text = String::from_utf8(out).unwrap();

    assert!(text.contains("collab-bot ⌁테스트방"));
    assert!(!text.contains("dev ⌁"), "an unwired session must stay bare");
}

#[test]
fn json_output_is_the_records() {
    let (_d, s) = store_with(vec![Agent::new("claude-1", "claude", at(0))]);
    let mut out = Vec::new();
    status(&mut out, &s, true, at(0), &HashMap::new()).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v[0]["id"], "claude-1");
    assert_eq!(v[0]["state"], "IDLE");
}

fn in_state(id: &str, state: AgentState) -> Agent {
    let mut a = Agent::new(id, "claude", at(0));
    a.transition(state, at(0));
    a
}

/// The tab-bar line: most urgent first, zero counts left out so a glance
/// never mistakes `0 WAIT` for a WAIT, and nothing at all with no one live.
#[test]
fn the_status_line_counts_only_what_asks_for_something() {
    use AgentState::*;
    let agents = [
        in_state("a", Working),
        in_state("b", Waiting),
        in_state("c", Working),
        in_state("d", Idle),
        in_state("e", Waiting),
        in_state("f", DoneUnread),
        in_state("g", Dead),
        in_state("h", Dead),
    ];
    assert_eq!(status_line(&agents), "🗼 2 wait · 1 done · 2 working");
    assert_eq!(
        status_line(&[in_state("x", Error), in_state("y", Waiting)]),
        "🗼 1 wait · 1 err"
    );
    assert_eq!(status_line(&[in_state("i", Idle)]), "🗼 idle");
    assert_eq!(status_line(&[in_state("g", Dead)]), "");
    assert_eq!(status_line(&[]), "");
}

/// Property, cross-checked against a naive counter over generated agent
/// sets (seeded): the line's counts are exactly the live agents per counted
/// state plus the Parallax count; their sum is every live agent that is not
/// idle, plus Parallax; no zero is ever printed; the words come in the fixed
/// order wait, parallax, err, done, active; and the empty and idle lines appear
/// exactly when the naive reading says they should.
#[test]
fn the_status_line_agrees_with_a_naive_counter() {
    use AgentState::*;
    const STATES: [AgentState; 6] = [Working, Waiting, DoneUnread, Idle, Error, Dead];
    const ORDER: [&str; 5] = ["wait", "parallax", "err", "done", "working"];
    let mut rng = tower_testrng::Rng::new(0x57A7_0511);
    for round in 0..600 {
        let n = if round % 50 == 0 { 300 } else { rng.below(12) };
        let agents: Vec<Agent> = (0..n)
            .map(|i| in_state(&format!("a{i}"), *rng.pick(&STATES).unwrap()))
            .collect();
        let parallax = [0, 0, 1, 2, 17][rng.below(5)];
        let line = status_line_with(&agents, parallax);

        let live = agents.iter().filter(|a| a.state != Dead).count();
        let of = |s: AgentState| agents.iter().filter(|a| a.state == s).count();
        let want = [
            ("wait", of(Waiting)),
            ("parallax", parallax),
            ("err", of(Error)),
            ("done", of(DoneUnread)),
            ("working", of(Working)),
        ];
        if live == 0 && parallax == 0 {
            assert_eq!(line, "", "{round}");
            continue;
        }
        let body = line.strip_prefix("🗼 ").expect("the tower prefix");
        if want.iter().all(|(_, n)| *n == 0) {
            assert_eq!(body, "idle", "{round}");
            continue;
        }
        let parts: Vec<(usize, &str)> = body
            .split(" · ")
            .map(|p| {
                let (n, w) = p.split_once(' ').unwrap();
                (n.parse().unwrap(), w)
            })
            .collect();
        assert!(parts.iter().all(|(n, _)| *n > 0), "no zero: {line}");
        let positions: Vec<usize> = parts
            .iter()
            .map(|(_, w)| ORDER.iter().position(|o| o == w).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "order: {line}");
        let got: Vec<(&str, usize)> = parts.iter().map(|(n, w)| (*w, *n)).collect();
        let naive: Vec<(&str, usize)> = want.into_iter().filter(|(_, n)| *n > 0).collect();
        assert_eq!(got, naive, "{line}");
        let sum: usize = parts.iter().map(|(n, _)| n).sum();
        assert_eq!(sum, live - of(Idle) + parallax, "{line}");
    }
    // `status_line` is `status_line_with` and no Parallax, for every set.
    let agents: Vec<Agent> = STATES
        .iter()
        .enumerate()
        .map(|(i, s)| in_state(&format!("s{i}"), *s))
        .collect();
    for k in 0..=agents.len() {
        assert_eq!(status_line(&agents[..k]), status_line_with(&agents[..k], 0));
    }
}

fn handoff(
    id: &str,
    workspace: &str,
    attention: tower_state::Attention,
) -> tower_state::ParallaxHandoff {
    tower_state::ParallaxHandoff {
        id: id.into(),
        workspace: workspace.into(),
        status: "needs_human".into(),
        attention,
        title: String::new(),
        created_at: None,
        updated_at: None,
    }
}

/// Parallax's handoffs sit right after WAIT, show with no agent live, and
/// only from a snapshot young enough to believe.
#[test]
fn the_status_line_puts_parallax_after_wait_and_only_when_fresh() {
    use tower_state::Attention;
    use AgentState::*;
    let agents = [
        in_state("a", Working),
        in_state("b", Waiting),
        in_state("c", Waiting),
        in_state("d", Working),
        in_state("e", Working),
    ];
    assert_eq!(
        status_line_with(&agents, 1),
        "🗼 2 wait · 1 parallax · 3 working"
    );
    assert_eq!(status_line_with(&agents, 0), status_line(&agents));
    assert_eq!(status_line_with(&[], 2), "🗼 2 parallax");
    assert_eq!(status_line_with(&[in_state("i", Idle)], 1), "🗼 1 parallax");
    assert_eq!(status_line_with(&[in_state("g", Dead)], 0), "");

    let snapshot = tower_state::ParallaxSnapshot {
        updated_at: at(0),
        reachable: true,
        workspaces: 3,
        handoffs: vec![
            handoff("u", "Default", Attention::Urgent),
            handoff("a", "Default", Attention::Action),
            handoff("r", "Default", Attention::Review),
        ],
    };
    let count = |now| tower_state::parallax_waiting(Some(&snapshot), now).len();
    assert_eq!(count(at(1)), 2, "review is not waiting");
    assert_eq!(
        status_line_with(&agents, count(at(1))),
        "🗼 2 wait · 2 parallax · 3 working"
    );
    assert_eq!(count(at(2)), 0, "two minutes old is stale");
    assert_eq!(status_line_with(&[], count(at(2))), "");
}

/// The table's Parallax section: absent when nothing waits, one row per
/// handoff otherwise, a redacted one without a title and no control
/// character from the other program's text.
#[test]
fn the_parallax_section_lists_what_waits_and_nothing_else() {
    use tower_state::Attention;
    let mut out = Vec::new();
    parallax_section(&mut out, &[]).unwrap();
    assert!(out.is_empty());

    let mut titled = handoff("a", "Research", Attention::Action);
    titled.title = "Pick a plan\u{1b}]52;c;x\u{7}".into();
    let mut redacted = handoff("u", "Default", Attention::Urgent);
    redacted.status = "waiting_approval".into();
    parallax_section(&mut out, &[&redacted, &titled]).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\nPARALLAX — handoffs waiting on you\n  Default   waiting for approval\n  Research  needs you             Pick a plan]52;c;x\n"
    );
}

/// One vocabulary for a working and a waiting agent, wherever it is shown:
/// the `status` label, the tab-bar line, the tower's row and the board's
/// column all use herdr's `working` and the tower's `wait`.
#[test]
fn every_surface_says_working_and_wait() {
    use tower_state::{Column, Progress};
    let now = at(1);
    let agent_in = |state| {
        let mut a = Agent::new("x", "claude", at(0));
        a.state = state;
        a.updated_at = at(0);
        a
    };
    let board = |progress| {
        Column {
            progress,
            since: None,
            uncertain: false,
        }
        .label()
    };
    for (state, progress, word) in [
        (AgentState::Working, Progress::Working, "working"),
        (AgentState::Waiting, Progress::Waiting, "wait"),
    ] {
        let a = agent_in(state);
        assert_eq!(state_label(&a, now), format!("[{word}]"));
        let row = tower_ui::view::state_label(&a, now);
        assert_eq!(row[row.find(' ').unwrap() + 1..].trim_end(), word);
        assert_eq!(board(progress), word);
        assert!(
            status_line(&[a]).ends_with(&format!("1 {word}")),
            "{}",
            status_line(&[agent_in(state)])
        );
    }
    // Stale work: the same word, marked in each surface's own way.
    let mut stale = agent_in(AgentState::Working);
    stale.updated_at = at(-600);
    assert_eq!(state_label(&stale, now), "[working?]");
    assert_eq!(tower_ui::view::state_label(&stale, now), "? working");
}
