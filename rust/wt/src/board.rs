// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Delivering a dispatch board item to a `wt` worker.
//!
//! The same delivery as `wt send` — found by worktree, checked to still hold
//! an agent in that worktree, typed with `agent.prompt` — plus what a board
//! needs on top: the task has to belong to the board's repository, the worker
//! has to be the only live one there on this backend and settled right now,
//! and the result says not just "ok or error" but which of three outcomes it
//! was, because the board records them differently (`docs/dispatch-board.md`).

use std::path::Path;

use tower_state::{Receipt, Store};

use crate::review::{deliver, deliver_to_pane, BoardGuard, Delivered, Failed, Invalidate};
use crate::Error;

/// How a board send ended.
#[derive(Debug)]
pub enum BoardDelivery {
    /// The worker certainly does not have it: a check refused before the
    /// prompt, or herdr answered the prompt with an error.
    Refused(Error),
    /// herdr took the prompt.
    Accepted {
        receipt: Receipt,
        /// A stale `drive` verdict could not be cleared.
        verdict_kept: bool,
    },
    /// The prompt went out and the exchange failed: the server may have
    /// received it. Never resent without the advisor saying so.
    Unknown(Error),
}

/// Sends `text` to `task`'s worker for the board of `repo`, through the herdr
/// server at `socket`.
///
/// The backend id is derived here from `socket` and `default_socket`, never
/// taken from the caller: the id picks the worker's record and the socket
/// carries the prompt, so they cannot be allowed to name different servers.
pub fn send_board_item(
    state_dir: &Path,
    task: &str,
    store: &Store,
    socket: &Path,
    default_socket: &Path,
    repo: &str,
    text: &str,
) -> BoardDelivery {
    let backend = herdrx::backend_id(socket, default_socket);
    let h = herdrx::Herdr::at(socket);
    let guard = BoardGuard {
        repo,
        backend: &backend,
    };
    let d = deliver(
        state_dir,
        task,
        store,
        &h,
        text,
        Invalidate::Verdict,
        Some(guard),
    );
    outcome_of(d, backend)
}

/// Sends `text` to a pane worker through the herdr server at `socket`, if the
/// pane still holds the agent `pin` names. The backend id is derived from the
/// sockets, as for a task.
pub fn send_board_item_to_pane(
    store: &Store,
    socket: &Path,
    default_socket: &Path,
    pin: &tower_state::PaneWorker,
    text: &str,
) -> BoardDelivery {
    let backend = herdrx::backend_id(socket, default_socket);
    let h = herdrx::Herdr::at(socket);
    outcome_of(deliver_to_pane(store, &h, &backend, pin, text), backend)
}

fn outcome_of(d: std::result::Result<Delivered, Failed>, backend: String) -> BoardDelivery {
    match d {
        Ok(d) => BoardDelivery::Accepted {
            receipt: Receipt {
                backend,
                session_id: d.session_id,
                work_seq: d.work_seq,
                agent_id: d.agent_id,
                adopted: false,
                record_born: d.record_born,
                at: d.at,
                boundary_lost: d.boundary_lost,
            },
            verdict_kept: d.verdict_kept,
        },
        Err(f) => classify(f),
    }
}

/// Sorts a failed delivery into refused or unknown.
///
/// Before the prompt nothing was sent, whatever failed. After it, herdr's own
/// rule decides ([`herdrx::Error::may_have_acted`]): a refusal is certain, and
/// anything that may have come after the server acted is unknown.
fn classify(f: Failed) -> BoardDelivery {
    if !f.prompted {
        return BoardDelivery::Refused(f.error);
    }
    let certain = match &f.error {
        Error::Herdr(e) => !e.may_have_acted(),
        _ => false,
    };
    if certain {
        BoardDelivery::Refused(f.error)
    } else {
        BoardDelivery::Unknown(f.error)
    }
}

