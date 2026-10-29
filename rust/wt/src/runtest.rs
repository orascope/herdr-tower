// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Running a task's tests and remembering the answer.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;

use time::OffsetDateTime;

use crate::meta::{load_meta, save_meta};
use crate::{Error, Result};

/// Runs the task's test command in its worktree and records pass or fail.
///
/// Output goes to `w` as it arrives rather than being captured, because the
/// interesting case is a failure and a failure is only useful if you can read
/// it.
///
/// A non-empty `cmd_override` also becomes the task's stored command, so `wt
/// test t --cmd '…'` has to be typed once.
///
/// **A command that could not start is recorded as a failure**, not returned as
/// an error. "sh is missing" and "exit 1" both set `pass = false`, because that
/// is the right answer to the question being asked: "did the tests pass" is no
/// either way.
///
/// `W: Send` is needed because stdout and stderr are read on two threads.
/// Handing the child one file descriptor for both would keep its own
/// interleaving, but `std::process` has no way to give one pipe to both, so two
/// readers serialize on a mutex around the writer instead. Interleaving is then
/// preserved at read-chunk granularity, which is the closest available without
/// reaching for libc. The alternative — read stdout to the end, then stderr —
/// deadlocks the moment a test suite fills the stderr pipe, which is exactly
/// what a failing suite does.
pub fn run_test<W: Write + Send>(
    w: &mut W,
    state_dir: &Path,
    task: &str,
    cmd_override: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    let mut m = load_meta(state_dir, task)?;
    let cmd = if cmd_override.is_empty() {
        m.test_cmd.clone()
    } else {
        m.test_cmd = cmd_override.to_string();
        cmd_override.to_string()
    };
    if cmd.is_empty() {
        return Err(Error::NoTestCmd(task.to_string()));
    }

    let pass = execute(w, &m.path, &cmd);

    // 🚨 Re-read, and under the record lock. `m` was loaded before a run that
    // can take hours, and saving that snapshot back republishes every field as
    // it stood before the suite started — a `Running` a parallel drive
    // published in the meantime is erased, and so is any `test_cmd` somebody
    // set. Only the three fields this call owns are written. The lock is the
    // same one `record_for_if` and the dispatch invalidation take, so a verdict
    // cannot land in the middle of this either.
    let _guard = crate::meta::claim_lock(&crate::meta::dir(state_dir));
    let mut fresh = load_meta(state_dir, task)?;
    if !cmd_override.is_empty() {
        fresh.test_cmd = cmd_override.to_string();
    }
    // A later test is a later verdict. Leaving a drive's `merge-ready` next to
    // a fresh `TEST ✖` would advertise a branch in the merge queue that just
    // failed; the drive itself records its own state after this returns.
    fresh.drive = None;
    fresh.test_pass = Some(pass);
    fresh.test_at = Some(now);
    save_meta(state_dir, &fresh)?;
    Ok(pass)
}

/// Runs `cmd` through the shell (`sh -c`; `cmd /c` on Windows) in `dir`, streaming both streams into `w`. `true` on a clean
/// exit.
///
/// `sh -c` and not a parsed argv: the stored command is a shell line a person
/// typed (`cargo test && npm test`), and splitting it would silently change
/// what it means.
fn execute<W: Write + Send>(w: &mut W, dir: &str, cmd: &str) -> bool {
    let child = tower_os::shell(cmd)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let sink = Mutex::new(Sink { w, last: None });
    std::thread::scope(|s| {
        if let Some(out) = out.as_mut() {
            s.spawn(|| pump(out, &sink));
        }
        if let Some(err) = err.as_mut() {
            s.spawn(|| pump(err, &sink));
        }
    });
    // 🚨 End the suite's last line if the suite did not. Whatever the caller
    // prints next — `wt test`'s `✔ tests passed`, a drive's verdict — was
    // otherwise appended to it: `42 passed, 0 failed✔ tests passed`. It lives
    // here, not in a caller, because both callers stream this output and then
    // write their own line, and an earlier fix in one of them left the other
    // with the same bug.
    let sink = sink.into_inner().unwrap_or_else(|e| e.into_inner());
    if sink.last.is_some_and(|b| b != b'\n') {
        let _ = sink.w.write_all(b"\n");
    }
    child.wait().map(|st| st.success()).unwrap_or(false)
}

/// The writer the two stream readers share, and the last byte either of them
/// put through it.
struct Sink<'a, W> {
    w: &'a mut W,
    last: Option<u8>,
}

/// Copies a stream into the shared sink until it ends.
///
/// Every error is the end of the stream. There is nothing useful to do with a
/// broken pipe here, and the test result itself comes from the exit status, not
/// from whether every byte made it to the terminal.
fn pump<R: Read, W: Write>(r: &mut R, sink: &Mutex<Sink<'_, W>>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if let Ok(mut g) = sink.lock() {
                    let _ = g.w.write_all(&buf[..n]);
                    g.last = Some(buf[n - 1]);
                }
            }
        }
    }
}

