// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Concurrent `config set`s — several MCP `tower` calls at once, say — each
//! keep their edit, and the file is never left torn (#123).
//!
//! Before the fix every writer used the one temp file `config.json.tmp` with
//! no lock around read → edit → write: one process truncated the file another
//! was writing, a half-written document could be renamed into place (which
//! `tower_config::load` then answers with the defaults), and the last rename
//! dropped the other writers' keys.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Stdio};

fn set(dir: &Path, key: &str, value: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-tower"))
        .args(["config", "set", key, value])
        .env("HOME", dir)
        .env("HERDR_TOWER_STATE_DIR", dir.join("state"))
        .env("HERDR_TOWER_CONFIG", dir.join("config.json"))
        .env("HERDR_SOCKET_PATH", dir.join("no-herdr.sock"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("the binary runs")
}

/// A key, the JSON pointer it lands at, and its value on round `k` — a new
/// value every round, so every call writes.
type Key = (&'static str, &'static str, fn(usize) -> String);

const KEYS: &[Key] = &[
    ("usage_guard_5h", "/usage_guard_5h", |k| {
        (20 + k).to_string()
    }),
    ("wait_renotify", "/wait_renotify", |k| format!("{}m", k + 1)),
    ("preview_interval", "/preview_interval", |k| {
        format!("{}s", k + 1)
    }),
    ("mcp.listen", "/mcp/listen", |k| {
        format!("127.0.0.1:{}", 8800 + k)
    }),
    ("mcp.public_url", "/mcp/public_url", |k| {
        format!("https://m{k}.t.ts.net")
    }),
    ("discord_webhook_url", "/discord_webhook_url", |k| {
        format!("https://discord.com/api/webhooks/{k}/a")
    }),
    ("notify_webhook_url", "/notify_webhook_url", |k| {
        format!("https://discord.com/api/webhooks/{k}/b")
    }),
];

const ROUNDS: usize = 12;

#[test]
fn concurrent_sets_keep_every_edit_and_never_tear_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    std::thread::scope(|s| {
        for &(key, _, value) in KEYS {
            s.spawn(move || {
                for k in 0..ROUNDS {
                    let out = set(path, key, &value(k));
                    assert!(
                        out.status.success(),
                        "config set {key} failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            });
        }
    });

    let body = std::fs::read(path.join("config.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!(
            "torn config.json ({e}):\n{}",
            String::from_utf8_lossy(&body)
        )
    });
    for &(key, pointer, value) in KEYS {
        let last = value(ROUNDS - 1);
        let got = doc
            .pointer(pointer)
            .unwrap_or_else(|| panic!("{key} was lost:\n{doc:#}"));
        let got = got.as_str().map_or_else(|| got.to_string(), str::to_string);
        assert_eq!(got, last, "{key} holds another writer's edit:\n{doc:#}");
    }
    // No temp files are left behind, from any writer.
    let strays: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("tmp"))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");
}
