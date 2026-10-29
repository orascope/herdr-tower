// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Running a command for the `tower` tool: no shell, nothing on stdin, a
//! deadline, and bounded output.
//!
//! Two things a plain `Command::output()` gets wrong here. It waits for the
//! pipes to close, not for the process to end — and a command that starts
//! something long-lived (`loop start` starts the supervisor) can leave a
//! grandchild holding them open forever. And it has no
//! deadline, so an interactive command would hold a request thread until the
//! server restarts.
//!
//! Both pipes are read here, on the caller's thread, with `poll` and
//! non-blocking reads — no reader threads. A reader thread waits for EOF, and
//! when a grandchild holds the pipe EOF never comes: each call used to leave
//! two threads blocked in `read` and two descriptors open for as long as the
//! grandchild lived (#102). Reading in place, the pipes are dropped — closed —
//! when this function returns, whoever else still has them.
//!
//! Windows has neither `poll` nor `O_NONBLOCK` for an anonymous pipe, but
//! `PeekNamedPipe` says how many bytes are waiting, and a read of no more than
//! that returns at once — the same in-place reading, on a short sleep instead
//! of `poll`.

use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use super::tools::Ran;

/// How long to keep collecting output after the process has ended, for what a
/// lingering grandchild has not flushed yet. Past it, whatever arrived is it.
pub const DRAIN: Duration = Duration::from_millis(500);

/// How long one `poll` waits before the process is checked on again.
const TICK: Duration = Duration::from_millis(50);

/// One of the child's output pipes and the tail of what it has said.
struct Pipe<R> {
    /// `None` once it reached EOF, and so was closed.
    pipe: Option<R>,
    buf: Vec<u8>,
}

