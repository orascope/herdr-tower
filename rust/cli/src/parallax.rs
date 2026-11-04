// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! How many of Parallax Browser's handoffs ask for the person, read for the
//! tower to show, and only read.
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
//! - **Counts, not contents.** `GET /handoffs/attention` answers
//!   `{urgent,action,review}` and says nothing about any handoff. Parallax keeps
//!   what a handoff says to the conversation it belongs to, and the tower holds
//!   no execution, so it could not read that and does not try.
//! - **Local connection, nothing to set up.** Parallax keeps a key the owner
//!   alone can read, and each side proves it holds the key without sending it
//!   (`docs/local-connect.md` in Parallax). The tower checks Parallax's proof
//!   before sending anything derived from the key. Connecting makes one card,
//!   `herdr-tower`, in Connected Agents, where a person can pause or remove it.
//! - **The token lives in memory.** It is dropped on any answer other than a
//!   good one, so the next poll proves the listener again: whatever takes the
//!   port while Parallax is closed gets a proof it cannot answer, not a bearer.
//! - **127.0.0.1 only, short timeouts, no redirects, no proxy.** A redirect or
//!   an environment `HTTP_PROXY` would carry the token somewhere else. It is
//!   never logged, printed or written anywhere.
//! - **Absent is silent.** No data root, nothing listening: that is a machine
//!   without Parallax running, not a fault, and the result is only "not
//!   available".
//!
//! The resident supervisor polls on a thread of its own ([`Refresher`]) and
//! writes `parallax.json` ([`tower_state::ParallaxStore`]); everything that
//! shows handoffs reads that file.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use time::OffsetDateTime;
use tower_state::{ParallaxCounts, ParallaxSnapshot, ParallaxStore};

/// Parallax's port when `api-port` is missing.
pub const DEFAULT_PORT: u16 = 8765;

/// The name on the tower's card in Parallax's Connected Agents.
const CLIENT: &str = "herdr-tower";

/// One request's whole budget. Parallax answers these from memory in
/// milliseconds; a second and a half is a Parallax that is not answering.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);

/// How often the supervisor asks. Not every two-second pass: a handoff is a
/// person-scale wait, and this is somebody else's server.
pub const POLL_EVERY: Duration = Duration::from_secs(10);

/// No answer this API gives is anywhere near this long; one that is, is not
/// read.
const BODY_LIMIT: u64 = 64 * 1024;

/// Parallax's data root: `$PARALLAX_DATA_ROOT`, else `home`'s `.parallax`, or
/// on Windows `%APPDATA%\Parallax Browser`.
pub fn data_root(home: &Path, env: Option<OsString>, appdata: Option<OsString>) -> PathBuf {
    match (env, appdata) {
        (Some(dir), _) if !dir.is_empty() => PathBuf::from(dir),
        (_, Some(dir)) if !dir.is_empty() => PathBuf::from(dir).join("Parallax Browser"),
        _ => home.join(".parallax"),
    }
}

/// [`data_root`] for this process.
pub fn data_root_here(home: &Path) -> PathBuf {
    let appdata = if cfg!(windows) {
        std::env::var_os("APPDATA")
    } else {
        None
    };
    data_root(home, std::env::var_os("PARALLAX_DATA_ROOT"), appdata)
}

/// What one poll found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// No data root: Parallax is not installed here.
    NotInstalled,
    /// Installed, but not answering or not letting us read. The reason never
    /// carries the key or the token.
    Unreachable(String),
    Read(ParallaxCounts),
}

/// What is kept between polls: the token, once connected.
#[derive(Default)]
pub struct Session {
    token: Option<String>,
}

// No derived `Debug`: a stray `{:?}` must not be able to print the token.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("connected", &self.token.is_some())
            .finish()
    }
}

/// The port from `api-port`, or the default when the file is missing.
fn port_of(root: &Path) -> Result<u16, String> {
    match std::fs::read_to_string(root.join("api-port")) {
        Ok(text) => match text.trim().parse::<u16>() {
            Ok(p) if p != 0 => Ok(p),
            _ => Err("api-port does not hold a port".into()),
        },
        Err(_) => Ok(DEFAULT_PORT),
    }
}

