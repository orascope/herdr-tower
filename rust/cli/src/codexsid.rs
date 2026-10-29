// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! A codex record's session ID, found when codex's own hooks never say it.
//!
//! claude's session ID reaches the record from two sides: its hooks carry it,
//! and herdr reports it for the pane (`agent_session`). codex has only the
//! first. herdr leaves `agent_session` empty for a codex pane, and codex runs
//! a hook only once somebody has trusted it through `/hooks` — so a codex
//! started without that shows an empty SID column for its whole life, and
//! `resume` has nothing to resume (its records show `last_hook` at 1970).
//!
//! Two places still know the ID, and the supervisor asks them, in this order:
//!
//! 1. **The process's arguments.** `codex … resume <id>` names the session it
//!    continues. herdr reports the pane's foreground processes with their
//!    argv (`pane.process_info`), so this is exact and costs one socket call.
//! 2. **The rollout codex writes.** Every session opens
//!    `~/.codex/sessions/YYYY/MM/DD/rollout-<local time>-<id>.jsonl` with a
//!    `session_meta` line carrying its ID, its start time and its folder. The
//!    rollout this process is writing is the newest one, in exactly the
//!    record's folder, that started after the process did — a `/new` inside
//!    the same codex opens a newer one, and that newer one *is* the session
//!    now.
//!
//! **Refusing to guess.** Two codex panes in one folder cannot be told apart
//! by their rollouts: both started after both processes, perhaps, and one
//! whose ID is known — from its arguments or filled before — may `/new` into a
//! rollout its record never learns of. A record in a folder with any other
//! live codex is skipped — a wrong ID on a row is
//! worse than an empty one, because `resume` would open somebody else's
//! conversation. For the same reason a session ID another *live* record
//! already holds is never handed to a second one, and a rollout that is
//! not an interactive session of its own — `codex exec`, the Desktop app, a
//! sub-agent the TUI spawned (which is *newer* than its parent and would
//! otherwise win) — is never a candidate.
//!
//! **Cheap.** Nothing runs unless a live codex record has an empty ID, which is
//! only ever the case for a codex with silent hooks, and then at most once per
//! [`INTERVAL`] per backend. The rollout walk reads only the day folders from
//! the process's start to today, not every year and month, and each rollout's
//! first line is kept once read: codex writes it once and never again.
//!
//! **Never fails the pass**, and never overwrites an ID: the record is loaded
//! again under the store lock right before the write, so a hook that filled
//! it in the meantime wins.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use herdrx::{ForegroundProcess, Herdr, ProcessInfo};
use serde::Deserialize;
use time::{Date, OffsetDateTime, UtcOffset};
use tower_state::{Agent, Store};

/// How often each backend's codex records are looked at while one of them
/// still has no ID. The ID does not change under a running session except by
/// `/new`, and a row that fills half a minute late loses nothing.
pub const INTERVAL: Duration = Duration::from_secs(30);

/// How much earlier than the process a rollout may say it started and still
/// be that process's. codex stamps `session_meta` after it starts, so the
/// slack only has to cover `ps` rounding the elapsed time to a whole second
/// and the two clocks being read a moment apart.
pub const START_SLACK: time::Duration = time::Duration::seconds(5);

/// How much of a rollout's first line is read. The `session_meta` line
/// carries codex's whole base prompt (about 23 KB today); a line longer than
/// this is not a header this code knows how to read.
const HEAD_LIMIT: u64 = 1024 * 1024;

/// The widest span of day folders one lookup walks. A codex left running for
/// more than a year is still found if its rollout is in the last year's
/// folders; beyond that, the walk would be the one the day range avoids.
const MAX_DAYS: i64 = 366;

/// Where a filled ID came from, as the supervisor log says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Argv,
    Rollout,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Argv => "argv",
            Source::Rollout => "rollout",
        }
    }
}

/// An ID to write into a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub agent_id: String,
    pub pane_id: String,
    pub session_id: String,
    pub source: Source,
}

/// Whether this record is one to look for: a live codex with no ID.
///
/// A dead record is recovery data — its process is gone, so neither its
/// arguments nor "the newest rollout since it started" mean anything now.
pub fn needs_id(a: &Agent) -> bool {
    a.is_live() && a.kind == "codex" && a.session_id.is_empty()
}

/// codex's flags that take a value as the next word, global and `resume`'s
/// alike (`codex --help`, `codex resume --help`). Everything else starting
/// with `-` is a switch. Knowing these is what tells `-m resume` (a model
/// called `resume`) from the subcommand.
const VALUE_FLAGS: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "-i",
    "--image",
    "-m",
    "--model",
    "--local-provider",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-C",
    "--cd",
    "--add-dir",
    "-a",
    "--ask-for-approval",
];

/// The positional words of a codex argv (after the program), flags and their
/// values skipped. `--flag=value` carries its own value; everything after a
/// bare `--` is positional.
fn positionals(args: &[String]) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut rest = false;
    while i < args.len() {
        let word = args[i].as_str();
        if rest || !word.starts_with('-') || word == "-" {
            out.push((i, word));
        } else if word == "--" {
            rest = true;
        } else if VALUE_FLAGS.contains(&word) {
            i += 1;
        }
        i += 1;
    }
    out
}

