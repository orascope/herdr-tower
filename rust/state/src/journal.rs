// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! A log of every state change, for looking back.
//!
//! The records say where each agent stands now; nothing said how it got there,
//! so "what happened overnight" had no answer. [`crate::Store::save`] appends a
//! line here whenever a record's state differs from the one on disk — so every
//! writer is covered, hook, scan and a person marking a turn read alike,
//! without any of them having to remember to.
//!
//! Best-effort by design: a line that cannot be written must not fail the save
//! that produced it. Bounded like the supervisor's log — past
//! [`JOURNAL_MAX`] it keeps its last [`JOURNAL_KEEP`].

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Agent, AgentState};

/// The journal's file name in the state directory.
pub const JOURNAL_FILE: &str = "transitions.jsonl";

/// Size past which the journal is cut back.
pub const JOURNAL_MAX: u64 = 4 * 1024 * 1024;

/// What is kept when it is: a week or more of a busy machine.
pub const JOURNAL_KEEP: u64 = 2 * 1024 * 1024;

/// One state change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub id: String,
    pub kind: String,
    /// The herdr workspace label at the time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tab: String,
    pub from: AgentState,
    pub to: AgentState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    /// The question, for a move into WAIT; the reason, into ERR.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ask: String,
}

impl Entry {
    pub fn new(a: &Agent, from: AgentState) -> Self {
        Entry {
            at: a.state_since,
            id: a.id.clone(),
            kind: a.kind.clone(),
            session: a.pane.session.clone(),
            tab: a.pane.tab.clone(),
            from,
            to: a.state,
            task: a.task.clone(),
            ask: a.ask.clone(),
        }
    }
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(JOURNAL_FILE)
}

/// Appends one entry, cutting the file back when it has grown past
/// [`JOURNAL_MAX`]. Errors are the caller's to ignore.
pub fn append(dir: &Path, e: &Entry) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(e).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let p = path(dir);
    // One `write` of one short line on an append-mode file: concurrent hooks
    // interleave whole lines, not bytes.
    // Appends share the lock; a trim takes it alone, so no line lands between
    // its read of the tail and its rewrite of the file. Best effort: where
    // locking is unsupported the journal is still written.
    //
    // The lock is a file of its own beside the journal. Windows locks are
    // mandatory: a shared lock on the journal itself refused this very write
    // (`ERROR_LOCK_VIOLATION`), and an exclusive one would refuse every reader.
    let lock = lock_file(&p);
    if let Some(l) = &lock {
        let _ = l.lock_shared();
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&p)?;
    f.write_all(&line)?;
    let len = f.metadata()?.len();
    drop(f);
    drop(lock);
    if len > JOURNAL_MAX {
        trim(&p)?;
    }
    Ok(())
}

/// Keeps the last [`JOURNAL_KEEP`] bytes, from a line boundary — unless
/// another writer already did while this one waited for the lock.
fn trim(p: &Path) -> std::io::Result<()> {
    let lock = lock_file(p);
    if let Some(l) = &lock {
        let _ = l.lock();
    }
    let mut f = fs::OpenOptions::new().read(true).write(true).open(p)?;
    let len = f.metadata()?.len();
    if len <= JOURNAL_MAX {
        return Ok(());
    }
    f.seek(SeekFrom::Start(len.saturating_sub(JOURNAL_KEEP)))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    let start = tail.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    f.set_len(0)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&tail[start..])
}

/// The journal's lock, `<journal>.lock`; `None` if it cannot be opened, and
/// then the journal goes unlocked rather than unwritten.
fn lock_file(journal: &Path) -> Option<fs::File> {
    let mut name = journal.as_os_str().to_owned();
    name.push(".lock");
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(PathBuf::from(name))
        .ok()
}

