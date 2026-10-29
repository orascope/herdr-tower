// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! A reader that leaves early — `herdr-tower status | head -1` — ends the
//! command quietly, the way a shell expects: exit 141, nothing on stderr.
//!
//! Run against the real binary with stdout attached to a pipe whose read end
//! is already closed, so the very first write fails with EPIPE. Before the fix
//! `help` and `version` panicked (`failed printing to stdout: Broken pipe`,
//! exit 101) and the `writeln!` commands printed `herdr-tower: Broken pipe
//! (os error 32)` and exited 1.

#![cfg(unix)]

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};

/// Held from creating a pipe until its read end is closed, and around every
/// spawn in this file.
///
/// 🚨 This is what made `a_reader_that_left_ends_the_command_quietly` flaky.
/// The tests run on parallel threads, and a child spawned by another test
/// between `pipe(2)` and dropping the read end inherited that read end: the
/// pipe then still had a reader, the command's output sat in its buffer, and
/// it exited 0 instead of 141. It showed only under load — the whole
/// workspace, a pre-push run — where that window is widest.
static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A pipe's write end, its read end already closed.
fn widowed_pipe() -> OwnedFd {
    let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills two fresh descriptors; each is owned exactly once.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // Close-on-exec as well: no child of any thread keeps either end.
    for fd in fds {
        // SAFETY: fcntl(2) on a descriptor this function just made.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            -1
        );
    }
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    drop(reader);
    writer
}

fn run(dir: &Path, args: &[&str], stdout: Stdio) -> std::process::Output {
    let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    Command::new(env!("CARGO_BIN_EXE_herdr-tower"))
        .args(args)
        .env("HOME", dir)
        .env("HERDR_TOWER_STATE_DIR", dir.join("state"))
        .env("HERDR_TOWER_CONFIG", dir.join("config.json"))
        .env("HERDR_SOCKET_PATH", dir.join("no-herdr.sock"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .output()
        .expect("the binary runs")
}

/// Every kind of stdout writer the binary has: `print!`-style (`help`,
/// `version`), `writeln!` through a command's own error path (`status`,
/// `config`), and a command with its own error type (`schedule`).
const COMMANDS: &[&[&str]] = &[
    &["help"],
    &["version"],
    &["status", "--json"],
    &["status", "--line"],
    &["status"],
    &["config", "keys"],
    &["schedule", "list"],
    &["mcp", "grants"],
];

#[test]
fn a_reader_that_left_ends_the_command_quietly() {
    let dir = tempfile::tempdir().unwrap();
    for args in COMMANDS {
        let out = run(dir.path(), args, Stdio::from(widowed_pipe()));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(141), "{args:?}: {stderr}");
        assert!(stderr.is_empty(), "{args:?} said: {stderr}");
    }
}

#[test]
fn the_same_commands_still_print_when_someone_reads() {
    let dir = tempfile::tempdir().unwrap();
    for args in COMMANDS {
        let out = run(dir.path(), args, Stdio::piped());
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.stdout.is_empty(), "{args:?} printed nothing");
    }
}

/// A real failure is still reported when stdout is fine — the quiet exit is
/// for a reader that left, not for any error.
#[test]
fn other_errors_are_still_reported() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["info", "nobody"], Stdio::piped());
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no agent"));
    // And with stdout gone, an error that is not a broken pipe still speaks.
    let out = run(dir.path(), &["info", "nobody"], Stdio::from(widowed_pipe()));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no agent"));
}

/// `println!` and `print!` panic on a closed stdout; the binary uses
/// `tower_cli::outln!` / `out!`, which return the error instead. A new one
/// would bring the panic back for its command, so none may appear.
#[test]
fn nothing_prints_with_a_panicking_macro() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    for sub in ["src", "../ui/src"] {
        let mut stack = vec![root.join(sub)];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap();
                for (n, line) in text.lines().enumerate() {
                    let code = line.trim_start();
                    if code.starts_with("//") {
                        continue;
                    }
                    for bad in ["println!(", "print!("] {
                        if let Some(i) = code.find(bad) {
                            let before = &code[..i];
                            if !before.ends_with('e') && !before.ends_with("tower_cli::out") {
                                found.push(format!("{}:{}: {code}", p.display(), n + 1));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "use tower_cli::outln!/out! instead:\n{}",
        found.join("\n")
    );
}
