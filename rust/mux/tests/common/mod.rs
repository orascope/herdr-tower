// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! A fake herdr server for these tests.
//!
//! Duplicated from `herdrx`'s own test file rather than shared: exporting a
//! test helper from a production crate to save the duplication would be the
//! worse trade.

#![allow(dead_code)] // each test file uses a different part of this

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use herdrx::Herdr;
use serde_json::Value;

type Responder = Arc<dyn Fn(&str, &Value) -> String + Send + Sync>;

/// A minimal herdr server: newline-delimited JSON, one request per connection,
/// except `events.subscribe` which stays open so a test can push events.
pub struct Fake {
    pub path: PathBuf,
    dir: PathBuf,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    subs: Arc<Mutex<Vec<UnixStream>>>,
}

static SEQ: AtomicU32 = AtomicU32::new(0);

impl Fake {
    pub fn new(resp: impl Fn(&str, &Value) -> String + Send + Sync + 'static) -> Fake {
        // Not a temp-dir crate: a unix socket path is capped at 104 bytes on
        // macOS, so the name stays short on purpose.
        let dir = std::env::temp_dir().join(format!(
            "mx{}-{}",
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

    pub fn herdr(&self) -> Herdr {
        Herdr::at(&self.path)
    }

    /// An environment that always points the socket at this fake, so no test
    /// can reach a real server.
    pub fn env(&self, extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + '_ {
        let mut vars: Vec<(String, String)> = vec![(
            "HERDR_SOCKET_PATH".to_string(),
            self.path.display().to_string(),
        )];
        vars.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        move |k: &str| {
            vars.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        }
    }

    pub fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }

    pub fn params(&self, method: &str) -> Value {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .unwrap_or(Value::Null)
    }

    /// Pushes one event to every open subscription.
    pub fn push(&self, event: &str) {
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
    // the subscription returns cannot race the registration.
    if method == "events.subscribe" {
        subs.lock().unwrap().push(conn.try_clone().unwrap());
    }

    let body = resp(&method, &params).replace("$ID", req["id"].as_str().unwrap_or(""));
    // Compacted, because the protocol is newline-delimited and the fixtures are
    // written across several lines to stay readable.
    let body = match serde_json::from_str::<Value>(&body) {
        Ok(v) => v.to_string(),
        Err(_) => body,
    };
    let _ = conn.write_all(format!("{body}\n").as_bytes());
    let _ = conn.flush();
}

pub const OK: &str = r#"{"id":"$ID","result":{"type":"ok"}}"#;
pub const ACK: &str = r#"{"id":"$ID","result":{"type":"subscription_started"}}"#;
pub const PANES: &str = r#"{"id":"$ID","result":{"type":"pane_list","panes":[
    {"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":"claude","agent_status":"working","cwd":"/p"}]}}"#;
pub const WORKSPACES: &str = r#"{"id":"$ID","result":{"type":"workspace_list","workspaces":[
    {"workspace_id":"w2","label":"dev","focused":true,"pane_count":1,"tab_count":1}]}}"#;

pub fn standard(method: &str, _: &Value) -> String {
    match method {
        "events.subscribe" => ACK.into(),
        "pane.list" => PANES.into(),
        "workspace.list" => WORKSPACES.into(),
        _ => OK.into(),
    }
}

/// One pushed `pane.updated`, the event herdr sends on every pane revision.
pub const PANE_UPDATED: &str = r#"{"event":"pane_updated","data":{"type":"pane_updated","pane":
    {"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":"claude",
     "agent_status":"working","cwd":"/p"}}}"#;
