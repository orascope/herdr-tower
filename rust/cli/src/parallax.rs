// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Parallax Browser's handoffs, read for the tower to show — and only read.
//!
//! Parallax is a separate local browser whose own agents hand work back to the
//! person ("handoffs"). The tower is the one place that shows who is waiting on
//! the person, so a Parallax agent asking for a human belongs on it beside a
//! claude sitting at a question. Nothing more than that: the tower does not
//! answer, resolve, drive or proxy anything in Parallax, and registers none of
//! its tools anywhere.
//!
//! How it reads, and why that way:
//!
//! - **Through Parallax's local API, the way Parallax meant it to be read.**
//!   `GET /workspaces`, then `GET /handoffs?openOnly=1&workspaceId=<id>` for
//!   each one. An unnamed `GET /handoffs` is refused by design (only the
//!   Parallax shell reads unnamed), and `handoffs.json` on disk is Parallax's
//!   own store; reading either would step around its entitlement model, which
//!   is also why a row Parallax *redacts* is shown with no title rather than
//!   looked up some other way.
//! - **127.0.0.1 only, short timeouts, no redirects, no proxy.** The bearer
//!   token is a local secret; a redirect or an environment `HTTP_PROXY` would
//!   carry it somewhere else. It is read fresh on every poll (Parallax may
//!   rotate it) and never logged, printed or written anywhere.
//! - **Absent is silent.** No data root, no token, nothing listening: that is
//!   a machine without Parallax running, not a fault, and the result is only
//!   "not available".
//! - **Lenient.** A row that does not parse is skipped, never the end of the
//!   poll; unknown fields are ignored and an unknown status is kept as words
//!   but asks for nobody.
//!
//! The resident supervisor polls on a thread of its own ([`Refresher`]) and
//! writes `parallax.json` ([`tower_state::ParallaxStore`]); everything that
//! shows handoffs reads that file.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use tower_state::{
    sort_handoffs, Attention, NotifiedHandoff, ParallaxHandoff, ParallaxSnapshot, ParallaxStore,
};

/// Parallax's port when `api-port` is missing.
pub const DEFAULT_PORT: u16 = 8765;

/// One request's whole budget. Parallax answers these from memory in
/// milliseconds; a second and a half is a Parallax that is not answering.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);

/// The whole poll's budget: a Parallax that answers each request just inside
/// its timeout must still not hold the poll for minutes across many
/// workspaces. What is left unread when it runs out counts as unreadable.
const POLL_BUDGET: Duration = Duration::from_secs(5);

/// How often the supervisor asks. Not every two-second pass: a handoff is a
/// person-scale wait, and this is somebody else's server.
pub const POLL_EVERY: Duration = Duration::from_secs(10);

/// No answer this API gives is anywhere near this long; one that is, is not
/// read.
const BODY_LIMIT: u64 = 2 * 1024 * 1024;

/// Bounds on what one poll keeps, so a runaway Parallax cannot make the
/// snapshot — read by the tab bar every five seconds — grow without end.
const MAX_WORKSPACES: usize = 64;
const MAX_HANDOFFS: usize = 200;
const MAX_TITLE_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 60;

/// How many new handoffs are announced one by one in a poll before the rest
/// are folded into a single notice.
const NOTICE_EACH: usize = 3;

/// How long an announced ID is remembered, and how many are.
const NOTIFIED_KEEP: time::Duration = time::Duration::days(30);
const NOTIFIED_MAX: usize = 1000;

/// Parallax's data root: `$PARALLAX_DATA_ROOT`, else `~/.parallax`.
pub fn data_root(home: &Path, env: Option<OsString>) -> PathBuf {
    match env {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(".parallax"),
    }
}

/// [`data_root`] for this process.
pub fn data_root_here(home: &Path) -> PathBuf {
    data_root(home, std::env::var_os("PARALLAX_DATA_ROOT"))
}

/// What one poll found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// No data root: Parallax is not installed here.
    NotInstalled,
    /// Installed, but not answering or not letting us read. The reason never
    /// carries the token.
    Unreachable(String),
    Read {
        /// Workspaces whose handoffs were read.
        workspaces: usize,
        /// Workspaces whose handoffs could not be.
        unreadable: usize,
        /// Open handoffs with some attention, most urgent first.
        handoffs: Vec<ParallaxHandoff>,
    },
}

/// Where the API is and the key to it.
struct Endpoint {
    port: u16,
    token: String,
}

// No derived `Debug`: a stray `{:?}` must not be able to print the token.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("port", &self.port)
            .field("token", &"<hidden>")
            .finish()
    }
}

/// Reads the port and the token from the data root, fresh.
fn endpoint(root: &Path) -> Result<Endpoint, Reading> {
    if !root.is_dir() {
        return Err(Reading::NotInstalled);
    }
    let port = match std::fs::read_to_string(root.join("api-port")) {
        Ok(text) => match text.trim().parse::<u16>() {
            Ok(p) if p != 0 => p,
            _ => return Err(Reading::Unreachable("api-port does not hold a port".into())),
        },
        Err(_) => DEFAULT_PORT,
    };
    let token = std::fs::read_to_string(root.join("api-token"))
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    // Visible ASCII only: anything else cannot be a header value, and a
    // newline in it would be one header smuggled into another.
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Reading::Unreachable("no usable api-token".into()));
    }
    Ok(Endpoint { port, token })
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .max_redirects(0)
            // Not the environment's proxy: that would hand the token to it.
            .proxy(None)
            .build(),
    )
}

/// One authorized GET against the API. The error is a fixed sentence plus a
/// status: ureq's own messages carry the URL, and nothing here may carry the
/// token, so neither is passed through.
fn get(agent: &ureq::Agent, ep: &Endpoint, path_and_query: &str) -> Result<String, String> {
    let resp = agent
        .get(format!("http://127.0.0.1:{}{path_and_query}", ep.port))
        .header("Authorization", &format!("Bearer {}", ep.token))
        .header("Accept", "application/json")
        .call()
        .map_err(|_| "not answering".to_string())?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(match status {
            401 | 403 => format!("refused the token ({status})"),
            s => format!("answered {s}"),
        });
    }
    resp.into_body()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|_| "its answer could not be read".to_string())
}

