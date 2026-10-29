// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The socket calls that were added for the control tower, against a fake
//! server: subscriptions, sidebar metadata, and layout.
//!
//! The fake is duplicated in `tower_mux`'s tests rather than shared. Exporting a
//! test helper from a production crate to save the duplication would be the
//! worse trade.
//!
//! The fake listens on a Unix socket, so this suite runs where herdr does that.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use herdrx::{
    AgentStatus, Herdr, Metadata, PaneMoveDestination, StartOptions, TabOptions, WorkspaceMetadata,
};
use serde_json::{json, Value};

// --- a fake herdr server ---------------------------------------------------

type Responder = Arc<dyn Fn(&str, &Value) -> String + Send + Sync>;

/// A minimal herdr server: newline-delimited JSON, one request per connection,
/// except `events.subscribe` which stays open so the test can push events.
struct Fake {
    path: PathBuf,
    dir: PathBuf,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    subs: Arc<Mutex<Vec<UnixStream>>>,
}

static SEQ: AtomicU32 = AtomicU32::new(0);

impl Fake {
    fn new(resp: impl Fn(&str, &Value) -> String + Send + Sync + 'static) -> Fake {
        // Not a temp-dir crate: a unix socket path is capped at 104 bytes on
        // macOS, so the name stays short on purpose.
        let dir = std::env::temp_dir().join(format!(
            "hx{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.sock");

        let listener = UnixListener::bind(&path).unwrap();
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let subs: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
        let resp: Responder = Arc::new(resp);

        let (c, s, r) = (calls.clone(), subs.clone(), resp.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let (c, s, r) = (c.clone(), s.clone(), r.clone());
                std::thread::spawn(move || handle(conn, &c, &s, &r));
            }
        });

        Fake {
            path,
            dir,
            calls,
            subs,
        }
    }

    fn herdr(&self) -> Herdr {
        Herdr::at(&self.path)
    }

    fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }

    fn params(&self, method: &str) -> Value {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .unwrap_or(Value::Null)
    }

    /// Pushes one event to every open subscription.
    fn push(&self, event: &str) {
        let line = serde_json::from_str::<Value>(event)
            .expect("fixture")
            .to_string();
        for s in self.subs.lock().unwrap().iter_mut() {
            let _ = s.write_all(format!("{line}\n").as_bytes());
            let _ = s.flush();
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn handle(
    mut conn: UnixStream,
    calls: &Mutex<Vec<(String, Value)>>,
    subs: &Mutex<Vec<UnixStream>>,
    resp: &Responder,
) {
    let mut line = String::new();
    if BufReader::new(conn.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap_or(0)
        == 0
    {
        return;
    }
    let req: Value = serde_json::from_str(&line).unwrap();
    let method = req["method"].as_str().unwrap_or_default().to_string();
    let params = req["params"].clone();
    calls.lock().unwrap().push((method.clone(), params.clone()));

    // Registered before the acknowledgement, so a test that pushes right after
    // subscribe() returns cannot race the registration.
    if method == "events.subscribe" {
        subs.lock().unwrap().push(conn.try_clone().unwrap());
    }

    let body = resp(&method, &params).replace("$ID", req["id"].as_str().unwrap_or(""));
    // Compacted, because the protocol is newline-delimited and the fixtures in
    // this file are written across several lines to stay readable.
    let body = match serde_json::from_str::<Value>(&body) {
        Ok(v) => v.to_string(),
        Err(_) => body,
    };
    let _ = conn.write_all(format!("{body}\n").as_bytes());
    let _ = conn.flush();
}

const OK: &str = r#"{"id":"$ID","result":{"type":"ok"}}"#;
const ACK: &str = r#"{"id":"$ID","result":{"type":"subscription_started"}}"#;
const PANES: &str = r#"{"id":"$ID","result":{"type":"pane_list","panes":[
    {"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":"claude","agent_status":"working","cwd":"/p"}]}}"#;

fn standard(method: &str, _: &Value) -> String {
    match method {
        "events.subscribe" => ACK.into(),
        "pane.list" => PANES.into(),
        _ => OK.into(),
    }
}

// --- subscriptions ---------------------------------------------------------

/// Subscriptions are spelled with dots, the events that arrive with
/// underscores. Subscribing with the event spelling is accepted by nothing, and
/// the watch then silently never fires — which looks exactly like a quiet
/// session.
#[test]
fn subscribe_sends_the_dot_spelling() {
    let f = Fake::new(standard);
    let h = f.herdr();
    let s = h.subscribe(&[herdrx::SUB_PANE_UPDATED]).expect("subscribe");

    let p = f.params("events.subscribe");
    assert_eq!(p["subscriptions"][0]["type"], "pane.updated");
    let _ = s.close();
}

