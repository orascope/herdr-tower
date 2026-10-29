// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Selection, the isolation warning, and the compositions built on herdr.
// The fake herdr server in `common` listens on a Unix socket.
#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use serde_json::json;
use tower_mux::Error;
use tower_state::AgentState;

/// A `HOME` that is present but empty is not a home directory.
///
/// The C API and Rust's `env::var` both hand back the empty string, and
/// without a guard the socket resolves to `/.config/herdr/herdr.sock` — a path
/// no server listens on and no error explains.
#[test]
fn an_empty_home_is_not_a_home() {
    let env = |k: &str| (k == "HOME").then(String::new);
    assert_eq!(tower_mux::socket_from(env), None);

    let env = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
    assert_eq!(
        tower_mux::socket_from(env),
        Some(PathBuf::from("/home/u/.config/herdr/herdr.sock"))
    );
}

/// The injected environment is authoritative end to end, `HOME` included.
/// Reading `HOME` from the process instead would make the resolution
/// untestable without mutating the real environment.
#[test]
fn selection_follows_the_environment_it_is_given() {
    let env = |k: &str| match k {
        "HOME" => Some("/home/u".to_string()),
        "HERDR_SESSION" => Some("lab".to_string()),
        _ => None,
    };
    assert_eq!(
        tower_mux::socket_from(env),
        Some(PathBuf::from(
            "/home/u/.config/herdr/sessions/lab/herdr.sock"
        ))
    );
}

#[test]
fn discovery_returns_one_descriptor_for_each_standard_socket() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".config/herdr");
    std::fs::create_dir_all(root.join("sessions/lab")).unwrap();
    std::fs::write(root.join("sessions/lab/herdr.sock"), b"").unwrap();

    let backends = tower_mux::discover_backends(home.path()).unwrap();
    assert_eq!(backends.len(), 2);
    assert!(backends.iter().any(|backend| backend.id.is_empty()));
    let named = backends
        .iter()
        .find(|backend| !backend.id.is_empty())
        .unwrap();
    assert_eq!(named.id.len(), 17);
    assert!(named.socket.ends_with("sessions/lab/herdr.sock"));
}

/// The routing decision that keeps a broadcast out of an approval dialog. A
/// pane hosting an agent goes through `agent.prompt`, which refuses to write to
/// an agent sitting at a permission prompt; a shell pane goes through
/// `pane.send_input`, which agent.prompt would reject outright.
#[test]
fn send_text_asks_what_lives_there_before_writing() {
    let agent_pane = r#"{"id":"$ID","result":{"pane":
        {"pane_id":"w2:p1","workspace_id":"w2","agent":"codex","agent_status":"idle"}}}"#;
    let shell_pane = r#"{"id":"$ID","result":{"pane":
        {"pane_id":"w2:p9","workspace_id":"w2","agent_status":"unknown"}}}"#;

    for (pane_json, want) in [
        (agent_pane, "agent.prompt"),
        (shell_pane, "pane.send_input"),
    ] {
        let f = Fake::new(move |method, _| match method {
            "pane.get" => pane_json.into(),
            _ => OK.into(),
        });
        tower_mux::send_text(&f.herdr(), "w2:p1", "hello").expect("send");
        assert_eq!(f.methods(), vec!["pane.get".to_string(), want.to_string()]);
    }
}

/// A reply that misses the deadline after the text went out may have landed;
/// a lookup that misses it, or a refusal, sent nothing. The supervisor resends
/// on the second and never on the first — resending what landed types it
/// twice, which is what a loaded machine used to do to scheduled messages.
#[test]
fn a_send_whose_answer_is_lost_may_have_landed() {
    let agent_pane = r#"{"id":"$ID","result":{"pane":
        {"pane_id":"w2:p1","workspace_id":"w2","agent":"codex","agent_status":"idle"}}}"#;
    let refused = r#"{"id":"$ID","error":{"code":"agent_busy","message":"busy"}}"#;
    let slow = |method: &str| {
        let late = method.to_string();
        Fake::new(move |m, _| {
            if m == late {
                std::thread::sleep(Duration::from_millis(600));
            }
            match m {
                "pane.get" => agent_pane.into(),
                _ => OK.into(),
            }
        })
    };
    let quick = |f: &Fake| {
        let mut h = f.herdr();
        h.client.timeout = Some(Duration::from_millis(200));
        h
    };

    let late_prompt = slow("agent.prompt");
    let err = tower_mux::send_text(&quick(&late_prompt), "w2:p1", "hi").unwrap_err();
    assert!(err.may_have_landed(), "the prompt went out: {err:?}");

    let late_lookup = slow("pane.get");
    let err = tower_mux::send_text(&quick(&late_lookup), "w2:p1", "hi").unwrap_err();
    assert!(!err.may_have_landed(), "nothing went out: {err:?}");
    assert_eq!(late_lookup.methods(), vec!["pane.get".to_string()]);

    let no = Fake::new(move |m, _| match m {
        "pane.get" => agent_pane.into(),
        _ => refused.into(),
    });
    let err = tower_mux::send_text(&no.herdr(), "w2:p1", "hi").unwrap_err();
    assert!(!err.may_have_landed(), "herdr said no: {err:?}");
}