/// Asks Parallax for every workspace's open handoffs. Never fails: the
/// reasons it could not are a [`Reading`] like any other.
pub fn read(root: &Path) -> Reading {
    let ep = match endpoint(root) {
        Ok(ep) => ep,
        Err(r) => return r,
    };
    let agent = agent();
    let started = Instant::now();
    let workspaces = match get(&agent, &ep, "/workspaces") {
        Ok(body) => match parse_workspaces(&body) {
            Some(w) => w,
            None => return Reading::Unreachable("the workspace list did not parse".into()),
        },
        Err(e) => return Reading::Unreachable(e),
    };
    let mut read = 0;
    let mut unreadable = 0;
    let mut seen = HashSet::new();
    let mut handoffs = Vec::new();
    for (id, name) in &workspaces {
        if started.elapsed() > POLL_BUDGET {
            unreadable += 1;
            continue;
        }
        let rows = get(
            &agent,
            &ep,
            &format!("/handoffs?openOnly=1&workspaceId={}", query_value(id)),
        )
        .ok()
        .and_then(|body| parse_handoffs(&body, name));
        match rows {
            Some(rows) => {
                read += 1;
                // One handoff can be visible from more than one workspace's
                // read; it is one thing waiting, shown once.
                handoffs.extend(rows.into_iter().filter(|h| seen.insert(h.id.clone())));
            }
            None => unreadable += 1,
        }
    }
    sort_handoffs(&mut handoffs);
    handoffs.truncate(MAX_HANDOFFS);
    Reading::Read {
        workspaces: read,
        unreadable,
        handoffs,
    }
}

/// A workspace ID as a query value. Parallax's are hex, but it is somebody
/// else's data going into a URL, so everything outside the unreserved set is
/// percent-encoded rather than trusted.
fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `{"ok":true,"workspaces":[{"id","name",…}]}` → `(id, name)` pairs. `None`
/// when the envelope is not that shape; a workspace without an ID is skipped.
pub fn parse_workspaces(body: &str) -> Option<Vec<(String, String)>> {
    let v: Value = serde_json::from_str(body).ok()?;
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let list = v.get("workspaces")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|w| {
                let id = w.get("id")?.as_str()?.trim();
                if id.is_empty() {
                    return None;
                }
                let name = w.get("name").and_then(Value::as_str).unwrap_or("");
                Some((id.to_string(), shown(name, MAX_NAME_CHARS)))
            })
            .take(MAX_WORKSPACES)
            .collect(),
    )
}

/// One row of `GET /handoffs`, full or redacted. Only what is shown is
/// named; everything else Parallax sends is ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireHandoff {
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default = "yes")]
    open: bool,
    #[serde(default)]
    attention_level: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    redacted: bool,
    #[serde(default)]
    workspace_name: Option<String>,
    #[serde(default)]
    created_at: Option<f64>,
    #[serde(default)]
    updated_at: Option<f64>,
}

fn yes() -> bool {
    true
}

/// Parallax's `getHandoffAttentionLevel` (`src/handoffs/attention.ts`), for a
/// row that did not say its level: closed or resolved asks nothing, and a
/// status this build does not know asks nothing either.
pub fn attention_of(status: &str, open: bool) -> Attention {
    if !open {
        return Attention::None;
    }
    match status {
        "needs_human" | "approval_expired" => Attention::Action,
        "blocked" | "waiting_approval" => Attention::Urgent,
        "ready_to_resume" | "completed_review" => Attention::Review,
        _ => Attention::None,
    }
}

/// `{"scope":…,"handoffs":[…]}` → the rows that ask for some attention.
/// `None` when the envelope is not that shape; a row that does not parse, or
/// asks for nothing, is left out.
///
/// `workspace` is the name of the workspace this read named. A redacted row
/// carries no workspace of its own — by design, see Parallax's
/// `RedactedHandoff` — but the tower knows which one it asked about.
pub fn parse_handoffs(body: &str, workspace: &str) -> Option<Vec<ParallaxHandoff>> {
    let v: Value = serde_json::from_str(body).ok()?;
    let rows = v.get("handoffs")?.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| serde_json::from_value::<WireHandoff>(row.clone()).ok())
            .filter_map(|w| row_of(w, workspace))
            .collect(),
    )
}

fn row_of(w: WireHandoff, workspace: &str) -> Option<ParallaxHandoff> {
    let id = w.id.trim();
    if id.is_empty() || id.len() > 200 {
        return None;
    }
    // Parallax's word when it gave one it knows, the status's otherwise; a
    // closed row asks nothing whatever it says.
    let attention = match w.attention_level.as_deref().map(Attention::from_wire) {
        _ if !w.open => Attention::None,
        Some(level) if level != Attention::None => level,
        Some(_) if w.attention_level.as_deref() == Some("none") => Attention::None,
        _ => attention_of(&w.status, w.open),
    };
    if attention == Attention::None {
        return None;
    }
    let own_workspace = w
        .workspace_name
        .as_deref()
        .map(|n| shown(n, MAX_NAME_CHARS))
        .filter(|n| !n.is_empty());
    Some(ParallaxHandoff {
        id: shown(id, 200),
        workspace: own_workspace.unwrap_or_else(|| workspace.to_string()),
        status: shown(&w.status, 40),
        attention,
        // A redacted row's title is Parallax's placeholder, not the handoff's:
        // it is left out rather than shown as if it were what was asked.
        title: if w.redacted {
            String::new()
        } else {
            shown(w.title.as_deref().unwrap_or(""), MAX_TITLE_CHARS)
        },
        created_at: w.created_at.and_then(from_millis),
        updated_at: w.updated_at.and_then(from_millis),
    })
}

/// Another program's text made fit for a terminal cell: controls cleaned,
/// ends trimmed, cut to `max` characters.
fn shown(s: &str, max: usize) -> String {
    tower_state::clean_text(s)
        .trim()
        .chars()
        .take(max)
        .collect()
}

fn from_millis(ms: f64) -> Option<OffsetDateTime> {
    if !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    OffsetDateTime::from_unix_timestamp_nanos((ms as i128) * 1_000_000).ok()
}

// ── the supervisor's side ────────────────────────────────────────────────

/// What the poll thread hands the pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// `parallax` is off in the config.
    Off,
    Done {
        reading: Reading,
        at: OffsetDateTime,
    },
}

/// The latest poll, taken by a thread of its own: a Parallax that holds a
/// request to its timeout must not hold up the supervisor's pass, which
/// drives idle loops and schedules on a two-second beat.
#[derive(Clone, Default)]
pub struct Refresher {
    latest: Arc<Mutex<(u64, Option<Poll>)>>,
}