/// Two event shapes, both of which the tower must handle: a whole record, and
/// coordinates alone for a pane that has gone.
#[test]
fn events_carry_a_record_or_bare_coordinates() {
    let f = Fake::new(standard);
    let h = f.herdr();
    let mut s = h.subscribe(&[herdrx::SUB_PANE_UPDATED]).expect("subscribe");

    f.push(
        r#"{"event":"pane_updated","data":{"type":"pane_updated","pane":
        {"pane_id":"w2:p4F","workspace_id":"w2","tab_id":"w2:t7","agent":"claude",
         "agent_status":"blocked","cwd":"/p"}}}"#,
    );
    f.push(r#"{"event":"pane_exited","data":{"type":"pane_exited","pane_id":"w2:pJ","workspace_id":"w2"}}"#);

    let ev = s.next_event().expect("first event");
    assert_eq!(ev.kind, herdrx::EventKind::PaneUpdated);
    let pane = ev.pane.as_ref().expect("record missing");
    assert_eq!(pane.agent_status, AgentStatus::Blocked);
    // The coordinates must come from the record: `data` carries no pane_id for
    // this kind, and a watch that cannot name the pane cannot act on it.
    assert_eq!(ev.pane_id, "w2:p4F");
    assert_eq!(ev.workspace_id, "w2");

    let ev = s.next_event().expect("second event");
    assert_eq!(ev.kind, herdrx::EventKind::PaneExited);
    assert_eq!(ev.pane_id, "w2:pJ");
    assert!(ev.pane.is_none());
    let _ = s.close();
}

/// A reader parked on a quiet subscription has no other way out: nothing will
/// arrive, and without this a control tower could never end a watch.
#[test]
fn a_closer_wakes_a_blocked_reader() {
    let f = Fake::new(standard);
    let h = f.herdr();
    let mut s = h.subscribe(&[herdrx::SUB_PANE_UPDATED]).expect("subscribe");
    let closer = s.closer().expect("closer");

    let reader = std::thread::spawn(move || s.next_event().is_err());
    std::thread::sleep(Duration::from_millis(50)); // let the read actually block
    closer.close().expect("close");

    assert!(
        reader.join().expect("reader panicked"),
        "the blocked read did not end"
    );
}

/// Since herdr 0.9.0 a subscription replays no history, so a snapshot taken
/// first would permanently lose whatever happened before the subscription
/// opened. Reversing these two lines is invisible until an agent's change goes
/// missing.
#[test]
fn watch_panes_subscribes_before_the_snapshot() {
    let f = Fake::new(standard);
    let h = f.herdr();
    let (s, panes) = h.watch_panes().expect("watch");

    assert_eq!(f.methods(), vec!["events.subscribe", "pane.list"]);
    assert_eq!(panes.len(), 1);
    let _ = s.close();
}

#[test]
fn a_subscription_needs_at_least_one_type() {
    let f = Fake::new(standard);
    assert!(f.herdr().subscribe(&[]).is_err());
}