impl BoardDelivery {
    /// The board's view of this delivery.
    pub fn outcome(&self) -> tower_state::Outcome {
        match self {
            BoardDelivery::Refused(_) => tower_state::Outcome::Refused,
            BoardDelivery::Accepted { receipt, .. } => {
                tower_state::Outcome::Accepted(receipt.clone())
            }
            BoardDelivery::Unknown(_) => tower_state::Outcome::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompted(error: Error) -> Failed {
        Failed {
            error,
            prompted: true,
        }
    }

    fn io(kind: std::io::ErrorKind) -> Error {
        Error::Herdr(herdrx::Error::Io(std::io::Error::from(kind)))
    }

    fn api() -> Error {
        Error::Herdr(herdrx::Error::Api(herdrx::ApiError {
            code: "agent_blocked".into(),
            message: "approval pending".into(),
        }))
    }

    fn is_refused(d: &BoardDelivery) -> bool {
        matches!(d, BoardDelivery::Refused(_))
    }

    #[test]
    fn anything_before_the_prompt_is_refused() {
        for e in [
            io(std::io::ErrorKind::BrokenPipe),
            Error::NoAgentIn("/w".into()),
            Error::WorkerBusy {
                task: "t".into(),
                status: "working",
            },
        ] {
            assert!(is_refused(&classify(Failed::from(e))));
        }
    }

    #[test]
    fn a_structured_answer_to_the_prompt_is_refused() {
        assert!(is_refused(&classify(prompted(api()))));
        assert!(is_refused(&classify(prompted(Error::Herdr(
            herdrx::Error::NoSocket
        )))));
    }

    #[test]
    fn a_connection_that_never_opened_is_refused() {
        assert!(is_refused(&classify(prompted(io(
            std::io::ErrorKind::ConnectionRefused
        )))));
        assert!(is_refused(&classify(prompted(io(
            std::io::ErrorKind::NotFound
        )))));
    }

    #[test]
    fn a_broken_exchange_after_the_prompt_is_unknown() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::ConnectionReset,
        ] {
            assert!(
                matches!(classify(prompted(io(kind))), BoardDelivery::Unknown(_)),
                "{kind:?}"
            );
        }
        let decode = serde_json::from_str::<u8>("x").unwrap_err();
        assert!(matches!(
            classify(prompted(Error::Herdr(herdrx::Error::Decode(decode)))),
            BoardDelivery::Unknown(_)
        ));
    }

    // --- end to end, against a fake herdr --------------------------------
    // The fake listens on a Unix socket, so this half runs where herdr does that.
    #[cfg(unix)]
    mod end_to_end {
        use super::*;

        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Mutex};

        use crate::git::tests::fixture_repo;
        use crate::lifecycle::{new_task, NewOptions};
        use tower_state::{Agent, AgentState};

        /// What the fake answers a method with; `None` hangs up without a reply.
        type Answer = Box<dyn Fn(&str) -> Option<String> + Send>;

        /// A herdr server that answers one request per connection.
        struct Fake {
            dir: std::path::PathBuf,
            calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        }

        static SEQ: AtomicU32 = AtomicU32::new(0);

        impl Fake {
            fn new(answer: Answer) -> Fake {
                // Short on purpose: a unix socket path is capped at 104 bytes on macOS.
                let dir = std::env::temp_dir().join(format!(
                    "wb{}-{}",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&dir).unwrap();
                let listener = UnixListener::bind(dir.join("h.sock")).unwrap();
                let calls = Arc::new(Mutex::new(Vec::new()));
                let seen = calls.clone();
                std::thread::spawn(move || {
                    for conn in listener.incoming() {
                        let Ok(mut conn) = conn else { return };
                        let mut line = String::new();
                        let mut reader = BufReader::new(conn.try_clone().unwrap());
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            continue;
                        }
                        let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let method = req["method"].as_str().unwrap_or_default().to_string();
                        seen.lock()
                            .unwrap()
                            .push((method.clone(), req["params"].clone()));
                        if let Some(body) = answer(&method) {
                            let id = req["id"].as_str().unwrap_or("").to_string();
                            let _ = conn.write_all(body.replace("$ID", &id).as_bytes());
                            let _ = conn.write_all(b"\n");
                        }
                    }
                });
                Fake { dir, calls }
            }

            fn socket(&self) -> std::path::PathBuf {
                self.dir.join("h.sock")
            }

