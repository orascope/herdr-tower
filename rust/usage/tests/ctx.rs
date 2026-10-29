// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Which source is attributed to which agent, and how honest the numbers are.

use std::fs;
use std::path::Path;
use std::time::{Duration as StdDuration, SystemTime};

use time::{Duration, OffsetDateTime};
use tower_state::Agent;
use tower_usage::*;

fn at(mins: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::minutes(mins)
}

fn write(dir: &Path, rel: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

/// Pins a file's mtime, since "which session is most recent" is decided by it
/// and two files written in the same test are otherwise the same age.
fn set_mtime(path: &Path, secs: u64) {
    let f = fs::File::options().write(true).open(path).unwrap();
    let t = SystemTime::UNIX_EPOCH + StdDuration::from_secs(secs);
    f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
}

fn agent(id: &str, kind: &str, cwd: &str) -> Agent {
    let mut a = Agent::new(id, kind, at(0));
    a.cwd = cwd.into();
    a
}

/// An estimate and a measurement must not be substitutable. If they were, a
/// renderer could print a guessed 62% with the authority of a measured one.
#[test]
fn approximate_and_exact_shares_are_different_things() {
    assert_eq!(UsedPct::Exact(14.0).label(), "14%");
    assert_eq!(UsedPct::approx(8.4).label(), "~8%");
    assert!(UsedPct::approx(1.0).is_approx());
    assert!(!UsedPct::Exact(1.0).is_approx());
    assert_ne!(UsedPct::Exact(14.0), UsedPct::Approx(14.0));
}

/// An estimate above 100% is a statement about the assumed window, not about
/// the session; capping keeps it from being read as usage.
#[test]
fn estimates_are_capped_at_full() {
    assert_eq!(UsedPct::approx(137.0), UsedPct::Approx(100.0));
}

/// A claude row with no transcript on disk shows nothing, and sharing a folder
/// with it leaves the gemini row its own model.
#[test]
fn a_claude_row_without_a_transcript_shadows_nothing() {
    let claude = agent("claude-1", "claude", "/w");
    let mut gem = agent("gemini-2", "gemini", "/w");
    gem.model = "gemini-3.6-flash".into();

    let empty = tempfile::tempdir().unwrap();
    let out = agent_ctx(&[claude, gem], empty.path(), empty.path(), empty.path());
    assert!(!out.contains_key("claude-1"), "{out:?}");
    assert_eq!(
        out["gemini-2"].model, "gemini-3.6-flash",
        "gemini keeps its own model"
    );
}

const CODEX_ROLLOUT: &str = r#"{"timestamp":"2026-08-25T01:00:00Z","type":"session_meta","payload":{"id":"x","cwd":"/Users/x/codexproj","originator":"codex_cli"}}
{"timestamp":"2026-08-25T01:00:01Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}
{"timestamp":"2026-08-25T01:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":50000},"last_token_usage":{"total_tokens":30400},"model_context_window":272000}}}
"#;

/// The share has to agree with what codex's own TUI shows, or the tower and the
/// pane it is watching disagree about the same session.
#[test]
fn codex_share_matches_the_tui_baseline() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "2026/08/25/s1.jsonl", CODEX_ROLLOUT);

    let info = codex_latest(d.path(), "/Users/x/codexproj");
    assert_eq!(info.model, "gpt-5.6-sol");
    // (30400 - 12000) / (272000 - 12000) — the baseline comes off both sides.
    let pct = info.used_pct.expect("a token_count line was present");
    assert!(
        !pct.is_approx(),
        "codex records both the tokens and the window"
    );
    assert!((pct.value() - 7.077).abs() < 0.01, "got {}", pct.value());
}

/// Subtracting the baseline from the usage alone would show a brand new session
/// as already 4% full.
#[test]
fn a_fresh_codex_session_reads_as_empty() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "2026/08/25/s1.jsonl",
        &CODEX_ROLLOUT.replace(
            r#""last_token_usage":{"total_tokens":30400}"#,
            r#""last_token_usage":{"total_tokens":12000}"#,
        ),
    );
    assert_eq!(
        codex_latest(d.path(), "/Users/x/codexproj").used_pct,
        Some(UsedPct::Exact(0.0))
    );
}

/// Two rollouts the filesystem calls the same age still resolve to the newer.
///
/// ext4 stamps in seconds, and so does the overlay a container gets, so two
/// sessions started in the same second arrive with one mtime between them. The
/// tie used to break on the path *ascending*, which answers with the older of
/// the two — codex writes under `YYYY/MM/DD/`, so ascending is oldest-first —
/// and the folder walk then handed every row that session's model and share.
/// APFS stamps in nanoseconds, which is why this was invisible on macOS.
#[test]
fn rollouts_the_clock_cannot_separate_break_toward_the_newer_path() {
    let d = tempfile::tempdir().unwrap();
    let head = |sid: &str, model: &str| {
        format!(
            "{{\"timestamp\":\"2026-08-25T01:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{sid}\",\"cwd\":\"/Users/x/codexproj\"}}}}\n\
             {{\"type\":\"turn_context\",\"payload\":{{\"model\":\"{model}\"}}}}\n"
        )
    };
    let old = write(
        d.path(),
        "2026/08/24/rollout.jsonl",
        &head("sid-old", "gpt-old"),
    );
    let new = write(
        d.path(),
        "2026/08/25/rollout.jsonl",
        &head("sid-new", "gpt-new"),
    );
    let same = 1_700_000_000;
    set_mtime(&old, same);
    set_mtime(&new, same);

    assert_eq!(
        codex_latest(d.path(), "/Users/x/codexproj").model,
        "gpt-new",
        "a tie on the clock is not a tie on the date in the path"
    );
}

