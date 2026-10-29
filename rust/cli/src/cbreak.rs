// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Character-at-a-time terminal input, without going all the way to raw mode.
//!
//! The distinction is the whole point. `cbreak` turns off line buffering
//! (`ICANON`) and local echo (`ECHO`) so a keypress arrives immediately, and
//! leaves output post-processing (`OPOST`, which turns `\n` into `\r\n`) and
//! `ISIG` alone. Raw mode clears `OPOST` too, and the popup's instructions
//! then walked down the screen in a staircase because every newline moved down
//! without returning to column zero — seen while filming on 2026-09-03. Ctrl-C
//! staying alive is the second half: a picker the user cannot escape is a
//! trap.
//!
//! **Why `stty`.** Rust's standard library has no termios binding and `libc`
//! is not among this crate's dependencies, so the three settings are applied
//! by running `stty` against the terminal device. `stty -g` hands back an
//! opaque string that `stty <string>` restores exactly, which is a complete
//! save-and-restore contract — one process per transition instead of one
//! ioctl, paid once when a picker opens and once when it closes.
//!
//! **Windows** has no `stty` and no `/dev/tty`: the terminal is the console's
//! input buffer, `CONIN$`, and the same three settings are its console mode —
//! line input and echo off, Ctrl+C still a signal. Virtual-terminal input is
//! turned on with them, so an arrow key arrives as the same escape sequence a
//! Unix terminal sends and the readers above need no second parser.

use std::io;
use std::path::{Path, PathBuf};

/// The process's controlling terminal.
///
/// Named rather than assumed, because the picker has to read the *terminal*
/// even when stdin has been redirected — that is what `/dev/tty` is for.
#[cfg(not(windows))]
pub const CONTROLLING_TTY: &str = "/dev/tty";
#[cfg(windows)]
pub const CONTROLLING_TTY: &str = "CONIN$";

/// Cbreak mode, held for as long as this value lives.
///
/// A guard rather than a returned `restore` closure: a returned closure can
/// be dropped on the floor, and a terminal left in cbreak mode after a
/// panic shows the user a shell that echoes nothing. Unwinding runs `Drop`.
#[derive(Debug)]
pub struct Cbreak {
    tty: PathBuf,
    /// `stty -g` output — the terminal's complete prior settings. On Windows
    /// the prior console mode, in decimal.
    saved: String,
}

impl Cbreak {
    /// Restores the terminal now, and reports whether that worked.
    ///
    /// `Drop` cannot, so a caller that needs to know says so here. Dropping
    /// afterwards is harmless: the saved settings are taken, and an empty one
    /// restores nothing.
    pub fn restore(mut self) -> io::Result<()> {
        let saved = std::mem::take(&mut self.saved);
        if saved.is_empty() {
            return Ok(());
        }
        put_back(&self.tty, &saved)
    }
}

impl Drop for Cbreak {
    fn drop(&mut self) {
        if self.saved.is_empty() {
            return;
        }
        // A failure here has nobody to tell — the process is on its way out,
        // and the alternative to a silent failure is a panic during unwind.
        let _ = put_back(&self.tty, &self.saved);
    }
}

/// Restores what [`enter`] saved.
#[cfg(not(windows))]
fn put_back(tty: &Path, saved: &str) -> io::Result<()> {
    stty(tty, &saved.split_whitespace().collect::<Vec<_>>())
}

#[cfg(windows)]
fn put_back(tty: &Path, saved: &str) -> io::Result<()> {
    let mode = saved
        .parse::<u32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not a console mode"))?;
    console::set_mode(tty, mode)
}

/// Lets Ctrl+C reach the reader as a key (`stty -isig`), for a screen that
/// handles it itself.
#[cfg(not(windows))]
pub fn keys_not_signals(tty: &Path) -> io::Result<()> {
    stty(tty, &["-isig"])
}

#[cfg(windows)]
pub fn keys_not_signals(tty: &Path) -> io::Result<()> {
    use windows_sys::Win32::System::Console::ENABLE_PROCESSED_INPUT;
    let mode = console::mode(tty)?;
    console::set_mode(tty, mode & !ENABLE_PROCESSED_INPUT)
}

/// The console mode of `CONIN$`, the Windows side of this module.
#[cfg(windows)]
mod console {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::System::Console::{GetConsoleMode, SetConsoleMode};

    fn open(tty: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new().read(true).write(true).open(tty)
    }