/// A label is what the tower stores as a session name; `tab.create` speaks IDs.
/// Skipping the resolution would create the tab in whatever workspace herdr
/// picked by default.
#[test]
fn spawn_in_resolves_the_label_before_creating_the_tab() {
    let f = Fake::new(|method, _| match method {
        "workspace.list" => WORKSPACES.into(),
        "tab.create" => {
            r#"{"id":"$ID","result":{"tab":{"tab_id":"w2:t9"},"root_pane":{"pane_id":"w2:pNEW"}}}"#
                .into()
        }
        _ => OK.into(),
    });

    let pane = tower_mux::spawn_in(
        &f.herdr(),
        "dev",
        "resume-claude-3",
        "/work",
        Some("claude -r"),
    )
    .expect("spawn");
    assert_eq!(pane, "w2:pNEW");
    assert_eq!(f.params("tab.create")["workspace_id"], "w2");
    assert_eq!(
        f.methods(),
        vec![
            "workspace.list",
            "tab.create",
            "pane.rename",
            "pane.send_input"
        ],
        "the command must run in the new pane, once it is named"
    );
    // The bare name goes into `tab.create` and stays; the pane's ID follows
    // once herdr has said what it made.
    assert_eq!(f.params("tab.create")["label"], "resume-claude-3");
    assert_eq!(
        f.params("pane.rename"),
        json!({"pane_id": "w2:pNEW", "label": "[w2:pNEW] resume-claude-3"})
    );
}

/// A name is cosmetic: a server that refuses both renames still gets its
/// command, and the caller still gets its pane.
#[test]
fn a_refused_name_never_fails_a_spawn() {
    let f = Fake::new(|method, _| match method {
        "workspace.list" => WORKSPACES.into(),
        "tab.create" => {
            r#"{"id":"$ID","result":{"tab":{"tab_id":"w2:t9"},"root_pane":{"pane_id":"w2:pNEW"}}}"#
                .into()
        }
        "tab.rename" | "pane.rename" => {
            r#"{"id":"$ID","error":{"code":"internal_error","message":"no"}}"#.into()
        }
        _ => OK.into(),
    });
    let pane = tower_mux::spawn_in(&f.herdr(), "dev", "w", "/work", Some("claude")).expect("spawn");
    assert_eq!(pane, "w2:pNEW");
    assert!(f.methods().contains(&"pane.send_input".to_string()));
}

/// No command means a bare shell the caller types into later. Running something
/// anyway would start a second agent in a pane meant to stay empty.
#[test]
fn spawn_in_runs_nothing_when_no_command_is_given() {
    let f = Fake::new(|method, _| match method {
        "workspace.list" => WORKSPACES.into(),
        "tab.create" => {
            r#"{"id":"$ID","result":{"tab":{"tab_id":"w2:t9"},"root_pane":{"pane_id":"w2:pNEW"}}}"#
                .into()
        }
        _ => OK.into(),
    });

    tower_mux::spawn_in(&f.herdr(), "dev", "worker", "/work", None).expect("spawn");
    assert!(!f.methods().contains(&"pane.send_input".to_string()));
}

#[test]
fn spawning_into_an_unknown_workspace_says_which_one() {
    let f = Fake::new(standard);
    let err =
        tower_mux::spawn_in(&f.herdr(), "nope", "n", "/d", Some("cmd")).expect_err("accepted");
    assert!(matches!(err, Error::NoWorkspace(l) if l == "nope"));
}

