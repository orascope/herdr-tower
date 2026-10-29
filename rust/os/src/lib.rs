// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The few operating-system calls herdr-tower makes, written once for Unix and
//! once for Windows so the crates above them need no `cfg` of their own.
//!
//! Windows has no permission bits: a file under the user's profile is already
//! private to that account by its inherited ACL, so the calls that tighten a
//! mode on Unix are no-ops there rather than approximations.

use std::fs::OpenOptions;
use std::path::Path;

pub mod host;

/// Restricts a file being created to its owner (`0600`) where modes exist.
///
/// On the options, not afterwards: a mode set after the write leaves a window
/// in which another local user can read what was written.
pub fn private(opts: &mut OpenOptions) -> &mut OpenOptions {
    mode(opts, 0o600)
}

/// Creates with `mode` where modes exist.
pub fn mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        opts
    }
}

/// Creates `dir` and its parents, owner-only (`0700`) where modes exist, and
/// narrows it if it was already there.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    set_mode(dir, 0o700)
}

/// Sets `path`'s permission bits where they exist.
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Sets an open file's permission bits where they exist — by handle, so a
/// path swapped since the open cannot redirect it.
pub fn set_file_mode(f: &std::fs::File, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (f, mode);
        Ok(())
    }
}

/// `path`'s permission bits, or `None` where there are none.
pub fn file_mode(meta: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// `name` as a file name this system accepts. Unix takes any name without `/`
/// and is given it unchanged — records already on disk keep their names. On
/// Windows `<>:"/\\|?*` are refused, and a record ID holds the pane's `w2:p6`,
/// so each of them becomes `%` and its hex code (`w2%3Ap6`), and `%` itself
/// becomes `%25` so two names can never meet.
pub fn file_name(name: &str) -> std::borrow::Cow<'_, str> {
    if !cfg!(windows) || !name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*', '%']) {
        return std::borrow::Cow::Borrowed(name);
    }
    let mut out = String::with_capacity(name.len() + 8);
    for c in name.chars() {
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '%' => {
                out.push_str(&format!("%{:02X}", c as u32))
            }
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// A path a tool printed, in this system's spelling: on Windows git and
/// others print `C:/Users/u`, while every path built here reads
/// `C:\\Users\\u`, and the two must compare equal as strings.
pub fn native_path(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

/// `n` bytes from the system's random source: `/dev/urandom`, or on Windows
/// the system-preferred RNG (`BCryptGenRandom`).
pub fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        // SAFETY: the buffer is `n` bytes and outlives the call.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(std::io::Error::other(format!(
                "BCryptGenRandom failed: {status:#x}"
            )));
        }
    }
    Ok(buf)
}

/// A command that runs `line` the way a person typed it: `sh -c` on Unix,
/// `cmd /d /s /c` on Windows — both run `a && b` as written, and neither
/// splits the line itself.
pub fn shell(line: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("cmd");
        // Raw: cmd parses its own line, and `/s` takes the outer quotes off.
        c.raw_arg(format!("/d /s /c \"{line}\""));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(line);
        c
    }
}

