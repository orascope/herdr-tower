// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The resident supervisor's event log: `<state>/supervisor.log`.
//!
//! `daemon.json` says how the supervisor is *now*, and every pass overwrites
//! it — so by the time anyone looks, the error that stopped an idle loop at
//! three in the morning is gone. This file keeps what happened, one line per
//! event with its time. Events, not passes: a backend that stays down is one
//! line when it goes and one when it comes back, not one every two seconds.
//!
//! Its own file rather than the `daemon.log` launchd writes stderr into. That
//! descriptor is launchd's, and rotating the file under it would send the rest
//! of this process's stderr into the old copy. Linux has no such file at all —
//! the unit's stderr goes to the journal — so this is the one place both share.
//!
//! It stays small: past [`MAX_BYTES`] the file becomes `supervisor.log.1`,
//! replacing the previous one, and a new file starts.
//!
//! Best effort throughout. A log that cannot be written must never be the
//! reason the supervisor stops; the line is dropped and the next one tries to
//! open the file again.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The log's name in the state directory.
pub const LOG_FILE: &str = "supervisor.log";

/// Where the log rolls over. At a few lines an hour this is months of events,
/// and small enough to read whole.
pub const MAX_BYTES: u64 = 1024 * 1024;

/// An open event log.
#[derive(Debug)]
pub struct Log {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max: u64,
}

impl Log {
    /// The log in `state_dir`, appended to.
    pub fn open(state_dir: &Path) -> Self {
        Self::with_limit(state_dir, MAX_BYTES)
    }

    fn with_limit(state_dir: &Path, max: u64) -> Self {
        let mut log = Log {
            path: state_dir.join(LOG_FILE),
            file: None,
            size: 0,
            max,
        };
        log.reopen();
        log
    }

    /// Where the log is written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The log's last line, without its time — read from the file's end, so a
    /// log of any size costs a few kilobytes.
    pub fn last_event(&self) -> Option<String> {
        const TAIL: u64 = 4096;
        let mut f = File::open(&self.path).ok()?;
        let len = f.metadata().ok()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).ok()?;
        // Bytes, then lossy: the seek can land inside a character, and this
        // log is full of `·`, `—` and Korean names.
        let tail = String::from_utf8_lossy(&bytes);
        // Started mid-file, the first line is a fragment and cannot be read
        // as an event; a last line longer than the whole tail is not known.
        let whole = if len > TAIL {
            tail.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &tail
        };
        let line = whole.lines().rev().find(|l| !l.trim().is_empty())?;
        Some(
            line.split_once(' ')
                .map_or(line, |(_, event)| event)
                .to_string(),
        )
    }

    /// Writes one event, stamped with the local time where it is known.
    pub fn line(&mut self, message: &str) {
        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
        self.line_at(now, message);
    }

    /// Writes one event, stamped with `at`.
    pub fn line_at(&mut self, at: OffsetDateTime, message: &str) {
        let stamp = at
            .replace_nanosecond(0)
            .ok()
            .and_then(|at| at.format(&Rfc3339).ok())
            .unwrap_or_default();
        // One event, one line — an error that carries its own line breaks
        // would otherwise read as several events, the later ones undated.
        let text = format!("{stamp} {}\n", message.trim().replace('\n', " / "));
        if self.size > 0 && self.size + text.len() as u64 > self.max {
            let _ = fs::rename(&self.path, rolled(&self.path));
            self.file = None;
        }
        if self.file.is_none() {
            self.reopen();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(text.as_bytes()).is_ok() {
            self.size += text.len() as u64;
        } else {
            self.file = None;
        }
    }

    /// Opens the file for appending: owner-only, like every record beside it,
    /// because a line can name a pane, a folder or what an agent was sent.
    fn reopen(&mut self) {
        self.file = tower_os::private(OpenOptions::new().create(true).append(true))
            .open(&self.path)
            .ok();
        self.size = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }
}