impl Refresher {
    /// Starts the thread. The setting is read every poll, so turning it off
    /// stops the requests without a restart.
    pub fn spawn(home: PathBuf) -> Self {
        let r = Refresher::default();
        let latest = r.latest.clone();
        let _ = std::thread::Builder::new()
            .name("parallax".into())
            .spawn(move || loop {
                let poll = if tower_config::load().parallax_enabled() {
                    Poll::Done {
                        reading: read(&data_root_here(&home)),
                        at: OffsetDateTime::now_utc(),
                    }
                } else {
                    Poll::Off
                };
                if let Ok(mut l) = latest.lock() {
                    l.0 += 1;
                    l.1 = Some(poll);
                }
                std::thread::sleep(POLL_EVERY);
            });
        r
    }

    /// The poll after `seen`, once: `None` until a new one lands.
    pub fn take_new(&self, seen: &mut u64) -> Option<Poll> {
        let l = self.latest.lock().ok()?;
        if l.0 == *seen {
            return None;
        }
        *seen = l.0;
        l.1.clone()
    }
}

/// What the pass remembers between polls: whether Parallax was answering,
/// so a change is logged once rather than every ten seconds.
#[derive(Debug, Default)]
pub struct Watch {
    seen: u64,
    /// `None` before the first answer or while off: an unreachable Parallax
    /// at start is a Parallax that is not running, which is not news.
    reachable: Option<bool>,
    /// The last trouble with `parallax.json`, said once until it clears.
    write_failed: Option<String>,
    /// The same for `parallax-notified.json`, apart: one file working must
    /// not clear the other's trouble and have it said again every poll.
    notified_failed: Option<String>,
    /// The handoffs announced, read from disk once and kept here: the file
    /// is what survives a restart, and a file that cannot be written must not
    /// have the same handoff announced on every poll.
    notified: Option<Vec<tower_state::NotifiedHandoff>>,
}

/// A notice for the person: a title and a line.
pub type Notice = (String, String);

impl Watch {
    /// Takes a new poll if there is one: writes the snapshot, and returns the
    /// log lines and the notices it gives rise to. Called every pass; costs
    /// nothing between polls.
    pub fn tick(
        &mut self,
        refresher: &Refresher,
        store: &ParallaxStore,
    ) -> (Vec<String>, Vec<Notice>) {
        match refresher.take_new(&mut self.seen) {
            Some(poll) => self.apply(poll, store),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// [`Watch::tick`] for one poll, apart from the thread that took it.
    pub fn apply(&mut self, poll: Poll, store: &ParallaxStore) -> (Vec<String>, Vec<Notice>) {
        let mut log = Vec::new();
        let mut notices = Vec::new();
        let (reading, at) = match poll {
            Poll::Off => {
                if self.reachable.take() == Some(true) {
                    log.push("parallax: turned off, no longer reading its handoffs".into());
                }
                if let Err(e) = store.remove() {
                    failed(
                        &mut self.write_failed,
                        &mut log,
                        &format!("removing parallax.json: {e}"),
                    );
                }
                return (log, notices);
            }
            Poll::Done { reading, at } => (reading, at),
        };
        let snapshot = match reading {
            Reading::Read {
                workspaces,
                unreadable,
                handoffs,
            } => {
                if self.reachable != Some(true) {
                    let mut line =
                        format!("parallax: reading handoffs from {workspaces} workspace(s)");
                    if unreadable > 0 {
                        line.push_str(&format!(", {unreadable} unreadable"));
                    }
                    log.push(line);
                }
                self.reachable = Some(true);
                ParallaxSnapshot {
                    updated_at: at,
                    reachable: true,
                    workspaces,
                    handoffs,
                }
            }
            Reading::NotInstalled | Reading::Unreachable(_) => {
                if self.reachable == Some(true) {
                    let why = match &reading {
                        Reading::Unreachable(why) => why.as_str(),
                        _ => "its data folder is gone",
                    };
                    log.push(format!("parallax: stopped answering: {why}"));
                }
                self.reachable = Some(false);
                ParallaxSnapshot {
                    updated_at: at,
                    reachable: false,
                    workspaces: 0,
                    handoffs: Vec::new(),
                }
            }
        };
        match store.write(&snapshot) {
            Ok(()) => {
                if self.write_failed.take().is_some() {
                    log.push("parallax: parallax.json is being written again".into());
                }
            }
            Err(e) => failed(
                &mut self.write_failed,
                &mut log,
                &format!("writing parallax.json: {e}"),
            ),
        }
        if snapshot.reachable {
            let notified = self.notified.get_or_insert_with(|| store.read_notified());
            let fresh = announce_new(notified, &snapshot.handoffs, at);
            if !fresh.is_empty() {
                // Written before the notices go, so a crash between the two
                // loses a notice rather than repeating it on every restart.
                match store.write_notified(notified) {
                    Ok(()) => {
                        if self.notified_failed.take().is_some() {
                            log.push(
                                "parallax: parallax-notified.json is being written again".into(),
                            );
                        }
                    }
                    Err(e) => failed(
                        &mut self.notified_failed,
                        &mut log,
                        &format!("writing parallax-notified.json: {e}"),
                    ),
                }
                notices = notices_for(&fresh);
            }
        }
        (log, notices)
    }
}

/// Says a write's trouble once, until it changes or clears.
fn failed(slot: &mut Option<String>, log: &mut Vec<String>, what: &str) {
    if slot.as_deref() != Some(what) {
        log.push(format!("parallax: {what}"));
        *slot = Some(what.to_string());
    }
}

/// The waiting handoffs in `handoffs` not announced before, which are added
/// to `notified`. Once per ID for good — a handoff that goes quiet and asks
/// again under the same ID is the same question — except that an ID is
/// forgotten after [`NOTIFIED_KEEP`], or when more than [`NOTIFIED_MAX`] are
/// remembered, oldest first, so the list cannot grow without end.
pub fn announce_new<'a>(
    notified: &mut Vec<NotifiedHandoff>,
    handoffs: &'a [ParallaxHandoff],
    now: OffsetDateTime,
) -> Vec<&'a ParallaxHandoff> {
    notified.retain(|n| now - n.at < NOTIFIED_KEEP);
    let known: HashSet<String> = notified.iter().map(|n| n.id.clone()).collect();
    let fresh: Vec<&ParallaxHandoff> = handoffs
        .iter()
        .filter(|h| h.attention.waits() && !known.contains(&h.id))
        .collect();
    notified.extend(fresh.iter().map(|h| NotifiedHandoff {
        id: h.id.clone(),
        at: now,
    }));
    if notified.len() > NOTIFIED_MAX {
        notified.sort_by_key(|n| n.at);
        let excess = notified.len() - NOTIFIED_MAX;
        notified.drain(..excess);
    }
    fresh
}