/// The session a codex command line resumes: `codex [global flags] resume
/// [flags] <id>`.
///
/// Only `resume`. `codex fork <id>` starts a *new* session from that one, so
/// its argument is the parent's ID, not this process's — the rollout decides
/// there. `--last` and the picker name no ID in the arguments at all.
///
/// A UUID only. `resume` also takes a session *name*, which is not an ID and
/// would be stored as one; and the value has to pass
/// [`tower_state::valid_session_id`] like every ID the tower keeps.
pub fn resume_id(argv: &[String]) -> Option<&str> {
    let args = argv.get(1..)?;
    let words = positionals(args);
    let (at, sub) = *words.first()?;
    if sub != "resume" {
        return None;
    }
    let after = &args[at + 1..];
    if after.iter().any(|w| w == "--last") {
        return None;
    }
    let (_, id) = *positionals(after).first()?;
    (is_uuid(id) && tower_state::valid_session_id(id)).then_some(id)
}

/// `8-4-4-4-12` hex digits: the shape of every codex session ID.
fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn basename(s: &str) -> &str {
    Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(s)
}

/// The codex command line in one foreground process, as codex's own argv:
/// the native binary's, or the npm launcher's (`node …/bin/codex …`) with
/// `node` taken off.
fn codex_argv(p: &ForegroundProcess) -> Option<&[String]> {
    match p.argv.first().map(|w| basename(w)) {
        Some("codex") => Some(&p.argv),
        Some("node") if p.argv.get(1).is_some_and(|w| basename(w) == "codex") => Some(&p.argv[1..]),
        Some(_) => None,
        None => (basename(&p.argv0) == "codex" || p.name == "codex").then_some(&p.argv),
    }
}

/// The pane's codex process and its command line, if codex is in the
/// foreground. The one with an ID in its arguments first, so a launcher and
/// the binary it started agree whichever herdr lists first.
pub fn codex_process(info: &ProcessInfo) -> Option<(&ForegroundProcess, &[String])> {
    let mut found: Vec<_> = info
        .foreground_processes
        .iter()
        .filter_map(|p| codex_argv(p).map(|argv| (p, argv)))
        .collect();
    found.sort_by_key(|(_, argv)| resume_id(argv).is_none());
    found.into_iter().next()
}

/// `ps -o etime=`: `[[dd-]hh:]mm:ss`, in seconds.
///
/// Elapsed time rather than `lstart`: it is digits and separators in every
/// locale, and it needs no time zone — `lstart` is local wall-clock time,
/// which would have to be read back in whatever zone `ps` used.
pub fn parse_etime(s: &str) -> Option<u64> {
    let s = s.trim();
    // With days, `ps` always prints the hours too.
    let (days, clock) = match s.split_once('-') {
        Some((d, rest)) if rest.matches(':').count() == 2 => (d.parse::<u64>().ok()?, rest),
        Some(_) => return None,
        None => (0, s),
    };
    let parts: Vec<u64> = clock
        .split(':')
        .map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse().ok())
                .flatten()
        })
        .collect::<Option<_>>()?;
    let (h, m, sec) = match parts[..] {
        [m, sec] => (0, m, sec),
        [h, m, sec] => (h, m, sec),
        _ => return None,
    };
    (m < 60 && sec < 60 && (days == 0 || h < 24)).then_some(((days * 24 + h) * 60 + m) * 60 + sec)
}

/// When process `pid` started, from its elapsed time.
fn process_started(pid: i64, now: OffsetDateTime) -> Option<OffsetDateTime> {
    if pid <= 0 {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs = parse_etime(&String::from_utf8_lossy(&out.stdout))?;
    Some(now - time::Duration::seconds(i64::try_from(secs).ok()?))
}

/// The day folders a rollout started at `since` or later can be in:
/// `<root>/YYYY/MM/DD` from the day before `since` to the day after `today`,
/// both in local time.
///
/// codex names the folder by its own local date, and the offset this process
/// reads may not be the one codex read (a `TZ` in one environment and not the
/// other), so a day of margin on each side. At most [`MAX_DAYS`] folders.
pub fn day_folders(root: &Path, since: Date, today: Date) -> Vec<PathBuf> {
    let last = today.next_day().unwrap_or(today);
    let mut day = since.previous_day().unwrap_or(since);
    if (last - day).whole_days() > MAX_DAYS {
        day = last - time::Duration::days(MAX_DAYS);
    }
    let mut out = Vec::new();
    while day <= last {
        out.push(
            root.join(format!("{:04}", day.year()))
                .join(format!("{:02}", u8::from(day.month())))
                .join(format!("{:02}", day.day())),
        );
        let Some(next) = day.next_day() else { break };
        day = next;
    }
    out
}

/// What a rollout's `session_meta` line says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutMeta {
    pub session_id: String,
    pub cwd: String,
    pub started: OffsetDateTime,
    /// An interactive session of its own. `false` for `codex exec`, the
    /// Desktop app and the VS Code extension, and for a sub-agent the TUI
    /// spawned — see [`is_interactive`].
    pub interactive: bool,
}

#[derive(Deserialize)]
struct HeadLine {
    #[serde(rename = "type")]
    kind: String,
    payload: HeadPayload,
}

#[derive(Deserialize)]
struct HeadPayload {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    id: String,
    timestamp: String,
    cwd: String,
    #[serde(default)]
    originator: String,
    #[serde(default)]
    source: serde_json::Value,
    #[serde(default)]
    thread_source: Option<String>,
}

/// Whether a rollout is a session a terminal codex runs as itself.
///
/// A sub-agent's `source` is an object (`{"subagent": …}`) and its
/// `thread_source` is `subagent`; it starts after its parent, in the same
/// folder, so letting it through would hand the parent the child's ID. The
/// others are other front ends: they can share a folder with a terminal codex
/// and are never the process in the pane.
fn is_interactive(p: &HeadPayload) -> bool {
    let source_ok = match &p.source {
        serde_json::Value::String(s) => s != "exec",
        serde_json::Value::Null => true,
        _ => false,
    };
    source_ok
        && p.thread_source.as_deref() != Some("subagent")
        && !matches!(
            p.originator.as_str(),
            "codex_exec" | "Codex Desktop" | "codex_vscode"
        )
}