/// An acknowledgement that is not `subscription_started` means the stream will
/// never deliver anything; treating it as success would hang the caller.
#[test]
fn a_wrong_acknowledgement_is_refused() {
    let f = Fake::new(|_, _| r#"{"id":"$ID","result":{"type":"pong"}}"#.into());
    assert!(f.herdr().subscribe(&[herdrx::SUB_PANE_UPDATED]).is_err());
}

// --- sidebar metadata ------------------------------------------------------

/// Everything is planted under this tool's own source name, so tokens herdr's
/// own integrations put on a pane are never disturbed — and clearing ours never
/// wipes theirs.
#[test]
fn metadata_is_planted_under_our_own_source() {
    let f = Fake::new(standard);
    f.herdr()
        .report_metadata(&Metadata {
            pane_id: "w2:p1".into(),
            tokens: [("ctx".to_string(), "ctx 14%".to_string())]
                .into_iter()
                .collect(),
            ttl: Some(Duration::from_secs(900)),
            ..Metadata::default()
        })
        .expect("report");

    let p = f.params("pane.report_metadata");
    assert_eq!(p["source"], "herdr-tower");
    assert_eq!(p["tokens"]["ctx"], "ctx 14%");
    // A TTL is what keeps a dead control tower from stranding stale numbers on
    // the user's sidebar forever.
    assert_eq!(p["ttl_ms"], json!(900_000));
}

/// Independent writers need independent sequence domains. In particular, a
/// launch lease must not race the tower's ordinary context/model decoration.
#[test]
fn metadata_can_select_a_separate_source() {
    let f = Fake::new(standard);
    f.herdr()
        .report_metadata(&Metadata {
            pane_id: "w2:p1".into(),
            source: Some("herdr-tower-dock".into()),
            tokens: [("dock-starting".to_string(), "lease-a".to_string())]
                .into_iter()
                .collect(),
            seq: 19,
            ..Metadata::default()
        })
        .expect("report");

    let p = f.params("pane.report_metadata");
    assert_eq!(p["source"], "herdr-tower-dock");
    assert_eq!(p["seq"], 19);
    assert_eq!(p["tokens"]["dock-starting"], "lease-a");
}

#[test]
fn workspace_metadata_uses_the_server_workspace_gate() {
    let f = Fake::new(standard);
    f.herdr()
        .report_workspace_metadata(&WorkspaceMetadata {
            workspace_id: "w2".into(),
            source: "dock-gate".into(),
            tokens: [("dock-coordinating".into(), "held:4".into())]
                .into_iter()
                .collect(),
            ttl: Some(Duration::from_secs(300)),
            seq: 4,
        })
        .expect("report workspace metadata");

    let p = f.params("workspace.report_metadata");
    assert_eq!(p["workspace_id"], "w2");
    assert_eq!(p["source"], "dock-gate");
    assert_eq!(p["tokens"]["dock-coordinating"], "held:4");
    assert_eq!(p["ttl_ms"], 300_000);
    assert_eq!(p["seq"], 4);
}

/// null deletes a token; an empty string would plant a blank value that
/// occupies a sidebar slot for the rest of the pane's life.
#[test]
fn an_empty_token_value_deletes_the_token() {
    let f = Fake::new(standard);
    f.herdr().clear_tokens("w2:p1", &[]).expect("clear");

    let p = f.params("pane.report_metadata");
    for name in herdrx::ALL_TOKENS {
        assert_eq!(p["tokens"][name], Value::Null, "{name} was not cleared");
    }
    // Clearing must not carry a TTL: there is nothing left to expire.
    assert!(p.get("ttl_ms").is_none());
}

/// herdr rejects the whole request on a bad token name, so one typo would
/// silently drop every other token in the same call. Catching it here names the
/// offender.
#[test]
fn a_bad_token_name_never_reaches_the_wire() {
    let f = Fake::new(standard);
    let err = f
        .herdr()
        .report_metadata(&Metadata {
            pane_id: "w2:p1".into(),
            tokens: [("has space".to_string(), "x".to_string())]
                .into_iter()
                .collect(),
            ..Metadata::default()
        })
        .expect_err("accepted an illegal token name");
    assert!(format!("{err}").contains("has space"));
    assert!(
        f.methods().is_empty(),
        "a malformed request went out anyway"
    );
}

// --- layout and agents -----------------------------------------------------

/// Text and Enter leave in one call. Sending them separately is what made a TUI
/// swallow an Enter arriving too soon after the text, and no send delay needs
/// tuning when they cannot be separated.
#[test]
fn run_sends_text_and_enter_in_one_call() {
    let f = Fake::new(standard);
    f.herdr().run("w2:p1", "ls").expect("run");

    assert_eq!(f.methods(), vec!["pane.send_input"]);
    let p = f.params("pane.send_input");
    assert_eq!(p["text"], "ls");
    assert_eq!(p["keys"], json!(["enter"]));
}

/// The right-click a pane receives is herdr's context menu — *Close pane*
/// included — until a caller routes the button into the pane itself.
#[test]
fn right_click_routing_names_one_pane_and_one_target() {
    let f = Fake::new(standard);
    f.herdr()
        .set_right_click("w2:p1", herdrx::RightClick::Pane)
        .expect("set right click");

    assert_eq!(f.methods(), vec!["pane.input.set"]);
    let p = f.params("pane.input.set");
    assert_eq!(p["pane_id"], "w2:p1");
    assert_eq!(p["right_click"], "pane");

    let back = Fake::new(standard);
    back.herdr()
        .set_right_click("w2:p1", herdrx::RightClick::Herdr)
        .expect("set right click");
    assert_eq!(back.params("pane.input.set")["right_click"], "herdr");
}

/// Process inspection is performed by the herdr server, so clients in another
/// mount namespace still get the host pane's actual process group.
#[test]
fn process_info_uses_the_server_observation() {
    let f = Fake::new(|method, _| match method {
        "pane.process_info" => r#"{"id":"$ID","result":{"process_info":{
            "pane_id":"w2:p1","shell_pid":91,"foreground_process_group_id":92,
            "foreground_processes":[{"pid":92,"argv0":"herdr-tower",
            "argv":["/opt/bin/herdr-tower"],"name":"herdr-tower"}]}}}"#
            .into(),
        _ => standard(method, &Value::Null),
    });

    let info = f.herdr().process_info("w2:p1").expect("process info");
    assert_eq!(info.pane_id, "w2:p1");
    assert_eq!(info.shell_pid, 91);
    assert_eq!(info.foreground_process_group_id, 92);
    assert_eq!(info.foreground_processes[0].pid, 92);
    assert_eq!(info.foreground_processes[0].argv[0], "/opt/bin/herdr-tower");
    assert_eq!(f.params("pane.process_info")["pane_id"], "w2:p1");
}

#[test]
fn tabs_are_listed_with_their_server_labels() {
    let f = Fake::new(|method, _| match method {
        "tab.list" => r#"{"id":"$ID","result":{"tabs":[
            {"tab_id":"w2:t1","workspace_id":"w2","label":"tower","pane_count":1}
        ]}}"#
            .into(),
        _ => standard(method, &Value::Null),
    });

    let tabs = f.herdr().tabs().expect("tabs");
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].tab_id, "w2:t1");
    assert_eq!(tabs[0].label, "tower");
    assert_eq!(f.params("tab.list"), json!({}));
}