            fn prompted(&self) -> bool {
                self.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(m, _)| m == "agent.prompt")
            }
        }

        impl Drop for Fake {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        const OK: &str = r#"{"id":"$ID","result":{}}"#;
        const BLOCKED: &str =
            r#"{"id":"$ID","error":{"code":"agent_blocked","message":"approval pending"}}"#;

        /// A task with a worker record in its worktree: session `S1`, 5 turns.
        struct World {
            _repo: tempfile::TempDir,
            state: tempfile::TempDir,
            meta: crate::Meta,
            store: Store,
        }

        impl World {
            fn new() -> World {
                let repo = fixture_repo();
                let state = tempfile::tempdir().unwrap();
                let meta = new_task(
                    state.path(),
                    &NewOptions {
                        task: "t",
                        repo: repo.path().to_str().unwrap(),
                        ..NewOptions::default()
                    },
                    time::OffsetDateTime::now_utc(),
                )
                .unwrap();
                let store = Store::new(state.path().join("agents")).unwrap();
                let mut a = Agent::new("codex-w1:p1", "codex", time::OffsetDateTime::now_utc());
                a.pane.pane_id = "w1:p1".into();
                a.cwd = meta.path.clone();
                a.state = AgentState::Idle;
                a.session_id = "S1".into();
                a.work_seq = 5;
                store.save(&a).unwrap();
                World {
                    _repo: repo,
                    state,
                    meta,
                    store,
                }
            }

            /// herdr's `pane.get` reply for the worker, in this worktree.
            fn pane(&self, status: &str, session: &str) -> String {
                serde_json::json!({
                    "id": "$ID",
                    "result": { "pane": {
                        "pane_id": "w1:p1",
                        "cwd": self.meta.path,
                        "agent": "codex",
                        "agent_status": status,
                        "agent_session": {
                            "agent": "codex", "kind": "id", "source": "hook", "value": session
                        }
                    }}
                })
                .to_string()
            }

            /// A fake that answers `pane.get` with `status`/`session` and
            /// `agent.prompt` with `prompt`.
            fn fake(&self, status: &str, session: &str, prompt: Option<&'static str>) -> Fake {
                let pane = self.pane(status, session);
                Fake::new(Box::new(move |m| match m {
                    "pane.get" => Some(pane.clone()),
                    "agent.prompt" => prompt.map(str::to_string),
                    _ => Some(OK.into()),
                }))
            }

            /// Sends through `fake` as the default server.
            fn send(&self, fake: &Fake) -> BoardDelivery {
                send_board_item(
                    self.state.path(),
                    "t",
                    &self.store,
                    &fake.socket(),
                    &fake.socket(),
                    &self.meta.repo,
                    "do the thing",
                )
            }
        }

        #[test]
        fn an_accepted_send_carries_the_receipt_read_before_the_prompt() {
            let w = World::new();
            let fake = w.fake("idle", "S1", Some(OK));
            match w.send(&fake) {
                BoardDelivery::Accepted { receipt, .. } => {
                    assert_eq!(receipt.session_id, "S1");
                    assert_eq!(receipt.work_seq, 5);
                    assert_eq!(receipt.backend, "");
                }
                other => panic!("{other:?}"),
            }
            assert!(fake.prompted());
        }

        #[test]
        fn a_busy_worker_is_refused_without_a_prompt() {
            for status in ["working", "blocked"] {
                let w = World::new();
                let fake = w.fake(status, "S1", Some(OK));
                match w.send(&fake) {
                    BoardDelivery::Refused(Error::WorkerBusy { .. }) => {}
                    other => panic!("{status}: {other:?}"),
                }
                assert!(!fake.prompted(), "{status}");
            }
        }

        #[test]
        fn a_different_live_conversation_is_refused_without_a_prompt() {
            let w = World::new();
            let fake = w.fake("idle", "S2", Some(OK));
            match w.send(&fake) {
                BoardDelivery::Refused(Error::SessionChanged { .. }) => {}
                other => panic!("{other:?}"),
            }
            assert!(!fake.prompted());
        }

        #[test]
        fn herdr_refusing_the_prompt_is_refused() {
            let w = World::new();
            let fake = w.fake("idle", "S1", Some(BLOCKED));
            assert!(is_refused(&w.send(&fake)));
            assert!(fake.prompted());
        }

        #[test]
        fn a_prompt_with_no_start_seen_is_unknown_not_refused() {
            // herdr took the text but saw no start within its window — the
            // swallowed-at-boot case. The worker may or may not have it.
            for code in ["agent_prompt_stalled", "timeout"] {
                let w = World::new();
                let body: &'static str = if code == "timeout" {
                    r#"{"id":"$ID","error":{"code":"timeout","message":"t"}}"#
                } else {
                    r#"{"id":"$ID","error":{"code":"agent_prompt_stalled","message":"no start"}}"#
                };
                let fake = w.fake("idle", "S1", Some(body));
                match w.send(&fake) {
                    BoardDelivery::Unknown(_) => {}
                    other => panic!("{code}: {other:?}"),
                }
            }
        }

        #[test]
        fn board_and_wt_sends_wait_for_a_start() {
            let w = World::new();
            let fake = w.fake("idle", "S1", Some(OK));
            let _ = w.send(&fake);
            let h = herdrx::Herdr::at(fake.socket());
            crate::send_instruction(w.state.path(), "t", &w.store, &h, "x").unwrap();
            let prompts: Vec<serde_json::Value> = fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| m == "agent.prompt")
                .map(|(_, p)| p.clone())
                .collect();
            assert_eq!(prompts.len(), 2);
            assert_eq!(
                prompts[0]["wait"]["until"],
                serde_json::json!(["working", "blocked"])
            );
            assert!(prompts[0]["wait"]["timeout_ms"].as_u64().unwrap() > 5000);
            assert_eq!(
                prompts[1]["wait"]["until"],
                serde_json::json!(["working", "blocked"]),
                "wt send confirms the start too"
            );
        }

        #[test]
        fn a_hang_up_after_the_prompt_is_unknown() {
            let w = World::new();
            let fake = w.fake("idle", "S1", None);
            match w.send(&fake) {
                BoardDelivery::Unknown(_) => {}
                other => panic!("{other:?}"),
            }
            assert!(fake.prompted());
        }

        #[test]
        fn the_backend_id_comes_from_the_socket_not_the_caller() {
            let w = World::new();
            let fake = w.fake("idle", "S1", Some(OK));
            // `fake` as a named server: its id is not the default's, so the
            // default-server record in the worktree is not this server's worker.
            let default = w.state.path().join("default.sock");
            let d = send_board_item(
                w.state.path(),
                "t",
                &w.store,
                &fake.socket(),
                &default,
                &w.meta.repo,
                "x",
            );
            match d {
                BoardDelivery::Refused(Error::NoAgentIn(_)) => {}
                other => panic!("{other:?}"),
            }
            assert!(!fake.prompted());
            // Its worker recorded under that server's id is found, and the
            // receipt names the same id.
            let id = herdrx::backend_id(&fake.socket(), &default);
            let mut a = w.store.load("codex-w1:p1").unwrap();
            a.pane.backend = id.clone();
            w.store.save(&a).unwrap();
            let d = send_board_item(
                w.state.path(),
                "t",
                &w.store,
                &fake.socket(),
                &default,
                &w.meta.repo,
                "x",
            );
            match d {
                BoardDelivery::Accepted { receipt, .. } => assert_eq!(receipt.backend, id),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn a_pane_that_reports_no_session_takes_the_records_own() {
            let w = World::new();
            let pane = serde_json::json!({
                "id": "$ID",
                "result": { "pane": {
                    "pane_id": "w1:p1", "cwd": w.meta.path, "agent": "codex", "agent_status": "idle"
                }}
            })
            .to_string();
            let fake = Fake::new(Box::new(move |m| match m {
                "pane.get" => Some(pane.clone()),
                _ => Some(OK.into()),
            }));
            match w.send(&fake) {
                BoardDelivery::Accepted { receipt, .. } => {
                    // herdr gives codex panes no session; the record's is pinned
                    // by its generation, which the receipt carries too.
                    assert_eq!(receipt.session_id, "S1");
                    assert!(receipt.record_born.is_some());
                }
                other => panic!("{other:?}"),
            }
            // A record from before `born_at` has no such guard: nothing borrowed.
            let mut a = w.store.load("codex-w1:p1").unwrap();
            a.born_at = None;
            w.store.save(&a).unwrap();
            match w.send(&fake) {
                BoardDelivery::Accepted { receipt, .. } => assert_eq!(receipt.session_id, ""),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn a_drive_retry_still_returns_on_acceptance() {
            let w = World::new();
            let fake = w.fake("idle", "S1", Some(OK));
            let h = herdrx::Herdr::at(fake.socket());
            crate::send_dispatch(
                w.state.path(),
                "t",
                &w.store,
                &h,
                "retry",
                crate::Invalidate::Nothing,
            )
            .unwrap();
            let calls = fake.calls.lock().unwrap();
            let (_, p) = calls.iter().find(|(m, _)| m == "agent.prompt").unwrap();
            assert!(p.get("wait").is_none(), "{p}");
        }

        #[test]
        fn a_wt_send_whose_start_is_never_seen_fails() {
            let w = World::new();
            let stalled: &'static str =
                r#"{"id":"$ID","error":{"code":"agent_prompt_stalled","message":"no start"}}"#;
            let fake = w.fake("idle", "S1", Some(stalled));
            let h = herdrx::Herdr::at(fake.socket());
            let e = crate::send_instruction(w.state.path(), "t", &w.store, &h, "x").unwrap_err();
            assert!(e.to_string().contains("agent_prompt_stalled"), "{e}");
        }

        #[test]
        fn wt_send_still_reaches_a_busy_worker() {
            // The board's checks are the board's: the shared delivery must leave
            // `wt send` exactly as it was, a follow-up into a working agent included.
            let w = World::new();
            let fake = w.fake("working", "S2", Some(OK));
            let h = herdrx::Herdr::at(fake.socket());
            let d =
                crate::send_instruction(w.state.path(), "t", &w.store, &h, "follow-up").unwrap();
            assert!(fake.prompted());
            assert_eq!(d.work_seq, 5);
        }

        fn pin_of(w: &World) -> tower_state::PaneWorker {
            let a = w.store.load("codex-w1:p1").unwrap();
            tower_state::PaneWorker {
                backend: String::new(),
                pane_id: "w1:p1".into(),
                agent_id: a.id.clone(),
                session_id: a.session_id.clone(),
                born_at: a.born_at,
            }
        }

        fn to_pane(w: &World, fake: &Fake, pin: &tower_state::PaneWorker) -> BoardDelivery {
            send_board_item_to_pane(&w.store, &fake.socket(), &fake.socket(), pin, "do it")
        }

        #[test]
        fn a_pane_worker_gets_the_item_with_a_receipt() {
            let w = World::new();
            let fake = w.fake("idle", "S1", Some(OK));
            match to_pane(&w, &fake, &pin_of(&w)) {
                BoardDelivery::Accepted { receipt, .. } => {
                    assert_eq!(receipt.work_seq, 5);
                    assert_eq!(receipt.agent_id, "codex-w1:p1");
                    assert_eq!(receipt.session_id, "S1");
                }
                other => panic!("{other:?}"),
            }
            assert!(fake.prompted());
        }

        #[test]
        fn a_pane_that_now_holds_another_agent_is_refused() {
            let w = World::new();
            let fake = w.fake("idle", "S1", Some(OK));
            let mut pin = pin_of(&w);
            pin.born_at = Some(time::OffsetDateTime::UNIX_EPOCH);
            match to_pane(&w, &fake, &pin) {
                BoardDelivery::Refused(Error::PaneReassigned { .. }) => {}
                other => panic!("{other:?}"),
            }
            let mut pin = pin_of(&w);
            pin.session_id = "S-old".into();
            match to_pane(&w, &fake, &pin) {
                BoardDelivery::Refused(Error::PaneReassigned { .. }) => {}
                other => panic!("{other:?}"),
            }
            assert!(!fake.prompted());
        }

        #[test]
        fn a_pane_send_keeps_the_busy_and_backend_guards() {
            let w = World::new();
            let fake = w.fake("working", "S1", Some(OK));
            match to_pane(&w, &fake, &pin_of(&w)) {
                BoardDelivery::Refused(Error::WorkerBusy { .. }) => {}
                other => panic!("{other:?}"),
            }
            let mut pin = pin_of(&w);
            pin.backend = "b0123456789abcdef".into();
            match to_pane(&w, &fake, &pin) {
                BoardDelivery::Refused(Error::PaneElsewhere { .. }) => {}
                other => panic!("{other:?}"),
            }
            let mut pin = pin_of(&w);
            pin.pane_id = "w1:p9".into();
            match to_pane(&w, &fake, &pin) {
                BoardDelivery::Refused(Error::NoAgentInPane(_)) => {}
                other => panic!("{other:?}"),
            }
            assert!(!fake.prompted());
        }

        #[test]
        fn outcomes_map_onto_the_board() {
            let receipt = Receipt {
                backend: String::new(),
                session_id: "s".into(),
                work_seq: 3,
                agent_id: String::new(),
                adopted: false,
                record_born: None,
                at: time::OffsetDateTime::UNIX_EPOCH,
                boundary_lost: false,
            };
            let d = BoardDelivery::Accepted {
                receipt: receipt.clone(),
                verdict_kept: false,
            };
            assert_eq!(d.outcome(), tower_state::Outcome::Accepted(receipt));
            assert_eq!(
                BoardDelivery::Refused(api()).outcome(),
                tower_state::Outcome::Refused
            );
            assert_eq!(
                BoardDelivery::Unknown(api()).outcome(),
                tower_state::Outcome::Unknown
            );
        }
    }
}