/// Reads a rollout's first line as JSON. `None` for anything that is not a
/// complete `session_meta` line with a valid ID and a readable start time.
pub fn read_meta(path: &Path) -> Option<RolloutMeta> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::new(file.take(HEAD_LIMIT))
        .read_until(b'\n', &mut line)
        .ok()?;
    // A line still being written has no newline yet; its JSON would not
    // parse either, but saying so here keeps a truncated read from counting.
    if line.last() != Some(&b'\n') {
        return None;
    }
    let head: HeadLine = serde_json::from_slice(&line).ok()?;
    if head.kind != "session_meta" {
        return None;
    }
    let p = head.payload;
    let session_id = if p.session_id.is_empty() {
        p.id.clone()
    } else {
        p.session_id.clone()
    };
    if !tower_state::valid_session_id(&session_id) {
        return None;
    }
    let started =
        OffsetDateTime::parse(&p.timestamp, &time::format_description::well_known::Rfc3339).ok()?;
    Some(RolloutMeta {
        interactive: is_interactive(&p),
        session_id,
        cwd: p.cwd,
        started,
    })
}

/// The rollout a codex that started at `since` in `cwd` is writing now: the
/// newest interactive one whose folder is exactly `cwd` and which started no
/// earlier than `since` less [`START_SLACK`], leaving out IDs another live
/// record holds. Ties on the start time go to the greater ID, so the answer does not
/// depend on the order the folders were listed in.
pub fn pick_rollout<'a>(
    metas: &'a [RolloutMeta],
    cwd: &str,
    since: OffsetDateTime,
    taken: &HashSet<String>,
) -> Option<&'a str> {
    metas
        .iter()
        .filter(|m| {
            m.interactive
                && m.cwd == cwd
                && m.started >= since - START_SLACK
                && !taken.contains(&m.session_id)
        })
        .max_by(|a, b| {
            a.started
                .cmp(&b.started)
                .then_with(|| a.session_id.cmp(&b.session_id))
        })
        .map(|m| m.session_id.as_str())
}

/// One record looked at: what its pane's process said.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub agent_id: String,
    pub pane_id: String,
    pub cwd: String,
    /// `None` when the process could not be read at all, or named no ID.
    pub argv_id: Option<String>,
    /// `None` when the process's start could not be learned.
    pub started: Option<OffsetDateTime>,
}

/// What to write, from the probes and the rollouts.
///
/// IDs from arguments first: they are exact, and each one taken is one the
/// rollout step must not hand out again. Then the rollout step, only for a
/// folder with exactly one live codex in it — the probe itself. Every other
/// probe there counts, named by argv or not, and so does each folder in
/// `others`: records that need an ID but were not probed this time, and
/// live codex records that already have one.
///
/// `taken` is the IDs live records hold. Not a dead record's: a session that
/// ended in one pane is resumed in another all the time (`codex resume <id>`
/// names exactly that), and a dead record's ID is not evidence of anything
/// now — records that outlived their pane have been seen picking up a live
/// session's ID from a hook.
pub fn decide(
    probes: &[Probe],
    others: &[&str],
    taken: &HashSet<String>,
    mut rollouts: impl FnMut(&Probe, OffsetDateTime) -> Vec<RolloutMeta>,
) -> Vec<Found> {
    let mut taken = taken.clone();
    let mut out = Vec::new();
    for p in probes {
        if let Some(id) = &p.argv_id {
            if taken.insert(id.clone()) {
                out.push(found(p, id, Source::Argv));
            }
        }
    }
    // Every live codex in the folder counts, the ones named by argv or
    // already filled too: any of them may `/new` into a rollout its own
    // record never learns of, and that rollout must not go to this one.
    let mut wanting: HashMap<&str, usize> = HashMap::new();
    for cwd in probes
        .iter()
        .map(|p| p.cwd.as_str())
        .chain(others.iter().copied())
    {
        *wanting.entry(cwd).or_default() += 1;
    }
    for p in probes.iter().filter(|p| p.argv_id.is_none()) {
        let Some(since) = p.started else { continue };
        if p.cwd.is_empty() || wanting.get(p.cwd.as_str()) != Some(&1) {
            continue;
        }
        let metas = rollouts(p, since);
        if let Some(id) = pick_rollout(&metas, &p.cwd, since, &taken) {
            let id = id.to_string();
            taken.insert(id.clone());
            out.push(found(p, &id, Source::Rollout));
        }
    }
    out
}

fn found(p: &Probe, id: &str, source: Source) -> Found {
    Found {
        agent_id: p.agent_id.clone(),
        pane_id: p.pane_id.clone(),
        session_id: id.to_string(),
        source,
    }
}

/// The session IDs live records hold — the `taken` of [`decide`].
pub fn live_ids(agents: &[Agent]) -> HashSet<String> {
    agents
        .iter()
        .filter(|a| a.is_live() && !a.session_id.is_empty())
        .map(|a| a.session_id.clone())
        .collect()
}

