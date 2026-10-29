// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The local connection to a herdr server: a Unix socket on macOS and Linux, a
//! named pipe on Windows.
//!
//! Both sides offer the same small surface — a bounded connect, read and write
//! deadlines, a second handle for another thread, and a shutdown that wakes a
//! reader blocked on that other thread — so the protocol code above it is
//! written once.

#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(unix)]
mod unix {
    use std::io;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    pub(crate) type Conn = UnixStream;

    /// Opens the socket under the same deadline as the request.
    ///
    /// `UnixStream::connect` blocks with no bound of its own, and it used to
    /// run *before* the read and write timeouts were installed — so the
    /// advertised three seconds did not cover connection setup. A server whose
    /// accept backlog is full is exactly the case that matters: the tower polls
    /// every couple of seconds, and a `connect` that never returns freezes the
    /// whole screen rather than reporting an unreachable backend.
    ///
    /// `socket2` is here for its `connect_timeout`, which std has for TCP and
    /// not for Unix sockets. `None` means no deadline was asked for, and then
    /// the plain blocking connect is the honest answer.
    pub(crate) fn connect_within(path: &Path, timeout: Option<Duration>) -> io::Result<Conn> {
        let Some(t) = timeout else {
            return UnixStream::connect(path);
        };
        let addr = socket2::SockAddr::unix(path)?;
        let sock = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        sock.connect_timeout(&addr, t)?;
        // Both are the same file descriptor; `socket2` keeps no state of its own.
        Ok(UnixStream::from(std::os::unix::io::OwnedFd::from(sock)))
    }

    pub(crate) fn shutdown(c: &Conn) -> io::Result<()> {
        c.shutdown(Shutdown::Both)
    }
}

#[cfg(windows)]
mod windows {
    //! herdr on Windows listens on a named pipe whose name is its socket path,
    //! verbatim, under `\\.\pipe\` (interprocess's `GenericNamespaced`):
    //! `C:\Users\u\AppData\Roaming\herdr\herdr.sock` is the pipe
    //! `\\.\pipe\C:\Users\u\AppData\Roaming\herdr\herdr.sock`.
    //!
    //! The handle is opened for overlapped I/O. A synchronous pipe read cannot
    //! be given a deadline, and cannot be woken by another thread except
    //! through `CancelSynchronousIo` on the reader's own thread handle; with
    //! overlapped I/O each operation waits on its own event under a deadline,
    //! and `CancelIoEx` from any thread completes it.

    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
        ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// `None` stored as zero: no deadline.
    const NO_DEADLINE: u64 = 0;

    struct Inner {
        handle: HANDLE,
        closed: AtomicBool,
        read_ms: AtomicU64,
        write_ms: AtomicU64,
    }

    // The handle is a kernel object; every operation on it is thread-safe, and
    // each overlapped operation carries its own `OVERLAPPED` and event.
    unsafe impl Send for Inner {}
    unsafe impl Sync for Inner {}

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    /// One connection; clones share the handle, as `UnixStream::try_clone`
    /// shares the descriptor.
    #[derive(Clone)]
    pub(crate) struct Conn(Arc<Inner>);

    impl std::fmt::Debug for Conn {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Conn").finish_non_exhaustive()
        }
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The pipe name herdr listens on for `path`.
    pub(crate) fn pipe_name(path: &Path) -> String {
        format!(r"\\.\pipe\{}", path.display())
    }

    fn millis(d: Option<Duration>) -> u64 {
        match d {
            None => NO_DEADLINE,
            // A zero deadline is refused by `set_read_timeout` on Unix; here it
            // would read as "none", so it is at least one millisecond.
            Some(d) => (d.as_millis() as u64).max(1),
        }
    }