/// Every entry at or after `since`, oldest first. Lines that do not parse — a
/// torn write, an older shape — are passed over.
pub fn read_since(dir: &Path, since: OffsetDateTime) -> Vec<Entry> {
    let Ok(text) = fs::read_to_string(path(dir)) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| e.at >= since)
        .collect();
    out.sort_by_key(|e| e.at);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mins: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::minutes(mins)
    }

    fn entry(mins: i64, to: AgentState) -> Entry {
        let mut a = Agent::new("claude-w1:p1", "claude", at(mins));
        a.state = to;
        a.state_since = at(mins);
        Entry::new(&a, AgentState::Idle)
    }

    #[test]
    fn entries_round_trip_and_filter_by_time() {
        let d = tempfile::tempdir().unwrap();
        append(d.path(), &entry(0, AgentState::Working)).unwrap();
        append(d.path(), &entry(5, AgentState::DoneUnread)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(path(d.path()))
            .unwrap()
            .write_all(b"{torn\n")
            .unwrap();
        let got = read_since(d.path(), at(1));
        assert_eq!(got, [entry(5, AgentState::DoneUnread)]);
        assert_eq!(read_since(d.path(), at(0)).len(), 2);
    }

    #[test]
    fn trimming_keeps_whole_lines_from_the_end() {
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        let line = b"x".repeat(99);
        let mut body = Vec::new();
        while body.len() as u64 <= JOURNAL_MAX {
            body.extend_from_slice(&line);
            body.push(b'\n');
        }
        fs::write(&p, &body).unwrap();
        append(d.path(), &entry(0, AgentState::Working)).unwrap();
        let kept = fs::read_to_string(&p).unwrap();
        assert!(kept.len() as u64 <= JOURNAL_KEEP);
        assert!(kept.lines().all(|l| l.len() == 99 || l.starts_with('{')));
        assert_eq!(
            read_since(d.path(), at(0)).len(),
            1,
            "the new entry survives"
        );
    }

    /// A second writer that also saw the file past the limit finds it already
    /// cut once it has the lock, and leaves it alone: trimming again would drop
    /// the first line even though it is whole.
    #[test]
    fn a_trim_that_finds_the_file_already_cut_leaves_it_alone() {
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        for body in [
            &b""[..],
            &b"one\n"[..],
            &b"no newline at all"[..],
            &b"a\nb\nc\n"[..],
        ] {
            fs::write(&p, body).unwrap();
            trim(&p).unwrap();
            assert_eq!(fs::read(&p).unwrap(), body);
        }
        let exact = vec![b'x'; JOURNAL_MAX as usize];
        fs::write(&p, &exact).unwrap();
        trim(&p).unwrap();
        assert_eq!(
            fs::read(&p).unwrap().len() as u64,
            JOURNAL_MAX,
            "at the limit, not past it"
        );
    }

    /// Past the limit by a byte is past it: the trim cuts back to whole lines
    /// within [`JOURNAL_KEEP`], and cutting again changes nothing.
    #[test]
    fn a_trim_past_the_limit_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        let mut body = Vec::new();
        let mut i = 0u64;
        while body.len() as u64 <= JOURNAL_MAX {
            body.extend_from_slice(format!("line {i}\n").as_bytes());
            i += 1;
        }
        fs::write(&p, &body).unwrap();
        trim(&p).unwrap();
        let once = fs::read(&p).unwrap();
        assert!(once.len() as u64 <= JOURNAL_KEEP);
        assert!(
            body.ends_with(&once),
            "the kept part is the file's own tail"
        );
        assert!(once.starts_with(b"line "), "from a line boundary");
        trim(&p).unwrap();
        assert_eq!(fs::read(&p).unwrap(), once);
    }

    /// An append waits while a trim holds the journal alone, and lands once
    /// it lets go — never inside the trim's rewrite.
    #[test]
    fn an_append_waits_for_a_trim_in_progress() {
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        fs::write(&p, b"").unwrap();
        let held = lock_file(&p).unwrap();
        held.lock().unwrap();
        let dir = d.path().to_path_buf();
        let writer = std::thread::spawn(move || append(&dir, &entry(0, AgentState::Working)));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            fs::read(&p).unwrap().len(),
            0,
            "written under the trim's lock"
        );
        drop(held);
        writer.join().unwrap().unwrap();
        assert_eq!(read_since(d.path(), at(0)), [entry(0, AgentState::Working)]);
    }

    /// Many writers at once, trims included: every line in the journal is a
    /// whole entry, and it ends within its bounds.
    #[test]
    fn concurrent_appends_and_trims_leave_only_whole_lines() {
        let d = tempfile::tempdir().unwrap();
        let p = path(d.path());
        // Start just under the limit so the writers' own lines set off trims.
        let pad = format!("{}\n", "p".repeat(1023));
        let mut body = String::new();
        while (body.len() + pad.len()) as u64 <= JOURNAL_MAX {
            body.push_str(&pad);
        }
        fs::write(&p, &body).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let dir = d.path().to_path_buf();
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let mut e = entry(t * 1000 + i, AgentState::Working);
                        e.task = "t".repeat(4096);
                        append(&dir, &e).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.len() as u64 <= JOURNAL_MAX + 8 * 5000,
            "{}",
            text.len()
        );
        assert!(text.ends_with('\n'));
        for l in text.lines() {
            assert!(
                l == pad.trim_end() || serde_json::from_str::<Entry>(l).is_ok(),
                "a torn line: {:.60}",
                l
            );
        }
        assert!(!read_since(d.path(), at(0)).is_empty());
    }
}