/// The previous log, kept beside the current one.
fn rolled(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-22T00:00:00Z, `secs` later.
    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_790_035_200 + secs).unwrap()
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn a_line_is_the_time_and_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::open(dir.path());
        let kst = time::UtcOffset::from_hms(9, 0, 0).unwrap();
        let stamp = at(2 * 3600 + 40 * 60 + 5)
            .replace_nanosecond(123_000_000)
            .unwrap()
            .to_offset(kst);
        log.line_at(stamp, "supervisor started");
        assert_eq!(
            read(&dir.path().join(LOG_FILE)),
            "2026-09-22T11:40:05+09:00 supervisor started\n"
        );
    }

    /// An error with its own line breaks is still one event.
    #[test]
    fn an_event_with_line_breaks_stays_on_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::open(dir.path());
        log.line_at(at(0), "failed:\nfirst\nsecond\n");
        assert_eq!(
            read(&dir.path().join(LOG_FILE)),
            "2026-09-22T00:00:00Z failed: / first / second\n"
        );
    }

    /// A restarted supervisor adds to the log rather than starting it over —
    /// the lines before a crash are the ones worth reading.
    #[test]
    fn a_new_supervisor_appends() {
        let dir = tempfile::tempdir().unwrap();
        Log::open(dir.path()).line_at(at(0), "one");
        Log::open(dir.path()).line_at(at(60), "two");
        assert_eq!(
            read(&dir.path().join(LOG_FILE)),
            "2026-09-22T00:00:00Z one\n2026-09-22T00:01:00Z two\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Log::open(dir.path()).line_at(at(0), "x");
        let mode = fs::metadata(dir.path().join(LOG_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    /// Past the limit the log rolls over once: the old lines move to `.1`,
    /// replacing whatever was there, and the new line starts a fresh file.
    #[test]
    fn a_full_log_rolls_over_to_one_previous_copy() {
        let dir = tempfile::tempdir().unwrap();
        let t = at(0);
        // Each line is 26 bytes: 20 of stamp, a space, the word, a newline.
        let mut log = Log::with_limit(dir.path(), 60);
        log.line_at(t, "aaaa");
        log.line_at(t, "bbbb");
        log.line_at(t, "cccc");
        let current = read(&dir.path().join(LOG_FILE));
        let previous = read(&dir.path().join("supervisor.log.1"));
        assert_eq!(current, "2026-09-22T00:00:00Z cccc\n");
        assert_eq!(
            previous,
            "2026-09-22T00:00:00Z aaaa\n2026-09-22T00:00:00Z bbbb\n"
        );

        log.line_at(t, "dddd");
        log.line_at(t, "eeee");
        assert_eq!(
            read(&dir.path().join("supervisor.log.1")),
            "2026-09-22T00:00:00Z cccc\n2026-09-22T00:00:00Z dddd\n",
            "one previous copy, not a growing pile"
        );
    }

    /// However the last 4 KiB happen to start — inside a character, inside a
    /// line — the last event is read whole.
    #[test]
    fn the_last_event_survives_a_tail_that_starts_mid_character() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::open(dir.path());
        for i in 0..400 {
            log.line_at(at(i), &format!("herdr 세션 · {i} — 연결됨"));
        }
        log.line_at(at(500), "supervisor stopped: done");
        assert_eq!(
            log.last_event().as_deref(),
            Some("supervisor stopped: done")
        );
        for pad in 0..4 {
            log.line_at(at(600), &"·".repeat(pad + 1));
            log.line_at(at(601), "supervisor started: pid 9 · v");
            assert_eq!(
                log.last_event().as_deref(),
                Some("supervisor started: pid 9 · v")
            );
        }
    }

    #[test]
    fn the_last_event_is_read_without_its_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::open(dir.path());
        assert_eq!(log.last_event(), None, "nothing logged yet");
        log.line_at(at(0), "supervisor started: pid 1");
        log.line_at(at(60), "supervisor stopped: disk full");
        assert_eq!(
            log.last_event().as_deref(),
            Some("supervisor stopped: disk full")
        );
    }

    /// A state directory that is not there yet costs the lines written while
    /// it is missing, and nothing else: the log opens once it exists.
    #[test]
    fn a_missing_directory_drops_lines_until_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("later");
        let mut log = Log::open(&state);
        log.line_at(at(0), "lost");
        fs::create_dir(&state).unwrap();
        log.line_at(at(60), "kept");
        assert_eq!(read(&state.join(LOG_FILE)), "2026-09-22T00:01:00Z kept\n");
    }

    // ── g4testadd ────────────────────────────────────────────────────────

    /// Round trip: whatever is logged is what the last event reads back —
    /// folded onto one line and trimmed, but otherwise the same, for text in
    /// any script.
    #[test]
    fn what_is_logged_is_what_the_last_event_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::open(dir.path());
        for (i, message) in [
            "supervisor started: pid 1 · v0.17.1",
            "  padded on both sides  ",
            "herdr 세션 — 연결됨 🗼",
            "two\nlines",
            "trailing newline\n",
            "a",
        ]
        .into_iter()
        .enumerate()
        {
            log.line_at(at(i as i64), message);
            let folded = message.trim().replace('\n', " / ");
            assert_eq!(
                log.last_event().as_deref(),
                Some(folded.as_str()),
                "{message:?}"
            );
        }
    }

    /// Edge: an empty event is still a line, read back as empty.
    #[test]
    fn an_empty_event_reads_back_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::open(dir.path());
        log.line_at(at(0), "");
        assert_eq!(log.last_event().as_deref(), Some(""));
    }

    /// Property: however many lines go in, neither file grows past the limit
    /// by more than the line that tipped it.
    #[test]
    fn neither_copy_outgrows_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::with_limit(dir.path(), 300);
        let mut longest = 0;
        for i in 0..200 {
            let message = "x".repeat(i % 37);
            longest = longest.max(21 + message.len() + 1);
            log.line_at(at(i as i64), &message);
            for name in [LOG_FILE, "supervisor.log.1"] {
                let size = fs::metadata(dir.path().join(name)).map_or(0, |m| m.len());
                assert!(
                    size <= 300 + longest as u64,
                    "{name} is {size} bytes after line {i}"
                );
            }
        }
    }
}