/// Two codex sessions in one checkout are two sessions. The folder walk answers
/// with the newest for every row, so each older agent wore the newest session's
/// model and context share until the recorded ID was consulted first.
#[test]
fn codex_context_follows_the_recorded_session_id() {
    let d = tempfile::tempdir().unwrap();
    let head = |sid: &str, model: &str, tokens: u32| {
        format!(
            "{{\"timestamp\":\"2026-08-25T01:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{sid}\",\"cwd\":\"/Users/x/codexproj\"}}}}\n\
             {{\"type\":\"turn_context\",\"payload\":{{\"model\":\"{model}\"}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"total_tokens\":{tokens}}},\"model_context_window\":272000}}}}}}\n"
        )
    };
    let old = write(
        d.path(),
        "2026/08/24/old.jsonl",
        &head("sid-old", "gpt-old", 30400),
    );
    let new = write(
        d.path(),
        "2026/08/25/new.jsonl",
        &head("sid-new", "gpt-new", 60000),
    );
    // Said rather than assumed. Which of the two is newer decides half of what
    // this test asserts, and two files written a microsecond apart are the same
    // age on any filesystem that stamps in seconds.
    set_mtime(&old, 1_700_000_000);
    set_mtime(&new, 1_700_000_060);

    // The folder alone: the newest wins, which is what every row used to get.
    assert_eq!(
        codex_latest(d.path(), "/Users/x/codexproj").model,
        "gpt-new"
    );

    // The recorded ID: each row gets its own session.
    assert_eq!(
        codex_for_session(d.path(), "sid-old", "/Users/x/codexproj").model,
        "gpt-old"
    );
    assert_eq!(
        codex_for_session(d.path(), "sid-new", "/Users/x/codexproj").model,
        "gpt-new"
    );

    // No ID, or one no rollout claims: the folder is still the fallback.
    assert_eq!(
        codex_for_session(d.path(), "", "/Users/x/codexproj").model,
        "gpt-new"
    );
    assert_eq!(
        codex_for_session(d.path(), "sid-gone", "/Users/x/codexproj").model,
        "gpt-new"
    );
}

#[test]
fn codex_ignores_rollouts_from_other_folders() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "2026/08/25/s1.jsonl", CODEX_ROLLOUT);
    assert!(codex_latest(d.path(), "/Users/x/elsewhere").is_empty());
    assert_eq!(codex_session_id(d.path(), "/Users/x/elsewhere"), None);
}

/// The most recent session in the folder is the one being watched; an older
/// rollout for the same folder must not answer for it.
#[test]
fn codex_takes_the_most_recent_rollout_for_the_folder() {
    let d = tempfile::tempdir().unwrap();
    let old = write(
        d.path(),
        "2026/08/24/old.jsonl",
        &CODEX_ROLLOUT.replace("gpt-5.6-sol", "gpt-5.4-mini"),
    );
    let new = write(d.path(), "2026/08/25/new.jsonl", CODEX_ROLLOUT);
    set_mtime(&old, 1_000_000);
    set_mtime(&new, 2_000_000);

    assert_eq!(
        codex_latest(d.path(), "/Users/x/codexproj").model,
        "gpt-5.6-sol"
    );
}

/// codex's notify hook carries no session ID, so resume has nowhere else to
/// learn one.
#[test]
fn codex_session_id_comes_from_the_rollout_header() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "2026/08/26/s1.jsonl",
        r#"{"timestamp":"2026-08-26T00:00:00Z","type":"session_meta","payload":{"session_id":"01a03b74-0823-7450","id":"01a03b74-0823-7450","cwd":"/Users/x/codexproj"}}
"#,
    );
    assert_eq!(
        codex_session_id(d.path(), "/Users/x/codexproj").as_deref(),
        Some("01a03b74-0823-7450")
    );
}

const GEMINI_CHAT: &str = r#"{"sessionId":"aaaa","kind":"main"}
{"type":"user","content":"hi"}
{"type":"gemini","model":"gemini-3-flash-preview","tokens":{"input":100,"output":10,"total":110}}
{"type":"gemini","model":"gemini-3-pro","tokens":{"input":200,"output":20,"total":220}}
"#;

/// Two stock Gemini sessions in one folder are two sessions. The newest chat log
/// was handed to both rows, so the older agent wore the newer one's model.
#[test]
fn gemini_context_follows_the_recorded_session_id() {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir_all(d.path()).unwrap();
    fs::write(
        d.path().join("projects.json"),
        r#"{"projects":{"/Users/x/proj":"projX"}}"#,
    )
    .unwrap();
    let chats = d.path().join("tmp").join("projX").join("chats");
    fs::create_dir_all(&chats).unwrap();
    let log = |sid: &str, model: &str| {
        format!(
            "{{\"sessionId\":\"{sid}\",\"kind\":\"main\"}}\n\
             {{\"type\":\"gemini\",\"model\":\"{model}\",\"tokens\":{{\"total\":110}}}}\n"
        )
    };
    fs::write(
        chats.join("session-old.jsonl"),
        log("sid-old", "gemini-old"),
    )
    .unwrap();
    // Written second, so it is the newest by mtime.
    fs::write(
        chats.join("session-new.jsonl"),
        log("sid-new", "gemini-new"),
    )
    .unwrap();

    assert_eq!(
        gemini_latest(d.path(), "/Users/x/proj").model,
        "gemini-new",
        "the folder alone answers with the newest"
    );
    assert_eq!(
        gemini_for_session(d.path(), "sid-old", "/Users/x/proj").model,
        "gemini-old"
    );
    assert_eq!(
        gemini_for_session(d.path(), "sid-new", "/Users/x/proj").model,
        "gemini-new"
    );
    // No ID, or one no log claims: the folder is still the fallback.
    for unknown in ["", "sid-gone"] {
        assert_eq!(
            gemini_for_session(d.path(), unknown, "/Users/x/proj").model,
            "gemini-new",
            "{unknown:?}"
        );
    }
}