/// Pane coordinates are reusable slots, so their suffixes cannot identify a
/// tab's supervising pane. The layout's top-left leaf can, even when the wire
/// array arrives in another order.
#[test]
fn a_session_snapshot_finds_tab_roots_from_layout() {
    let f = Fake::new(|method, _| match method {
        "session.snapshot" => r#"{"id":"$ID","result":{"type":"session_snapshot","snapshot":{
            "panes":[
                {"pane_id":"w2:p5Z","workspace_id":"w2","tab_id":"w2:tP"},
                {"pane_id":"w2:p50","workspace_id":"w2","tab_id":"w2:tP"}
            ],
            "workspaces":[{"workspace_id":"wZ","label":"[wZ]zeta","number":99},
                          {"workspace_id":"w2","label":"[w2]dbstore","number":2},
                          {"workspace_id":"wA","label":"","number":1}],
            "tabs":[{"tab_id":"w2:t7","workspace_id":"w2","label":"pr-2752"},
                    {"tab_id":"w2:t8","workspace_id":"w2","label":""}],
            "layouts":[{"panes":[
                {"pane_id":"w2:p50","rect":{"x":96,"y":0}},
                {"pane_id":"w2:p5Z","rect":{"x":0,"y":0}}
            ]}]
        }}}"#
            .into(),
        _ => standard(method, &Value::Null),
    });

    let snapshot = f.herdr().pane_snapshot().expect("session snapshot");
    assert_eq!(snapshot.panes.len(), 2);
    assert_eq!(snapshot.workspace_order, ["wZ", "w2", "wA"]);
    assert_eq!(snapshot.workspace_display_labels["wZ"], "[wZ]zeta");
    assert_eq!(snapshot.workspace_display_labels["w2"], "[w2]dbstore");
    assert_eq!(snapshot.workspace_display_labels["wA"], "");
    assert_eq!(
        snapshot.workspace_labels.get("w2").map(String::as_str),
        Some("dbstore")
    );
    assert_eq!(
        snapshot.tab_labels.get("w2:t7").map(String::as_str),
        Some("pr-2752")
    );
    assert!(
        !snapshot.tab_labels.contains_key("w2:t8"),
        "no label, no entry"
    );
    assert!(snapshot.tab_roots.contains("w2:p5Z"));
    assert!(!snapshot.tab_roots.contains("w2:p50"));
    assert_eq!(f.params("session.snapshot"), json!({}));
}

/// `tab.list` gives the labels the plain scan path reads: keyed by tab ID,
/// non-Latin labels intact, an unlabeled tab left out rather than mapped to
/// "", and a server that fails the call reports it instead of an empty map.
#[test]
fn tab_labels_are_keyed_by_tab_id_and_leave_out_the_unlabeled() {
    let f = Fake::new(|method, _| match method {
        "tab.list" => r#"{"id":"$ID","result":{"tabs":[
            {"tab_id":"w2:t1","workspace_id":"w2","label":"tower"},
            {"tab_id":"w2:t2","workspace_id":"w2","label":"리뷰 › 🗼"},
            {"tab_id":"w3:t1","workspace_id":"w3","label":""},
            {"tab_id":"w3:t2","workspace_id":"w3"}
        ]}}"#
            .into(),
        _ => standard(method, &Value::Null),
    });
    let labels = f.herdr().tab_labels().expect("tab labels");
    assert_eq!(labels.len(), 2, "{labels:?}");
    assert_eq!(labels.get("w2:t1").map(String::as_str), Some("tower"));
    assert_eq!(labels.get("w2:t2").map(String::as_str), Some("리뷰 › 🗼"));
    assert_eq!(f.params("tab.list"), json!({}));

    let broken = Fake::new(|method, _| match method {
        "tab.list" => r#"{"id":"$ID","error":{"code":"internal","message":"boom"}}"#.into(),
        _ => standard(method, &Value::Null),
    });
    let e = broken.herdr().tab_labels().expect_err("a failed lookup");
    assert!(e.is_code("internal"), "{e}");
}

/// A server whose snapshot has no `tabs` at all — one from before it sent
/// them — still gives a snapshot, with no tab labels in it.
#[test]
fn a_snapshot_without_tabs_has_no_tab_labels() {
    let f = Fake::new(|method, _| match method {
        "session.snapshot" => r#"{"id":"$ID","result":{"type":"session_snapshot","snapshot":{
            "panes":[{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1"}],
            "workspaces":[{"workspace_id":"w2","label":"dbstore"}]
        }}}"#
            .into(),
        _ => standard(method, &Value::Null),
    });
    let snapshot = f.herdr().pane_snapshot().expect("session snapshot");
    assert!(snapshot.tab_labels.is_empty(), "{:?}", snapshot.tab_labels);
    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(
        snapshot.workspace_labels.get("w2").map(String::as_str),
        Some("dbstore")
    );
}