/// One notice per new handoff, up to [`NOTICE_EACH`]; more than that in one
/// poll — a first start with a backlog, say — is one notice for the lot.
pub fn notices_for(fresh: &[&ParallaxHandoff]) -> Vec<Notice> {
    if fresh.len() > NOTICE_EACH {
        let mut workspaces: Vec<&str> = fresh.iter().map(|h| h.workspace.as_str()).collect();
        workspaces.sort();
        workspaces.dedup();
        return vec![(
            format!("◆ Parallax: {} handoffs need you", fresh.len()),
            workspaces.join(", "),
        )];
    }
    fresh
        .iter()
        .map(|h| {
            let place = if h.workspace.is_empty() {
                "Parallax".to_string()
            } else {
                format!("Parallax {}", h.workspace)
            };
            let body = if h.title.is_empty() {
                h.status_label().to_string()
            } else {
                format!("{} — {}", h.title, h.status_label())
            };
            (format!("◆ {place} needs you"), body)
        })
        .collect()
}

// ── doctor ───────────────────────────────────────────────────────────────

/// `doctor`'s parallax section. Informational in every state: Parallax is
/// somebody else's program, closed as often as it is open, and nothing the
/// tower's own setup does fixes it.
pub fn doctor(root: &Path, enabled: bool) -> crate::doctorcmd::Finding {
    doctor_line(if enabled { Some(read(root)) } else { None })
}

