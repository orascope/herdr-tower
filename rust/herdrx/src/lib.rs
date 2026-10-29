// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Client for the herdr socket API.
//!
//! It observes (`pane.list`, `pane.read`), moves or delivers (`agent.focus`,
//! `agent.prompt`), and in [`layout`] makes and names places for agents. It
//! closes a pane, tab or workspace (`pane.close`, `tab.close`,
//! `workspace.close`) only when a caller asks for that one by ID; nothing in
//! this crate closes anything on its own.
//!
//! Protocol, measured against a running herdr 0.8.2 server and cross-checked
//! with the 0.9.0 docs (both speak protocol 20):
//!
//! - Newline-delimited JSON. Write one request line, read one response line.
//! - **One request per connection.** The server closes once it answers, so a
//!   second request on the same connection yields `EPIPE`. [`Client::call`]
//!   therefore connects every time. The one exception is `events.subscribe`,
//!   which holds the connection open and pushes events.
//! - No authentication. Socket file permissions are the only boundary.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The herdr socket protocol generation this client targets.
pub const PROTOCOL: i64 = 20;

/// Bounds one request/response exchange. Every call goes over a local socket
/// and finishes in milliseconds; the timeout exists so a wedged server cannot
/// stall the caller's polling loop.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Covers the gap between the server finishing a wait it was asked for and its
/// response landing on the socket.
const DEADLINE_SLACK: Duration = Duration::from_secs(5);

/// A structured error from herdr. Callers branch on [`ApiError::code`] rather
/// than matching strings — `Herdr::focus` falls back to `pane.focus` when
/// `agent.focus` reports `agent_not_found`.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

/// Anything that can go wrong talking to herdr.
#[derive(Debug)]
pub enum Error {
    /// The server answered with a structured error.
    Api(ApiError),
    /// The socket could not be reached, or the exchange failed.
    Io(std::io::Error),
    /// The response was not the shape we expected.
    Decode(serde_json::Error),
    /// The socket path could not be resolved.
    NoSocket,
    /// The request was rejected here, before it reached the server — a token
    /// name herdr's schema forbids, a subscription with no types. Catching
    /// these locally points at the cause far better than a server-side error
    /// does, and it keeps a malformed request off the wire.
    Invalid(String),
}

impl Error {
    /// Reports whether this is an [`ApiError`] carrying that code.
    pub fn is_code(&self, code: &str) -> bool {
        matches!(self, Error::Api(e) if e.code == code)
    }

    /// Whether the server may have acted on the request this error ended.
    ///
    /// A structured answer is a refusal — the server read the request and
    /// said no — except `agent_prompt_stalled` and `timeout`, which come after
    /// the text was submitted and say only that no start was seen. A failure
    /// to connect or to find the socket never reached it. Anything else (the
    /// exchange broke or timed out, the answer did not decode) may have come
    /// after the server acted — on a loaded machine, a reply that misses the
    /// deadline is the common case, not the rare one.
    pub fn may_have_acted(&self) -> bool {
        match self {
            Error::Api(e) => matches!(e.code.as_str(), "agent_prompt_stalled" | "timeout"),
            Error::NoSocket | Error::Invalid(_) => false,
            Error::Io(e) => !matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ),
            Error::Decode(_) => true,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Api(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "herdr socket: {e}"),
            Error::Decode(e) => write!(f, "cannot decode herdr response: {e}"),
            Error::NoSocket => write!(f, "cannot resolve the herdr socket path"),
            Error::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Decode(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Resolves the socket path the way herdr's socket-api page documents:
/// `HERDR_SOCKET_PATH`, then `HERDR_SESSION=<name>`, then the default session.
///
/// `env` and `home` are parameters so tests can drive the resolution.
pub fn socket_path<F>(env: F, home: &str) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(p) = env("HERDR_SOCKET_PATH").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = config_dir(&env, home);
    match env("HERDR_SESSION").filter(|s| !s.is_empty()) {
        Some(name) => base.join("sessions").join(name).join("herdr.sock"),
        None => base.join("herdr.sock"),
    }
}

/// herdr's config folder, where its sockets live: `~/.config/herdr` on macOS
/// and Linux, `%APPDATA%\herdr` on Windows — what herdr's own `config_dir`
/// picks when `XDG_CONFIG_HOME` is not set.
pub fn config_dir<F>(env: &F, home: &str) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    if cfg!(windows) {
        if let Some(appdata) = env("APPDATA").filter(|p| !p.is_empty()) {
            return PathBuf::from(appdata).join("herdr");
        }
        return PathBuf::from(home)
            .join("AppData")
            .join("Roaming")
            .join("herdr");
    }
    PathBuf::from(home).join(".config").join("herdr")
}

/// Stable namespace for a socket when several Herdr servers share one store.
/// The default socket is un-namespaced: its id is empty.
pub fn backend_id(socket: &Path, default: &Path) -> String {
    let socket = socket
        .canonicalize()
        .unwrap_or_else(|_| socket.to_path_buf());
    let default = default
        .canonicalize()
        .unwrap_or_else(|_| default.to_path_buf());
    if socket == default {
        return String::new();
    }
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in socket.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("b{hash:016x}")
}

/// Backend namespace selected by one pane's injected environment.
pub fn backend_from_env<F>(env: F, home: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let socket = socket_path(&env, home);
    let default = socket_path(|_| None, home);
    backend_id(&socket, &default)
}

/// Resolves the socket path in the real environment.
///
/// An explicitly configured path is honored before the home directory is even
/// looked up: a container with `HOME` unset has no resolvable home, and failing
/// there would discard a path the caller already gave us in full.
pub fn default_socket_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(socket_path(|k| std::env::var(k).ok(), &home))
}