/// The session file never records the window size, so the share is derived from
/// a constant. Losing the `~` would present that assumption as a measurement.
#[test]
fn gemini_shares_are_marked_approximate() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "projects.json",
        r#"{"projects":{"/w/proj":"proj","/w":"w"}}"#,
    );
    write(
        d.path(),
        "tmp/proj/chats/session-2026-08-01T00-00-aaaa.jsonl",
        GEMINI_CHAT,
    );

    let info = gemini_latest(d.path(), "/w/proj");
    assert_eq!(
        info.model, "gemini-3-pro",
        "the last turn's model, not the first"
    );
    assert!(info.ts.is_some(), "the file's mtime dates the reading");
    let pct = info.used_pct.expect("the last turn carried a token total");
    assert!(pct.is_approx(), "no recorded window means no exact answer");
    // 220 tokens against an assumed 1M window.
    assert!((pct.value() - 0.022).abs() < 0.001, "got {}", pct.value());
}

/// A bot that cd's into a subfolder must not fall off its own session.
#[cfg(unix)]
#[test]
fn gemini_falls_back_to_the_longest_mapped_ancestor() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "projects.json",
        r#"{"projects":{"/w/proj":"proj","/w":"w"}}"#,
    );
    write(d.path(), "tmp/proj/chats/session-a.jsonl", GEMINI_CHAT);
    write(
        d.path(),
        "tmp/w/chats/session-a.jsonl",
        &GEMINI_CHAT.replace("gemini-3-pro", "wrong-project"),
    );

    assert_eq!(
        gemini_latest(d.path(), "/w/proj/sub/dir").model,
        "gemini-3-pro"
    );
}

#[test]
fn gemini_with_nothing_on_disk_is_empty() {
    let d = tempfile::tempdir().unwrap();
    assert!(gemini_latest(d.path(), "/w/proj").is_empty());
    assert!(agy_ctx(d.path(), "no-such-conversation").is_empty());
    assert!(
        agy_ctx(d.path(), "").is_empty(),
        "no conversation ID, nothing to look up"
    );
}

/// The transcript does not contain the system prompt, skills or tool
/// definitions, but the model request does. Without the measured baseline a
/// freshly started agy session reads as 0% when it is already a tenth full.
#[test]
fn agy_estimate_includes_the_invisible_baseline() {
    let d = tempfile::tempdir().unwrap();
    let transcript = "a".repeat(40 * 1024);
    write(
        d.path(),
        "antigravity-cli/brain/cid-1/.system_generated/logs/transcript_full.jsonl",
        &transcript,
    );

    let info = agy_ctx(d.path(), "cid-1");
    let pct = info.used_pct.expect("a transcript was present");
    assert!(
        pct.is_approx(),
        "bytes-to-tokens is a guess and has to say so"
    );
    // (100_000 + 40_960) / 4 bytes-per-token / 1M window.
    assert!((pct.value() - 3.524).abs() < 0.01, "got {}", pct.value());
    let without_baseline = 40_960.0 / AGY_BYTES_PER_TOKEN / GEMINI_WINDOW * 100.0;
    assert!(
        pct.value() > without_baseline * 3.0,
        "the baseline dominates a short session"
    );
    assert!(
        info.ts.is_some(),
        "the transcript's mtime dates the reading"
    );
}

/// The stock CLI's free tier is closed (IneligibleTierError, measured 2026-08),
/// so wherever agy is installed it is the CLI actually being run.
#[test]
fn gemini_command_prefers_agy_where_it_is_installed() {
    let d = tempfile::tempdir().unwrap();
    let absent = |_: &str| false;
    let on_path = |_: &str| true;
    assert_eq!(
        gemini_command_with(d.path(), &absent),
        "gemini",
        "no trace of agy: stock CLI"
    );
    // Any one of the three traces is enough. A freshly installed agy has written
    // no folders yet, and choosing stock `gemini` for it starts a CLI that
    // refuses to run (IneligibleTierError).
    assert_eq!(gemini_command_with(d.path(), &on_path), "agy", "on PATH");
    fs::create_dir_all(d.path().join("config")).unwrap();
    assert_eq!(gemini_command_with(d.path(), &absent), "agy", "config/");
    fs::create_dir_all(d.path().join("antigravity-cli")).unwrap();
    assert_eq!(
        gemini_command_with(d.path(), &absent),
        "agy",
        "antigravity-cli/"
    );
}

/// agy writes no model to disk, so the row would be blank without the hook's.
#[test]
fn a_gemini_row_falls_back_to_the_model_the_hook_recorded() {
    let d = tempfile::tempdir().unwrap();
    let transcript = "a".repeat(4096);
    write(
        d.path(),
        "antigravity-cli/brain/cid-1/.system_generated/logs/transcript.jsonl",
        &transcript,
    );

    let mut a = agent("gemini-1", "gemini", "/w");
    a.session_id = "cid-1".into();
    a.model = "Gemini 3.6 Flash (Medium)".into();

    let empty = tempfile::tempdir().unwrap();
    let out = agent_ctx(&[a], empty.path(), d.path(), empty.path());
    let info = &out["gemini-1"];
    assert_eq!(info.model, "Gemini 3.6 Flash (Medium)");
    assert!(info.used_pct.is_some_and(|p| p.is_approx()));
}

/// An agent with no working directory has no folder to look anything up by.
#[test]
fn agents_without_a_folder_are_skipped() {
    let a = Agent::new("claude-1", "claude", at(0));
    let empty = tempfile::tempdir().unwrap();
    assert!(agent_ctx(&[a], empty.path(), empty.path(), empty.path()).is_empty());
}

// ── codex rollouts, read on a clock ──────────────────────────────────────

/// A rollout's opening: a `session_meta` header and a first model.
fn rollout_head(sid: &str, model: &str) -> String {
    format!(
        "{{\"timestamp\":\"2026-09-20T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{sid}\",\"cwd\":\"/Users/x/codexproj\"}}}}\n\
         {{\"type\":\"turn_context\",\"payload\":{{\"model\":\"{model}\"}}}}\n"
    )
}