    /// Opens the pipe, retrying while every instance is busy, until `timeout`.
    pub(crate) fn connect_within(path: &Path, timeout: Option<Duration>) -> io::Result<Conn> {
        let name = wide(std::ffi::OsStr::new(&pipe_name(path)));
        let start = Instant::now();
        loop {
            let h = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if h != INVALID_HANDLE_VALUE {
                return Ok(Conn(Arc::new(Inner {
                    handle: h,
                    closed: AtomicBool::new(false),
                    read_ms: AtomicU64::new(NO_DEADLINE),
                    write_ms: AtomicU64::new(NO_DEADLINE),
                })));
            }
            let err = unsafe { GetLastError() };
            if err == ERROR_FILE_NOT_FOUND {
                // The Unix socket's ENOENT, so callers that say "herdr is not
                // running" keep saying it.
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no herdr pipe at {}", pipe_name(path)),
                ));
            }
            if err != ERROR_PIPE_BUSY {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            let left = match timeout {
                None => INFINITE,
                Some(t) => {
                    let Some(left) = t.checked_sub(start.elapsed()) else {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "every herdr pipe instance stayed busy",
                        ));
                    };
                    (left.as_millis() as u32).max(1)
                }
            };
            // Returns once an instance frees up or the wait ends; either way
            // the open is tried again and decides.
            unsafe { WaitNamedPipeW(name.as_ptr(), left) };
        }
    }

    pub(crate) fn shutdown(c: &Conn) -> io::Result<()> {
        c.0.closed.store(true, Ordering::SeqCst);
        // Completes any pending read or write, on whichever thread issued it.
        unsafe { CancelIoEx(c.0.handle, std::ptr::null()) };
        Ok(())
    }

    impl Conn {
        pub(crate) fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
            self.0.read_ms.store(millis(d), Ordering::SeqCst);
            Ok(())
        }

        pub(crate) fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
            self.0.write_ms.store(millis(d), Ordering::SeqCst);
            Ok(())
        }

        pub(crate) fn try_clone(&self) -> io::Result<Conn> {
            Ok(self.clone())
        }

        /// One overlapped operation under `deadline_ms`; `start` issues it.
        fn overlapped(
            &self,
            deadline_ms: u64,
            start: impl FnOnce(*mut OVERLAPPED) -> i32,
        ) -> io::Result<usize> {
            if self.0.closed.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the herdr connection was shut down",
                ));
            }
            let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            struct Event(HANDLE);
            impl Drop for Event {
                fn drop(&mut self) {
                    unsafe { CloseHandle(self.0) };
                }
            }
            let event = Event(event);
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = event.0;

            let ok = start(&mut ov);
            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err != ERROR_IO_PENDING {
                    return Err(map_err(err));
                }
                let wait = if deadline_ms == NO_DEADLINE {
                    INFINITE
                } else {
                    deadline_ms.min(u64::from(u32::MAX - 1)) as u32
                };
                if unsafe { WaitForSingleObject(event.0, wait) } == WAIT_TIMEOUT {
                    unsafe { CancelIoEx(self.0.handle, &ov) };
                    // The operation must finish before `ov` and its buffer go
                    // away; after the cancel this returns promptly.
                    let mut n = 0u32;
                    unsafe { GetOverlappedResult(self.0.handle, &ov, &mut n, 1) };
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the herdr server did not answer in time",
                    ));
                }
            }
            let mut n = 0u32;
            if unsafe { GetOverlappedResult(self.0.handle, &ov, &mut n, 1) } == 0 {
                let err = unsafe { GetLastError() };
                return Err(map_err(err));
            }
            Ok(n as usize)
        }
    }

    /// The pipe's errors in the vocabulary the Unix socket's callers expect.
    fn map_err(err: u32) -> io::Error {
        match err {
            // The server closed its end: a Unix read would return 0 here, and
            // the read side turns this into exactly that.
            ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => {
                io::Error::new(io::ErrorKind::BrokenPipe, "the herdr pipe closed")
            }
            ERROR_OPERATION_ABORTED => io::Error::new(
                io::ErrorKind::NotConnected,
                "the herdr connection was shut down",
            ),
            other => io::Error::from_raw_os_error(other as i32),
        }
    }

    impl Read for &Conn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            let ptr = buf.as_mut_ptr();
            let h = self.0.handle;
            match self.overlapped(self.0.read_ms.load(Ordering::SeqCst), |ov| unsafe {
                ReadFile(h, ptr, len, std::ptr::null_mut(), ov)
            }) {
                // End of stream, as a closed Unix socket reports it.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(0),
                other => other,
            }
        }
    }

    impl Write for &Conn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            let ptr = buf.as_ptr();
            let h = self.0.handle;
            self.overlapped(self.0.write_ms.load(Ordering::SeqCst), |ov| unsafe {
                WriteFile(h, ptr, len, std::ptr::null_mut(), ov)
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for Conn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&*self).read(buf)
        }
    }

    impl Write for Conn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&*self).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_pipe_name_is_the_socket_path_under_the_pipe_namespace() {
            assert_eq!(
                pipe_name(Path::new(r"C:\Users\u\AppData\Roaming\herdr\herdr.sock")),
                r"\\.\pipe\C:\Users\u\AppData\Roaming\herdr\herdr.sock"
            );
        }

        #[test]
        fn a_missing_pipe_is_not_found() {
            let e = connect_within(
                Path::new(r"C:\no\such\herdr-tower-test.sock"),
                Some(Duration::from_millis(200)),
            )
            .unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::NotFound);
        }
    }
}