/// [`doctor`] for a reading already taken (`None`: the setting is off).
pub fn doctor_line(reading: Option<Reading>) -> crate::doctorcmd::Finding {
    let line = match reading {
        None => "off — `herdr-tower config set parallax true` shows its handoffs".to_string(),
        Some(Reading::NotInstalled) => "not installed — nothing to show".to_string(),
        Some(Reading::Unreachable(why)) => {
            format!("installed, but its API is not answering ({why}) — is Parallax running?")
        }
        Some(Reading::Read {
            workspaces,
            unreadable,
            handoffs,
        }) => {
            let waiting = handoffs.iter().filter(|h| h.attention.waits()).count();
            let review = handoffs
                .iter()
                .filter(|h| h.attention == Attention::Review)
                .count();
            let mut line = format!(
                "answering: handoffs readable in {workspaces} workspace(s) · {waiting} waiting on you · {review} to review"
            );
            if unreadable > 0 {
                line.push_str(&format!(" · {unreadable} workspace(s) unreadable"));
            }
            line
        }
    };
    crate::doctorcmd::Finding { ok: true, line }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read as _, Write as _};
    use std::net::TcpListener;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::seconds(secs)
    }

    const FULL: &str = r#"{"scope":{"workspaceId":"w1"},"handoffs":[{
        "id":"h1","status":"needs_human","title":"Pick the \u001b[31mplan","body":"secret body",
        "reason":"","workspaceId":"w1","originScope":null,"originRetention":"persistent",
        "tabId":null,"agentId":"a","source":null,"actionLabel":null,"approvalAction":null,
        "taskId":null,"stepId":null,"open":true,"createdAt":1790000000000,"updatedAt":1790000060000,
        "ownerSequence":3,"actionable":true,"attentionLevel":"action","workspaceName":"Research",
        "tabTitle":null,"tabUrl":null,"approvalAuthority":null,"redacted":false,"someNewField":{"x":1}
    }]}"#;

    const REDACTED: &str = r#"{"scope":null,"handoffs":[{
        "id":"h2","status":"waiting_approval","open":true,"actionable":true,
        "attentionLevel":"urgent","title":"CoDriver needs attention","createdAt":1790000000000,
        "updatedAt":1790000000000,"resolvedAt":null,"ownerSequence":1,"redacted":true
    }]}"#;

    #[test]
    fn a_full_row_parses_and_its_text_is_cleaned() {
        let rows = parse_handoffs(FULL, "Queried").unwrap();
        assert_eq!(rows.len(), 1);
        let h = &rows[0];
        assert_eq!(h.id, "h1");
        assert_eq!(h.workspace, "Research", "the row's own name wins");
        assert_eq!(h.status, "needs_human");
        assert_eq!(h.attention, Attention::Action);
        assert_eq!(h.title, "Pick the [31mplan", "no ESC reaches a terminal");
        assert_eq!(h.created_at.unwrap().unix_timestamp(), 1_790_000_000);
        assert_eq!(h.updated_at.unwrap().unix_timestamp(), 1_790_000_060);
    }

    #[test]
    fn a_redacted_row_has_no_title_and_the_queried_workspace() {
        let rows = parse_handoffs(REDACTED, "Default").unwrap();
        assert_eq!(rows.len(), 1);
        let h = &rows[0];
        assert_eq!(
            h.title, "",
            "Parallax's placeholder is not the handoff's title"
        );
        assert_eq!(h.workspace, "Default");
        assert_eq!(h.attention, Attention::Urgent);
        assert_eq!(h.headline(), "waiting for approval");
    }

    #[test]
    fn an_unknown_status_asks_nothing_unless_parallax_says_it_does() {
        let body = r#"{"handoffs":[
            {"id":"a","status":"teleporting","open":true},
            {"id":"b","status":"teleporting","open":true,"attentionLevel":"urgent"},
            {"id":"c","status":"needs_human","open":true,"attentionLevel":"cosmic"},
            {"id":"d","status":"needs_human","open":true,"attentionLevel":"none"}
        ]}"#;
        let rows = parse_handoffs(body, "W").unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|h| (h.id.as_str(), h.attention, h.status.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("b", Attention::Urgent, "teleporting"),
                ("c", Attention::Action, "needs_human"),
            ],
            "an unknown level falls back to the status; Parallax's own none is kept"
        );
    }

    #[test]
    fn garbage_skips_rows_or_fails_only_the_envelope() {
        let body = r#"{"handoffs":[
            42, "x", null, {"status":"blocked"}, {"id":7,"status":"blocked"},
            {"id":"","status":"blocked"}, {"id":"ok","status":"blocked","createdAt":"soon"},
            {"id":"good","status":"blocked","createdAt":-5}
        ]}"#;
        let rows = parse_handoffs(body, "W").unwrap();
        let ids: Vec<_> = rows.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["good"]);
        assert_eq!(rows[0].created_at, None, "a negative time is no time");
        assert_eq!(parse_handoffs("not json", "W"), None);
        assert_eq!(parse_handoffs(r#"{"handoffs":{}}"#, "W"), None);
        assert_eq!(parse_handoffs(r#"[]"#, "W"), None);
        assert_eq!(parse_workspaces("{"), None);
        assert_eq!(parse_workspaces(r#"{"ok":false,"workspaces":[]}"#), None);
        assert_eq!(
            parse_workspaces(
                r#"{"ok":true,"workspaces":[{"id":"a","name":"A"},{"name":"x"},{"id":"b"}]}"#
            ),
            Some(vec![("a".into(), "A".into()), ("b".into(), String::new())])
        );
    }

    #[test]
    fn attention_follows_parallax_and_closed_asks_nothing() {
        for (status, level) in [
            ("needs_human", Attention::Action),
            ("blocked", Attention::Urgent),
            ("waiting_approval", Attention::Urgent),
            ("approval_expired", Attention::Action),
            ("ready_to_resume", Attention::Review),
            ("completed_review", Attention::Review),
            ("resolved", Attention::None),
        ] {
            assert_eq!(attention_of(status, true), level, "{status}");
            assert_eq!(attention_of(status, false), Attention::None, "{status}");
        }
        let closed = r#"{"handoffs":[{"id":"x","status":"blocked","open":false,"attentionLevel":"urgent"}]}"#;
        assert!(parse_handoffs(closed, "W").unwrap().is_empty());
        let review = r#"{"handoffs":[{"id":"r","status":"completed_review","open":true}]}"#;
        let rows = parse_handoffs(review, "W").unwrap();
        assert_eq!(rows[0].attention, Attention::Review);
        assert!(!rows[0].attention.waits(), "review is not waiting");
    }

    #[test]
    fn a_workspace_id_cannot_break_out_of_its_query_value() {
        assert_eq!(query_value("ad7d44e06369832a"), "ad7d44e06369832a");
        assert_eq!(query_value("a&b=c d/%"), "a%26b%3Dc%20d%2F%25");
    }

    fn waiting(id: &str, workspace: &str) -> ParallaxHandoff {
        ParallaxHandoff {
            id: id.into(),
            workspace: workspace.into(),
            status: "needs_human".into(),
            attention: Attention::Action,
            title: String::new(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn an_unwritable_notified_file_neither_repeats_notices_nor_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        // A folder where the file goes: every write of it fails.
        std::fs::create_dir(dir.path().join(tower_state::PARALLAX_NOTIFIED_FILE)).unwrap();
        let poll = |rows: Vec<ParallaxHandoff>, secs| Poll::Done {
            reading: Reading::Read {
                workspaces: 1,
                unreadable: 0,
                handoffs: rows,
            },
            at: at(secs),
        };
        let mut watch = Watch::default();
        let (log, notices) = watch.apply(poll(vec![waiting("a", "W")], 0), &store);
        assert_eq!(notices.len(), 1, "announced once");
        assert!(
            log.iter()
                .any(|l| l.contains("writing parallax-notified.json")),
            "{log:?}"
        );
        for secs in [10, 20, 30] {
            let (log, notices) = watch.apply(poll(vec![waiting("a", "W")], secs), &store);
            assert!(notices.is_empty(), "repeated at {secs}s: {notices:?}");
            assert!(log.is_empty(), "the trouble said again at {secs}s: {log:?}");
        }
        // A new handoff: announced, and the unchanged trouble is not said again
        // just because parallax.json was written in between.
        let (log, notices) =
            watch.apply(poll(vec![waiting("a", "W"), waiting("b", "X")], 40), &store);
        assert_eq!(notices.len(), 1);
        assert!(log.is_empty(), "{log:?}");
    }

    #[test]
    fn a_handoff_is_announced_once_across_polls_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let mut review = waiting("r", "W");
        review.attention = Attention::Review;
        let rows = vec![waiting("a", "W"), review];
        let poll = |rows: Vec<ParallaxHandoff>, secs| Poll::Done {
            reading: Reading::Read {
                workspaces: 1,
                unreadable: 0,
                handoffs: rows,
            },
            at: at(secs),
        };
        let mut watch = Watch::default();
        let (log, notices) = watch.apply(poll(rows.clone(), 0), &store);
        assert_eq!(log, ["parallax: reading handoffs from 1 workspace(s)"]);
        assert_eq!(
            notices,
            [(
                "◆ Parallax W needs you".to_string(),
                "needs you".to_string()
            )],
            "the review-level one is not announced"
        );
        let (log, notices) = watch.apply(poll(rows.clone(), 10), &store);
        assert!(
            log.is_empty() && notices.is_empty(),
            "nothing new: {log:?} {notices:?}"
        );
        // A new supervisor starts with a new watch, and the file remembers.
        let mut restarted = Watch::default();
        let (_, notices) = restarted.apply(poll(rows.clone(), 20), &store);
        assert!(notices.is_empty(), "a restart must not repeat: {notices:?}");
        let (_, notices) =
            restarted.apply(poll(vec![waiting("a", "W"), waiting("b", "X")], 30), &store);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "◆ Parallax X needs you");
        let snap = store.read().unwrap();
        assert!(snap.reachable);
        assert_eq!(snap.updated_at, at(30));
    }

    #[test]
    fn many_at_once_are_one_notice_and_the_memory_is_bounded() {
        let rows: Vec<_> = (0..5).map(|i| waiting(&format!("h{i}"), "W")).collect();
        let mut notified = Vec::new();
        let fresh = announce_new(&mut notified, &rows, at(0));
        assert_eq!(fresh.len(), 5);
        assert_eq!(
            notices_for(&fresh),
            [(
                "◆ Parallax: 5 handoffs need you".to_string(),
                "W".to_string()
            )]
        );
        // Forgotten after the keep window.
        let later = at(0) + NOTIFIED_KEEP + time::Duration::seconds(1);
        assert_eq!(announce_new(&mut notified, &rows[..1], later).len(), 1);
        assert_eq!(notified.len(), 1);
        // Never more than the cap.
        let many: Vec<_> = (0..NOTIFIED_MAX + 10)
            .map(|i| waiting(&format!("m{i}"), "W"))
            .collect();
        announce_new(&mut notified, &many, later);
        assert_eq!(notified.len(), NOTIFIED_MAX);
    }

    #[test]
    fn a_titled_notice_says_what_was_asked() {
        let mut h = waiting("t", "Research");
        h.title = "Approve the upload".into();
        h.status = "waiting_approval".into();
        assert_eq!(
            notices_for(&[&h]),
            [(
                "◆ Parallax Research needs you".to_string(),
                "Approve the upload — waiting for approval".to_string()
            )]
        );
    }

    #[test]
    fn going_away_is_logged_once_and_empties_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let mut watch = Watch::default();
        let gone = |secs| Poll::Done {
            reading: Reading::Unreachable("not answering".into()),
            at: at(secs),
        };
        let (log, _) = watch.apply(gone(0), &store);
        assert!(log.is_empty(), "not running from the start is not news");
        let up = Poll::Done {
            reading: Reading::Read {
                workspaces: 3,
                unreadable: 1,
                handoffs: vec![waiting("a", "W")],
            },
            at: at(10),
        };
        let (log, _) = watch.apply(up, &store);
        assert_eq!(
            log,
            ["parallax: reading handoffs from 3 workspace(s), 1 unreadable"]
        );
        let (log, _) = watch.apply(gone(20), &store);
        assert_eq!(log, ["parallax: stopped answering: not answering"]);
        let (log, _) = watch.apply(gone(30), &store);
        assert!(log.is_empty());
        let snap = store.read().unwrap();
        assert!(!snap.reachable && snap.handoffs.is_empty());
        let (_, _) = watch.apply(Poll::Off, &store);
        assert_eq!(store.read(), None, "off removes the snapshot");
    }

    // ── against a server ────────────────────────────────────────────────

    /// A tiny HTTP/1.1 server on 127.0.0.1 that answers by path and records
    /// each request's path and Authorization header.
    struct Fake {
        port: u16,
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }

    fn fake(routes: Vec<(&'static str, u16, String)>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut auth = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("authorization:") {
                        auth = v.trim().to_string();
                    }
                }
                log.lock().unwrap().push((path.clone(), auth));
                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| path.starts_with(p))
                    .map(|(_, s, b)| (*s, b.clone()))
                    .unwrap_or((404, "{}".into()));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nLocation: http://127.0.0.1:{port}/workspaces\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
                let mut rest = Vec::new();
                let _ = reader.read_to_end(&mut rest);
            }
        });
        Fake { port, seen }
    }

    fn root_for(port: Option<u16>, token: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(p) = port {
            std::fs::write(dir.path().join("api-port"), format!("{p}\n")).unwrap();
        }
        if let Some(t) = token {
            std::fs::write(dir.path().join("api-token"), format!("{t}\n")).unwrap();
        }
        dir
    }

    #[test]
    fn it_reads_every_workspace_with_the_token_and_names_them() {
        let server = fake(vec![
            (
                "/workspaces",
                200,
                r#"{"ok":true,"workspaces":[{"id":"w1","name":"Research"},{"id":"w2","name":"Default"},{"id":"w3","name":"Broken"}]}"#.into(),
            ),
            ("/handoffs?openOnly=1&workspaceId=w1", 200, FULL.into()),
            ("/handoffs?openOnly=1&workspaceId=w2", 200, REDACTED.into()),
            ("/handoffs?openOnly=1&workspaceId=w3", 500, "{}".into()),
        ]);
        let root = root_for(Some(server.port), Some("s3cret"));
        let reading = read(root.path());
        let Reading::Read {
            workspaces,
            unreadable,
            handoffs,
        } = reading
        else {
            panic!("expected a reading, got {reading:?}");
        };
        assert_eq!((workspaces, unreadable), (2, 1));
        let ids: Vec<_> = handoffs
            .iter()
            .map(|h| (h.id.as_str(), h.workspace.as_str()))
            .collect();
        assert_eq!(ids, [("h2", "Default"), ("h1", "Research")], "urgent first");
        let seen = server.seen.lock().unwrap().clone();
        assert!(
            seen.iter().all(|(_, auth)| auth == "bearer s3cret"),
            "{seen:?}"
        );
        assert!(
            seen.iter()
                .all(|(p, _)| p == "/workspaces"
                    || p.starts_with("/handoffs?openOnly=1&workspaceId=")),
            "never an unnamed /handoffs: {seen:?}"
        );
        let line = doctor_line(Some(Reading::Read {
            workspaces,
            unreadable,
            handoffs,
        }));
        assert!(line.ok);
        assert_eq!(
            line.line,
            "answering: handoffs readable in 2 workspace(s) · 2 waiting on you · 0 to review · 1 workspace(s) unreadable"
        );
    }

    #[test]
    fn absent_refused_and_silent_are_not_available() {
        let nowhere = tempfile::tempdir().unwrap();
        let missing = nowhere.path().join("no-parallax");
        assert_eq!(read(&missing), Reading::NotInstalled);
        assert_eq!(
            doctor_line(Some(Reading::NotInstalled)).line,
            "not installed — nothing to show"
        );

        let no_token = root_for(Some(1), None);
        assert_eq!(
            read(no_token.path()),
            Reading::Unreachable("no usable api-token".into())
        );
        let bad_token = root_for(Some(1), Some("two words"));
        assert_eq!(
            read(bad_token.path()),
            Reading::Unreachable("no usable api-token".into())
        );
        let bad_port = root_for(None, Some("t"));
        std::fs::write(bad_port.path().join("api-port"), "eighty").unwrap();
        assert!(matches!(read(bad_port.path()), Reading::Unreachable(_)));

        // Installed, nothing listening: bind a port and let it go.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let silent = root_for(Some(port), Some("t0ken"));
        let r = read(silent.path());
        assert_eq!(r, Reading::Unreachable("not answering".into()));
        let f = doctor_line(Some(r));
        assert!(f.ok);
        assert!(
            f.line
                .starts_with("installed, but its API is not answering"),
            "{}",
            f.line
        );
        assert!(!f.line.contains("t0ken"));

        let refused = fake(vec![("/workspaces", 401, r#"{"error":"no"}"#.into())]);
        let root = root_for(Some(refused.port), Some("t0ken"));
        let r = read(root.path());
        assert_eq!(r, Reading::Unreachable("refused the token (401)".into()));

        assert!(doctor_line(None).line.starts_with("off"));
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let server = fake(vec![("/workspaces", 302, "{}".into())]);
        let root = root_for(Some(server.port), Some("t"));
        assert_eq!(
            read(root.path()),
            Reading::Unreachable("answered 302".into())
        );
        assert_eq!(server.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_data_root_honours_the_environment_value() {
        let home = Path::new("/home/x");
        assert_eq!(data_root(home, None), home.join(".parallax"));
        assert_eq!(data_root(home, Some("".into())), home.join(".parallax"));
        assert_eq!(
            data_root(home, Some("/data/p".into())),
            PathBuf::from("/data/p")
        );
    }

    #[test]
    fn the_endpoint_never_prints_its_token() {
        let ep = Endpoint {
            port: 1,
            token: "s3cret".into(),
        };
        assert!(!format!("{ep:?}").contains("s3cret"));
    }

    // ── generated and edge cases ────────────────────────────────────────

    /// Parallax's `STATUS_TO_ATTENTION_LEVEL` and `getHandoffAttentionLevel`,
    /// copied by hand from `~/parallax-browser/src/handoffs/attention.ts`:
    /// an independent oracle for `attention_of`. If Parallax changes its
    /// table, this copy is what should change with it.
    const TS_TABLE: [(&str, &str); 7] = [
        ("needs_human", "action"),
        ("blocked", "urgent"),
        ("waiting_approval", "urgent"),
        ("approval_expired", "action"),
        ("ready_to_resume", "review"),
        ("completed_review", "review"),
        ("resolved", "none"),
    ];

    fn ts_level(status: &str, open: bool) -> &'static str {
        if !open || status == "resolved" {
            return "none";
        }
        TS_TABLE
            .iter()
            .find(|(s, _)| *s == status)
            .map(|(_, l)| *l)
            // TS types forbid other statuses; the tower reads them as none.
            .unwrap_or("none")
    }

    /// Cross-check: for every status in Parallax's table, and generated
    /// statuses it does not have (case variants, padding, unicode), open or
    /// closed, `attention_of` gives the TS function's level — and a row with
    /// no `attentionLevel` of its own parses to that same level, or is left
    /// out when it is none.
    #[test]
    fn attention_of_matches_the_parallax_typescript_table() {
        let mut statuses: Vec<String> = TS_TABLE.iter().map(|(s, _)| s.to_string()).collect();
        for (s, _) in TS_TABLE {
            statuses.push(s.to_uppercase());
            statuses.push(format!(" {s}"));
            statuses.push(format!("{s}\n"));
            statuses.push(s.replace('_', "-"));
        }
        statuses.extend(["", "open", "none", "urgent", "상태", "needs_human\0"].map(String::from));
        for status in &statuses {
            for open in [true, false] {
                let want = Attention::from_wire(ts_level(status, open));
                assert_eq!(attention_of(status, open), want, "{status:?} open={open}");
                let body = serde_json::json!({"handoffs":[{"id":"x","status":status,"open":open}]})
                    .to_string();
                let rows = parse_handoffs(&body, "W").unwrap();
                match want {
                    Attention::None => assert!(rows.is_empty(), "{status:?}"),
                    level => assert_eq!(rows[0].attention, level, "{status:?}"),
                }
            }
        }
    }

    /// Edges of the workspace list: names are cleaned, trimmed and cut to 60
    /// characters (by character, so a Korean name is not cut mid-letter), IDs
    /// are trimmed and a blank one skipped, duplicates are kept as the list
    /// had them, and the list is capped at 64.
    #[test]
    fn workspace_names_are_bounded_and_ids_trimmed() {
        let long = "가".repeat(500);
        let body = serde_json::json!({"workspaces":[
            {"id":" w1 ","name":long},
            {"id":"w2","name":"\u{1b}[2J  spaced\tname  "},
            {"id":"   ","name":"blank"},
            {"id":"w1","name":"again"},
            {"id":"w3","name":7},
        ]})
        .to_string();
        let got = parse_workspaces(&body).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].0, "w1");
        assert_eq!(got[0].1, "가".repeat(60));
        assert_eq!(got[1].1, "[2J  spaced name");
        assert_eq!(got[2], ("w1".to_string(), "again".to_string()));
        assert_eq!(
            got[3],
            ("w3".to_string(), String::new()),
            "a non-string name"
        );
        let many: Vec<_> = (0..100)
            .map(|i| serde_json::json!({"id":format!("w{i}")}))
            .collect();
        let body = serde_json::json!({ "workspaces": many }).to_string();
        let got = parse_workspaces(&body).unwrap();
        assert_eq!(got.len(), 64);
        assert_eq!(got[63].0, "w63");
        assert_eq!(parse_workspaces(r#"{"workspaces":[]}"#), Some(vec![]));
        assert_eq!(parse_workspaces(r#"{"ok":true}"#), None);
        assert_eq!(parse_workspaces(""), None);
    }

    /// Edges of a handoff row: a huge unicode title is cut to 200 characters,
    /// an ID of exactly 200 bytes is kept and 201 skipped, an ID is trimmed, a
    /// row of only `id` and `status` takes `open` as true, and times that
    /// cannot be a date (zero, out of range, fractional) are handled.
    #[test]
    fn handoff_rows_at_their_bounds() {
        let title = "😀".repeat(1000);
        let id200 = "i".repeat(200);
        let id201 = "i".repeat(201);
        let body = serde_json::json!({"handoffs":[
            {"id":"  t  ","status":"blocked","title":title,"createdAt":0,"updatedAt":1.0e30},
            {"id":id200,"status":"blocked","createdAt":1_790_000_000_123.9_f64},
            {"id":id201,"status":"blocked"},
            {"id":"u","status":"needs_human","title":"   ","workspaceName":"   "},
        ]})
        .to_string();
        let rows = parse_handoffs(&body, "Queried").unwrap();
        let ids: Vec<_> = rows.iter().map(|h| h.id.len()).collect();
        assert_eq!(ids, [1, 200, 1]);
        assert_eq!(rows[0].id, "t");
        assert_eq!(rows[0].title.chars().count(), 200);
        assert!(rows[0].title.chars().all(|c| c == '😀'));
        assert_eq!(rows[0].created_at, None, "zero is no time");
        assert_eq!(rows[0].updated_at, None, "out of range is no time");
        assert_eq!(
            rows[1].created_at.unwrap().unix_timestamp_nanos(),
            1_790_000_000_123_000_000,
            "whole milliseconds"
        );
        assert_eq!(rows[2].title, "", "a blank title is no title");
        assert_eq!(rows[2].workspace, "Queried", "a blank name is no name");
        assert_eq!(rows[2].headline(), "needs you");
        assert_eq!(parse_handoffs(r#"{"handoffs":[]}"#, "W"), Some(vec![]));
    }

    /// One handoff visible from two workspaces is one thing waiting: `read`
    /// shows it once (the first workspace's), and the rest stay.
    #[test]
    fn a_handoff_seen_from_two_workspaces_is_read_once() {
        let row = |id: &str| {
            format!(
                r#"{{"handoffs":[{{"id":"{id}","status":"blocked"}},{{"id":"same","status":"needs_human"}}]}}"#
            )
        };
        let server = fake(vec![
            (
                "/workspaces",
                200,
                r#"{"workspaces":[{"id":"w1","name":"One"},{"id":"w2","name":"Two"}]}"#.into(),
            ),
            ("/handoffs?openOnly=1&workspaceId=w1", 200, row("a")),
            ("/handoffs?openOnly=1&workspaceId=w2", 200, row("b")),
        ]);
        let root = root_for(Some(server.port), Some("t"));
        let Reading::Read { handoffs, .. } = read(root.path()) else {
            panic!("expected a reading");
        };
        let ids: Vec<_> = handoffs
            .iter()
            .map(|h| (h.id.as_str(), h.workspace.as_str()))
            .collect();
        assert_eq!(ids, [("a", "One"), ("b", "Two"), ("same", "One")]);
    }

    /// Edges of `notices_for`: none is no notice, exactly three are three,
    /// four are one that names each workspace once, sorted; an unnamed
    /// workspace is plain "Parallax".
    #[test]
    fn notices_switch_to_one_summary_above_three() {
        assert!(notices_for(&[]).is_empty());
        let rows: Vec<_> = ["B", "A", "B", ""]
            .iter()
            .enumerate()
            .map(|(i, w)| waiting(&format!("n{i}"), w))
            .collect();
        let refs: Vec<&ParallaxHandoff> = rows.iter().collect();
        let three = notices_for(&refs[..3]);
        assert_eq!(three.len(), 3);
        assert_eq!(three[1].0, "◆ Parallax A needs you");
        assert_eq!(
            notices_for(&refs[3..]),
            [("◆ Parallax needs you".to_string(), "needs you".to_string())]
        );
        assert_eq!(
            notices_for(&refs),
            [(
                "◆ Parallax: 4 handoffs need you".to_string(),
                ", A, B".to_string()
            )]
        );
    }

    /// The bounds exactly: an ID announced 30 days ago to the nanosecond is
    /// forgotten, one a nanosecond younger is not; 1000 remembered is kept
    /// whole, 1001 drops the oldest.
    #[test]
    fn announce_bounds_are_exact() {
        let rows = vec![waiting("h", "W")];
        let mut notified = vec![NotifiedHandoff {
            id: "h".into(),
            at: at(0),
        }];
        let young = at(0) + NOTIFIED_KEEP - time::Duration::nanoseconds(1);
        assert!(announce_new(&mut notified, &rows, young).is_empty());
        let old = at(0) + NOTIFIED_KEEP;
        assert_eq!(announce_new(&mut notified, &rows, old).len(), 1);
        assert_eq!(
            notified,
            [NotifiedHandoff {
                id: "h".into(),
                at: old
            }]
        );

        let mut full: Vec<NotifiedHandoff> = (0..NOTIFIED_MAX)
            .map(|i| NotifiedHandoff {
                id: format!("k{i}"),
                at: at(i as i64),
            })
            .collect();
        assert!(announce_new(&mut full, &[], at(NOTIFIED_MAX as i64)).is_empty());
        assert_eq!(full.len(), NOTIFIED_MAX);
        announce_new(&mut full, &rows, at(NOTIFIED_MAX as i64));
        assert_eq!(full.len(), NOTIFIED_MAX);
        assert!(!full.iter().any(|n| n.id == "k0"), "the oldest goes");
        assert!(full.iter().any(|n| n.id == "h"));
    }

    /// Property over generated polls (seeded): the memory never exceeds 1000
    /// or keeps anything 30 days old; what is announced is always waiting and
    /// was not in the memory before; and an ID once announced is not
    /// announced again while it is remembered.
    #[test]
    fn announce_new_properties_over_generated_polls() {
        let mut rng = tower_testrng::Rng::new(0x0A11_00CE);
        let mut notified: Vec<NotifiedHandoff> = Vec::new();
        let mut secs = 0i64;
        for _ in 0..400 {
            secs += [1, 60, 3600, 86_400 * 3][rng.below(4)];
            let now = at(secs);
            let rows: Vec<ParallaxHandoff> = (0..rng.below(40))
                .map(|_| {
                    let mut h = waiting(&format!("h{}", rng.below(1500)), "W");
                    h.attention = [
                        Attention::None,
                        Attention::Review,
                        Attention::Action,
                        Attention::Urgent,
                    ][rng.below(4)];
                    h
                })
                .collect();
            let before: HashSet<String> = notified
                .iter()
                .filter(|n| now - n.at < NOTIFIED_KEEP)
                .map(|n| n.id.clone())
                .collect();
            let fresh = announce_new(&mut notified, &rows, now);
            for h in &fresh {
                assert!(h.attention.waits(), "{h:?}");
                assert!(!before.contains(&h.id), "{} announced twice", h.id);
            }
            assert!(notified.len() <= NOTIFIED_MAX);
            assert!(notified.iter().all(|n| now - n.at < NOTIFIED_KEEP));
            let naive = rows
                .iter()
                .filter(|h| h.attention.waits() && !before.contains(&h.id))
                .count();
            assert_eq!(fresh.len(), naive);
        }
    }

    /// Round trip: rows parsed from Parallax's wire, applied as a poll, are
    /// what the store reads back — unicode, a cut title, a redacted row — and
    /// the notified memory written by the pass reads back with the waiting IDs.
    #[test]
    fn a_parsed_poll_round_trips_through_the_store() {
        let body = serde_json::json!({"handoffs":[
            {"id":"a","status":"blocked","title":"배포 승인 😀".repeat(40),"createdAt":1_790_000_000_000_u64},
            {"id":"b","status":"completed_review","title":"\"quoted\" \\ back"},
            {"id":"c","status":"waiting_approval","redacted":true,"title":"placeholder"},
        ]})
        .to_string();
        let mut handoffs = parse_handoffs(&body, "작업").unwrap();
        sort_handoffs(&mut handoffs);
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let mut watch = Watch::default();
        let (_, notices) = watch.apply(
            Poll::Done {
                reading: Reading::Read {
                    workspaces: 1,
                    unreadable: 0,
                    handoffs: handoffs.clone(),
                },
                at: at(0),
            },
            &store,
        );
        assert_eq!(notices.len(), 2, "a and c wait; b is a review");
        let snap = store.read().unwrap();
        assert_eq!(snap.handoffs, handoffs);
        assert_eq!(snap.workspaces, 1);
        let mut ids: Vec<_> = store.read_notified().into_iter().map(|n| n.id).collect();
        ids.sort();
        assert_eq!(ids, ["a", "c"]);
    }
}