/// Lines with no model in them, `bytes` long in all — enough of them push the
/// model out of the tail the lookup reads first.
fn filler(bytes: usize) -> String {
    let line = "{\"type\":\"response_item\",\"payload\":{\"text\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}}\n";
    line.repeat(bytes / line.len() + 1)
}

fn append(path: &Path, body: &str) {
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
}

fn cached_model(root: &Path, sid: &str, cache: &mut CtxCache) -> String {
    codex_for_session_in(&Rollouts::scan(root), sid, "/Users/x/codexproj", cache).model
}

/// A model written early in a long session has scrolled out of the tail; it is
/// still the session's model, found by scanning what lies before the tail.
#[test]
fn a_model_that_scrolled_out_of_the_tail_is_still_found() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        &(rollout_head("sid-a", "gpt-early") + &filler(300 * 1024)),
    );
    assert_eq!(
        codex_for_session(d.path(), "sid-a", "/Users/x/codexproj").model,
        "gpt-early"
    );
}

/// 🚨 An unchanged rollout is not read again. Rewritten under the same length
/// and mtime, the cache still answers with what it read — which is how this
/// test can tell that it did not read the file a second time.
#[test]
fn an_unchanged_rollout_is_answered_from_the_cache() {
    let d = tempfile::tempdir().unwrap();
    let path = write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        &rollout_head("sid-a", "gpt-aaaa"),
    );
    set_mtime(&path, 1_800_000_000);
    let mut cache = CtxCache::default();
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-aaaa");

    fs::write(&path, rollout_head("sid-a", "gpt-bbbb")).unwrap();
    set_mtime(&path, 1_800_000_000);
    assert_eq!(
        cached_model(d.path(), "sid-a", &mut cache),
        "gpt-aaaa",
        "same length, same mtime: not read again"
    );

    set_mtime(&path, 1_800_000_060);
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-bbbb");
}

/// A rollout that grew is read again, and a model it switched to is found
/// wherever it landed: in the tail, or before it once more has been written.
#[test]
fn a_grown_rollout_answers_with_its_latest_model() {
    let d = tempfile::tempdir().unwrap();
    let path = write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        &(rollout_head("sid-a", "gpt-first") + &filler(300 * 1024)),
    );
    let mut cache = CtxCache::default();
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-first");

    // Grew with nothing new to say: the model before the tail still stands.
    append(&path, &filler(200 * 1024));
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-first");

    // A switch that is still in the tail.
    append(
        &path,
        "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-second\"}}\n",
    );
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-second");

    // A switch that more than a tail of output has already buried.
    append(
        &path,
        &("{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-third\"}}\n".to_string()
            + &filler(300 * 1024)),
    );
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-third");
}

/// A rollout replaced by a shorter one is a different file: nothing
/// remembered about the old one carries over.
#[test]
fn a_rollout_that_shrank_is_read_from_the_start() {
    let d = tempfile::tempdir().unwrap();
    let path = write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        &(rollout_head("sid-a", "gpt-long") + &filler(300 * 1024)),
    );
    let mut cache = CtxCache::default();
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-long");

    fs::write(
        &path,
        rollout_head("sid-a", "gpt-short") + &filler(200 * 1024),
    )
    .unwrap();
    assert_eq!(cached_model(d.path(), "sid-a", &mut cache), "gpt-short");
}

/// codex names each rollout after its session, so the session's own file is
/// found by name — even one whose header spells the ID some other way, which
/// the header scan would miss and answer for with the folder's newest session.
#[test]
fn a_session_is_found_by_its_rollouts_name() {
    let d = tempfile::tempdir().unwrap();
    let own = write(
        d.path(),
        "2026/09/19/rollout-2026-09-19T10-00-00-01a0bc20-17aa-7fc1-9f2d-bd0033fcd441.jsonl",
        &rollout_head("", "gpt-own").replace("\"session_id\":\"\",", ""),
    );
    let newer = write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T10-00-00-01a0bc99-0000-7000-8000-000000000000.jsonl",
        &rollout_head("01a0bc99-0000-7000-8000-000000000000", "gpt-newer"),
    );
    set_mtime(&own, 1_800_000_000);
    set_mtime(&newer, 1_800_000_600);

    assert_eq!(
        codex_for_session(
            d.path(),
            "01a0bc20-17aa-7fc1-9f2d-bd0033fcd441",
            "/Users/x/codexproj"
        )
        .model,
        "gpt-own"
    );
}

/// One cache and one walk serve every codex agent in a pass, each still getting
/// its own session.
#[test]
fn agent_ctx_cached_gives_each_codex_agent_its_own_session() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        &rollout_head("sid-a", "gpt-a"),
    );
    write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-01-sid-b.jsonl",
        &rollout_head("sid-b", "gpt-b"),
    );
    let mut a = agent("codex-a", "codex", "/Users/x/codexproj");
    a.session_id = "sid-a".into();
    let mut b = agent("codex-b", "codex", "/Users/x/codexproj");
    b.session_id = "sid-b".into();
    let empty = tempfile::tempdir().unwrap();
    let mut cache = CtxCache::default();
    for _ in 0..2 {
        let out = agent_ctx_cached(
            &[a.clone(), b.clone()],
            d.path(),
            empty.path(),
            empty.path(),
            &mut cache,
        );
        assert_eq!(out["codex-a"].model, "gpt-a");
        assert_eq!(out["codex-b"].model, "gpt-b");
    }
}