/// Writes one found ID, if the record still wants it: re-read under the
/// store lock, and left alone unless it is still the same live codex in the
/// same pane with an empty ID. Says whether it wrote.
pub fn apply(store: &Store, f: &Found, cwd: &str) -> std::io::Result<bool> {
    let _lock = store.lock();
    let mut a = store.load(&f.agent_id)?;
    if !needs_id(&a) || a.pane.pane_id != f.pane_id || a.cwd != cwd {
        return Ok(false);
    }
    a.session_id = f.session_id.clone();
    store.save_if_changed(&a)
}

/// The supervisor's side: when each backend was last looked at, and the
/// rollout headers already read.
#[derive(Default)]
pub struct Filler {
    last: HashMap<String, Instant>,
    heads: HashMap<PathBuf, RolloutMeta>,
}

impl Filler {
    /// What this pass would write, without writing it: the records that need
    /// an ID on backends due for a look, probed and decided. `clients` maps a
    /// record's `pane.backend` to its connection; a record whose backend is
    /// not connected is not probed (and still counts against its folder).
    pub fn find(
        &mut self,
        agents: &[Agent],
        clients: &HashMap<String, Herdr>,
        root: &Path,
        now: OffsetDateTime,
        clock: Instant,
    ) -> Vec<(Found, String)> {
        let needy: Vec<&Agent> = agents.iter().filter(|a| needs_id(a)).collect();
        if needy.is_empty() {
            return Vec::new();
        }
        let due: HashSet<&str> = needy
            .iter()
            .map(|a| a.pane.backend.as_str())
            .filter(|b| clients.contains_key(*b))
            .filter(|b| {
                self.last
                    .get(*b)
                    .is_none_or(|at| clock.duration_since(*at) >= INTERVAL)
            })
            .collect();
        if due.is_empty() {
            return Vec::new();
        }
        for b in &due {
            self.last.insert(b.to_string(), clock);
        }
        let mut probes = Vec::new();
        let mut others = Vec::new();
        for a in &needy {
            match clients.get(&a.pane.backend) {
                Some(h) if due.contains(a.pane.backend.as_str()) => {
                    probes.push(probe(a, h.process_info(&a.pane.pane_id).ok(), now))
                }
                _ => others.push(a.cwd.as_str()),
            }
        }
        // A codex that has its ID shares the folder all the same.
        others.extend(
            agents
                .iter()
                .filter(|a| a.is_live() && a.kind == "codex" && !a.session_id.is_empty())
                .map(|a| a.cwd.as_str()),
        );
        let taken = live_ids(agents);
        let offset = tower_state::local_offset();
        let mut seen = HashSet::new();
        let heads = &mut self.heads;
        let found = decide(&probes, &others, &taken, |_, since| {
            rollouts_since(root, since, now, offset, heads, &mut seen)
        });
        heads.retain(|path, _| seen.contains(path));
        let cwd_of: HashMap<&str, &str> = probes
            .iter()
            .map(|p| (p.agent_id.as_str(), p.cwd.as_str()))
            .collect();
        found
            .into_iter()
            .map(|f| {
                let cwd = cwd_of.get(f.agent_id.as_str()).copied().unwrap_or_default();
                (f, cwd.to_string())
            })
            .collect()
    }

    /// [`Filler::find`], then the writes. Returns the lines worth logging.
    /// A write that fails is tried again on the next look, and said nowhere:
    /// the pass has its own trouble reporting for a store it cannot write.
    pub fn tick(
        &mut self,
        store: &Store,
        agents: &[Agent],
        clients: &HashMap<String, Herdr>,
        root: &Path,
        now: OffsetDateTime,
        clock: Instant,
    ) -> Vec<String> {
        self.find(agents, clients, root, now, clock)
            .into_iter()
            .filter(|(f, cwd)| matches!(apply(store, f, cwd), Ok(true)))
            .map(|(f, _)| {
                format!(
                    "codex session id for {} found in {}",
                    f.pane_id,
                    f.source.label()
                )
            })
            .collect()
    }
}

/// A record and what its pane's process said.
fn probe(a: &Agent, info: Option<ProcessInfo>, now: OffsetDateTime) -> Probe {
    let mut p = Probe {
        agent_id: a.id.clone(),
        pane_id: a.pane.pane_id.clone(),
        cwd: a.cwd.clone(),
        argv_id: None,
        started: None,
    };
    if let Some((process, argv)) = info.as_ref().and_then(codex_process) {
        p.argv_id = resume_id(argv).map(String::from);
        if p.argv_id.is_none() {
            p.started = process_started(process.pid, now);
        }
    }
    p
}