/// Cross validation: the two ways the scan learns tab labels — the snapshot
/// the tower reads and the `tab.list` the CLI reads — agree with each other
/// and with a plain reading of the same tabs, over generated tab sets —
/// including a tab ID listed twice, where the later labeled entry wins and a
/// later empty one does not blank an earlier label. If they did not agree,
/// the tower and a CLI `status` would rewrite each other's records back and
/// forth on every pass.
#[test]
fn the_snapshot_and_tab_list_agree_on_every_tab_label() {
    let pieces = ["", "7", "pr-2752", "리뷰", " ", "›", "🗼", "\\\"q\\\""];
    let mut rng = tower_testrng::Rng::new(24);
    let mut rounds_with_a_duplicate = 0;
    for round in 0..40 {
        // IDs drawn from a small space, so some rounds list one twice.
        let tabs: Vec<(String, String)> = (0..rng.below(6))
            .map(|_| {
                let label: String = (0..rng.below(3))
                    .map(|_| pieces[rng.below(pieces.len())])
                    .collect();
                (format!("w{}:t{}", rng.below(2), rng.below(3)), label)
            })
            .collect();
        let distinct: std::collections::HashSet<_> = tabs.iter().map(|(id, _)| id).collect();
        if distinct.len() < tabs.len() {
            rounds_with_a_duplicate += 1;
        }
        let wire = tabs
            .iter()
            .map(|(id, label)| {
                let ws = id.split(':').next().unwrap();
                format!(r#"{{"tab_id":"{id}","workspace_id":"{ws}","label":"{label}"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        let listed = format!(r#"{{"id":"$ID","result":{{"tabs":[{wire}]}}}}"#);
        let snap = format!(
            r#"{{"id":"$ID","result":{{"type":"session_snapshot","snapshot":{{"tabs":[{wire}]}}}}}}"#
        );
        let f = Fake::new(move |method, _| match method {
            "tab.list" => listed.clone(),
            "session.snapshot" => snap.clone(),
            _ => standard(method, &Value::Null),
        });

        // The plain reading: the wire's JSON escapes undone, empty labels
        // dropped, and a later labeled duplicate ID replacing an earlier one.
        let want: std::collections::HashMap<String, String> = tabs
            .iter()
            .map(|(id, label)| (id.clone(), label.replace("\\\"", "\"")))
            .filter(|(_, label)| !label.is_empty())
            .collect();
        let listed = f.herdr().tab_labels().expect("tab labels");
        let snapshot = f.herdr().pane_snapshot().expect("snapshot").tab_labels;
        assert_eq!(listed, want, "round {round}: tab.list");
        assert_eq!(snapshot, want, "round {round}: session.snapshot");
    }
    assert!(
        rounds_with_a_duplicate >= 5,
        "only {rounds_with_a_duplicate} rounds listed an ID twice"
    );
}

/// Preparing a place for someone else must not yank the user's view.
#[test]
fn new_places_do_not_steal_focus() {
    let f = Fake::new(|method, _| match method {
        "workspace.create" => r#"{"id":"$ID","result":{"workspace":{"workspace_id":"w7"},
            "tab":{"tab_id":"w7:t1"},"root_pane":{"pane_id":"w7:p1"}}}"#
            .into(),
        "tab.create" => {
            r#"{"id":"$ID","result":{"tab":{"tab_id":"w2:t9"},"root_pane":{"pane_id":"w2:pN"}}}"#
                .into()
        }
        _ => OK.into(),
    });
    let h = f.herdr();

    h.new_workspace("lab", "/lab", false).expect("workspace");
    assert_eq!(f.params("workspace.create")["focus"], json!(false));

    h.new_tab(&TabOptions {
        workspace_id: Some("w2".into()),
        label: Some("worker".into()),
        ..TabOptions::default()
    })
    .expect("tab");
    assert_eq!(f.params("tab.create")["focus"], json!(false));
    // An empty cwd is omitted rather than sent as "": herdr would read the
    // empty string as a value and start the pane somewhere unintended.
    assert!(f.params("tab.create").get("cwd").is_none());
}

#[test]
fn pane_move_uses_the_live_terminal_and_returns_the_new_id() {
    let f = Fake::new(|method, _| match method {
        "pane.move" => r#"{"id":"$ID","result":{"type":"pane_move","move_result":{
            "changed":true,"previous_pane_id":"w2:p1","previous_workspace_id":"w2",
            "pane":{"pane_id":"w7:p3","workspace_id":"w7","tab_id":"w7:t1",
                "terminal_id":"term-1"},
            "created_workspace":{"workspace_id":"w7","label":"sorted"},
            "target_layout":{},"focused_pane_id":"w2:p2"
        }}}"#
            .into(),
        _ => OK.into(),
    });
    let moved = f
        .herdr()
        .move_pane(
            "w2:p1",
            &PaneMoveDestination::NewWorkspace {
                label: "sorted".into(),
            },
        )
        .unwrap();
    assert_eq!(moved.previous_pane_id, "w2:p1");
    assert_eq!(moved.pane.pane_id, "w7:p3");
    assert_eq!(moved.pane.terminal_id, "term-1");
    assert_eq!(moved.created_workspace.unwrap().workspace_id, "w7");
    assert_eq!(
        f.params("pane.move"),
        json!({
            "pane_id":"w2:p1", "focus":false,
            "destination":{"type":"new_workspace","label":"sorted"}
        })
    );
    assert_eq!(f.methods().iter().filter(|m| *m == "pane.move").count(), 1);

    let f = Fake::new(|method, _| match method {
        "pane.move" => r#"{"id":"$ID","result":{"type":"pane_move","move_result":{
            "changed":true,"previous_pane_id":"w2:p2","previous_workspace_id":"w2",
            "pane":{"pane_id":"w7:p4","workspace_id":"w7","tab_id":"w7:t2",
                "terminal_id":"term-2"},
            "target_layout":{},"focused_pane_id":"w2:p3"
        }}}"#
            .into(),
        _ => OK.into(),
    });
    f.herdr()
        .move_pane(
            "w2:p2",
            &PaneMoveDestination::NewTab {
                workspace_id: "w7".into(),
            },
        )
        .unwrap();
    assert_eq!(
        f.params("pane.move")["destination"],
        json!({
            "type":"new_tab", "workspace_id":"w7"
        })
    );
}