/// 🚨 A model key that the tail begins in the middle of: the tail sees its end,
/// the scan before it its start, and neither saw it whole — the row showed no
/// model, and kept showing it, since an unchanged file is not read again.
#[test]
fn a_model_on_the_tails_edge_is_found() {
    const TAIL: usize = 128 * 1024;
    let key = "\"model\":\"gpt-straddle\"";
    for inside in [1, 5, key.len() - 1] {
        let d = tempfile::tempdir().unwrap();
        // The key starts `inside` bytes before where the tail begins.
        let rest_len = TAIL + inside - key.len();
        let rest = format!("}}}}\n{}\n", "x".repeat(rest_len - 4));
        let body = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"sid-a\",\"cwd\":\"/Users/x/codexproj\"}}}}\n{}\
             {{\"type\":\"turn_context\",\"payload\":{{{key}{rest}",
            filler(200 * 1024)
        );
        write(
            d.path(),
            "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
            &body,
        );
        assert_eq!(
            codex_for_session(d.path(), "sid-a", "/Users/x/codexproj").model,
            "gpt-straddle",
            "the key starts {inside} bytes before the tail"
        );
    }
}

/// The cache holds what the last pass looked at, not every rollout the tower
/// ever resolved.
#[test]
fn a_pass_forgets_the_rollouts_it_did_not_look_at() {
    let d = tempfile::tempdir().unwrap();
    for sid in ["sid-a", "sid-b"] {
        write(
            d.path(),
            &format!("2026/09/20/rollout-2026-09-20T00-00-00-{sid}.jsonl"),
            &rollout_head(sid, "gpt"),
        );
    }
    let codex_agent = |sid: &str| {
        let mut a = agent(&format!("codex-{sid}"), "codex", "/Users/x/codexproj");
        a.session_id = sid.into();
        a
    };
    let empty = tempfile::tempdir().unwrap();
    let mut cache = CtxCache::default();
    let run = |agents: &[Agent], cache: &mut CtxCache| {
        agent_ctx_cached(agents, d.path(), empty.path(), empty.path(), cache);
    };
    run(&[codex_agent("sid-a"), codex_agent("sid-b")], &mut cache);
    assert_eq!(cache.len(), 2);
    run(&[codex_agent("sid-a")], &mut cache);
    assert_eq!(cache.len(), 1, "sid-b's session ended");
}

/// An ID recorded short, or one that happens to end another's, must not claim
/// that session's rollout by the end of its name.
#[test]
fn a_short_id_does_not_claim_another_sessions_rollout() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-01a0bc20-17aa-7fc1-9f2d-bd0033fcd441.jsonl",
        &rollout_head("01a0bc20-17aa-7fc1-9f2d-bd0033fcd441", "gpt-other"),
    );
    assert!(codex_for_session(d.path(), "bd0033fcd441", "/Users/x/elsewhere").is_empty());
}

/// An agent found by its folder — no session ID — used to have every newer
/// rollout's header opened and read on every pass. A header does not change
/// once written, so each is read once: rewritten under the cache, the folder
/// still matches the header it first read.
#[test]
fn a_rollouts_header_is_read_once() {
    let d = tempfile::tempdir().unwrap();
    // A real header runs to kilobytes — codex writes its instructions into
    // it — and only a head read to the full 4 KiB is kept.
    let long_head = |sid: &str, model: &str, cwd: &str| {
        rollout_head(sid, model)
            .replace("/Users/x/codexproj", cwd)
            .replacen(
                "}}\n",
                &format!(",\"instructions\":\"{}\"}}}}\n", "x".repeat(5000)),
                1,
            )
    };
    let path = write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        &long_head("sid-a", "gpt-a", "/Users/x/codexproj"),
    );
    let mut cache = CtxCache::default();
    let by_folder = |cache: &mut CtxCache| {
        cache.begin_pass();
        let info = codex_for_session_in(&Rollouts::scan(d.path()), "", "/Users/x/codexproj", cache);
        cache.end_pass();
        info.model
    };
    assert_eq!(by_folder(&mut cache), "gpt-a");
    fs::write(
        &path,
        // A different length too, so the body is seen to change even where
        // the clock stamps in whole seconds.
        long_head("sid-a", "gpt-bbbb", "/Users/x/elsewhere"),
    )
    .unwrap();
    assert_eq!(
        by_folder(&mut cache),
        "gpt-bbbb",
        "the header was not read again; the body, which changed, was"
    );
}

/// A header caught while codex is still writing it is not remembered: the
/// folder it names may not be in it yet.
#[test]
fn a_header_caught_half_written_is_read_again() {
    let d = tempfile::tempdir().unwrap();
    let full = rollout_head("sid-a", "gpt-a");
    let cut = full.find("/Users/x/codexproj").unwrap() + 5;
    let path = write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        &full[..cut],
    );
    let mut cache = CtxCache::default();
    let rollouts = Rollouts::scan(d.path());
    assert!(codex_for_session_in(&rollouts, "", "/Users/x/codexproj", &mut cache).is_empty());
    fs::write(&path, &full).unwrap();
    assert_eq!(
        codex_for_session_in(&rollouts, "", "/Users/x/codexproj", &mut cache).model,
        "gpt-a"
    );
}

// ── g4testadd: the incremental read, cross-checked against a fresh one ──

/// Cross validation and property at once: however a rollout grows — model
/// switches in the tail, buried behind more than a tail of output, token
/// counts or none, growth of a few bytes or of several chunks — reading it
/// through a cache kept since the start answers exactly what reading it from
/// scratch answers. The cache is an optimisation; it must never change a
/// result.
#[test]
fn the_cached_read_always_matches_a_fresh_one() {
    let mut rng = tower_testrng::Rng::new(0xC0DE_CAFE);
    let mut next = move |n: u64| rng.below(n as usize) as u64;
    for round in 0..6 {
        let d = tempfile::tempdir().unwrap();
        let path = write(
            d.path(),
            "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
            &rollout_head("sid-a", "gpt-start"),
        );
        let mut cache = CtxCache::default();
        for step in 0..25 {
            let switch = format!(
                "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-{round}-{step}\"}}}}\n"
            );
            let grow = match next(6) {
                0 => switch,
                // A switch buried behind more than a tail of output before the
                // next read — the case the growth scan is there for.
                5 => switch + &filler(130 * 1024 + next(70 * 1024) as usize),
                1 => format!(
                    "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"total_tokens\":{}}},\"model_context_window\":272000}}}}}}\n",
                    20_000 + next(200_000)
                ),
                2 => filler(next(200 * 1024) as usize),
                3 => filler(next(2048) as usize),
                _ => String::new(),
            };
            append(&path, &grow);
            let cached = codex_for_session_in(
                &Rollouts::scan(d.path()),
                "sid-a",
                "/Users/x/codexproj",
                &mut cache,
            );
            let fresh = codex_for_session(d.path(), "sid-a", "/Users/x/codexproj");
            assert_eq!(cached.model, fresh.model, "round {round} step {step}");
            assert_eq!(cached.used_pct, fresh.used_pct, "round {round} step {step}");
        }
    }
}