/// A workspace prepared for someone else must not yank the user's view: it is
/// created in the background.
#[test]
fn new_session_creates_a_background_workspace() {
    let f = Fake::new(|method, _| match method {
        "workspace.create" => r#"{"id":"$ID","result":{"workspace":{"workspace_id":"w7"},
            "tab":{"tab_id":"w7:t1"},"root_pane":{"pane_id":"w7:p1"}}}"#
            .into(),
        _ => OK.into(),
    });

    assert_eq!(
        tower_mux::new_session(&f.herdr(), "lab", "/lab").unwrap(),
        "w7:p1"
    );
    assert_eq!(f.params("workspace.create")["focus"], json!(false));
    // The workspace and its first tab keep their names; the pane gets its ID.
    assert_eq!(f.params("workspace.create")["label"], "lab");
    assert!(!f.methods().contains(&"tab.rename".to_string()));
    assert_eq!(
        f.params("pane.rename"),
        json!({"pane_id": "w7:p1", "label": "[w7:p1] lab"})
    );
    assert!(!f.methods().contains(&"workspace.rename".to_string()));
}

/// The tower's vocabulary in herdr's words. Without it a herdr agent sits at
/// IDLE forever, and a wait never matches.
#[test]
fn wait_translates_the_states_the_backend_knows() {
    let f = Fake::new(standard);
    tower_mux::wait_for_agent(
        &f.herdr(),
        "w2:p1",
        &[
            AgentState::DoneUnread,
            AgentState::Waiting,
            AgentState::Idle,
        ],
        Some(Duration::from_secs(3)),
    )
    .expect("wait");

    assert_eq!(
        f.params("agent.wait")["until"],
        json!(["done", "blocked", "idle"])
    );
}

/// States herdr has no word for are dropped from the wait. When *every*
/// requested state is one of those, the remainder is empty — and an empty
/// `until` is herdr's own default, "the first settled idle, done, or blocked".
/// Passing it through turns "wait until DEAD" into "wait until anything" and
/// reports success for a state that never happened.
#[test]
fn a_wait_for_states_herdr_cannot_see_is_refused_not_widened() {
    let f = Fake::new(standard);
    let err = tower_mux::wait_for_agent(
        &f.herdr(),
        "w2:p1",
        &[AgentState::Dead, AgentState::Error],
        Some(Duration::from_secs(3)),
    )
    .expect_err("silently widened the wait");

    assert!(matches!(err, Error::NothingToWaitFor));
    assert!(f.methods().is_empty(), "a widened wait went out anyway");
}

/// An empty `until` from the caller is a different request: it means "settle",
/// and herdr's default is exactly that.
#[test]
fn an_empty_request_still_means_herdrs_own_default() {
    let f = Fake::new(standard);
    tower_mux::wait_for_agent(&f.herdr(), "w2:p1", &[], Some(Duration::from_secs(3)))
        .expect("wait");
    assert!(f.params("agent.wait").get("until").is_none());
}

