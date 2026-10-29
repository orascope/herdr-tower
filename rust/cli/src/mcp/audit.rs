// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! A record of every remote action: who sent what to which agent, and what
//! came of it.
//!
//! A file of its own, `mcp-audit.jsonl`, rather than lines in the transition
//! journal. Every journal line is a state change with a `from` and a `to`,
//! and `report` folds them as such; an action is not one. `report` reads both
//! and shows the actions beside the transitions.
//!
//! Best-effort like the journal: a line that cannot be written does not fail
//! the action. Bounded the same way.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const FILE: &str = "mcp-audit.jsonl";
/// Past this size the log is cut back to its last [`KEEP`].
pub const MAX: u64 = 1024 * 1024;
pub const KEEP: u64 = 512 * 1024;
/// How much of the text sent is kept.
pub const TEXT_KEPT: usize = 200;

/// One remote action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The grant that made the call (`mcp grants` lists them).
    pub grant: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client: String,
    pub tool: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    /// The first [`TEXT_KEPT`] characters of what was sent, if anything was.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// `sent`, `settled`, `refused: …` — what came of it.
    pub outcome: String,
}

/// `text` cut to what the log keeps, on a character boundary.
pub fn kept(text: &str) -> String {
    let mut out: String = text.chars().take(TEXT_KEPT).collect();
    if text.chars().count() > TEXT_KEPT {
        out.push('…');
    }
    out
}

/// The notification-channel line for a remote action:
/// `📱 send_prompt → api: "go ahead" — sent (Claude)`.
pub fn line(e: &Entry) -> String {
    let place = if e.session.is_empty() {
        &e.agent
    } else {
        &e.session
    };
    let mut text: String = e.text.chars().take(80).collect();
    if e.text.chars().count() > 80 {
        text.push('…');
    }
    let text = text.replace(['\n', '\r'], " ").replace('`', "'");
    let who = if e.client.is_empty() {
        String::new()
    } else {
        format!(" ({})", discord_escape(&e.client))
    };
    // A `tower` run has no agent: the command is the whole story.
    let head = if place.is_empty() {
        format!("📱 {}", e.tool)
    } else {
        format!("📱 {} → {place}", e.tool)
    };
    if text.is_empty() {
        format!("{head} — {}{who}", e.outcome)
    } else {
        format!("{head}: `{text}` — {}{who}", e.outcome)
    }
}

/// Whether a remote action is announced in the notification channel as well
/// as written to the log. Not a `tower config …` run: settings are not the
/// channel's business, and a value set there — a webhook, a URL — is
/// exactly what should not be posted to it (#27). The audit line keeps the
/// command, with the value redacted.
pub fn announced(e: &Entry) -> bool {
    !(e.tool == "tower" && (e.text == "config" || e.text.starts_with("config ")))
}

/// Text a client chose, made inert for Discord markdown: every character
/// that could start formatting, a link or a mention is backslash-escaped.
///
/// A client's name is whatever it registered with (`/register` is open to
/// the internet). `allowed_mentions` already keeps it from pinging anyone,
/// but `[Claude](https://…)` rendered as a link in the channel the person
/// trusts for these notices (#27). An escaped `:` also keeps a bare URL
/// from turning into a link.
pub fn discord_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            continue;
        }
        if matches!(
            c,
            '\\' | '*'
                | '_'
                | '~'
                | '`'
                | '|'
                | '>'
                | '<'
                | '['
                | ']'
                | '('
                | ')'
                | '#'
                | '-'
                | '@'
                | ':'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The notification-channel line for a new grant (#26): a pairing that
/// turned into a connection. A connection nobody expected shows up the
/// moment it exists.
pub fn grant_line(client: &str, act: bool) -> String {
    let client = if client.is_empty() {
        "(unnamed client)".to_string()
    } else {
        discord_escape(client)
    };
    format!(
        "🔗 new remote MCP connection: {client} (act: {}) — `herdr-tower mcp grants` lists it, `mcp revoke <id>` drops it",
        if act { "yes" } else { "no" }
    )
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Appends one entry. Errors are returned for the caller to ignore or log;
/// nothing here panics.
///
/// The append, the size check and a trim happen under one `flock`
/// (`mcp-audit.lock`). The server writes from several request threads, and a
/// trim replaces the file: without the lock a line appended meanwhile went
/// into the file being replaced and was lost, and two trims at once raced on
/// the same temporary file (#105).
pub fn append(dir: &Path, e: &Entry) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let _lock = super::grants::Lock::acquire(&dir.join("mcp-audit.lock"))?;
    let p = path(dir);
    let mut line = serde_json::to_vec(e)?;
    line.push(b'\n');
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&p)?.write_all(&line)?;
    if fs::metadata(&p)?.len() > MAX {
        trim(&p)?;
    }
    Ok(())
}

/// Keeps the last [`KEEP`] bytes, starting at a whole line.
fn trim(p: &Path) -> std::io::Result<()> {
    let mut f = fs::File::open(p)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(KEEP)))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    let start = tail.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
    let tmp = p.with_extension("jsonl.tmp");
    // Private like the log it replaces. `fs::write` creates with the umask's
    // mode (0644 on a usual Mac), and the rename would hand that mode to the
    // log — world-readable from the first trim on (#113). The mode on the open
    // applies only on creation, so it is set again before the rename, as
    // `grants.rs` does for its own file.
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&tmp)?.write_all(&tail[start..])?;
    }
    #[cfg(unix)]
    fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    fs::rename(tmp, p)
}