/// The commands below are POSIX shell lines; Windows runs `cmd`, whose
/// spelling of the same things differs — it has its own test underneath.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::git::tests::fixture_repo;
    use crate::lifecycle::{new_task, NewOptions};
    use crate::meta::load_meta;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_789_000_496).unwrap()
    }

    fn a_task(state: &Path, test_cmd: &str) -> (tempfile::TempDir, crate::Meta) {
        let repo = fixture_repo();
        let m = new_task(
            state,
            &NewOptions {
                task: "t",
                repo: repo.path().to_str().unwrap(),
                test_cmd,
                ..NewOptions::default()
            },
            now(),
        )
        .unwrap();
        (repo, m)
    }

    #[test]
    fn a_passing_command_is_recorded_as_a_pass() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, _m) = a_task(state.path(), "true");
        let mut buf = Vec::new();
        assert!(run_test(&mut buf, state.path(), "t", "", now()).unwrap());

        let stored = load_meta(state.path(), "t").unwrap();
        assert_eq!(stored.test_pass, Some(true));
        assert_eq!(stored.test_at, Some(now()));
    }

    /// The overriding command is stored, so the next `wt test` needs no flag.
    #[test]
    fn a_failing_override_is_recorded_along_with_the_command_that_failed() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, _m) = a_task(state.path(), "true");
        let mut buf = Vec::new();
        assert!(!run_test(&mut buf, state.path(), "t", "false", now()).unwrap());

        let stored = load_meta(state.path(), "t").unwrap();
        assert_eq!(stored.test_pass, Some(false));
        assert_eq!(stored.test_cmd, "false");
    }

    #[test]
    fn a_task_with_no_test_command_says_how_to_give_it_one() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, _m) = a_task(state.path(), "");
        let err = run_test(&mut Vec::new(), state.path(), "t", "", now()).expect_err("no command");
        assert!(matches!(err, Error::NoTestCmd(t) if t == "t"));
    }

    /// A failure is only actionable if its output reaches the person, and test
    /// runners put failures on stderr as often as on stdout.
    #[test]
    fn both_streams_reach_the_writer() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, _m) = a_task(state.path(), "");
        let mut buf = Vec::new();
        let pass = run_test(
            &mut buf,
            state.path(),
            "t",
            "echo to-stdout; echo to-stderr >&2; exit 3",
            now(),
        )
        .unwrap();
        assert!(!pass);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("to-stdout"), "{out}");
        assert!(out.contains("to-stderr"), "{out}");
    }

    /// The command runs in the worktree, not wherever the tool happens to be —
    /// a test suite invoked from the wrong directory passes or fails for the
    /// wrong repository.
    #[test]
    fn the_command_runs_inside_the_worktree() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, m) = a_task(state.path(), "");
        let mut buf = Vec::new();
        assert!(run_test(&mut buf, state.path(), "t", "test -f a.txt", now()).unwrap());
        assert!(Path::new(&m.path).join("a.txt").is_file());
    }

    /// More output than any pipe buffer holds, to prove the reader is draining
    /// rather than waiting for the child to finish.
    #[test]
    fn a_command_that_outruns_the_pipe_buffer_does_not_hang() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, _m) = a_task(state.path(), "");
        let mut buf = Vec::new();
        assert!(run_test(
            &mut buf,
            state.path(),
            "t",
            "i=0; while [ $i -lt 2000 ]; do echo 'a line of output on stdout'; echo 'and one on stderr' >&2; i=$((i+1)); done",
            now(),
        )
        .unwrap());
        // Comfortably past any pipe buffer a platform here uses (64 KiB on
        // Linux, 16 KiB on macOS), so a reader that waited for exit would have
        // deadlocked long before this point.
        assert!(buf.len() > 64 * 1024, "got {} bytes", buf.len());
    }

    /// Where `save_meta` puts task `t` — asked of `meta`, not rebuilt here, so a
    /// change to how records are named cannot leave these tests pointing at a
    /// file nothing writes.
    fn record_file(state: &Path) -> std::path::PathBuf {
        crate::meta::record_path(state, "t")
    }

    /// A shell command that, run as the suite, replaces task `t`'s record with
    /// one whose stored command is `changed` — another writer, mid-run.
    fn a_concurrent_writer(
        state: &Path,
        m: &crate::Meta,
        changed: &str,
    ) -> (tempfile::TempDir, String) {
        let staged = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(crate::meta::dir(staged.path())).unwrap();
        let mut other = m.clone();
        other.test_cmd = changed.into();
        crate::meta::save_meta(staged.path(), &other).unwrap();
        let swap = format!(
            "cp '{}' '{}'",
            record_file(staged.path()).display(),
            record_file(state).display()
        );
        (staged, swap)
    }

    /// 🚨 The record is re-read after the run, not written back from before it.
    /// A suite can take hours, and `wt test --cmd` from another terminal can
    /// change the stored command meanwhile; saving the snapshot loaded before
    /// the run put the old command back. Here the suite *is* that other
    /// writer: it replaces the record while it runs.
    #[test]
    fn a_command_changed_during_the_run_survives_it() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, m) = a_task(state.path(), "placeholder");

        // The record as another writer would leave it, prepared elsewhere and
        // copied into place by the suite itself.
        let (_staged, swap) = a_concurrent_writer(state.path(), &m, "make changed-meanwhile");
        let mut before = m.clone();
        before.test_cmd = swap;
        crate::meta::save_meta(state.path(), &before).unwrap();

        let mut buf = Vec::new();
        assert!(run_test(&mut buf, state.path(), "t", "", now()).unwrap());
        let after = load_meta(state.path(), "t").unwrap();
        assert_eq!(
            after.test_cmd, "make changed-meanwhile",
            "the change made during the run is kept"
        );
        assert_eq!(
            after.test_pass,
            Some(true),
            "and the result is still recorded"
        );
    }

    /// An explicit override still wins — it is this call's to write — even
    /// over a change another writer made to the stored command during the run.
    /// Here the override *is* that writer: it runs, and it rewrites the record.
    #[test]
    fn an_override_is_written_even_over_a_concurrent_change() {
        let state = tempfile::tempdir().unwrap();
        let (_repo, m) = a_task(state.path(), "false");
        let (_staged, swap) = a_concurrent_writer(state.path(), &m, "make changed-meanwhile");

        let mut buf = Vec::new();
        assert!(run_test(&mut buf, state.path(), "t", &swap, now()).unwrap());
        assert_eq!(
            load_meta(state.path(), "t").unwrap().test_cmd,
            swap,
            "the override, not the command written while it ran"
        );
    }

    /// 🚨 A suite whose last line has no newline gets one, before the caller
    /// prints anything. `wt test` and a drive both print their own line right
    /// after, and it was being appended to the suite's: `42 passed, 0
    /// failed✔ tests passed`. Tested here, where both callers get it.
    #[test]
    fn an_unterminated_last_line_is_ended() {
        let cases = [
            (
                "unterminated",
                "printf '42 passed, 0 failed'",
                "42 passed, 0 failed\n",
            ),
            ("terminated", "echo ok", "ok\n"),
            ("silent", "true", ""),
            ("stderr last", "echo out; printf 'err' >&2", ""),
        ];
        for (name, cmd, want) in cases {
            let state = tempfile::tempdir().unwrap();
            let (_repo, _m) = a_task(state.path(), cmd);
            let mut buf = Vec::new();
            run_test(&mut buf, state.path(), "t", "", now()).unwrap();
            let got = String::from_utf8(buf).unwrap();
            if want.is_empty() && name == "stderr last" {
                // The two streams interleave in either order; what matters is
                // that the output ends a line and gains no blank one.
                assert!(
                    got.ends_with('\n') && !got.ends_with("\n\n"),
                    "{name}: {got:?}"
                );
            } else {
                assert_eq!(got, want, "{name}");
            }
        }
    }

    /// A later test is a later verdict: whatever a drive had concluded is gone
    /// once the suite has run again, pass or fail.
    #[test]
    fn a_test_run_clears_the_previous_drive_verdict() {
        for (cmd, passes) in [("true", true), ("false", false)] {
            let state = tempfile::tempdir().unwrap();
            let (_repo, m) = a_task(state.path(), cmd);
            let mut with_verdict = m.clone();
            with_verdict.drive = Some(crate::drive::Drive {
                state: crate::drive::DriveState::MergeReady,
                attempts: 1,
                failure: String::new(),
                head: "abc123".into(),
                at: now(),
            });
            crate::meta::save_meta(state.path(), &with_verdict).unwrap();

            let mut buf = Vec::new();
            assert_eq!(
                run_test(&mut buf, state.path(), "t", "", now()).unwrap(),
                passes
            );
            assert!(
                load_meta(state.path(), "t").unwrap().drive.is_none(),
                "{cmd}: the old verdict must not sit beside the new result"
            );
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// `cmd` runs the stored line as typed: its exit code is the verdict, and
    /// both streams reach the writer.
    #[test]
    fn a_cmd_line_passes_or_fails_by_its_exit_code() {
        let dir = std::env::temp_dir();
        let dir = dir.to_str().unwrap();
        let mut buf = Vec::new();
        assert!(execute(
            &mut buf,
            dir,
            "echo to-stdout&& echo to-stderr 1>&2"
        ));
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.contains("to-stdout") && text.contains("to-stderr"),
            "{text}"
        );
        assert!(!execute(&mut Vec::new(), dir, "exit /b 3"));
    }
}