/// Refuses a key another account could have read or written. On Windows
/// Parallax sets an owner-only DACL inside the user's own profile, and the
/// tower relies on that rather than reading the DACL back.
#[cfg(unix)]
fn check_private(path: &Path, want_dir: bool) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).map_err(|_| "its key could not be read")?;
    // SAFETY: geteuid takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    let right_kind = if want_dir {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    if !right_kind || meta.uid() != me || meta.mode() & 0o077 != 0 {
        return Err("its key is not private to this user".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path, _want_dir: bool) -> Result<(), String> {
    Ok(())
}

/// Parallax's key, from `<root>/local-connect/key`.
fn read_key(root: &Path) -> Result<Vec<u8>, String> {
    let dir = root.join("local-connect");
    let file = dir.join("key");
    if !file.exists() {
        return Err("Parallax has not published a local key: it needs v0.26.2 or later".into());
    }
    check_private(&dir, true)?;
    check_private(&file, false)?;
    let text = std::fs::read_to_string(&file).map_err(|_| "its key could not be read")?;
    match URL_SAFE_NO_PAD.decode(text.trim()) {
        Ok(key) if key.len() == 32 => Ok(key),
        _ => Err("its key is not in the expected form".into()),
    }
}

fn hmac_of(key: &[u8], parts: &[&str]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(parts.join("\0").as_bytes());
    mac
}