/// Whether `bin` can be run by name: `command -v` on Unix, as a shell would
/// find it; on Windows, an executable on `PATH` by the extensions it tries.
pub fn on_path(bin: &str) -> bool {
    #[cfg(windows)]
    {
        std::env::var("PATH").is_ok_and(|p| look_path(bin, &p).is_some())
    }
    #[cfg(not(windows))]
    {
        shell(&format!("command -v {bin}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// What separates `PATH` entries: `:`, or `;` on Windows.
pub const PATH_SEP: char = if cfg!(windows) { ';' } else { ':' };

/// `PATH`'s entries, without the empty ones. An empty entry means the current
/// directory, which no lookup here wants to search.
pub fn split_path(path: &str) -> impl Iterator<Item = std::path::PathBuf> + '_ {
    path.split(PATH_SEP)
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
}

/// The files `name` can be in `dir`: `dir/name`, and on Windows, for a bare
/// name, `dir/name.exe`, `.cmd`, `.bat` and `.com` too — what the shell would
/// try. npm installs `claude.cmd` and `codex.cmd`, so a bare-name lookup that
/// tried only `codex` would find nothing there.
pub fn exe_candidates(dir: &Path, name: &str) -> Vec<std::path::PathBuf> {
    let plain = dir.join(name);
    if !cfg!(windows) || Path::new(name).extension().is_some() {
        return vec![plain];
    }
    ["exe", "cmd", "bat", "com"]
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .collect()
}

/// Whether `p` is a file this user can run: a file with an execute bit on
/// Unix (a directory has one too), and on Windows a file whose extension
/// Windows runs — the ones [`exe_candidates`] tries.
pub fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        p.extension().is_some_and(|e| {
            ["exe", "cmd", "bat", "com"]
                .iter()
                .any(|x| e.eq_ignore_ascii_case(x))
        })
    }
}

/// Finds `name` on `path` (a `PATH` value).
pub fn look_path(name: &str, path: &str) -> Option<std::path::PathBuf> {
    split_path(path)
        .flat_map(|d| exe_candidates(&d, name))
        .find(|p| is_executable_file(p))
}

/// The executable's path as the first word of a hook command on Windows, where
/// the agents run hooks through Git Bash, `cmd` or PowerShell depending on
/// what is installed: forward slashes, which all three accept, and no quotes
/// unless the path needs them — a quoted first word is a string, not a
/// command, to PowerShell.
pub fn windows_command_word(path: &str) -> String {
    let path = path.replace('\\', "/");
    let plain = !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c));
    if plain {
        path
    } else {
        format!("\"{path}\"")
    }
}

/// The file name a command's first word runs, however it was written: quoted
/// or not, with `/` or `\\`, with or without Windows' `.exe`.
pub fn command_basename(word: &str) -> &str {
    let word = word.trim().trim_matches(|c| c == '\'' || c == '"');
    let file = word.rsplit(['/', '\\']).next().unwrap_or(word);
    match file.len().checked_sub(4) {
        Some(i) if file.is_char_boundary(i) && file[i..].eq_ignore_ascii_case(".exe") => &file[..i],
        _ => file,
    }
}

/// What identifies an open file whatever path names it: (device, inode) on
/// Unix, (volume serial number, file index) on Windows.
pub fn file_identity(f: &std::fs::File) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = f.metadata()?;
        Ok((m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        // SAFETY: `info` is a plain struct the call fills.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }
}

/// [`file_identity`] of the file at `path`.
pub fn path_identity(path: &Path) -> std::io::Result<(u64, u64)> {
    file_identity(&std::fs::File::open(path)?)
}

/// stdout's terminal size as (columns, rows), or `None` when stdout is not a
/// terminal.
pub fn stdout_size() -> Option<(usize, usize)> {
    #[cfg(unix)]
    {
        // SAFETY: TIOCGWINSZ only fills the struct it is given.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
        ok.then_some((ws.ws_col as usize, ws.ws_row as usize))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
        };
        // SAFETY: the struct is plain data the call fills.
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
                return None;
            }
            let w = info.srWindow;
            Some((
                (w.Right - w.Left + 1).max(0) as usize,
                (w.Bottom - w.Top + 1).max(0) as usize,
            ))
        }
    }
}

/// Whether a process with this pid exists.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists and may be signalled.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: the handle is checked and closed; no pointers outlive the call.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // Access denied still means the process exists.
                return std::io::Error::last_os_error().raw_os_error() == Some(5);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
}

/// Starts `command_line` as a process of its own with no window and no
/// handle of ours, and returns its pid (Windows only).
///
/// `std::process::Command` passes every inheritable handle to the child, and
/// the caller's may include a pipe its own parent reads to EOF — an SSH
/// session, a PowerShell pipeline. A long-lived child holding that pipe keeps
/// the caller's caller waiting forever, so this calls `CreateProcessW` with
/// inheritance off. The child leaves the caller's job when the job allows it,
/// so the end of an SSH session does not end it too.
#[cfg(windows)]
///
/// It starts in `cwd`: a long-lived process left in the caller's folder keeps
/// that folder from being deleted, on Windows, for as long as it runs.
pub fn spawn_detached(command_line: &str, cwd: &Path) -> std::io::Result<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        PROCESS_INFORMATION, STARTUPINFOW,
    };
    let start = |flags: u32| -> std::io::Result<u32> {
        // CreateProcessW may write into the command line, so it is a fresh,
        // mutable, NUL-terminated buffer each time.
        let mut line: Vec<u16> = command_line
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let dir: Vec<u16> = cwd
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: the structs are plain data, sized as the call expects; the
        // handles it returns are closed here.
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            if CreateProcessW(
                std::ptr::null(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                flags,
                std::ptr::null(),
                dir.as_ptr(),
                &si,
                &mut pi,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            Ok(pi.dwProcessId)
        }
    };
    let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    start(base | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| start(base))
}