#[test]
fn workspace_and_tab_labels_are_renamed_by_id() {
    let f = Fake::new(standard);
    let h = f.herdr();

    h.rename_workspace("w7", "🗼 herdr-tower 🗼")
        .expect("rename workspace");
    h.rename_tab("w7:t2", "tower").expect("rename tab");

    assert_eq!(
        f.params("workspace.rename"),
        json!({"workspace_id": "w7", "label": "🗼 herdr-tower 🗼"})
    );
    assert_eq!(
        f.params("tab.rename"),
        json!({"tab_id": "w7:t2", "label": "tower"})
    );
}

/// `pane.rename` takes `label`, the same key `pane.list` reports the name
/// under — checked against herdr 0.9.1's own request schema.
#[test]
fn a_pane_is_renamed_by_id() {
    let f = Fake::new(standard);
    f.herdr()
        .rename_pane("w7:p1", "[w7:p1] worker")
        .expect("rename pane");
    assert_eq!(
        f.params("pane.rename"),
        json!({"pane_id": "w7:p1", "label": "[w7:p1] worker"})
    );
}

#[test]
fn a_pane_name_is_cleared_with_a_null_label() {
    let f = Fake::new(standard);
    f.herdr().clear_pane_name("w7:p1").expect("clear pane name");
    assert_eq!(
        f.params("pane.rename"),
        json!({"pane_id": "w7:p1", "label": null})
    );
}

/// A new place's pane is named after its id; its tab keeps the plain name it
/// was created with — no tab rename at all.
#[test]
fn a_new_place_names_its_pane_and_leaves_its_tab() {
    let f = Fake::new(standard);
    f.herdr().name_with_id("w2:p1", "hero-dark-codex");
    assert_eq!(f.methods(), vec!["pane.rename"]);
    assert_eq!(
        f.params("pane.rename"),
        json!({"pane_id": "w2:p1", "label": "[w2:p1] hero-dark-codex"})
    );
}

#[test]
fn a_losing_workspace_candidate_is_closed_by_id() {
    let f = Fake::new(standard);
    f.herdr().close_workspace("w9").expect("close workspace");

    assert_eq!(f.methods(), vec!["workspace.close"]);
    assert_eq!(f.params("workspace.close")["workspace_id"], "w9");
}

#[test]
fn starting_an_agent_needs_a_name_a_kind_and_a_pane() {
    let f = Fake::new(standard);
    let err = f.herdr().start_agent(&StartOptions {
        kind: "claude".into(),
        pane_id: "w2:p1".into(),
        ..StartOptions::default()
    });
    assert!(err.is_err());
    assert!(f.methods().is_empty(), "an incomplete request went out");
}

#[test]
fn a_bounded_wait_sends_the_timeout_and_the_states() {
    let f = Fake::new(standard);
    f.herdr()
        .wait(
            "w2:p1",
            &[AgentStatus::Done, AgentStatus::Blocked],
            Some(Duration::from_secs(5)),
        )
        .expect("wait");

    let p = f.params("agent.wait");
    assert_eq!(p["timeout_ms"], json!(5_000));
    assert_eq!(p["until"], json!(["done", "blocked"]));
}