/// What the tower sends to prove it holds the key.
fn client_proof(key: &[u8], server_nonce: &str, client: &str, port: u16) -> String {
    let mac = hmac_of(
        key,
        &[
            "parallax-local-connect/client",
            server_nonce,
            client,
            &port.to_string(),
        ],
    );
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Whether `proof` shows the listener holds the key, in constant time. The
/// port is the one the tower dialled, so a relay on another port fails this.
fn server_proof_holds(
    key: &[u8],
    client_nonce: &str,
    server_nonce: &str,
    port: u16,
    proof: &str,
) -> bool {
    let Ok(proof) = URL_SAFE_NO_PAD.decode(proof) else {
        return false;
    };
    hmac_of(
        key,
        &[
            "parallax-local-connect/server",
            client_nonce,
            server_nonce,
            &port.to_string(),
        ],
    )
    .verify_slice(&proof)
    .is_ok()
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

/// An answer: its status and body. The error is a fixed sentence: ureq's own
/// messages carry the URL, and nothing here may carry a secret.
fn answer(resp: ureq::http::Response<ureq::Body>) -> Result<(u16, String), String> {
    let status = resp.status().as_u16();
    resp.into_body()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map(|body| (status, body))
        .map_err(|_| "its answer could not be read".to_string())
}

fn post(agent: &ureq::Agent, port: u16, path: &str, body: &str) -> Result<(u16, String), String> {
    let resp = agent
        .post(format!("http://127.0.0.1:{port}{path}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map_err(|_| "not answering".to_string())?;
    answer(resp)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Challenge {
    server_nonce: String,
    proof: String,
}

#[derive(Deserialize)]
struct Connected {
    token: String,
}

/// The two routes of a local connection. Nothing derived from the key goes out
/// until the listener has proved it holds the key too.
fn connect(agent: &ureq::Agent, root: &Path, port: u16) -> Result<String, String> {
    let key = read_key(root)?;
    let nonce = URL_SAFE_NO_PAD
        .encode(tower_os::random_bytes(32).map_err(|_| "no random source for a local connection")?);
    let body = serde_json::json!({ "clientNonce": nonce }).to_string();
    let (status, text) = post(agent, port, "/pairing/local/challenge", &body)?;
    if status != 200 {
        return Err(format!(
            "it offers no local connection ({status}): it needs v0.26.2 or later"
        ));
    }
    let challenge: Challenge = serde_json::from_str(&text)
        .map_err(|_| format!("the service on port {port} is not this user's Parallax"))?;
    if !server_proof_holds(
        &key,
        &nonce,
        &challenge.server_nonce,
        port,
        &challenge.proof,
    ) {
        return Err(format!(
            "the service on port {port} is not this user's Parallax"
        ));
    }
    let proof = client_proof(&key, &challenge.server_nonce, CLIENT, port);
    let body = serde_json::json!({
        "serverNonce": challenge.server_nonce,
        "client": CLIENT,
        "proof": proof,
    })
    .to_string();
    let (status, text) = post(agent, port, "/pairing/local/connect", &body)?;
    match status {
        200 => {}
        403 => return Err("paused or removed in Parallax's Connected Agents".into()),
        s => return Err(format!("it refused the local connection ({s})")),
    }
    let connected: Connected =
        serde_json::from_str(&text).map_err(|_| "its connection answer did not parse")?;
    // Visible ASCII only: anything else cannot be a header value, and a
    // newline in it would be one header smuggled into another.
    let token = connected.token;
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("it gave an unusable token".into());
    }
    Ok(token)
}

/// `{"urgent":n,"action":n,"review":n}`. All three or nothing: a missing one
/// is an API this build does not know, not a zero.
pub fn parse_counts(body: &str) -> Option<ParallaxCounts> {
    serde_json::from_str(body).ok()
}

/// One authorized read: `Ok` for an answer, whatever its status.
fn get_attention(agent: &ureq::Agent, port: u16, token: &str) -> Result<(u16, String), String> {
    let resp = agent
        .get(format!("http://127.0.0.1:{port}/handoffs/attention"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/json")
        .call()
        .map_err(|_| "not answering".to_string())?;
    answer(resp)
}

/// Asks Parallax how many handoffs ask for the person. Never fails: the
/// reasons it could not are a [`Reading`] like any other.
pub fn read(root: &Path, session: &mut Session) -> Reading {
    if !root.is_dir() {
        session.token = None;
        return Reading::NotInstalled;
    }
    let port = match port_of(root) {
        Ok(port) => port,
        Err(why) => {
            session.token = None;
            return Reading::Unreachable(why);
        }
    };
    let agent = agent();
    // A token that was already held may be from before Parallax restarted, so
    // a 401 on it connects again once. One just issued being refused is final.
    let mut held = session.token.is_some();
    loop {
        let token = match session.token.take() {
            Some(token) => token,
            None => match connect(&agent, root, port) {
                Ok(token) => token,
                Err(why) => return Reading::Unreachable(why),
            },
        };
        let (status, body) = match get_attention(&agent, port, &token) {
            Ok(answered) => answered,
            Err(why) => return Reading::Unreachable(why),
        };
        match status {
            200 => {
                return match parse_counts(&body) {
                    Some(counts) => {
                        session.token = Some(token);
                        Reading::Read(counts)
                    }
                    None => Reading::Unreachable("its answer did not parse".into()),
                }
            }
            401 if held => held = false,
            401 | 403 => return Reading::Unreachable(format!("refused the tower ({status})")),
            404 => {
                return Reading::Unreachable(
                    "it has no handoff counts: it needs v0.26.2 or later".into(),
                )
            }
            s => return Reading::Unreachable(format!("answered {s}")),
        }
    }
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
            .spawn(move || {
                let mut session = Session::default();
                loop {
                    let poll = if tower_config::load().parallax_enabled() {
                        Poll::Done {
                            reading: read(&data_root_here(&home), &mut session),
                            at: OffsetDateTime::now_utc(),
                        }
                    } else {
                        session.token = None;
                        Poll::Off
                    };
                    if let Ok(mut l) = latest.lock() {
                        l.0 += 1;
                        l.1 = Some(poll);
                    }
                    std::thread::sleep(POLL_EVERY);
                }
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

/// What the pass remembers between polls: whether Parallax was answering, so
/// a change is logged once rather than every ten seconds, and how many waited
/// at the last answer, so only a rise is announced.
#[derive(Debug, Default)]
pub struct Watch {
    seen: u64,
    /// `None` before the first answer or while off: an unreachable Parallax
    /// at start is a Parallax that is not running, which is not news.
    reachable: Option<bool>,
    /// The last trouble with `parallax.json`, said once until it clears.
    write_failed: Option<String>,
    /// How many waited at the last answer. Filled from the file on the first
    /// answer, so a restarted supervisor does not announce them again, and
    /// kept across a Parallax that stopped answering for the same reason.
    last_waiting: Option<u64>,
    /// Whether what earlier versions left has been cleared.
    legacy_cleared: bool,
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
        if !self.legacy_cleared {
            self.legacy_cleared = true;
            if let Err(e) = store.remove_legacy() {
                log.push(format!("parallax: removing an old file: {e}"));
            }
        }
        let (reading, at) = match poll {
            Poll::Off => {
                if self.reachable.take() == Some(true) {
                    log.push("parallax: turned off, no longer reading its handoffs".into());
                }
                self.last_waiting = None;
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
            Reading::Read(counts) => {
                if self.reachable != Some(true) {
                    log.push("parallax: reading its handoff counts".into());
                }
                self.reachable = Some(true);
                // Before the write below replaces what the last supervisor left.
                let last = self.last_waiting.unwrap_or_else(|| {
                    store
                        .read()
                        .filter(|s| s.reachable)
                        .map_or(0, |s| s.counts.waiting())
                });
                let waiting = counts.waiting();
                if waiting > last {
                    notices.push(notice_for(waiting));
                }
                self.last_waiting = Some(waiting);
                ParallaxSnapshot {
                    updated_at: at,
                    reachable: true,
                    counts,
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
                    counts: ParallaxCounts::default(),
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

/// The notice for a rise to `waiting`. It says how many now wait, since the
/// counts cannot say which one is new; what each asks is in Parallax.
pub fn notice_for(waiting: u64) -> Notice {
    let title = if waiting == 1 {
        "◆ Parallax: 1 handoff needs you".to_string()
    } else {
        format!("◆ Parallax: {waiting} handoffs need you")
    };
    (title, "answer in Parallax Browser".to_string())
}

// ── doctor ───────────────────────────────────────────────────────────────

/// `doctor`'s parallax section. Informational in every state: Parallax is
/// somebody else's program, closed as often as it is open, and nothing the
/// tower's own setup does fixes it.
pub fn doctor(root: &Path, enabled: bool) -> crate::doctorcmd::Finding {
    doctor_line(if enabled {
        Some(read(root, &mut Session::default()))
    } else {
        None
    })
}

/// [`doctor`] for a reading already taken (`None`: the setting is off).
pub fn doctor_line(reading: Option<Reading>) -> crate::doctorcmd::Finding {
    let line = match reading {
        None => "off — `herdr-tower config set parallax true` shows its handoffs".to_string(),
        Some(Reading::NotInstalled) => "not installed — nothing to show".to_string(),
        Some(Reading::Unreachable(why)) => format!("installed, but not readable: {why}"),
        Some(Reading::Read(counts)) => format!(
            "answering: {} waiting on you · {} to review",
            counts.waiting(),
            counts.review
        ),
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

    const KEY: [u8; 32] = [1; 32];

    /// Computed by Python's `hmac`, as an oracle apart from this code, for the
    /// formula in Parallax's `src/pairing/local-connect.ts`.
    #[test]
    fn the_proofs_match_the_formula_computed_elsewhere() {
        assert!(server_proof_holds(
            &KEY,
            "cn",
            "sn",
            8765,
            "x0MCs1dD9SHfFKG6Coi1apwvYeE8fU8qLacF_d0Gfe0"
        ));
        assert_eq!(
            client_proof(&KEY, "sn", "herdr-tower", 8765),
            "ZdgRyyiKbcQB8rm4xxIna7hmrLG1mEpdGH1FKDwLkCg"
        );
    }

    /// The proof binds every part: another port, nonce or key, or a proof that
    /// is not one, fails.
    #[test]
    fn a_server_proof_binds_every_part() {
        let good = "x0MCs1dD9SHfFKG6Coi1apwvYeE8fU8qLacF_d0Gfe0";
        assert!(!server_proof_holds(&KEY, "cn", "sn", 8766, good));
        assert!(!server_proof_holds(&KEY, "cx", "sn", 8765, good));
        assert!(!server_proof_holds(&KEY, "cn", "sx", 8765, good));
        assert!(!server_proof_holds(&[2; 32], "cn", "sn", 8765, good));
        for bad in ["", "!!!", "x0MCs1dD9SHfFKG6"] {
            assert!(!server_proof_holds(&KEY, "cn", "sn", 8765, bad), "{bad}");
        }
    }

    #[test]
    fn counts_need_all_three_numbers() {
        assert_eq!(
            parse_counts(r#"{"urgent":1,"action":2,"review":3,"later":{"x":1}}"#),
            Some(ParallaxCounts {
                urgent: 1,
                action: 2,
                review: 3
            })
        );
        for bad in [
            "",
            "{",
            "[]",
            r#"{"urgent":1,"action":2}"#,
            r#"{"urgent":-1,"action":0,"review":0}"#,
            r#"{"urgent":"1","action":0,"review":0}"#,
            r#"{"urgent":1.5,"action":0,"review":0}"#,
        ] {
            assert_eq!(parse_counts(bad), None, "{bad}");
        }
    }

    fn counts(urgent: u64, action: u64, review: u64) -> Reading {
        Reading::Read(ParallaxCounts {
            urgent,
            action,
            review,
        })
    }

    fn done(reading: Reading, secs: i64) -> Poll {
        Poll::Done {
            reading,
            at: at(secs),
        }
    }

    /// A rise is announced with the new total; the same number, a fall and
    /// review-level work are not.
    #[test]
    fn only_a_rise_in_waiting_is_announced() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let mut watch = Watch::default();
        let (log, notices) = watch.apply(done(counts(0, 1, 0), 0), &store);
        assert_eq!(log, ["parallax: reading its handoff counts"]);
        assert_eq!(notices, [notice_for(1)]);
        assert_eq!(notices[0].0, "◆ Parallax: 1 handoff needs you");
        for (reading, secs) in [
            (counts(0, 1, 0), 10),
            (counts(1, 0, 0), 20),
            (counts(1, 0, 9), 30),
            (counts(0, 0, 9), 40),
        ] {
            let (log, notices) = watch.apply(done(reading, secs), &store);
            assert!(log.is_empty() && notices.is_empty(), "{log:?} {notices:?}");
        }
        let (_, notices) = watch.apply(done(counts(2, 1, 0), 50), &store);
        assert_eq!(notices[0].0, "◆ Parallax: 3 handoffs need you");
    }

    /// A new supervisor starts with a new watch, and the file remembers.
    #[test]
    fn a_restart_does_not_announce_the_same_number_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let _ = Watch::default().apply(done(counts(1, 1, 0), 0), &store);
        let mut restarted = Watch::default();
        let (_, notices) = restarted.apply(done(counts(1, 1, 0), 10), &store);
        assert!(notices.is_empty(), "{notices:?}");
        let (_, notices) = restarted.apply(done(counts(1, 2, 0), 20), &store);
        assert_eq!(notices.len(), 1);
    }

    /// A Parallax that goes away and comes back with the same handoffs has
    /// nothing new to say; one that comes back with more does.
    #[test]
    fn going_away_is_logged_once_and_coming_back_is_no_news() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let mut watch = Watch::default();
        let gone = |secs| done(Reading::Unreachable("not answering".into()), secs);
        let (log, _) = watch.apply(gone(0), &store);
        assert!(log.is_empty(), "not running from the start is not news");
        let (log, notices) = watch.apply(done(counts(0, 2, 0), 10), &store);
        assert_eq!(log, ["parallax: reading its handoff counts"]);
        assert_eq!(notices.len(), 1);
        let (log, _) = watch.apply(gone(20), &store);
        assert_eq!(log, ["parallax: stopped answering: not answering"]);
        let (log, _) = watch.apply(gone(30), &store);
        assert!(log.is_empty());
        let snap = store.read().unwrap();
        assert!(!snap.reachable);
        assert_eq!(snap.counts, ParallaxCounts::default());
        let (_, notices) = watch.apply(done(counts(0, 2, 0), 40), &store);
        assert!(notices.is_empty(), "{notices:?}");
        let (_, notices) = watch.apply(gone(50), &store);
        assert!(notices.is_empty());
        let (_, notices) = watch.apply(done(counts(0, 3, 0), 60), &store);
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn off_removes_the_snapshot_and_forgets_the_number() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let mut watch = Watch::default();
        let _ = watch.apply(done(counts(0, 1, 0), 0), &store);
        let (log, _) = watch.apply(Poll::Off, &store);
        assert_eq!(
            log,
            ["parallax: turned off, no longer reading its handoffs"]
        );
        assert_eq!(store.read(), None);
        let (_, notices) = watch.apply(done(counts(0, 1, 0), 10), &store);
        assert_eq!(notices.len(), 1, "on again: it is on screen anew");
    }

    #[test]
    fn an_unwritable_snapshot_is_said_once_and_notices_do_not_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        // A folder where the file goes: every write of it fails.
        std::fs::create_dir(dir.path().join(tower_state::PARALLAX_FILE)).unwrap();
        let mut watch = Watch::default();
        let (log, notices) = watch.apply(done(counts(0, 1, 0), 0), &store);
        assert_eq!(notices.len(), 1);
        assert!(
            log.iter().any(|l| l.contains("writing parallax.json")),
            "{log:?}"
        );
        for secs in [10, 20] {
            let (log, notices) = watch.apply(done(counts(0, 1, 0), secs), &store);
            assert!(log.is_empty() && notices.is_empty(), "{log:?} {notices:?}");
        }
    }

    #[test]
    fn the_first_poll_clears_what_earlier_versions_left() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let old = dir.path().join("parallax-notified.json");
        std::fs::write(&old, "[]").unwrap();
        let _ = Watch::default().apply(done(counts(0, 0, 0), 0), &store);
        assert!(!old.exists());
    }

    #[test]
    fn the_doctor_says_each_state() {
        let line = |r| doctor_line(r).line;
        assert!(line(None).starts_with("off"));
        assert_eq!(
            line(Some(Reading::NotInstalled)),
            "not installed — nothing to show"
        );
        assert_eq!(
            line(Some(counts(1, 2, 3))),
            "answering: 3 waiting on you · 3 to review"
        );
        let unreachable = doctor_line(Some(Reading::Unreachable("not answering".into())));
        assert!(unreachable.ok, "Parallax being closed is not a fault");
        assert_eq!(
            unreachable.line,
            "installed, but not readable: not answering"
        );
    }

    // ── against a server ────────────────────────────────────────────────

    /// What a fake Parallax does, by what it is asked.
    #[derive(Clone)]
    struct Script {
        key: Vec<u8>,
        /// Proofs name this port rather than the socket's, for a relay.
        proof_port: Option<u16>,
        connect_status: u16,
        /// The answers to `/handoffs/attention`, in order; the last repeats.
        attention: Vec<(u16, String)>,
    }

    impl Default for Script {
        fn default() -> Self {
            Script {
                key: KEY.to_vec(),
                proof_port: None,
                connect_status: 200,
                attention: vec![(200, r#"{"urgent":1,"action":2,"review":3}"#.into())],
            }
        }
    }

    /// One request as the fake saw it.
    #[derive(Debug, Clone, PartialEq)]
    struct Seen {
        path: String,
        auth: String,
    }

    /// A tiny HTTP/1.1 server on 127.0.0.1 that plays Parallax's half.
    struct Fake {
        port: u16,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    fn fake(script: Script) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut attention = script.attention.clone().into_iter().peekable();
            let mut last = None;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let (mut auth, mut length) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("authorization:") {
                        auth = v.trim().to_string();
                    }
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let body: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                log.lock().unwrap().push(Seen {
                    path: path.clone(),
                    auth,
                });
                let proof_port = script.proof_port.unwrap_or(port);
                let (status, out) = match path.as_str() {
                    "/pairing/local/challenge" => {
                        let nonce = body["clientNonce"].as_str().unwrap_or("");
                        let mac = hmac_of(
                            &script.key,
                            &[
                                "parallax-local-connect/server",
                                nonce,
                                "server-nonce",
                                &proof_port.to_string(),
                            ],
                        );
                        let proof = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
                        (
                            200,
                            serde_json::json!({"serverNonce":"server-nonce","proof":proof})
                                .to_string(),
                        )
                    }
                    "/pairing/local/connect" => {
                        let want = client_proof(&script.key, "server-nonce", CLIENT, port);
                        if body["proof"].as_str() != Some(want.as_str())
                            || body["client"].as_str() != Some(CLIENT)
                        {
                            (
                                401,
                                r#"{"error":"local-connect-proof-refused"}"#.to_string(),
                            )
                        } else {
                            (
                                script.connect_status,
                                r#"{"token":"tok3n","bindingId":"b","connectionKind":"direct-agent"}"#
                                    .to_string(),
                            )
                        }
                    }
                    "/handoffs/attention" => {
                        if let Some(next) = attention.next() {
                            last = Some(next);
                        }
                        last.clone().unwrap()
                    }
                    _ => (404, "{}".to_string()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nLocation: http://127.0.0.1:{port}/handoffs/attention\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                    out.len()
                );
                let _ = stream.flush();
            }
        });
        Fake { port, seen }
    }

    /// A data root with the key, private as Parallax leaves it.
    fn root_for(port: Option<u16>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(p) = port {
            std::fs::write(dir.path().join("api-port"), format!("{p}\n")).unwrap();
        }
        let keys = dir.path().join("local-connect");
        std::fs::create_dir(&keys).unwrap();
        let file = keys.join("key");
        std::fs::write(&file, format!("{}\n", URL_SAFE_NO_PAD.encode(KEY))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        dir
    }

    fn paths(server: &Fake) -> Vec<String> {
        let seen = server.seen.lock().unwrap();
        seen.iter().map(|s| s.path.clone()).collect()
    }

    const CHALLENGE: &str = "/pairing/local/challenge";
    const CONNECT: &str = "/pairing/local/connect";
    const ATTENTION: &str = "/handoffs/attention";

    #[test]
    fn it_connects_once_then_reuses_the_token() {
        let server = fake(Script::default());
        let root = root_for(Some(server.port));
        let mut session = Session::default();
        let want = counts(1, 2, 3);
        assert_eq!(read(root.path(), &mut session), want);
        assert_eq!(read(root.path(), &mut session), want);
        assert_eq!(paths(&server), [CHALLENGE, CONNECT, ATTENTION, ATTENTION]);
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen[0].auth, "", "no bearer before the proof");
        assert_eq!(seen[1].auth, "");
        assert_eq!(seen[2].auth, "bearer tok3n");
        assert_eq!(seen[3].auth, "bearer tok3n");
    }

    /// A Parallax that restarted issues new tokens: a 401 on an old one
    /// connects again, once, within the same poll.
    #[test]
    fn a_stale_token_connects_again_within_the_poll() {
        let server = fake(Script {
            attention: vec![
                (200, r#"{"urgent":0,"action":0,"review":0}"#.into()),
                (401, "{}".into()),
                (200, r#"{"urgent":0,"action":1,"review":0}"#.into()),
            ],
            ..Script::default()
        });
        let root = root_for(Some(server.port));
        let mut session = Session::default();
        let _ = read(root.path(), &mut session);
        assert_eq!(read(root.path(), &mut session), counts(0, 1, 0));
        assert_eq!(
            paths(&server),
            [CHALLENGE, CONNECT, ATTENTION, ATTENTION, CHALLENGE, CONNECT, ATTENTION]
        );
    }

    /// A token just issued being refused is not retried: that would be a
    /// connection on every request of every poll.
    #[test]
    fn a_fresh_token_refused_is_final() {
        let server = fake(Script {
            attention: vec![(401, "{}".into())],
            ..Script::default()
        });
        let root = root_for(Some(server.port));
        let mut session = Session::default();
        assert_eq!(
            read(root.path(), &mut session),
            Reading::Unreachable("refused the tower (401)".into())
        );
        assert_eq!(paths(&server), [CHALLENGE, CONNECT, ATTENTION]);
        assert!(session.token.is_none());
    }

    /// A listener without the key gets nothing derived from it: the client
    /// proof and the token never go out.
    #[test]
    fn a_listener_without_the_key_is_sent_nothing() {
        let server = fake(Script {
            key: vec![9; 32],
            ..Script::default()
        });
        let root = root_for(Some(server.port));
        let reading = read(root.path(), &mut Session::default());
        assert_eq!(
            reading,
            Reading::Unreachable(format!(
                "the service on port {} is not this user's Parallax",
                server.port
            ))
        );
        assert_eq!(paths(&server), [CHALLENGE]);
    }

    /// The real Parallax behind a relay on another port: its proof names its
    /// own port, so the tower stops there.
    #[test]
    fn a_relay_on_another_port_is_sent_nothing() {
        let server = fake(Script {
            proof_port: Some(8765),
            ..Script::default()
        });
        let root = root_for(Some(server.port));
        assert!(matches!(
            read(root.path(), &mut Session::default()),
            Reading::Unreachable(_)
        ));
        assert_eq!(paths(&server), [CHALLENGE]);
    }

    /// Once connected, any bad answer drops the token, so the next poll proves
    /// the listener again rather than handing the bearer to whatever is there.
    #[test]
    fn a_bad_answer_drops_the_token() {
        for (status, body) in [
            (500, "{}"),
            (403, "{}"),
            (404, "{}"),
            (302, "{}"),
            (200, "not json"),
        ] {
            let server = fake(Script {
                attention: vec![
                    (200, r#"{"urgent":0,"action":0,"review":0}"#.into()),
                    (status, body.into()),
                ],
                ..Script::default()
            });
            let root = root_for(Some(server.port));
            let mut session = Session::default();
            let _ = read(root.path(), &mut session);
            assert!(session.token.is_some());
            assert!(matches!(
                read(root.path(), &mut session),
                Reading::Unreachable(_)
            ));
            assert!(session.token.is_none(), "{status}");
        }
    }

    #[test]
    fn nothing_listening_drops_the_token_and_is_not_answering() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let root = root_for(Some(port));
        let mut session = Session {
            token: Some("t0ken".into()),
        };
        let reading = read(root.path(), &mut session);
        assert_eq!(reading, Reading::Unreachable("not answering".into()));
        assert!(session.token.is_none());
        let again = read(root.path(), &mut session);
        assert_eq!(again, Reading::Unreachable("not answering".into()));
    }

    #[test]
    fn a_paused_binding_says_so() {
        let server = fake(Script {
            connect_status: 403,
            ..Script::default()
        });
        let root = root_for(Some(server.port));
        assert_eq!(
            read(root.path(), &mut Session::default()),
            Reading::Unreachable("paused or removed in Parallax's Connected Agents".into())
        );
        assert_eq!(paths(&server), [CHALLENGE, CONNECT]);
    }

    #[test]
    fn a_missing_root_is_not_installed_and_a_bad_port_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::default();
        assert_eq!(
            read(&dir.path().join("none"), &mut session),
            Reading::NotInstalled
        );
        let root = root_for(None);
        std::fs::write(root.path().join("api-port"), "eighty").unwrap();
        assert_eq!(
            read(root.path(), &mut session),
            Reading::Unreachable("api-port does not hold a port".into())
        );
    }

    #[test]
    fn a_key_that_is_absent_or_malformed_is_not_used() {
        let root = root_for(Some(1));
        let file = root.path().join("local-connect/key");
        // The last decodes, but to 33 bytes rather than 32.
        for bad in ["", "short", "!!!!", &"a".repeat(44)] {
            std::fs::write(&file, bad).unwrap();
            assert!(read_key(root.path()).is_err(), "{bad:?}");
        }
        std::fs::remove_file(&file).unwrap();
        assert!(read_key(root.path()).unwrap_err().contains("v0.26.2"));
    }

    #[cfg(unix)]
    #[test]
    fn a_key_others_can_reach_is_refused() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = root_for(Some(1));
        let dir = root.path().join("local-connect");
        let file = dir.join("key");
        assert_eq!(read_key(root.path()).unwrap(), KEY);
        for (path, mode) in [(&file, 0o640), (&file, 0o604), (&dir, 0o750), (&dir, 0o701)] {
            let good = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                read_key(root.path()).unwrap_err(),
                "its key is not private to this user",
                "{path:?} {mode:o}"
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(good)).unwrap();
        }
        // A link to a private file elsewhere is not the file.
        let elsewhere = root.path().join("elsewhere");
        std::fs::rename(&file, &elsewhere).unwrap();
        symlink(&elsewhere, &file).unwrap();
        assert!(read_key(root.path()).is_err());
    }

    #[test]
    fn the_data_root_honours_the_environment() {
        let home = Path::new("/home/x");
        assert_eq!(data_root(home, None, None), home.join(".parallax"));
        assert_eq!(
            data_root(home, Some("".into()), None),
            home.join(".parallax")
        );
        assert_eq!(
            data_root(home, Some("/data/p".into()), Some("/app".into())),
            PathBuf::from("/data/p")
        );
        assert_eq!(
            data_root(home, None, Some("/app".into())),
            PathBuf::from("/app").join("Parallax Browser")
        );
        assert_eq!(
            data_root(home, None, Some("".into())),
            home.join(".parallax")
        );
    }

    #[test]
    fn a_session_never_prints_its_token() {
        let session = Session {
            token: Some("s3cret".into()),
        };
        assert!(!format!("{session:?}").contains("s3cret"));
    }
}
