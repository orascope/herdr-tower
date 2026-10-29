// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! When whoever reads our stdout has gone — `herdr-tower status | head -1`.
//!
//! The Rust runtime ignores SIGPIPE, so a write to a pipe nobody reads fails
//! with EPIPE instead of ending the process. `println!` turns that into a
//! panic (`failed printing to stdout: Broken pipe`), and `writeln!` into an
//! error that `main` used to print as `herdr-tower: Broken pipe (os error 32)`
//! with exit 1. Neither is what a shell user expects of a command whose output
//! was simply not wanted any more.
//!
//! SIGPIPE is **not** put back to its default, which would be the usual fix
//! for a small CLI. This binary is also the resident supervisor, the MCP
//! server and a long-lived herdr socket client, and a default SIGPIPE ends the
//! process on a write to any socket the other side has closed — a phone
//! dropping its connection mid-reply would take the MCP server down with it.
//!
//! So the error travels as an error, and `main` decides: EPIPE *and* stdout
//! itself is gone means the reader left, which is an ordinary end (exit 141,
//! 128 + SIGPIPE, as a shell reports a process SIGPIPE ended). An EPIPE from a
//! socket or a child's stdin, with stdout still open, is a real failure and is
//! reported as before.

use std::error::Error;

/// The exit status of a process SIGPIPE ended, as a shell reports it.
pub const EXIT_READER_GONE: u8 = 128 + 13;

/// Whether an error is, anywhere in its chain, a broken pipe. Some commands
/// carry their I/O errors as text, so the OS's own wording counts too.
pub fn is_broken_pipe(e: &(dyn Error + 'static)) -> bool {
    let os_epipe = format!("(os error {})", libc::EPIPE);
    let mut cur: Option<&(dyn Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::BrokenPipe {
                return true;
            }
        }
        if err.to_string().contains(&os_epipe) {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Whether stdout is a pipe or socket whose reader has gone.
///
/// Asked of the descriptor rather than inferred from the error: a hang-up on
/// fd 1 (`POLLHUP` on macOS, `POLLERR` on Linux) is the reader closing, while a
/// file, a terminal or `/dev/null` never reports either.
#[cfg(unix)]
pub fn stdout_gone() -> bool {
    stdout_fd_gone(libc::STDOUT_FILENO)
}

/// Windows: stdout is a pipe, and even an empty write to it fails — what a
/// write does once the reading end has closed (`ERROR_NO_DATA`).
#[cfg(windows)]
pub fn stdout_gone() -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, WriteFile, FILE_TYPE_PIPE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: a zero-byte write from a valid (empty) buffer to stdout's handle.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if GetFileType(h) != FILE_TYPE_PIPE {
            return false;
        }
        let mut n = 0u32;
        WriteFile(h, [0u8; 1].as_ptr(), 0, &mut n, std::ptr::null_mut()) == 0
    }
}

#[cfg(unix)]
fn stdout_fd_gone(fd: libc::c_int) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: poll(2) on one pollfd this function owns, with no wait.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// `println!` that returns the error instead of panicking. Use `?` on it.
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        writeln!(::std::io::stdout(), $($arg)*)
    }};
}

/// `print!` that returns the error instead of panicking. Use `?` on it.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        write!(::std::io::stdout(), $($arg)*)
    }};
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn pipe() -> (std::fs::File, std::fs::File) {
        let mut fds = [0; 2];
        // SAFETY: pipe(2) fills the two descriptors, which are handed to Files
        // that own and close them.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        use std::os::fd::FromRawFd;
        // SAFETY: fresh descriptors from pipe(2), each owned exactly once.
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        }
    }

    #[test]
    fn a_pipe_is_gone_only_once_its_reader_closes() {
        let (reader, writer) = pipe();
        assert!(!stdout_fd_gone(writer.as_raw_fd()), "someone is reading");
        drop(reader);
        assert!(stdout_fd_gone(writer.as_raw_fd()), "nobody is reading");
    }

    #[test]
    fn a_file_is_never_gone() {
        let f = tempfile::tempfile().unwrap();
        assert!(!stdout_fd_gone(f.as_raw_fd()));
        let null = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        assert!(!stdout_fd_gone(null.as_raw_fd()));
    }

    #[test]
    fn a_broken_pipe_is_recognised_however_it_travels() {
        let io = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(is_broken_pipe(&io));
        let os = std::io::Error::from_raw_os_error(libc::EPIPE);
        assert!(is_broken_pipe(&os));
        // Carried as text, as some commands do.
        #[derive(Debug)]
        struct Text(String);
        impl std::fmt::Display for Text {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl Error for Text {}
        assert!(is_broken_pipe(&Text(os.to_string())));
        assert!(!is_broken_pipe(&Text("no such file".into())));
        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!is_broken_pipe(&other));
    }
}