    pub(super) fn mode(tty: &Path) -> io::Result<u32> {
        let f = open(tty)?;
        let mut mode = 0u32;
        // SAFETY: a console handle this function owns and a u32 to fill.
        if unsafe { GetConsoleMode(f.as_raw_handle() as _, &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode)
    }

    pub(super) fn set_mode(tty: &Path, mode: u32) -> io::Result<()> {
        let f = open(tty)?;
        // SAFETY: as above.
        if unsafe { SetConsoleMode(f.as_raw_handle() as _, mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Waits up to `ms` for input; whether any arrived.
    pub(super) fn wait(tty: &Path, ms: u32) -> io::Result<bool> {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        let f = open(tty)?;
        // SAFETY: a console input handle, which is signalled while input waits.
        Ok(unsafe { WaitForSingleObject(f.as_raw_handle() as _, ms) } == WAIT_OBJECT_0)
    }
}

/// Puts `tty` into cbreak mode until the returned guard is dropped.
#[cfg(unix)]
pub fn enter(tty: &Path) -> io::Result<Cbreak> {
    let saved = stty_out(tty, &["-g"])?;
    let saved = saved.trim().to_string();
    if saved.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stty -g said nothing about this terminal",
        ));
    }
    // `min 1 time 0`: one byte is enough to return, and never time out. The
    // two go together — `min 1` alone with a nonzero `time` would start an
    // inter-byte timer on a terminal that is simply idle.
    stty(tty, &["-icanon", "-echo", "min", "1", "time", "0"])?;
    Ok(Cbreak {
        tty: tty.to_path_buf(),
        saved,
    })
}

#[cfg(windows)]
pub fn enter(tty: &Path) -> io::Result<Cbreak> {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    };
    let saved = console::mode(tty)?;
    console::set_mode(
        tty,
        (saved & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT,
    )?;
    Ok(Cbreak {
        tty: tty.to_path_buf(),
        saved: saved.to_string(),
    })
}

#[cfg(not(any(unix, windows)))]
pub fn enter(_tty: &Path) -> io::Result<Cbreak> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cbreak is not supported on this platform",
    ))
}

/// Runs `stty`, with the terminal on its standard input.
///
/// Standard input rather than BSD's `-f` or GNU's `-F`, because those two
/// spell the same option differently and this has to work on both.
/// Reads once with a short inter-byte deadline instead of blocking.
///
/// Cbreak mode is `min 1 time 0` — a read blocks until at least one byte. That is
/// right for waiting on a keypress and wrong for asking "is there more of this
/// escape sequence?", because a real Escape has nothing following it and the read
/// would hang.
///
/// `min 0 time <deciseconds>` returns whatever has arrived by the deadline,
/// including nothing. The setting is put back afterwards, so the surrounding
/// cbreak contract is unchanged even if the read fails.
pub fn read_with_deadline(
    tty: &Path,
    r: &mut impl io::Read,
    buf: &mut [u8],
    deciseconds: u8,
) -> io::Result<usize> {
    #[cfg(windows)]
    {
        // The console has no inter-byte timer; waiting on it is the same
        // question asked another way.
        if !console::wait(tty, u32::from(deciseconds) * 100)? {
            return Ok(0);
        }
        r.read(buf)
    }
    #[cfg(not(windows))]
    {
        stty(tty, &["min", "0", "time", &deciseconds.to_string()])?;
        let n = r.read(buf);
        let _ = stty(tty, &["min", "1", "time", "0"]);
        n
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn stty(tty: &Path, args: &[&str]) -> io::Result<()> {
    stty_out(tty, args).map(|_| ())
}

/// How many columns the terminal has, or `None` when it will not say.
///
/// `stty size` prints `rows cols`, and this module already talks to the
/// terminal that way — see the module note on why there is no termios binding
/// here. A caller that needs a number rather than an answer should use
/// [`columns_or_default`].
#[cfg(unix)]
pub fn columns(tty: &Path) -> Option<usize> {
    let out = stty_out(tty, &["size"]).ok()?;
    out.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(unix))]
pub fn columns(_tty: &Path) -> Option<usize> {
    tower_os::stdout_size().map(|(cols, _)| cols)
}

/// The terminal's width, falling back to 80.
///
/// A redirected or absent terminal has no width, and 80 is the assumption every
/// such caller has made since terminals had a width at all. It only has to be
/// *a* number: it decides how a repaint counts rows, so being wrong makes the
/// repaint wrong in the same way it already was, and being right fixes it.
pub fn columns_or_default(tty: &Path) -> usize {
    columns(tty).filter(|c| *c > 0).unwrap_or(80)
}

fn stty_out(tty: &Path, args: &[&str]) -> io::Result<String> {
    let dev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tty)?;
    let out = std::process::Command::new("stty")
        .args(args)
        .stdin(dev)
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "stty {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test process has no controlling terminal, and the failure has to be
    /// an error rather than a hang or a wrecked parent terminal.
    ///
    /// This is the only property assertable without a pty: `cargo test`
    /// detaches from the terminal, so `/dev/tty` is either absent or not ours.
    /// The real mode change is exercised by hand — there is nothing to fake it
    /// against that would prove anything.
    #[test]
    fn a_device_that_is_not_a_terminal_is_refused_rather_than_configured() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(
            enter(f.path()).is_err(),
            "a plain file must not be reported as a terminal put into cbreak"
        );
    }
}