/// Edge cases for a rollout's content: only a header, a model with an empty
/// value (not a model), and a file that is empty.
#[test]
fn a_rollout_with_nothing_to_say_says_nothing() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-00-sid-a.jsonl",
        "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"sid-a\",\"cwd\":\"/w\"}}\n",
    );
    write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-01-sid-b.jsonl",
        "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"sid-b\",\"cwd\":\"/w\"}}\n{\"payload\":{\"model\":\"\"}}\n",
    );
    write(
        d.path(),
        "2026/09/20/rollout-2026-09-20T00-00-02-sid-c.jsonl",
        "",
    );
    for sid in ["sid-a", "sid-b", "sid-c"] {
        let info = codex_for_session(d.path(), sid, "/nowhere");
        assert!(info.model.is_empty(), "{sid}: {:?}", info.model);
        assert_eq!(info.used_pct, None, "{sid}");
    }
}

// ── g4testadd: the pass cache at the agent level, and the emptiness gate ──

/// Property and cross validation at once, one level up from
/// [`the_cached_read_always_matches_a_fresh_one`]: however the set of agents
/// changes from pass to pass — rows appearing, rows dropping out and coming
/// back, a session switching model underneath — a [`CtxCache`] kept since
/// the start answers each pass exactly what the cacheless [`agent_ctx`]
/// answers for that same list. A pass also forgets the rollouts it did not
/// look at, so a row that returns has to be read again rather than answered
/// from something the cache happened to keep.
#[test]
fn a_kept_cache_answers_every_pass_what_a_fresh_read_would() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let sids = ["sid-a", "sid-b", "sid-c"];
    let paths: Vec<_> = sids
        .iter()
        .enumerate()
        .map(|(i, sid)| {
            write(
                d.path(),
                &format!("2026/09/20/rollout-2026-09-20T00-00-0{i}-{sid}.jsonl"),
                &rollout_head(sid, &format!("gpt-{sid}")),
            )
        })
        .collect();
    let agents: Vec<Agent> = sids
        .iter()
        .map(|sid| {
            let mut a = agent(&format!("codex-{sid}"), "codex", "/Users/x/codexproj");
            a.session_id = (*sid).into();
            a
        })
        .collect();

    // xorshift64*, as in `tower_discord`'s properties: the output multiply is
    // the crate convention, and the append below is where the randomness has to
    // land — which pass writes to which rollout is the thing no fixed schedule
    // should decide.
    let mut rng = tower_testrng::Rng::new(0x5EED_1234);
    let mut next = move |n: u64| rng.below(n as usize) as u64;
    let mut cache = CtxCache::default();
    for step in 0..40 {
        // A subset of the three as a bitmask, so rows fall out of a pass and
        // return in a later one. Walked rather than drawn: there are only eight
        // subsets and forty passes, and forty draws out of eight buckets leave
        // one of them visited once or not at all — the empty set took eleven of
        // the forty and `0b110` took one. Walking gives each subset five passes,
        // the empty one included, which is the case that must not leave anything
        // in the cache behind.
        let pick = step as u64 % 8;
        let subset: Vec<Agent> = agents
            .iter()
            .enumerate()
            .filter(|(i, _)| pick & (1 << i) != 0)
            .map(|(_, a)| a.clone())
            .collect();
        if next(3) == 0 {
            let i = next(3) as usize;
            append(
                &paths[i],
                &format!(
                    "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-{step}\"}}}}\n"
                ),
            );
        }

        let cached = agent_ctx_cached(&subset, d.path(), empty.path(), empty.path(), &mut cache);
        let fresh = agent_ctx(&subset, d.path(), empty.path(), empty.path());
        assert_eq!(cached, fresh, "step {step}, pick {pick:b}");
        assert_eq!(
            cached.len(),
            subset.len(),
            "step {step}: every row answered"
        );
        assert!(
            cache.len() <= subset.len(),
            "step {step}: {} rollouts remembered for {} agents",
            cache.len(),
            subset.len()
        );
    }
}

/// A gemini row is stamped whatever happens — `updated_at` stands in when no
/// source spoke at all — so [`CtxInfo::is_empty`] has to be about what a row
/// can *display*. A timestamp is neither a model nor a share, and a row
/// carrying nothing else must be left out of the map rather than drawn blank.
#[test]
fn a_row_with_nothing_but_a_timestamp_is_left_off() {
    let empty = tempfile::tempdir().unwrap();
    let a = agent("gemini-1", "gemini", "/w/proj");
    assert!(a.model.is_empty(), "the fixture records no model either");
    assert!(agent_ctx(&[a], empty.path(), empty.path(), empty.path()).is_empty());

    assert!(CtxInfo {
        ts: Some(at(0)),
        ..CtxInfo::default()
    }
    .is_empty());
    // A zero share is a reading, not an absence: a session that has just
    // opened is 0% full, and the row says so.
    assert!(!CtxInfo {
        used_pct: Some(UsedPct::Exact(0.0)),
        ..CtxInfo::default()
    }
    .is_empty());
    assert!(!CtxInfo {
        model: "gemini-3-pro".into(),
        ..CtxInfo::default()
    }
    .is_empty());
}