/// The entries at or after `since`, oldest first. Unreadable lines are skipped.
pub fn read_since(dir: &Path, since: OffsetDateTime) -> Vec<Entry> {
    let Ok(body) = fs::read_to_string(path(dir)) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| e.at >= since)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::seconds(secs)
    }

    fn entry(secs: i64, text: &str) -> Entry {
        Entry {
            at: at(secs),
            grant: "g1".into(),
            client: "Claude".into(),
            tool: "send_prompt".into(),
            agent: "claude-w1:p1".into(),
            session: "api".into(),
            text: text.into(),
            outcome: "sent".into(),
        }
    }

    #[test]
    fn entries_come_back_from_their_window() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &entry(0, "old")).unwrap();
        append(dir.path(), &entry(100, "new")).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(path(dir.path()))
            .unwrap()
            .write_all(b"{torn line\n")
            .unwrap();
        let got = read_since(dir.path(), at(50));
        assert_eq!(got, vec![entry(100, "new")], "the torn line is skipped");
        assert!(read_since(tempfile::tempdir().unwrap().path(), at(0)).is_empty());
    }

    #[test]
    fn the_log_is_cut_back_to_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(TEXT_KEPT);
        let mut n = 0;
        while fs::metadata(path(dir.path())).map_or(0, |m| m.len()) <= KEEP {
            append(dir.path(), &entry(n, &big)).unwrap();
            n += 1;
        }
        for i in 0..(MAX / 200) as i64 {
            append(dir.path(), &entry(n + i, &big)).unwrap();
        }
        let len = fs::metadata(path(dir.path())).unwrap().len();
        assert!(len <= MAX, "{len}");
        let all = read_since(dir.path(), at(0));
        let lines = fs::read_to_string(path(dir.path()))
            .unwrap()
            .lines()
            .count();
        assert_eq!(all.len(), lines, "every line kept is whole");
    }

    #[test]
    fn the_channel_line_names_the_place_the_text_and_the_outcome() {
        let mut e = entry(0, "go ahead\nwith `rm`");
        assert_eq!(
            line(&e),
            "📱 send_prompt → api: `go ahead with 'rm'` — sent (Claude)"
        );
        e.session.clear();
        e.client.clear();
        e.text.clear();
        assert_eq!(line(&e), "📱 send_prompt → claude-w1:p1 — sent");
        e.text = "x".repeat(100);
        assert!(line(&e).contains(&format!("`{}…`", "x".repeat(80))));
        e.tool = "tower".into();
        e.agent.clear();
        e.text = "loop start api".into();
        e.outcome = "exit 0".into();
        assert_eq!(line(&e), "📱 tower: `loop start api` — exit 0");
    }

    #[test]
    fn text_is_kept_short() {
        assert_eq!(kept("short"), "short");
        let long = "가".repeat(TEXT_KEPT + 5);
        let k = kept(&long);
        assert_eq!(k.chars().count(), TEXT_KEPT + 1);
        assert!(k.ends_with('…'));
    }

    /// #105: appends from many threads while the log is being trimmed lose
    /// nothing. What survives a trim is the newest part of the log in the
    /// order it was written, so each thread's surviving lines must be an
    /// unbroken run ending at its last line — a line written into a file that
    /// a trim was replacing would leave a gap, or lose the last one.
    #[test]
    fn concurrent_appends_survive_trims() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        const THREADS: usize = 8;
        const EACH: usize = 1500;
        let pad = "x".repeat(TEXT_KEPT);
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let d = dir_path.clone();
                let pad = pad.clone();
                std::thread::spawn(move || {
                    for i in 0..EACH {
                        let mut e = entry(0, &pad);
                        e.grant = format!("t{t}");
                        e.outcome = i.to_string();
                        append(&d, &e).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let body = fs::read_to_string(path(&dir_path)).unwrap();
        assert!(
            (THREADS * EACH * 250) as u64 > MAX,
            "enough writing to trim at least once"
        );
        let mut seen: Vec<Vec<usize>> = vec![Vec::new(); THREADS];
        for line in body.lines() {
            let e: Entry = serde_json::from_str(line).expect("every kept line is whole");
            let t: usize = e.grant[1..].parse().unwrap();
            seen[t].push(e.outcome.parse().unwrap());
        }
        for (t, got) in seen.iter().enumerate() {
            if got.is_empty() {
                continue;
            }
            let first = got[0];
            let want: Vec<usize> = (first..EACH).collect();
            assert_eq!(got, &want, "thread {t} lost lines");
        }
    }

    /// #113: a trim keeps the log private. `fs::write` would create the
    /// replacement with the umask's mode (0644 here), and the rename hands
    /// that mode to the log — world-readable from the first trim on.
    #[cfg(unix)]
    #[test]
    fn a_trim_keeps_the_log_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(TEXT_KEPT);
        let mut n = 0;
        let mode = |d: &Path| fs::metadata(path(d)).unwrap().permissions().mode() & 0o777;
        loop {
            append(dir.path(), &entry(n, &big)).unwrap();
            n += 1;
            if n == 1 {
                assert_eq!(mode(dir.path()), 0o600, "created private");
            }
            // A trim has happened once the file is back under MAX after passing KEEP.
            if n > 10
                && fs::metadata(path(dir.path())).unwrap().len() <= KEEP + 1024
                && n as u64 * 250 > MAX
            {
                break;
            }
        }
        assert_eq!(mode(dir.path()), 0o600, "still private after a trim");
    }

    /// #27: a client's name cannot carry markdown into the channel.
    #[test]
    fn a_client_name_is_inert_in_the_channel() {
        let mut e = entry(0, "go");
        e.client = "[Claude](https://evil.example) **ok** @everyone".into();
        let l = line(&e);
        assert!(
            l.ends_with(r"(\[Claude\]\(https\://evil.example\) \*\*ok\*\* \@everyone)"),
            "{l}"
        );
        assert_eq!(discord_escape("a_b\n`c`"), r"a\_b\`c\`");
        assert_eq!(discord_escape("Claude"), "Claude");
    }

    /// #27: `tower config …` goes to the log, never to the channel.
    #[test]
    fn config_runs_are_not_announced() {
        let mut e = entry(0, "config set notify_webhook_url <redacted>");
        e.tool = "tower".into();
        assert!(!announced(&e));
        e.text = "config".into();
        assert!(!announced(&e));
        e.text = "configure".into();
        assert!(announced(&e), "only the config command itself");
        e.text = "status --json".into();
        assert!(announced(&e));
        assert!(announced(&entry(0, "config set x y")), "send_prompt text");
    }

    #[test]
    fn a_new_grant_is_announced_with_its_rights() {
        assert!(grant_line("Claude", true)
            .starts_with("🔗 new remote MCP connection: Claude (act: yes)"));
        assert!(grant_line("", false)
            .starts_with("🔗 new remote MCP connection: (unnamed client) (act: no)"));
        assert!(grant_line("*x*", false).contains(r"\*x\*"));
    }
}