/// Reports whether this process runs inside a herdr-managed pane.
pub fn inside() -> bool {
    std::env::var("HERDR_ENV").as_deref() == Ok("1")
}

/// The public pane ID herdr injects into each managed pane (`"w2:p4S"`).
pub fn current_pane_id() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

static REQ_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_id() -> String {
    format!("herdr-tower-{}", REQ_SEQ.fetch_add(1, Ordering::Relaxed))
}

#[derive(Serialize)]
struct Request<'a, P> {
    id: String,
    method: &'a str,
    params: P,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<ApiError>,
}

/// Issues herdr socket calls. An empty socket resolves to
/// [`default_socket_path`]; tests point it at a temporary socket to stay
/// isolated from the real server.
#[derive(Debug, Clone, Default)]
pub struct Client {
    pub socket: Option<PathBuf>,
    pub timeout: Option<Duration>,
}

impl Client {
    /// A client talking to a specific socket.
    pub fn at(socket: impl Into<PathBuf>) -> Self {
        Client {
            socket: Some(socket.into()),
            timeout: None,
        }
    }

    /// Invokes `method` once and decodes the result.
    pub fn call<P: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        self.call_within(self.timeout.unwrap_or(DEFAULT_TIMEOUT), method, params)
    }

    /// [`Client::call`] with a longer deadline for this one request.
    ///
    /// The default suits calls that answer immediately, but herdr has several
    /// that block on purpose — `agent.prompt` with a wait, `agent.wait`,
    /// `agent.start`. Telling the server to wait two minutes while the socket
    /// gives up after three seconds produces a local timeout long before the
    /// server would have answered.
    pub fn call_within<P: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        timeout: Duration,
        method: &str,
        params: P,
    ) -> Result<R> {
        self.call_deadline(Some(timeout), method, params)
    }

    /// [`Client::call`] with no deadline at all.
    ///
    /// One herdr call can legitimately block forever: `agent.wait` without
    /// `timeout_ms` waits indefinitely by design. Padding a deadline around a
    /// wait that has none produces a local timeout that says nothing about the
    /// agent — the caller cannot tell "still working" from "the wait failed".
    /// The honest socket deadline for an unbounded wait is no deadline.
    pub fn call_blocking<P: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        self.call_deadline(None, method, params)
    }

    fn call_deadline<P: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        timeout: Option<Duration>,
        method: &str,
        params: P,
    ) -> Result<R> {
        let path = match &self.socket {
            Some(p) => p.clone(),
            None => default_socket_path().ok_or(Error::NoSocket)?,
        };

        let stream = connect_within(&path, timeout)?;
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;

        let body = serde_json::to_vec(&Request {
            id: next_id(),
            method,
            params,
        })?;
        let mut w = &stream;
        w.write_all(&body)?;
        w.write_all(b"\n")?;
        w.flush()?;

        // The response is one line, but pane.list grows with the pane count;
        // read_line grows the buffer as needed.
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;

        let env: Envelope = serde_json::from_str(&line)?;
        if let Some(e) = env.error {
            return Err(Error::Api(e));
        }
        let value = env.result.unwrap_or(serde_json::Value::Null);
        Ok(serde_json::from_value(value)?)
    }
}