/// A wait that was never given a timeout gets no local deadline either: herdr
/// waits **indefinitely** when no `timeout_ms` is sent. Padding a deadline
/// around it would make an unbounded wait die locally after the socket's
/// ordinary three seconds while the server was still waiting, and the caller
/// could not tell that apart from the agent having failed.
///
/// The server here answers only after that ordinary deadline has passed.
#[test]
fn an_unbounded_wait_outlives_the_ordinary_socket_deadline() {
    let f = Fake::new(|method, params| {
        if method == "agent.wait" {
            assert!(
                params.get("timeout_ms").is_none(),
                "an unbounded wait must not send a timeout"
            );
            std::thread::sleep(Duration::from_millis(3_300));
        }
        OK.into()
    });

    let start = Instant::now();
    f.herdr().wait("w2:p1", &[], None).expect("unbounded wait");
    assert!(
        start.elapsed() >= Duration::from_millis(3_300),
        "the wait returned before the server answered"
    );
}

/// A label is what the tower stores as a session name; the API speaks IDs. A
/// label nobody carries is a fact, not a failure — and not a reason to make the
/// caller parse an error string.
#[test]
fn an_unknown_label_resolves_to_nothing() {
    let f = Fake::new(|method, _| match method {
        "workspace.list" => r#"{"id":"$ID","result":{"workspaces":[
            {"workspace_id":"w2","label":"dev","focused":true}]}}"#
            .into(),
        _ => OK.into(),
    });
    let h = f.herdr();

    assert_eq!(
        h.workspace_id_by_label("dev").unwrap().as_deref(),
        Some("w2")
    );
    assert_eq!(
        h.workspace_id_by_label("de").unwrap(),
        None,
        "partial match"
    );
    assert!(h.has_workspace("dev").unwrap());
    assert!(!h.has_workspace("nope").unwrap());
}

/// A server that is bound but never accepts must not hang the caller.
///
/// `UnixStream::connect` has no bound of its own and used to run *before* the
/// read and write timeouts were installed, so the advertised deadline did not
/// cover connection setup. That is the case that matters in practice: the tower
/// polls every couple of seconds, and a `connect` that never returns freezes the
/// screen instead of reporting an unreachable backend.
///
/// The listener here is bound with nobody calling `accept`, and the backlog is
/// filled first so the next connect has nowhere to queue. The assertion is only
/// that the call *returns* inside a generous multiple of its deadline — how it
/// fails is the platform's business.
#[test]
fn a_server_that_never_accepts_still_returns_within_the_deadline() {
    // Not a temp-dir crate, for the same reason `Fake` avoids one: a unix
    // socket path is capped at 104 bytes on macOS.
    let dir = std::env::temp_dir().join(format!("hxw{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("w.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();

    // Hold the connections open so the kernel's queue cannot drain.
    let mut queued: Vec<UnixStream> = Vec::new();
    for _ in 0..512 {
        match UnixStream::connect(&path) {
            Ok(s) => queued.push(s),
            Err(_) => break, // backlog full: exactly the state we want
        }
    }

    let mut h = herdrx::Herdr::at(&path);
    h.client.timeout = Some(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    let got = h.ping();
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "call took {elapsed:?} against a server that never accepts"
    );
    assert!(got.is_err(), "a wedged server must not answer: {got:?}");
    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unlabeled workspace answers to its own ID.
///
/// `tower_scan::sync` stores `workspace_id` as the session name when a workspace has
/// no label, because the record has to say something. Resolving on the label
/// alone therefore could not find a workspace that plainly exists: `resume`
/// treated it as gone and `restore` created a duplicate beside it.
#[test]
fn an_unlabeled_workspace_is_found_by_its_id() {
    let f = Fake::new(|method, _| match method {
        "workspace.list" => r#"{"id":"$ID","result":{"type":"workspace_list","workspaces":[
            {"workspace_id":"w1","label":"dbstore"},
            {"workspace_id":"w2","label":""}
        ]}}"#
            .into(),
        _ => OK.into(),
    });
    let h = f.herdr();

    assert_eq!(
        h.workspace_id_by_label("w2").unwrap().as_deref(),
        Some("w2"),
        "an unlabeled workspace is stored under its id"
    );
    assert!(h.has_workspace("w2").unwrap());

    // A label still wins, and a name nothing answers to is still absent.
    assert_eq!(
        h.workspace_id_by_label("dbstore").unwrap().as_deref(),
        Some("w1")
    );
    assert!(!h.has_workspace("w9").unwrap());
    assert!(
        !h.has_workspace("").unwrap(),
        "an empty name must not match the unlabeled workspace"
    );
}