/// Edge, property and cross validation for the share itself.
///
/// An empty window and a full one have to read as `0%` and `100%` — the two
/// figures a person acts on — an estimate never climbs past full however large
/// the number behind it, a share never falls as the tokens grow, and the label
/// agrees with a percentage worked out by hand without the type.
#[test]
fn a_share_reads_as_a_plain_truncated_percentage_and_never_passes_full() {
    assert_eq!(UsedPct::Exact(0.0).label(), "0%");
    assert_eq!(UsedPct::Exact(100.0).label(), "100%");
    assert_eq!(UsedPct::approx(0.0).label(), "~0%");
    // Exactly at the cap, and a hair over it: both are "full", not "101%".
    assert_eq!(UsedPct::approx(100.0).label(), "~100%");
    assert_eq!(UsedPct::approx(100.000_1).label(), "~100%");

    let mut rng = tower_testrng::Rng::new(0xBEEF_0042);
    let mut next = move |n: u64| rng.below(n as usize) as u64;
    let window = 272_000.0_f64;
    let mut tokens = 0_u64;
    let mut last = 0.0_f64;
    for step in 0..400 {
        tokens += next(9_000);
        let raw = tokens as f64 * 100.0 / window;
        let approx = UsedPct::approx(raw);

        // The same figure, worked out without the type.
        let naive = format!("~{}%", raw.min(100.0).floor() as i64);
        assert_eq!(approx.label(), naive, "step {step}, raw {raw}");
        assert!(
            (0.0..=100.0).contains(&approx.value()),
            "step {step}: {} is outside a window",
            approx.value()
        );
        assert!(
            approx.value() >= last,
            "step {step}: {} after {last}, on more tokens",
            approx.value()
        );
        last = approx.value();

        // A measurement is never capped and never wears the marker — that is
        // the whole distinction the two variants exist to keep.
        assert_eq!(UsedPct::Exact(raw).value(), raw, "step {step}");
        assert_eq!(UsedPct::Exact(raw).marker(), "");
        assert_eq!(approx.marker(), "~");
    }
    assert!(
        last == 100.0,
        "the loop has to reach a full window, got {last}"
    );
}

/// 🚨 A second pass does not read the headers again, even for a rollout too
/// short to fill the head budget — which is most of them.
///
/// The folder fallback (a codex agent with no session ID of its own, the
/// notify-only path) opens every rollout newer than its match and reads the
/// opening bytes of each. Only *full* heads were kept, so a session of a few
/// messages was re-opened and re-read on every pass, for as long as the row was
/// on screen.
///
/// Read again is observed by rewriting the file with a different folder while
/// keeping its length and mtime: nothing else can tell a cached answer from a
/// fresh one, and a `chmod` game would prove the file was unreadable rather than
/// unread.
#[test]
fn a_second_pass_does_not_read_a_short_rollout_head_again() {
    let d = tempfile::tempdir().unwrap();
    let path = write(
        d.path(),
        "2026/09/20/s-short.jsonl",
        &rollout_head("sid-x", "gpt-5"),
    );
    assert!(
        fs::metadata(&path).unwrap().len() < 4096,
        "the point of this test is a head that does not fill the budget"
    );
    set_mtime(&path, 1_700_000_000);

    let mut cache = CtxCache::default();
    cache.begin_pass();
    // No ID, so the folder is what the walk matches on.
    assert_eq!(cached_model(d.path(), "", &mut cache), "gpt-5");
    cache.end_pass();

    // The same bytes count, a different folder, and the clock put back.
    let moved = fs::read_to_string(&path)
        .unwrap()
        .replace("/Users/x/codexproj", "/Users/x/codexpr0j");
    assert_eq!(moved.len(), fs::read_to_string(&path).unwrap().len());
    fs::write(&path, &moved).unwrap();
    set_mtime(&path, 1_700_000_000);

    cache.begin_pass();
    assert_eq!(
        cached_model(d.path(), "", &mut cache),
        "gpt-5",
        "the head was read again: the answer came from the file, not the cache"
    );
    cache.end_pass();

    // And a file that *has* moved is read again, because its next lines may be
    // the ones that name the folder.
    set_mtime(&path, 1_700_000_900);
    cache.begin_pass();
    assert!(
        cached_model(d.path(), "", &mut cache).is_empty(),
        "a rollout whose head changed must be read again"
    );
    cache.end_pass();
}

fn claude_turn(model: &str, input: u64, cache_read: u64) -> String {
    format!(
        r#"{{"type":"assistant","isSidechain":false,"message":{{"model":"{model}","usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":{cache_read},"output_tokens":10}}}}}}"#
    )
}

fn claude_agent(id: &str, sid: &str, cwd: &str) -> Agent {
    let mut a = agent(id, "claude", cwd);
    a.session_id = sid.into();
    a
}

/// A claude row reads its own transcript: the model of the last turn and the
/// prompt it sent, against the assumed 200K window — so marked `~`.
#[test]
fn a_claude_row_reads_its_transcript() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let body = format!(
        "{}\n{}\n",
        claude_turn("claude-sonnet-5", 10, 1000),
        claude_turn("claude-opus-5-5", 2, 49_998)
    );
    write(d.path(), "projects/-w-my-proj/s1.jsonl", &body);

    let a = claude_agent("claude-1", "s1", "/w/my_proj");
    let out = agent_ctx(&[a], empty.path(), empty.path(), d.path());
    assert_eq!(out["claude-1"].model, "Opus 5.5");
    assert_eq!(out["claude-1"].used_pct, Some(UsedPct::Approx(25.0)));
    assert!(out["claude-1"].ts.is_some());
}

/// Two claude sessions in one folder each keep their own transcript.
#[test]
fn two_claude_sessions_in_one_folder_keep_their_own_models() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "projects/-w/a.jsonl",
        &claude_turn("claude-opus-5-5", 1, 0),
    );
    write(
        d.path(),
        "projects/-w/b.jsonl",
        &claude_turn("claude-sonnet-5", 1, 0),
    );

    let out = agent_ctx(
        &[
            claude_agent("c-a", "a", "/w"),
            claude_agent("c-b", "b", "/w"),
        ],
        empty.path(),
        empty.path(),
        d.path(),
    );
    assert_eq!(out["c-a"].model, "Opus 5.5");
    assert_eq!(out["c-b"].model, "Sonnet 5");
}