/// Pads a requested server-side wait into a socket deadline.
pub fn deadline_for(wait: Duration) -> Duration {
    if wait.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        wait + DEADLINE_SLACK
    }
}

/// How often, and how many times, [`retry_while`] calls again while herdr is
/// still catching up with a pane it just made.
const READY_RETRY: Duration = Duration::from_millis(250);
const READY_TRIES: u32 = 40;

/// Runs `call`, again while herdr answers `code`, for up to ten seconds.
///
/// Only that one refusal is retried: each is herdr not having caught up yet.
/// Anything else — a blocked agent, a dead pane — is an answer, not a race.
pub fn retry_while(code: &str, call: impl FnMut() -> Result<()>) -> Result<()> {
    retry_every(code, READY_TRIES, READY_RETRY, call)
}

fn retry_every(
    code: &str,
    most: u32,
    pause: Duration,
    mut call: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut tries = 1;
    loop {
        match call() {
            Err(e) if e.is_code(code) && tries < most => {
                tries += 1;
                std::thread::sleep(pause);
            }
            other => return other,
        }
    }
}
mod conn;
pub(crate) use conn::{connect_within, Conn};
pub mod events;
pub mod herdr;
pub mod layout;
pub mod metadata;
pub mod types;
pub use events::*;
pub use herdr::*;
pub use layout::*;
pub use metadata::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn api(code: &str) -> Error {
        Error::Api(ApiError {
            code: code.into(),
            message: "m".into(),
        })
    }

    fn decode() -> Error {
        Error::Decode(serde_json::from_str::<u8>("x").unwrap_err())
    }

    /// Each kind of failure, and whether the server may have acted on the
    /// request before it. A refusal and a request that never left are certain;
    /// the two API codes that come after a submitted prompt, a broken or late
    /// exchange, and an unreadable answer are not.
    #[test]
    fn may_have_acted_sorts_each_failure() {
        let cases: Vec<(&str, Error, bool)> = vec![
            ("a plain refusal", api("agent_busy"), false),
            ("an empty code", api(""), false),
            ("a code that only resembles one", api("timeout_ms"), false),
            (
                "submitted, no start seen",
                api("agent_prompt_stalled"),
                true,
            ),
            ("submitted, the wait ran out", api("timeout"), true),
            ("no socket path", Error::NoSocket, false),
            (
                "rejected before sending",
                Error::Invalid("bad".into()),
                false,
            ),
            (
                "no socket file",
                Error::Io(ErrorKind::NotFound.into()),
                false,
            ),
            (
                "nobody listening",
                Error::Io(ErrorKind::ConnectionRefused.into()),
                false,
            ),
            (
                "a read deadline",
                Error::Io(ErrorKind::WouldBlock.into()),
                true,
            ),
            ("a timeout", Error::Io(ErrorKind::TimedOut.into()), true),
            ("a hang-up", Error::Io(ErrorKind::BrokenPipe.into()), true),
            (
                "cut short",
                Error::Io(ErrorKind::UnexpectedEof.into()),
                true,
            ),
            ("an unreadable answer", decode(), true),
        ];
        for (name, e, want) in cases {
            assert_eq!(e.may_have_acted(), want, "{name}: {e}");
        }
    }

    /// Calls `retry_every` with answers taken in order, returning the result
    /// and how many calls it made.
    fn retried(code: &str, most: u32, answers: Vec<Result<()>>) -> (Result<()>, usize) {
        let mut answers = answers.into_iter();
        let mut calls = 0;
        let got = retry_every(code, most, Duration::ZERO, || {
            calls += 1;
            answers.next().unwrap_or(Ok(()))
        });
        (got, calls)
    }

    #[test]
    fn retry_waits_out_the_named_refusal() {
        let (got, calls) = retried(
            "agent_pane_busy",
            40,
            vec![Err(api("agent_pane_busy")), Err(api("agent_pane_busy"))],
        );
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(calls, 3, "two refusals, then the call that went in");
    }

    #[test]
    fn retry_takes_any_other_failure_as_the_answer() {
        let (got, calls) = retried(
            "agent_pane_busy",
            40,
            vec![Err(api("agent_pane_busy")), Err(api("agent_not_ready"))],
        );
        assert!(got.unwrap_err().is_code("agent_not_ready"));
        assert_eq!(calls, 2);
    }

    /// Against a model of it: with `busy` refusals before the call goes in
    /// and `most` tries allowed, it makes `min(busy + 1, most)` calls — at
    /// least one, even for a budget of none — and succeeds only when the
    /// refusals ran out first. The last refusal is what it returns otherwise.
    #[test]
    fn retry_matches_its_model_for_every_budget() {
        for busy in 0..8usize {
            for most in 0..8u32 {
                let answers = (0..busy).map(|_| Err(api("agent_pane_busy"))).collect();
                let (got, calls) = retried("agent_pane_busy", most, answers);
                let budget = most.max(1) as usize;
                assert_eq!(calls, (busy + 1).min(budget), "busy {busy} most {most}");
                if busy < budget {
                    assert!(got.is_ok(), "busy {busy} most {most}: {got:?}");
                } else {
                    assert!(got.unwrap_err().is_code("agent_pane_busy"));
                }
            }
        }
    }

    /// The code is matched whole: a near miss is another refusal, and is the
    /// answer.
    #[test]
    fn retry_matches_the_code_exactly() {
        for near in ["agent_pane", "agent_pane_busy ", "AGENT_PANE_BUSY", ""] {
            let (got, calls) = retried("agent_pane_busy", 40, vec![Err(api(near))]);
            assert!(got.unwrap_err().is_code(near));
            assert_eq!(calls, 1, "{near:?}");
        }
        let (got, calls) = retried(
            "agent_pane_busy",
            40,
            vec![Err(Error::Io(std::io::ErrorKind::TimedOut.into()))],
        );
        assert!(got.is_err());
        assert_eq!(calls, 1, "a broken exchange is not a refusal to wait out");
    }

    #[test]
    fn retry_gives_up_after_its_last_try() {
        let busy = || Err(api("agent_pane_busy"));
        let (got, calls) = retried("agent_pane_busy", 3, (0..5).map(|_| busy()).collect());
        assert!(got.unwrap_err().is_code("agent_pane_busy"));
        assert_eq!(calls, 3);
    }

    /// Every I/O failure but the two that prove the socket was never reached
    /// may have come after the server acted — so a new kind added to the
    /// standard library falls on the cautious side without a change here.
    #[test]
    fn only_an_unreached_socket_is_a_certain_io_failure() {
        let kinds = [
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::NotConnected,
            ErrorKind::AddrInUse,
            ErrorKind::BrokenPipe,
            ErrorKind::WouldBlock,
            ErrorKind::InvalidData,
            ErrorKind::TimedOut,
            ErrorKind::WriteZero,
            ErrorKind::Interrupted,
            ErrorKind::UnexpectedEof,
            ErrorKind::Other,
        ];
        for kind in kinds {
            let unreached = matches!(kind, ErrorKind::NotFound | ErrorKind::ConnectionRefused);
            assert_eq!(
                Error::Io(kind.into()).may_have_acted(),
                !unreached,
                "{kind:?}"
            );
        }
    }

    /// The two questions an error answers about API codes agree: a code
    /// `is_code` names is one `may_have_acted` judges, and only the
    /// post-submit pair says the server may have acted.
    #[test]
    fn may_have_acted_agrees_with_is_code() {
        for code in ["agent_prompt_stalled", "timeout", "agent_busy", "internal"] {
            let e = api(code);
            assert!(e.is_code(code));
            assert_eq!(
                e.may_have_acted(),
                e.is_code("agent_prompt_stalled") || e.is_code("timeout"),
                "{code}"
            );
        }
    }
}