/// The one byte a Windows lock covers: far past any end a lock file reaches.
/// See [`try_lock`].
#[cfg(windows)]
const LOCK_OFFSET: u64 = u64::MAX - 1;

/// Takes `f`'s exclusive lock, waiting for it.
///
/// On Unix this is `flock`, as `File::lock` is. On Windows `File::lock`
/// locks the whole file, and Windows locks are mandatory: while one process
/// holds it, another cannot even read what the file says — and these files
/// carry a marker that other processes read. So there the lock covers one
/// byte past any end the file will have, which excludes other lockers exactly
/// as before and leaves the contents readable.
pub fn lock(f: &std::fs::File) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        win_lock(f, true).map(drop)
    }
    #[cfg(not(windows))]
    {
        f.lock()
    }
}

/// Tries [`lock`] once: `Ok(false)` when another holder has it.
pub fn try_lock(f: &std::fs::File) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        win_lock(f, false)
    }
    #[cfg(not(windows))]
    {
        match f.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Releases [`lock`]. Closing the file releases it too.
pub fn unlock(f: &std::fs::File) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
        use windows_sys::Win32::System::IO::OVERLAPPED;
        // SAFETY: a zeroed OVERLAPPED carrying the offset, for a synchronous call.
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.Anonymous.Anonymous.Offset = LOCK_OFFSET as u32;
            ov.Anonymous.Anonymous.OffsetHigh = (LOCK_OFFSET >> 32) as u32;
            if UnlockFileEx(f.as_raw_handle() as _, 0, 1, 0, &mut ov) == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        f.unlock()
    }
}

#[cfg(windows)]
fn win_lock(f: &std::fs::File, wait: bool) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let flags = LOCKFILE_EXCLUSIVE_LOCK | if wait { 0 } else { LOCKFILE_FAIL_IMMEDIATELY };
    // SAFETY: a zeroed OVERLAPPED carrying the offset, for a synchronous call
    // on a handle opened without FILE_FLAG_OVERLAPPED.
    unsafe {
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.Anonymous.Anonymous.Offset = LOCK_OFFSET as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (LOCK_OFFSET >> 32) as u32;
        if LockFileEx(f.as_raw_handle() as _, flags, 0, 1, 0, &mut ov) != 0 {
            return Ok(true);
        }
    }
    let err = std::io::Error::last_os_error();
    if !wait && err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        return Ok(false);
    }
    Err(err)
}

/// The file names of `pid`'s direct children (`herdr-tower.exe`), or `None`
/// where this is not looked up — everywhere but Windows, where herdr reports a
/// pane's foreground processes itself.
///
/// On Windows herdr reports a pane's shell however busy it is, so what runs in
/// the pane is found here instead: the processes whose parent is that shell.
pub fn child_names(pid: u32) -> Option<Vec<String>> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        // SAFETY: the snapshot handle is checked and closed; the entry is a
        // plain struct sized as the calls expect.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut out = Vec::new();
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                if e.th32ParentProcessID == pid && e.th32ProcessID != pid {
                    let len = e
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(e.szExeFile.len());
                    out.push(String::from_utf16_lossy(&e.szExeFile[..len]));
                }
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
            Some(out)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// Asks a process to stop: `SIGTERM` on Unix. Windows has no polite signal for
/// a console process in another console, so there it ends the process — what
/// `SIGKILL` would do.
pub fn terminate(pid: u32) -> std::io::Result<()> {
    signal(pid, false)
}

/// Ends a process at once: `SIGKILL`, or `TerminateProcess`.
pub fn kill(pid: u32) -> std::io::Result<()> {
    signal(pid, true)
}