/// The transcript stays filed where the session started; an agent that has
/// since moved is still found by its session ID.
#[test]
fn a_claude_transcript_is_found_after_the_agent_moved() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "projects/-started-here/s1.jsonl",
        &claude_turn("claude-opus-5-5", 1, 0),
    );

    let a = claude_agent("claude-1", "s1", "/somewhere/else");
    let out = agent_ctx(&[a], empty.path(), empty.path(), d.path());
    assert_eq!(out["claude-1"].model, "Opus 5.5");
}

/// A pinned `[1m]` default of the same family puts the session on the 1M
/// window, and the model says so for the `(1M)` label.
#[test]
fn a_pinned_1m_default_sizes_the_claude_window() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    write(d.path(), "settings.json", r#"{"model":"opus[1m]"}"#);
    write(
        d.path(),
        "projects/-w/s1.jsonl",
        &claude_turn("claude-opus-5-5", 0, 100_000),
    );

    let out = agent_ctx(
        &[claude_agent("c", "s1", "/w")],
        empty.path(),
        empty.path(),
        d.path(),
    );
    assert_eq!(out["c"].model, "Opus 5.5 (1M)");
    assert_eq!(out["c"].used_pct, Some(UsedPct::Approx(10.0)));
}

/// A context past 200K can only be on the 1M window, pinned or not — a share
/// over 100% would be reporting the wrong window.
#[test]
fn a_claude_context_past_200k_is_on_the_1m_window() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "projects/-w/s1.jsonl",
        &claude_turn("claude-sonnet-5", 0, 300_000),
    );

    let out = agent_ctx(
        &[claude_agent("c", "s1", "/w")],
        empty.path(),
        empty.path(),
        d.path(),
    );
    assert_eq!(out["c"].model, "Sonnet 5 (1M)");
    assert_eq!(out["c"].used_pct, Some(UsedPct::Approx(30.0)));
}

/// A claude record without a session ID names no transcript, and no other
/// session's file is borrowed for it.
#[test]
fn a_claude_row_without_a_session_id_reads_nothing() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "projects/-w/s1.jsonl",
        &claude_turn("claude-opus-5-5", 1, 0),
    );

    let out = agent_ctx(
        &[claude_agent("c", "", "/w")],
        empty.path(),
        empty.path(),
        d.path(),
    );
    assert!(out.is_empty(), "{out:?}");
}

/// An opencode row carries what its plugin reported through the hook: the
/// model, and a share measured against the window opencode itself knows — so
/// not marked `~`.
#[test]
fn an_opencode_row_reads_what_its_hook_recorded() {
    let empty = tempfile::tempdir().unwrap();
    let mut a = agent("oc", "opencode", "/w");
    a.model = "qwen3.8-flash".into();
    a.ctx_tokens = 32_768;
    a.ctx_window = 131_072;
    let out = agent_ctx(&[a], empty.path(), empty.path(), empty.path());
    assert_eq!(out["oc"].model, "qwen3.8-flash");
    assert_eq!(out["oc"].used_pct, Some(UsedPct::Exact(25.0)));

    let bare = agent("oc2", "opencode", "/w");
    let out = agent_ctx(&[bare], empty.path(), empty.path(), empty.path());
    assert!(out.is_empty(), "nothing reported, nothing shown: {out:?}");
}

/// An unchanged transcript is not read again: same length and mtime, the
/// remembered turn stands — shown here by swapping the bytes underneath it.
/// A transcript that grows is read afresh.
#[test]
fn an_unchanged_claude_transcript_is_answered_from_the_cache() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let path = write(
        d.path(),
        "projects/-w/s1.jsonl",
        &claude_turn("claude-opus-5-5", 1, 0),
    );
    set_mtime(&path, 1_000);
    let a = [claude_agent("c", "s1", "/w")];
    let mut cache = CtxCache::default();
    let pass = |cache: &mut CtxCache| {
        agent_ctx_cached(&a, empty.path(), empty.path(), d.path(), cache)["c"]
            .model
            .clone()
    };
    assert_eq!(pass(&mut cache), "Opus 5.5");

    // Same length, same mtime, different model: the cache is what answers.
    fs::write(&path, claude_turn("claude-opus-5-6", 1, 0)).unwrap();
    set_mtime(&path, 1_000);
    assert_eq!(pass(&mut cache), "Opus 5.5");

    // A new turn appended: read again.
    let grown = format!(
        "{}\n{}",
        claude_turn("claude-opus-5-5", 1, 0),
        claude_turn("claude-sonnet-5", 1, 0)
    );
    fs::write(&path, grown).unwrap();
    assert_eq!(pass(&mut cache), "Sonnet 5");
}

/// A transcript that moved (the session was continued elsewhere) is looked up
/// again rather than answered from a path that is gone.
#[test]
fn a_moved_claude_transcript_is_found_again() {
    let d = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let old = write(
        d.path(),
        "projects/-a/s1.jsonl",
        &claude_turn("claude-opus-5-5", 1, 0),
    );
    let a = [claude_agent("c", "s1", "/b")];
    let mut cache = CtxCache::default();
    let run =
        |cache: &mut CtxCache| agent_ctx_cached(&a, empty.path(), empty.path(), d.path(), cache);
    assert_eq!(run(&mut cache)["c"].model, "Opus 5.5");

    fs::remove_file(&old).unwrap();
    write(
        d.path(),
        "projects/-b/s1.jsonl",
        &claude_turn("claude-sonnet-5", 1, 0),
    );
    assert_eq!(run(&mut cache)["c"].model, "Sonnet 5");
}