/// A lifecycle value this build has never heard of must not blind the tower.
///
/// The derived decoder rejected anything outside the five known statuses, which
/// fails the whole `Pane`, which fails `pane.list` — and `tower_scan::sync_backend`
/// reads a failed listing as an unreachable backend and leaves every record
/// untouched. One new herdr status would have frozen the status output with no
/// error printed anywhere, and nothing checks the protocol version on this path.
#[test]
fn a_status_this_build_does_not_know_decodes_as_unknown() {
    let f = Fake::new(|method, _| match method {
        "pane.list" => r#"{"id":"$ID","result":{"type":"pane_list","panes":[
            {"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":"claude",
             "agent_status":"compacting","cwd":"/p"},
            {"pane_id":"w2:p2","workspace_id":"w2","tab_id":"w2:t1","agent":"codex",
             "agent_status":"working","cwd":"/p"}]}}"#
            .into(),
        _ => OK.into(),
    });

    let panes = f
        .herdr()
        .list_panes()
        .expect("one odd status must not fail the listing");
    assert_eq!(panes.len(), 2, "the whole listing was lost");
    assert_eq!(panes[0].agent_status, herdrx::AgentStatus::Unknown);
    assert_eq!(
        panes[1].agent_status,
        herdrx::AgentStatus::Working,
        "its neighbour is unaffected"
    );
}

/// A jump lands where it was aimed, which on herdr 0.9 means the tab too.
///
/// Each client holds its own view from 0.9 — "Multiple clients can now view
/// different workspaces and tabs independently" — so focusing a pane moves the
/// active pane *within the tab that client is already showing*, and does nothing
/// when the target is in another tab. The server's `focused` flag moves either
/// way, which is what made the old behaviour look correct: measured on 0.9.0, a
/// cross-tab focus flipped the flag and left the screen where it was. Pressing
/// enter in the tower did nothing for that reason.
#[test]
fn a_focus_brings_the_targets_workspace_and_tab_with_it() {
    let f = Fake::new(|method, _| match method {
        "pane.get" => r#"{"id":"$ID","result":{"pane":{"pane_id":"w2:p4C",
            "tab_id":"w2:t7","workspace_id":"w2"}}}"#
            .into(),
        _ => OK.into(),
    });
    let h = f.herdr();

    h.focus("w2:p4C").expect("focus");

    assert_eq!(f.params("workspace.focus")["workspace_id"], json!("w2"));
    assert_eq!(f.params("tab.focus")["tab_id"], json!("w2:t7"));
    assert_eq!(f.params("agent.focus")["target"], json!("w2:p4C"));

    // The order is the point: the tab has to be showing before the pane inside
    // it can be selected.
    let m = f.methods();
    let at = |name: &str| m.iter().position(|x| x == name).expect(name);
    assert!(at("workspace.focus") < at("tab.focus"));
    assert!(at("tab.focus") < at("agent.focus"));
}

/// A target that is not a pane ID still jumps.
///
/// `focus` takes a herdr agent name too, and a name cannot be looked up as a
/// pane. The lookup failing must leave the jump exactly as good as it was
/// before the tab step existed, not turn it into an error.
#[test]
fn a_focus_by_agent_name_survives_having_no_pane_to_look_up() {
    let f = Fake::new(|method, _| match method {
        "pane.get" => {
            r#"{"id":"$ID","error":{"code":"pane_not_found","message":"pane codex1 not found"}}"#
                .into()
        }
        _ => OK.into(),
    });
    let h = f.herdr();

    h.focus("codex1").expect("focus by name");
    assert_eq!(f.params("agent.focus")["target"], json!("codex1"));
    assert!(f.params("tab.focus").is_null(), "nothing to reveal");
}

/// A tab that will not come forward is a failed jump, not a quiet one.
///
/// The caller acts on the answer: `tower_ui::App::attached` marks the completion
/// read and lets the tower quit once focus reports success. Swallowing this
/// would leave the client on the old tab and take the unread marker with it —
/// the person loses the only thing that would bring them back to a result they
/// never saw.
#[test]
fn a_tab_that_refuses_to_come_forward_fails_the_focus() {
    let f = Fake::new(|method, _| match method {
        "pane.get" => r#"{"id":"$ID","result":{"pane":{"pane_id":"w2:p4C",
            "tab_id":"w2:t7","workspace_id":"w2"}}}"#
            .into(),
        "tab.focus" => r#"{"id":"$ID","error":{"code":"tab_not_found","message":"gone"}}"#.into(),
        _ => OK.into(),
    });

    let e = f.herdr().focus("w2:p4C").expect_err("a tab that is gone");
    assert!(e.is_code("tab_not_found"), "{e}");
    assert!(
        f.params("agent.focus").is_null(),
        "the pane must not be focused as though the tab had come forward"
    );
}

/// A lookup that failed for any other reason is a failed jump.
///
/// A dropped socket says nothing about where the target lives. Reading it as
/// "an agent name, carry on" hides exactly what the tab step exists to stop
/// hiding: the client stays on its tab, the caller is told the jump worked, and
/// `tower_ui::App::attached` spends the unread marker on a pane nobody saw.
#[test]
fn a_lookup_that_failed_for_another_reason_fails_the_focus() {
    let f = Fake::new(|method, _| match method {
        "pane.get" => r#"{"id":"$ID","error":{"code":"internal","message":"boom"}}"#.into(),
        _ => OK.into(),
    });

    let e = f.herdr().focus("w2:p4C").expect_err("a broken lookup");
    assert!(e.is_code("internal"), "{e}");
    assert!(f.params("agent.focus").is_null(), "nothing was focused");
}