/// herdr splits attaching and selecting into two gestures, and this builds
/// only the first: the command attaches, and nothing is selected here. The
/// selection moves every client on that session, so it waits until the attach is
/// actually running — `tower_ui::Action::Run` carries it.
///
/// A workspace label is a place inside a session, not a session name, so it must
/// not reach `herdr session attach`.
#[cfg(unix)]
#[test]
fn attach_builds_the_command_and_selects_nothing() {
    let f = Fake::new(standard);
    let dir = std::env::temp_dir().join(format!("tower_mux-bin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("herdr");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = dir.display().to_string();

    // Inside a named session: attach by name.
    let env = |k: &str| match k {
        "PATH" => Some(path.clone()),
        "HERDR_SESSION" => Some("lab".to_string()),
        _ => None,
    };
    let (got, args) = tower_mux::attach_cmd(env).expect("attach");
    assert_eq!(got, bin);
    assert_eq!(args, vec!["session", "attach", "lab"]);
    assert!(
        f.params("agent.focus").is_null(),
        "building the command must not move anybody's view"
    );

    // The default session: bare `herdr` launches or attaches to it.
    let env = |k: &str| (k == "PATH").then(|| path.clone());
    let (_, args) = tower_mux::attach_cmd(env).expect("attach");
    assert!(args.is_empty());

    let env = |_: &str| None::<String>;
    assert!(matches!(
        tower_mux::attach_cmd(env),
        Err(Error::NoExecutable)
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A tab that was made and then failed to run its command stays open on
/// purpose, so somebody can attach and see what blocked. The failure has to
/// name it — otherwise the tab is real, visible, and unreachable by ID.
#[test]
fn a_failed_command_still_names_the_pane_it_left_behind() {
    let f = Fake::new(|method, _| match method {
        "workspace.list" => WORKSPACES.into(),
        "tab.create" => {
            r#"{"id":"$ID","result":{"tab":{"tab_id":"w2:t9"},"root_pane":{"pane_id":"w2:pNEW"}}}"#
                .into()
        }
        "pane.send_input" => r#"{"id":"$ID","error":{"code":"pane_busy","message":"nope"}}"#.into(),
        _ => OK.into(),
    });

    let err = tower_mux::spawn_in(&f.herdr(), "dev", "worker", "/work", Some("claude"))
        .expect_err("the command failed but the call succeeded");
    assert!(matches!(err, Error::Spawned { ref pane_id, .. } if pane_id == "w2:pNEW"));
}

/// A worker tab belongs in the workspace that asked for it.
///
/// Focus is user-interface state and moves on its own. `wt new` run from
/// workspace A while another herdr client has B in front of it used to put the
/// tab in B — away from the session that asked and away from the panes the
/// coordinator watches. Orchestrating in the background while somebody works
/// elsewhere is the whole point of this tool, so that is the normal case, not a
/// corner one.
#[test]
fn a_new_tab_lands_in_the_callers_own_workspace_not_the_focused_one() {
    let f = Fake::new(|method, _| match method {
        "pane.get" => r#"{"id":"$ID","result":{"pane":{"pane_id":"wA:p1",
            "workspace_id":"wA","tab_id":"wA:t1","cwd":"/proj"}}}"#
            .into(),
        // wB is the one the user is looking at; wA is where we live.
        "workspace.list" => r#"{"id":"$ID","result":{"workspaces":[
            {"workspace_id":"wA","label":"mine","focused":false},
            {"workspace_id":"wB","label":"other","focused":true}]}}"#
            .into(),
        "tab.create" => {
            r#"{"id":"$ID","result":{"tab":{"tab_id":"wA:t9"},"root_pane":{"pane_id":"wA:pNEW"}}}"#
                .into()
        }
        _ => OK.into(),
    });

    // `current_pane_id` reads the real environment, and this test process runs
    // inside herdr, so the lookup happens for real — against the fake.
    if herdrx::current_pane_id().is_none() {
        eprintln!("skip: not inside herdr, so there is no own pane to prefer");
        return;
    }
    let _ = tower_mux::spawn_in_focused(&f.herdr(), "worker", "/proj", None);

    assert!(
        f.methods().iter().any(|m| m == "tab.create"),
        "no tab was created"
    );
    assert_eq!(
        f.params("tab.create")
            .get("workspace_id")
            .and_then(|v| v.as_str()),
        Some("wA"),
        "the tab went to the focused workspace instead of the caller's"
    );
}

/// With no own pane to resolve, the focused workspace is still the answer.
///
/// A caller outside herdr has no pane, and refusing to create a tab because a
/// `pane.get` hiccupped would be worse than using the focused workspace.
#[test]
fn a_caller_with_no_pane_of_its_own_falls_back_to_the_focused_workspace() {
    let f = Fake::new(|method, _| match method {
        "pane.get" => r#"{"id":"$ID","error":{"code":"pane_not_found","message":"gone"}}"#.into(),
        // wB is the one the user is looking at; wA is where we live.
        "workspace.list" => r#"{"id":"$ID","result":{"workspaces":[
            {"workspace_id":"wA","label":"mine","focused":false},
            {"workspace_id":"wB","label":"other","focused":true}]}}"#
            .into(),
        "tab.create" => {
            r#"{"id":"$ID","result":{"tab":{"tab_id":"wB:t9"},"root_pane":{"pane_id":"wB:pNEW"}}}"#
                .into()
        }
        _ => OK.into(),
    });
    let _ = tower_mux::spawn_in_focused(&f.herdr(), "worker", "/proj", None);

    assert!(
        f.methods().iter().any(|m| m == "tab.create"),
        "no tab was created"
    );
    assert_eq!(
        f.params("tab.create")
            .get("workspace_id")
            .and_then(|v| v.as_str()),
        Some("wB")
    );
}