/// Every rollout header in the day folders from `since` to `now`, read once
/// and kept in `heads`; `seen` collects the paths still there.
fn rollouts_since(
    root: &Path,
    since: OffsetDateTime,
    now: OffsetDateTime,
    offset: UtcOffset,
    heads: &mut HashMap<PathBuf, RolloutMeta>,
    seen: &mut HashSet<PathBuf>,
) -> Vec<RolloutMeta> {
    let mut out = Vec::new();
    for day in day_folders(
        root,
        since.to_offset(offset).date(),
        now.to_offset(offset).date(),
    ) {
        let Ok(entries) = std::fs::read_dir(&day) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if !path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".jsonl"))
            {
                continue;
            }
            seen.insert(path.clone());
            // Only a header that read whole is kept: one caught mid-write is
            // read again next time rather than remembered as nothing.
            let meta = match heads.get(&path) {
                Some(m) => Some(m.clone()),
                None => read_meta(&path).inspect(|m| {
                    heads.insert(path.clone(), m.clone());
                }),
            };
            out.extend(meta);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    const ID: &str = "01a0dcef-5eea-7840-9406-289b10119e50";
    const ID2: &str = "01a0dcf2-7366-7741-90bc-863f424ee260";
    const ID3: &str = "01a0dd26-8686-7c30-9dcd-05571fca42f6";

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn resume_id_is_read_through_global_and_resume_flags() {
        let cases: &[(&[&str], Option<&str>)] = &[
            (&["codex", "resume", ID], Some(ID)),
            (
                &[
                    "codex",
                    "-s",
                    "danger-full-access",
                    "-a",
                    "never",
                    "resume",
                    ID,
                ],
                Some(ID),
            ),
            (&["/opt/bin/codex", "--search", "resume", ID], Some(ID)),
            (&["codex", "resume", "-m", "o3", "--all", ID], Some(ID)),
            (
                &["codex", "-c", "model=\"o3\"", "resume", ID, "go on"],
                Some(ID),
            ),
            (&["codex", "--model=o3", "resume", "--", ID], Some(ID)),
            // `-m resume` is a model called `resume`, not the subcommand.
            (&["codex", "-m", "resume", ID], None),
            (&["codex", "resume", "--last"], None),
            (&["codex", "resume", "--last", ID], None),
            (&["codex", "resume"], None),
            (&["codex"], None),
            (&["codex", "-s", "danger-full-access"], None),
            // A fork's argument is its parent, not this session.
            (&["codex", "fork", ID], None),
            // A prompt that happens to say `resume` second is not the subcommand.
            (&["codex", "fix it", "resume", ID], None),
            // A session name, not an ID.
            (&["codex", "resume", "my-work"], None),
            (
                &["codex", "resume", "01a0dcef-5eea-7840-9406-289b10119e50;rm"],
                None,
            ),
            (
                &["codex", "resume", "01a0dcef-5eea-7840-9406-289b10119e5;"],
                None,
            ),
            (
                &["codex", "resume", "zzzzzzzz-5eea-7840-9406-289b10119e50"],
                None,
            ),
            (&[], None),
        ];
        for (words, want) in cases {
            assert_eq!(resume_id(&argv(words)), *want, "{words:?}");
        }
    }

    fn process(argv_words: &[&str], argv0: &str, name: &str, pid: i64) -> ForegroundProcess {
        ForegroundProcess {
            argv: argv(argv_words),
            argv0: argv0.into(),
            name: name.into(),
            pid,
            ..Default::default()
        }
    }

    #[test]
    fn the_codex_process_is_found_by_its_program_or_its_launcher() {
        let info = |ps: Vec<ForegroundProcess>| ProcessInfo {
            foreground_processes: ps,
            ..Default::default()
        };
        let native = info(vec![process(&["/x/codex", "resume", ID], "", "", 7)]);
        let (p, a) = codex_process(&native).unwrap();
        assert_eq!((p.pid, resume_id(a)), (7, Some(ID)));

        // The npm launcher and the binary it runs, launcher listed first
        // with no ID visible: the one naming the ID wins.
        let pair = info(vec![
            process(&["node", "/usr/local/bin/codex"], "", "node", 1),
            process(&["/x/codex-darwin/codex", "resume", ID], "", "codex", 2),
        ]);
        assert_eq!(codex_process(&pair).unwrap().0.pid, 2);
        let launcher = info(vec![process(
            &["node", "/usr/local/bin/codex", "resume", ID],
            "",
            "node",
            3,
        )]);
        let (p, a) = codex_process(&launcher).unwrap();
        assert_eq!((p.pid, resume_id(a)), (3, Some(ID)));

        // No argv: argv0 or the name says codex.
        let bare = info(vec![process(&[], "/x/codex", "", 4)]);
        assert_eq!(codex_process(&bare).unwrap().0.pid, 4);

        let other = info(vec![
            process(&["zsh"], "zsh", "zsh", 5),
            process(&["node", "server.js"], "node", "node", 6),
            process(&["codexx"], "codexx", "codexx", 8),
        ]);
        assert!(codex_process(&other).is_none());
        assert!(codex_process(&ProcessInfo::default()).is_none());
    }

    #[test]
    fn etime_reads_every_ps_shape_and_nothing_else() {
        assert_eq!(parse_etime("   00:05"), Some(5));
        assert_eq!(parse_etime("12:34\n"), Some(754));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(
            parse_etime("2-03:04:05"),
            Some(2 * 86400 + 3 * 3600 + 4 * 60 + 5)
        );
        assert_eq!(parse_etime("123-00:00:00"), Some(123 * 86400));
        for bad in [
            "",
            "5",
            "1:60",
            "1:2:3:4",
            "a:b",
            "-1:00",
            "1-",
            "1-2:3",
            "01:-2",
            "x-01:02:03",
            "1-24:00:00",
        ] {
            assert_eq!(parse_etime(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_real_ps_reads_this_process_as_just_started() {
        let now = OffsetDateTime::now_utc();
        let Some(started) = process_started(i64::from(std::process::id()), now) else {
            return; // no `ps` here
        };
        assert!(started <= now && now - started < time::Duration::hours(1));
        assert_eq!(process_started(0, now), None);
    }

    #[test]
    fn day_folders_span_the_start_to_today_with_a_day_either_side() {
        let root = Path::new("/r");
        let got = day_folders(root, date!(2026 - 09 - 30), date!(2026 - 10 - 01));
        let want: Vec<PathBuf> = ["2026/09/29", "2026/09/30", "2026/10/01", "2026/10/02"]
            .iter()
            .map(|d| root.join(d))
            .collect();
        assert_eq!(got, want);
        // Across a year.
        let got = day_folders(root, date!(2026 - 12 - 31), date!(2026 - 12 - 31));
        assert_eq!(got.first().unwrap(), &root.join("2026/12/30"));
        assert_eq!(got.last().unwrap(), &root.join("2027/01/01"));
        // A start after today (a clock that moved back) still covers today.
        assert_eq!(
            day_folders(root, date!(2026 - 09 - 27), date!(2026 - 09 - 26)),
            [root.join("2026/09/26"), root.join("2026/09/27")]
        );
        // Capped.
        let got = day_folders(root, date!(2020 - 01 - 01), date!(2026 - 09 - 26));
        assert_eq!(got.len() as i64, MAX_DAYS + 1);
        assert_eq!(got.last().unwrap(), &root.join("2026/09/27"));
    }

    fn meta(id: &str, cwd: &str, started: OffsetDateTime) -> RolloutMeta {
        RolloutMeta {
            session_id: id.into(),
            cwd: cwd.into(),
            started,
            interactive: true,
        }
    }

    #[test]
    fn the_newest_rollout_in_exactly_this_folder_since_the_start_wins() {
        let start = datetime!(2026-09-26 08:57:48 UTC);
        let secs = time::Duration::seconds;
        let none = HashSet::new();
        let mut metas = vec![
            meta("before", "/a/b", start - secs(6)),
            meta("slack", "/a/b", start - secs(4)),
            meta("prefix", "/a/bc", start + secs(60)),
            meta("parent", "/a", start + secs(60)),
            meta("child", "/a/b/c", start + secs(60)),
        ];
        assert_eq!(pick_rollout(&metas, "/a/b", start, &none), Some("slack"));

        // A `/new` later in the same codex: the newest is the session now.
        metas.push(meta("new", "/a/b", start + secs(30)));
        metas.push(meta("newer", "/a/b", start + secs(40)));
        assert_eq!(pick_rollout(&metas, "/a/b", start, &none), Some("newer"));

        // Held by another record: the next newest.
        let taken: HashSet<String> = ["newer".to_string()].into();
        assert_eq!(pick_rollout(&metas, "/a/b", start, &taken), Some("new"));

        // Not interactive (a sub-agent, exec): never.
        let mut sub = meta("sub", "/a/b", start + secs(50));
        sub.interactive = false;
        metas.push(sub);
        assert_eq!(pick_rollout(&metas, "/a/b", start, &none), Some("newer"));

        // Only older ones: nothing.
        assert_eq!(pick_rollout(&metas[..1], "/a/b", start, &none), None);
        assert_eq!(pick_rollout(&[], "/a/b", start, &none), None);

        // A tie on the start: the greater ID, whatever the order.
        let t = start + secs(1);
        let tie = vec![meta("x1", "/t", t), meta("x2", "/t", t)];
        let mut rev = tie.clone();
        rev.reverse();
        assert_eq!(pick_rollout(&tie, "/t", start, &none), Some("x2"));
        assert_eq!(pick_rollout(&rev, "/t", start, &none), Some("x2"));
    }

    /// A rollout's opening, as codex writes it: a long `session_meta` line
    /// (the base prompt rides in it), then the turns.
    fn head(id: &str, cwd: &str, ts: &str, extra: &str) -> String {
        let cwd = serde_json::to_string(cwd).unwrap();
        let prompt = "x".repeat(30_000);
        format!(
            "{{\"timestamp\":\"{ts}\",\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{id}\",\"id\":\"{id}\",\"timestamp\":\"{ts}\",\"cwd\":{cwd}{extra},\"base_instructions\":{{\"text\":\"{prompt}\"}}}}}}\n{{\"type\":\"turn\"}}\n"
        )
    }

    #[test]
    fn a_rollout_head_is_parsed_as_json_and_anything_else_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        let cwd = "/Users/q/it's \"quoted\"\\dir";
        let ok = write(
            "ok.jsonl",
            &head(
                ID,
                cwd,
                "2026-09-26T08:57:50.085Z",
                ",\"originator\":\"codex-tui\",\"source\":\"cli\"",
            ),
        );
        let m = read_meta(&ok).unwrap();
        assert_eq!(m.session_id, ID);
        assert_eq!(m.cwd, cwd);
        assert_eq!(m.started, datetime!(2026-09-26 08:57:50.085 UTC));
        assert!(m.interactive);

        for extra in [
            ",\"source\":{\"subagent\":\"review\"}",
            ",\"source\":\"cli\",\"thread_source\":\"subagent\"",
            ",\"source\":\"exec\"",
            ",\"originator\":\"Codex Desktop\",\"source\":\"vscode\"",
            ",\"originator\":\"codex_exec\",\"source\":\"cli\"",
        ] {
            let p = write("other.jsonl", &head(ID, cwd, "2026-09-26T08:57:50Z", extra));
            assert!(!read_meta(&p).unwrap().interactive, "{extra}");
        }

        let full = head(ID, cwd, "2026-09-26T08:57:50Z", "");
        for (name, body) in [
            ("empty.jsonl", String::new()),
            ("half.jsonl", full[..500].to_string()),
            ("noeol.jsonl", full[..full.find('\n').unwrap()].to_string()),
            ("notjson.jsonl", "not json\n".to_string()),
            (
                "turn.jsonl",
                "{\"type\":\"turn\",\"payload\":{}}\n".to_string(),
            ),
            ("badts.jsonl", head(ID, cwd, "yesterday", "")),
            ("badid.jsonl", head("a;b", cwd, "2026-09-26T08:57:50Z", "")),
        ] {
            assert_eq!(read_meta(&write(name, &body)), None, "{name}");
        }
        assert_eq!(read_meta(&dir.path().join("missing.jsonl")), None);
    }

    fn probe_of(
        id: &str,
        cwd: &str,
        argv_id: Option<&str>,
        started: Option<OffsetDateTime>,
    ) -> Probe {
        Probe {
            agent_id: id.into(),
            pane_id: format!("pane-{id}"),
            cwd: cwd.into(),
            argv_id: argv_id.map(String::from),
            started,
        }
    }

    fn ids(found: &[Found]) -> Vec<(&str, &str, Source)> {
        found
            .iter()
            .map(|f| (f.agent_id.as_str(), f.session_id.as_str(), f.source))
            .collect()
    }

    #[test]
    fn two_unnamed_codexes_in_one_folder_are_both_left_alone() {
        let start = datetime!(2026-09-26 08:00:00 UTC);
        let later = start + time::Duration::minutes(1);
        let rollouts = |_: &Probe, _: OffsetDateTime| {
            vec![
                meta(ID, "/a", later),
                meta(ID2, "/b", later),
                meta(ID3, "/c", later),
            ]
        };
        let none = HashSet::new();

        // Alone in its folder: the rollout.
        let got = decide(
            &[probe_of("x", "/a", None, Some(start))],
            &[],
            &none,
            rollouts,
        );
        assert_eq!(ids(&got), [("x", ID, Source::Rollout)]);

        // Two in /a with no argv ID: neither. /b is unaffected.
        let probes = [
            probe_of("x", "/a", None, Some(start)),
            probe_of("y", "/a", None, Some(start)),
            probe_of("z", "/b", None, Some(start)),
        ];
        assert_eq!(
            ids(&decide(&probes, &[], &none, rollouts)),
            [("z", ID2, Source::Rollout)]
        );

        // One whose start is unknown still counts against its folder.
        let probes = [
            probe_of("x", "/a", None, Some(start)),
            probe_of("y", "/a", None, None),
        ];
        assert!(decide(&probes, &[], &none, rollouts).is_empty());

        // So does a record that was not probed at all.
        assert!(decide(&probes[..1], &["/a"], &none, rollouts).is_empty());

        // Both named by argv: both filled, no rollout consulted.
        let probes = [
            probe_of("x", "/a", Some(ID2), None),
            probe_of("y", "/a", Some(ID3), None),
        ];
        let got = decide(&probes, &[], &none, |_, _| panic!("no rollout needed"));
        assert_eq!(
            ids(&got),
            [("x", ID2, Source::Argv), ("y", ID3, Source::Argv)]
        );

        // One named by argv, one not: the named one is filled, and the
        // other is skipped — the named codex may `/new` into a rollout its
        // record never learns of, and the other would be handed it.
        let rollouts_a = |_: &Probe, _: OffsetDateTime| {
            vec![
                meta(ID, "/a", later),
                meta(ID2, "/a", later + time::Duration::seconds(1)),
            ]
        };
        let probes = [
            probe_of("x", "/a", Some(ID2), None),
            probe_of("y", "/a", None, Some(start)),
        ];
        assert_eq!(
            ids(&decide(&probes, &[], &none, rollouts_a)),
            [("x", ID2, Source::Argv)]
        );

        // A live codex that already has its ID (passed in `others`) and ran
        // `/new`: its newer rollout is in nobody's `taken`, and the empty
        // record beside it must not get it.
        let after_new = |_: &Probe, _: OffsetDateTime| vec![meta(ID3, "/a", later)];
        let taken: HashSet<String> = [ID.to_string()].into();
        assert!(decide(
            &[probe_of("y", "/a", None, Some(start))],
            &["/a"],
            &taken,
            after_new
        )
        .is_empty());

        // An argv ID another record already holds is not written twice.
        let taken: HashSet<String> = [ID2.to_string()].into();
        assert!(decide(
            &[probe_of("x", "/a", Some(ID2), None)],
            &[],
            &taken,
            rollouts
        )
        .is_empty());
        // No folder: nothing to match a rollout against.
        assert!(decide(
            &[probe_of("x", "", None, Some(start))],
            &[],
            &none,
            rollouts
        )
        .is_empty());
    }

    fn agent(id: &str, kind: &str, state: tower_state::AgentState, sid: &str) -> Agent {
        let mut a = Agent::new(id, kind, datetime!(2026-09-26 08:00:00 UTC));
        a.state = state;
        a.session_id = sid.into();
        a.cwd = "/a".into();
        a.pane.pane_id = format!("w1:{id}");
        a
    }

    #[test]
    fn only_a_live_codex_with_an_empty_id_is_looked_for() {
        use tower_state::AgentState::*;
        assert!(needs_id(&agent("a", "codex", Working, "")));
        assert!(needs_id(&agent("a", "codex", Idle, "")));
        assert!(!needs_id(&agent("a", "codex", Dead, "")));
        assert!(!needs_id(&agent("a", "codex", Working, ID)));
        assert!(!needs_id(&agent("a", "claude", Working, "")));
        assert!(!needs_id(&agent("a", "gemini", Working, "")));

        // Only a live record's ID is off limits to another: a dead one's may
        // be resumed, or be wrong.
        let agents = [
            agent("a", "codex", Working, ID),
            agent("b", "codex", Dead, ID2),
            agent("c", "claude", Idle, ID3),
            agent("d", "codex", Working, ""),
        ];
        let want: HashSet<String> = [ID.to_string(), ID3.to_string()].into();
        assert_eq!(live_ids(&agents), want);
    }

    #[test]
    fn nothing_is_asked_while_no_record_can_be_looked_at() {
        use tower_state::AgentState::*;
        let mut f = Filler::default();
        let agents = [
            agent("a", "codex", Working, ID),
            agent("b", "codex", Dead, ""),
            agent("c", "claude", Working, ""),
            // Needs one, but its backend is not connected.
            agent("d", "codex", Working, ""),
        ];
        let got = f.find(
            &agents,
            &HashMap::new(),
            Path::new("/nonexistent"),
            datetime!(2026-09-26 08:00:00 UTC),
            Instant::now(),
        );
        assert!(got.is_empty());
        assert!(f.last.is_empty());
    }

    #[test]
    fn a_found_id_is_written_only_into_a_record_that_still_wants_it() {
        use tower_state::AgentState::*;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path()).unwrap();
        let f = |agent_id: &str| Found {
            agent_id: agent_id.into(),
            pane_id: format!("w1:{agent_id}"),
            session_id: ID.into(),
            source: Source::Rollout,
        };

        store.save(&agent("a", "codex", Working, "")).unwrap();
        assert!(apply(&store, &f("a"), "/a").unwrap());
        assert_eq!(store.load("a").unwrap().session_id, ID);
        // Already filled (by us, or by a hook in between): left alone.
        let other = Found {
            session_id: ID2.into(),
            ..f("a")
        };
        assert!(!apply(&store, &other, "/a").unwrap());
        assert_eq!(store.load("a").unwrap().session_id, ID);

        store.save(&agent("b", "codex", Working, ID3)).unwrap();
        assert!(!apply(&store, &f("b"), "/a").unwrap());
        assert_eq!(store.load("b").unwrap().session_id, ID3);

        store.save(&agent("c", "codex", Dead, "")).unwrap();
        assert!(!apply(&store, &f("c"), "/a").unwrap());

        // The pane or the folder moved since the look: not this one's ID.
        store.save(&agent("d", "codex", Working, "")).unwrap();
        let moved = Found {
            pane_id: "w9:p9".into(),
            ..f("d")
        };
        assert!(!apply(&store, &moved, "/a").unwrap());
        assert!(!apply(&store, &f("d"), "/elsewhere").unwrap());
        assert_eq!(store.load("d").unwrap().session_id, "");

        assert!(apply(&store, &f("missing"), "/a").is_err());
    }

    #[test]
    fn rollouts_are_read_from_the_day_folders_only_and_heads_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let put = |day: &str, id: &str, ts: &str| {
            let d = root.join(day);
            std::fs::create_dir_all(&d).unwrap();
            let p = d.join(format!("rollout-2026-09-26T08-00-00-{id}.jsonl"));
            std::fs::write(&p, head(id, "/a", ts, "")).unwrap();
            p
        };
        let today = put("2026/09/26", ID, "2026-09-26T09:00:00Z");
        put("2026/09/25", ID2, "2026-09-25T23:00:00Z");
        // Outside the range: never read, whatever it says.
        put("2026/09/20", ID3, "2026-09-26T10:00:00Z");
        std::fs::write(root.join("2026/09/26/notes.txt"), "x").unwrap();

        let now = datetime!(2026-09-26 12:00:00 UTC);
        let since = datetime!(2026-09-26 08:00:00 UTC);
        let mut heads = HashMap::new();
        let mut seen = HashSet::new();
        let mut got = rollouts_since(root, since, now, UtcOffset::UTC, &mut heads, &mut seen);
        got.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let got_ids: Vec<&str> = got.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(got_ids, [ID, ID2]);
        assert_eq!((heads.len(), seen.len()), (2, 2));

        // Kept: a header rewritten on disk is not read again.
        std::fs::write(&today, "garbage\n").unwrap();
        let again = rollouts_since(
            root,
            since,
            now,
            UtcOffset::UTC,
            &mut heads,
            &mut HashSet::new(),
        );
        assert_eq!(again.len(), 2);

        // And end to end: the picker takes the newest in the folder.
        assert_eq!(pick_rollout(&got, "/a", since, &HashSet::new()), Some(ID));
    }

    /// Against this machine, read-only: the real records, the real herdr
    /// servers and the real `~/.codex/sessions`, printing what a pass would
    /// write and writing nothing. The records are read straight from their
    /// files rather than through [`Store::new`], which would create and
    /// narrow directories.
    ///
    /// `cargo test -p tower_cli --lib codexsid::tests::live_dry_run -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the real state dir, herdr and ~/.codex; run by hand"]
    fn live_dry_run() {
        let home = tower_usage::home_dir().expect("HOME");
        let agents: Vec<Agent> = std::fs::read_dir(tower_state::default_dir().join("agents"))
            .expect("the state dir")
            .flatten()
            .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
            .collect();
        let mut clients = HashMap::new();
        for backend in tower_mux::discover_backends(&home).expect("herdr sockets") {
            let h = backend.client();
            if h.ping().is_ok() {
                clients.insert(backend.id.clone(), h);
            }
        }
        for a in agents.iter().filter(|a| needs_id(a)) {
            eprintln!("needs an id: {} ({}) in {}", a.id, a.pane.pane_id, a.cwd);
        }
        let found = Filler::default().find(
            &agents,
            &clients,
            &tower_usage::codex_sessions_root(&home),
            OffsetDateTime::now_utc(),
            Instant::now(),
        );
        for (f, cwd) in &found {
            eprintln!(
                "would set {} ({}) session_id = {} from {} [cwd {cwd}]",
                f.agent_id,
                f.pane_id,
                f.session_id,
                f.source.label()
            );
        }
        eprintln!("{} record(s) would be filled", found.len());
    }
}