fn signal(pid: u32, hard: bool) -> std::io::Result<()> {
    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pid 0 names no single process",
        ));
    }
    #[cfg(unix)]
    {
        let sig = if hard { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: kill(2) takes plain integers.
        if unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        let _ = hard;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        // SAFETY: the handle is checked and closed.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let ok = TerminateProcess(h, 1);
            let err = std::io::Error::last_os_error();
            CloseHandle(h);
            if ok != 0 {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

/// The effective user id; `0` is root. Windows has none, and no root: `None`.
pub fn euid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid(2) takes no arguments and cannot fail.
        Some(unsafe { libc::geteuid() } as u32)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The user's home folder: `HOME`, or on Windows `USERPROFILE` when `HOME` is
/// not set (it usually is not, outside Git Bash and MSYS).
pub fn home() -> Option<std::path::PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    var("HOME")
        .or_else(|| {
            if cfg!(windows) {
                var("USERPROFILE")
            } else {
                None
            }
        })
        .map(std::path::PathBuf::from)
}

/// Makes `HOME` name the home folder for the rest of the process.
///
/// herdr-tower keeps its files where the agents keep theirs — `~/.claude`,
/// `~/.codex`, `~/.local/state/herdr-tower` — and Claude Code and Codex put
/// `~` at `%USERPROFILE%` on Windows. Setting `HOME` once, first thing in
/// `main` and before any thread exists, lets every `~` in the code mean that.
pub fn ensure_home() {
    #[cfg(windows)]
    if std::env::var_os("HOME").filter(|v| !v.is_empty()).is_none() {
        if let Some(p) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            // Called before any other thread starts; see the doc above.
            std::env::set_var("HOME", p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_name_is_kept_on_unix_and_escaped_on_windows() {
        let got = file_name("claude-w2:p6");
        if cfg!(windows) {
            assert_eq!(got, "claude-w2%3Ap6");
            assert_eq!(file_name("a%3Ab"), "a%253Ab");
        } else {
            assert_eq!(got, "claude-w2:p6");
        }
        assert_eq!(file_name("plain-id"), "plain-id");
    }

    #[cfg(windows)]
    #[test]
    fn a_childs_name_is_found_under_its_parent() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 3 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let names = child_names(std::process::id()).unwrap();
        assert!(
            names.iter().any(|n| n.eq_ignore_ascii_case("cmd.exe")),
            "{names:?}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_shell_line_runs_as_typed() {
        let out = shell("echo one&& echo two").output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).replace('\r', "");
        assert!(out.status.success());
        assert_eq!(text.split_whitespace().collect::<Vec<_>>(), ["one", "two"]);
    }

    #[test]
    fn random_bytes_are_random() {
        let a = random_bytes(32).unwrap();
        let b = random_bytes(32).unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn a_windows_path_is_one_command_word_for_every_shell() {
        assert_eq!(
            windows_command_word(r"C:\Users\u\.local\bin\herdr-tower.exe"),
            "C:/Users/u/.local/bin/herdr-tower.exe"
        );
        assert_eq!(
            windows_command_word(r"C:\Program Files\herdr-tower.exe"),
            "\"C:/Program Files/herdr-tower.exe\""
        );
    }

    #[test]
    fn the_basename_ignores_quotes_separators_and_exe() {
        for w in [
            "herdr-tower",
            "'/abs/herdr-tower'",
            "/opt/x/herdr-tower",
            "C:/Users/u/.local/bin/herdr-tower.exe",
            r"C:\Users\u\herdr-tower.EXE",
            "\"C:/Program Files/herdr-tower.exe\"",
        ] {
            assert_eq!(command_basename(w), "herdr-tower", "{w}");
        }
        assert_eq!(command_basename("/src/herdr-tower/other"), "other");
    }

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[test]
    fn a_finished_child_is_not_alive() {
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/c", "exit"])
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
    }

    #[test]
    fn terminate_ends_a_child() {
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/c", "ping -n 30 127.0.0.1 >nul"])
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap()
        };
        terminate(child.id()).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn a_private_file_is_owner_only_where_modes_exist() {
        let dir = std::env::temp_dir().join(format!("tower-os-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        let _ = std::fs::remove_file(&p);
        private(OpenOptions::new().write(true).create_new(true))
            .open(&p)
            .unwrap();
        let m = file_mode(&std::fs::metadata(&p).unwrap());
        if cfg!(unix) {
            assert_eq!(m, Some(0o600));
        } else {
            assert_eq!(m, None);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