#[cfg(unix)]
impl<R: Read + AsRawFd> Pipe<R> {
    fn new(pipe: R) -> io::Result<Self> {
        let fd = pipe.as_raw_fd();
        // SAFETY: fcntl(2) on a descriptor this Pipe owns; no memory involved.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe {
            pipe: Some(pipe),
            buf: Vec::new(),
        })
    }

    fn open(&self) -> Option<libc::c_int> {
        self.pipe.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Reads whatever is there without blocking, keeping the last `cap` bytes.
    /// EOF or an error closes the pipe.
    fn drain(&mut self, cap: usize) {
        let Some(p) = self.pipe.as_mut() else { return };
        let mut chunk = [0u8; 8192];
        loop {
            match p.read(&mut chunk) {
                Ok(0) => {
                    self.pipe = None;
                    return;
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    if self.buf.len() > cap {
                        let over = self.buf.len() - cap;
                        self.buf.drain(..over);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.pipe = None;
                    return;
                }
            }
        }
    }
}

#[cfg(windows)]
impl<R: Read + AsRawHandle> Pipe<R> {
    fn new(pipe: R) -> io::Result<Self> {
        Ok(Pipe {
            pipe: Some(pipe),
            buf: Vec::new(),
        })
    }

    fn open(&self) -> Option<()> {
        self.pipe.as_ref().map(|_| ())
    }

    /// Reads what is waiting without blocking, keeping the last `cap` bytes.
    /// A closed pipe — or any error — closes this side.
    fn drain(&mut self, cap: usize) {
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        let Some(p) = self.pipe.as_mut() else { return };
        let mut chunk = [0u8; 8192];
        loop {
            let mut avail = 0u32;
            // SAFETY: a peek of no bytes into no buffer, reporting the count.
            let ok = unsafe {
                PeekNamedPipe(
                    p.as_raw_handle() as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut avail,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                // ERROR_BROKEN_PIPE: the writers are gone — EOF.
                self.pipe = None;
                return;
            }
            if avail == 0 {
                return;
            }
            let want = (avail as usize).min(chunk.len());
            match p.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => {
                    self.pipe = None;
                    return;
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    if self.buf.len() > cap {
                        let over = self.buf.len() - cap;
                        self.buf.drain(..over);
                    }
                }
            }
        }
    }
}

impl<R> Pipe<R> {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

/// Waits up to `wait` for any of `fds` to have something to read (or to hang
/// up). With no descriptors it is a plain sleep.
#[cfg(unix)]
fn wait_readable(fds: &[libc::c_int], wait: Duration) {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = libc::c_int::try_from(wait.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: poll(2) over a slice this function owns, for its exact length.
    unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
}

/// Windows: no `poll` to wait on, so a short sleep between peeks.
#[cfg(windows)]
fn wait_readable(open: &[()], wait: Duration) {
    let _ = open;
    std::thread::sleep(wait);
}

/// Runs `cmd` to its end or to `timeout`, whichever is first. On the deadline
/// the whole process group is killed, so a command's own children go with it.
pub fn run(mut cmd: Command, timeout: Duration, cap: usize) -> io::Result<Ran> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn()?;
    let mut out: Pipe<ChildStdout> = Pipe::new(child.stdout.take().expect("piped"))?;
    let mut err: Pipe<ChildStderr> = Pipe::new(child.stderr.take().expect("piped"))?;

    let deadline = Instant::now() + timeout;
    let mut ended: Option<(Option<std::process::ExitStatus>, bool)> = None;
    let mut drained_by: Option<Instant> = None;
    loop {
        let fds: Vec<_> = [out.open(), err.open()].into_iter().flatten().collect();
        if fds.is_empty() && ended.is_some() {
            break;
        }
        wait_readable(&fds, TICK);
        out.drain(cap);
        err.drain(cap);

        if ended.is_none() {
            if let Some(status) = child.try_wait()? {
                ended = Some((Some(status), false));
            } else if Instant::now() >= deadline {
                #[cfg(unix)]
                if let Ok(pid) = i32::try_from(child.id()) {
                    // SAFETY: kill(2) on the group this function created; it
                    // touches no memory. A group already gone is not an error here.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
                // No process groups: `taskkill /T` ends the tree under it.
                #[cfg(windows)]
                let _ = Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &child.id().to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                ended = Some((child.wait().ok(), true));
            }
            if ended.is_some() {
                drained_by = Some(Instant::now() + DRAIN);
            }
        }
        if drained_by.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }

    let (status, timed_out) = ended.expect("the loop ends only after the process has");
    let ran = Ran {
        code: if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        },
        stdout: out.text(),
        stderr: err.text(),
        timed_out,
    };
    // Dropping the pipes here closes this process's read ends, whatever a
    // lingering descendant still holds.
    drop(out);
    drop(err);
    Ok(ran)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn output_and_exit_code_come_back() {
        let r = run(
            sh("echo out; echo err >&2; exit 3"),
            Duration::from_secs(10),
            1024,
        )
        .unwrap();
        assert_eq!(r.code, Some(3));
        assert_eq!(r.stdout, "out\n");
        assert_eq!(r.stderr, "err\n");
        assert!(!r.timed_out);
    }

    #[test]
    fn nothing_is_read_from_stdin() {
        let r = run(
            sh("read x && echo got || echo eof"),
            Duration::from_secs(10),
            1024,
        )
        .unwrap();
        assert_eq!(
            r.stdout, "eof\n",
            "a confirmation prompt reads end-of-file, never a yes"
        );
    }

    #[test]
    fn a_command_that_runs_too_long_is_killed_with_its_children() {
        let started = Instant::now();
        let r = run(sh("sleep 30 & sleep 30"), Duration::from_millis(300), 1024).unwrap();
        assert!(r.timed_out);
        assert_eq!(r.code, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_grandchild_holding_the_pipes_does_not_hold_the_answer() {
        let started = Instant::now();
        let r = run(
            sh("(sleep 30 &); echo started"),
            Duration::from_secs(10),
            1024,
        )
        .unwrap();
        assert_eq!(r.code, Some(0));
        assert_eq!(r.stdout, "started\n");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn output_keeps_only_its_tail() {
        let r = run(
            sh("i=0; while [ $i -lt 2000 ]; do echo line$i; i=$((i+1)); done"),
            Duration::from_secs(10),
            100,
        )
        .unwrap();
        assert!(r.stdout.len() <= 100, "{}", r.stdout.len());
        assert!(r.stdout.ends_with("line1999\n"));
    }

    /// #102: once `run` has returned, nothing of it holds the pipes. A
    /// descendant that outlives the command writes into a pipe with no
    /// reader — which it can only see if this process closed its read ends
    /// instead of leaving a reader thread blocked on them forever.
    #[test]
    fn nothing_holds_the_pipes_after_run_returns() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("epipe");
        let script = format!(
            "(trap '' PIPE; sleep 1; echo late || touch '{m}'; echo late >&2 || touch '{m}.err') & echo started",
            m = marker.display()
        );
        let r = run(sh(&script), Duration::from_secs(10), 1024).unwrap();
        assert_eq!(r.stdout, "started\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(marker.exists() && dir.path().join("epipe.err").exists()) {
            assert!(
                Instant::now() < deadline,
                "the descendant could still write: a reader was left holding the pipes"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
