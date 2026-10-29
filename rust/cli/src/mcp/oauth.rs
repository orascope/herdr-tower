// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The authorization server: a single-user OAuth 2.1 server whose only way to
//! say yes is a pairing code issued on the Mac.
//!
//! Pure over [`Grants`]: every function takes the state, the time and a source
//! of randomness, and the HTTP layer does the reading and writing. That keeps
//! the whole flow — register, authorize, exchange, refresh, reuse — testable
//! without a socket.

use std::collections::HashMap;
use std::io;

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use super::grants::{self, AuthCode, Client, Grant, Grants, Pairing, Pending};

pub const SCOPE_READ: &str = "tower.read";
pub const SCOPE_ACT: &str = "tower.act";

pub const ACCESS_TTL: Duration = Duration::hours(1);
pub const REFRESH_TTL: Duration = Duration::days(30);
pub const CODE_TTL: Duration = Duration::seconds(60);
pub const PAIRING_TTL: Duration = Duration::minutes(5);
/// Wrong codes one approval page may take before it is closed.
pub const MAX_FAILURES: u32 = 5;
/// How long an approval page waits for its code.
pub const REQUEST_TTL: Duration = Duration::minutes(10);
/// Open approval pages at once. Past it the oldest page that holds no pairing
/// code makes room ([`open_request`]); a new page is never refused.
///
/// A page is a few hundred bytes of the grants file, so the ceiling is there
/// to bound the file, not to ration pages. It was 20 with a refusal past it,
/// and anyone could hold all 20 and answer every real `GET /authorize` with
/// `503` (#24).
pub const MAX_PENDING_REQUESTS: usize = 100;
/// Open approval pages kept per client; past it that client's oldest goes.
pub const MAX_REQUESTS_PER_CLIENT: usize = 3;
/// Registrations that have not turned into a grant yet. `/register` is open to
/// the internet, and without a ceiling anyone could grow the grants file; past
/// it, the oldest pending one that is not in the middle of connecting is
/// dropped ([`register`]).
///
/// Twice [`MAX_PENDING_REQUESTS`]: every client with an open page is kept,
/// and at most that many can have one, so there is always an idle
/// registration to drop.
pub const MAX_PENDING_CLIENTS: usize = 2 * MAX_PENDING_REQUESTS;
/// Spent refresh tokens remembered per grant, for reuse detection.
pub const MAX_SPENT_REFRESH: usize = 100;

/// Where secrets come from. The real one reads `/dev/urandom`; tests count.
pub trait Random {
    fn token(&mut self) -> io::Result<String>;
    fn code(&mut self) -> io::Result<String>;
}

pub struct OsRandom;

impl Random for OsRandom {
    fn token(&mut self) -> io::Result<String> {
        grants::random_token()
    }
    fn code(&mut self) -> io::Result<String> {
        grants::random_code()
    }
}

/// An OAuth error response (RFC 6749 §5.2, RFC 7591 §3.2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub status: u16,
    pub error: &'static str,
    pub description: String,
}

impl OAuthError {
    fn new(status: u16, error: &'static str, description: impl Into<String>) -> Self {
        OAuthError {
            status,
            error,
            description: description.into(),
        }
    }

    pub fn body(&self) -> Value {
        json!({ "error": self.error, "error_description": self.description })
    }
}

fn internal(e: io::Error) -> OAuthError {
    OAuthError::new(500, "server_error", e.to_string())
}

// ------------------------------------------------------------------ metadata

/// RFC 9728: where this resource's authorization server is.
pub fn protected_resource_metadata(base: &str) -> Value {
    json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "scopes_supported": [SCOPE_READ, SCOPE_ACT],
        "bearer_methods_supported": ["header"],
        "resource_name": "herdr-tower",
    })
}

/// RFC 8414: the authorization server's endpoints and what it accepts.
pub fn authorization_server_metadata(base: &str) -> Value {
    json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "scopes_supported": [SCOPE_READ, SCOPE_ACT],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "authorization_response_iss_parameter_supported": true,
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
    })
}

// ------------------------------------------------------------------ register

/// Dynamic client registration (RFC 7591).
///
/// Every redirect URI has to be on `allowed` exactly. That is what stops a
/// stranger's client from registering a callback of its own and walking off
/// with a code — it can still register, but only to send the code to claude.ai.
/// A callback on this machine — `http://127.0.0.1:<port>/…`,
/// `http://localhost:<port>/…`, `http://[::1]:<port>/…` — where a local
/// client such as Claude Code waits for its code. Only a process here can
/// receive on it. Allowed while the server has no public URL and serves
/// this machine alone ([`register`]'s `loopback`).
pub fn is_loopback_callback(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("http://") else {
        return false;
    };
    let host_port = rest.split('/').next().unwrap_or("");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => (host_port, ""),
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        && port.bytes().all(|b| b.is_ascii_digit())
        && !rest.contains('@')
}

pub fn register(
    g: &mut Grants,
    req: &Value,
    allowed: &[String],
    loopback: bool,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Result<Value, OAuthError> {
    let bad = |d: String| OAuthError::new(400, "invalid_client_metadata", d);
    let uris: Vec<String> = req
        .get("redirect_uris")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if uris.is_empty() {
        return Err(OAuthError::new(
            400,
            "invalid_redirect_uri",
            "redirect_uris is required",
        ));
    }
    if let Some(u) = uris
        .iter()
        .find(|u| !(allowed.contains(u) || loopback && is_loopback_callback(u)))
    {
        return Err(OAuthError::new(
            400,
            "invalid_redirect_uri",
            format!("{u} is not an allowed callback (mcp.redirect_uris)"),
        ));
    }
    let listed = |key: &str, ok: &[&str]| -> Result<(), OAuthError> {
        match req.get(key).and_then(Value::as_array) {
            Some(a) => match a
                .iter()
                .find(|v| !v.as_str().is_some_and(|s| ok.contains(&s)))
            {
                Some(v) => Err(bad(format!("{key}: {v} is not supported"))),
                None => Ok(()),
            },
            None => Ok(()),
        }
    };
    listed("grant_types", &["authorization_code", "refresh_token"])?;
    listed("response_types", &["code"])?;
    // RFC 7591 §2: an absent method means client_secret_basic.
    let method = req
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str)
        .unwrap_or("client_secret_basic");
    if !matches!(
        method,
        "none" | "client_secret_post" | "client_secret_basic"
    ) {
        return Err(bad(format!(
            "token_endpoint_auth_method {method} is not supported"
        )));
    }
    // Full: the oldest idle registration makes room. Refusing instead would
    // let anyone on the internet fill the slots and lock the real client out.
    //
    // Idle means no grant, no open approval page and no authorization code
    // waiting to be exchanged. A client in the middle of connecting is never
    // dropped: before, twenty `/register` calls pushed out the person's client
    // while its page was open, they approved it, and `/token` then answered
    // "unknown client" (#24). Clients with a page are at most
    // [`MAX_PENDING_REQUESTS`], and codes live a minute, so below
    // [`MAX_PENDING_CLIENTS`] there is always an idle one; if there is not,
    // the newcomer is refused rather than anyone in flight dropped.
    let busy = |id: &str| {
        g.grants.iter().any(|x| x.client_id == id)
            || g.requests.iter().any(|r| r.client_id == id)
            || g.codes.iter().any(|k| k.client_id == id)
    };
    let pending = g
        .clients
        .iter()
        .filter(|c| !g.grants.iter().any(|x| x.client_id == c.client_id))
        .count();
    if pending >= MAX_PENDING_CLIENTS {
        let oldest = g
            .clients
            .iter()
            .enumerate()
            .filter(|(_, c)| !busy(&c.client_id))
            .min_by_key(|(_, c)| c.created_at)
            .map(|(i, _)| i);
        let Some(oldest) = oldest else {
            return Err(OAuthError::new(
                503,
                "temporarily_unavailable",
                "too many clients are connecting right now — try again in a few minutes",
            ));
        };
        g.clients.remove(oldest);
    }
    let name: String = req
        .get("client_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    let client_id = format!("c-{}", rnd.token().map_err(internal)?);
    let secret = if method == "none" {
        None
    } else {
        Some(rnd.token().map_err(internal)?)
    };
    g.clients.push(Client {
        client_id: client_id.clone(),
        client_name: name.clone(),
        redirect_uris: uris.clone(),
        secret_hash: secret.as_deref().map(grants::hash).unwrap_or_default(),
        created_at: now,
    });
    let mut out = json!({
        "client_id": client_id,
        "client_id_issued_at": now.unix_timestamp(),
        "client_name": name,
        "redirect_uris": uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": method,
    });
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
        out["client_secret_expires_at"] = json!(0);
    }
    Ok(out)
}

// ------------------------------------------------------------------ authorize

/// A validated authorization request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeRequest {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub state: Option<String>,
    pub scopes: Vec<String>,
}

/// Checks an authorization request before anything is shown.
///
/// A failure here is never redirected: until the client and its callback are
/// known to match, the callback is just a URL somebody typed.
pub fn check_authorize(
    g: &Grants,
    p: &HashMap<String, String>,
) -> Result<AuthorizeRequest, String> {
    let get = |k: &str| p.get(k).map(String::as_str).filter(|v| !v.is_empty());
    if get("response_type") != Some("code") {
        return Err("response_type must be code".into());
    }
    let client_id = get("client_id").ok_or("client_id is missing")?;
    let client = g
        .client(client_id)
        .ok_or("this client is not registered here — add the connector again")?;
    let redirect_uri = match get("redirect_uri") {
        Some(u) if client.redirect_uris.iter().any(|r| r == u) => u.to_string(),
        Some(_) => return Err("redirect_uri does not match the registration".into()),
        None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
        None => return Err("redirect_uri is missing".into()),
    };
    let challenge = get("code_challenge").ok_or("code_challenge is missing (PKCE is required)")?;
    if get("code_challenge_method") != Some("S256") {
        return Err("code_challenge_method must be S256".into());
    }
    Ok(AuthorizeRequest {
        client_id: client_id.to_string(),
        client_name: client.client_name.clone(),
        redirect_uri,
        challenge: challenge.to_string(),
        state: get("state").map(String::from),
        scopes: granted_scopes(get("scope").unwrap_or("")),
    })
}

/// What a request starts with: read, always. Act is never taken from what a
/// client asked for — it is a separate box the person ticks on the approval
/// page (see [`approve`]).
fn granted_scopes(_asked: &str) -> Vec<String> {
    vec![SCOPE_READ.to_string()]
}

/// Opens an approval request for a checked authorization request, and
/// returns the id the page shows. Never refused for want of room: see the
/// eviction below.
///
/// A pairing code is issued for one request (`mcp pair`), and wrong codes
/// count against that request alone. Before, one global counter and a
/// 15-minute lockout sat behind the unauthenticated `POST /authorize`, so five
/// wrong guesses from anyone stopped everyone from pairing (#103). Counting per
/// client instead would have let a stranger register clients without end and
/// guess five times with each; binding the code to a request leaves a stranger
/// nothing to guess, since no code is ever issued for their request.
pub fn open_request(
    g: &mut Grants,
    req: &AuthorizeRequest,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Result<String, OAuthError> {
    let id = loop {
        let id = grants::hash(&rnd.token().map_err(internal)?)[..6].to_ascii_uppercase();
        if !g.requests.iter().any(|r| r.id == id) {
            break id;
        }
    };
    // The page a pairing code was issued for is never pushed out, by anyone:
    // the person is typing that code into it. A client over its own limit
    // gives up its own oldest other page.
    //
    // Past the overall limit the oldest page without a code makes room, and a
    // new page is never refused. Refusing was worse (#24): anyone could hold
    // every slot with pages of their own and answer each real `GET /authorize`
    // with `503`, for as long as they cared to renew them. Now the cost of a
    // flood is that it may push out a page the person opened but has not
    // paired yet, and a reload opens a new one — and once `mcp pair` has
    // issued its code, that page stays put however many others are opened.
    // (#118 had it the other way round: no page pushed out, new ones refused.)
    let paired = g.pairing.as_ref().map(|p| p.request.clone());
    let unpaired = |r: &Pending| paired.as_deref() != Some(r.id.as_str());
    let mine = g
        .requests
        .iter()
        .filter(|r| r.client_id == req.client_id)
        .count();
    if mine >= MAX_REQUESTS_PER_CLIENT {
        let oldest = g
            .requests
            .iter()
            .enumerate()
            .filter(|(_, r)| r.client_id == req.client_id && unpaired(r))
            .min_by_key(|(_, r)| r.created_at)
            .map(|(i, _)| i);
        if let Some(i) = oldest {
            g.requests.remove(i);
        }
    }
    while g.requests.len() >= MAX_PENDING_REQUESTS {
        let oldest = g
            .requests
            .iter()
            .enumerate()
            .filter(|(_, r)| unpaired(r))
            .min_by_key(|(_, r)| r.created_at)
            .map(|(i, _)| i)
            .expect("one page at most holds the code, and the limit is above one");
        g.requests.remove(oldest);
    }
    g.requests.push(Pending {
        id: id.clone(),
        client_id: req.client_id.clone(),
        client_name: req.client_name.clone(),
        redirect_uri: req.redirect_uri.clone(),
        challenge: req.challenge.clone(),
        state: req.state.clone(),
        scopes: req.scopes.clone(),
        created_at: now,
        expires_at: now + REQUEST_TTL,
        failures: 0,
    });
    Ok(id)
}

/// Why an approval was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The page's request expired, was closed, or never existed.
    UnknownRequest,
    /// No code has been issued for this request yet, or it expired.
    NoPairing {
        request: String,
    },
    WrongCode {
        left: u32,
    },
    /// Too many wrong codes on this page; it was closed.
    Exhausted,
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::UnknownRequest => {
                "This approval page has expired or was closed. Add the connector again to get a new one.".into()
            }
            Refusal::NoPairing { request } => format!(
                "No code has been issued for request {request} yet. On the Mac run `herdr-tower mcp pair {request}`, then enter the code it shows."
            ),
            Refusal::WrongCode { left } => {
                format!("That code is wrong. {left} attempt(s) left on this page.")
            }
            Refusal::Exhausted => {
                "Too many wrong codes on this page, so it was closed. Add the connector again to start over.".into()
            }
        }
    }
}

/// Approves an open request with the pairing code the person typed, and
/// returns where to send the browser. `act` is whether they also allowed the
/// connection to write to agents.
pub fn approve(
    g: &mut Grants,
    request: &str,
    typed: &str,
    act: bool,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Result<Result<String, Refusal>, OAuthError> {
    let Some(i) = g
        .requests
        .iter()
        .position(|r| r.id == request && r.expires_at > now)
    else {
        return Ok(Err(Refusal::UnknownRequest));
    };
    let Some(pairing) = g
        .pairing
        .as_ref()
        .filter(|p| p.request == request && p.expires_at > now)
    else {
        // Nothing to guess against, so nothing is counted.
        return Ok(Err(Refusal::NoPairing {
            request: request.to_string(),
        }));
    };
    let typed: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    if grants::hash(&typed) != pairing.code_hash {
        let r = &mut g.requests[i];
        r.failures += 1;
        if r.failures >= MAX_FAILURES {
            g.requests.remove(i);
            g.pairing = None;
            return Ok(Err(Refusal::Exhausted));
        }
        return Ok(Err(Refusal::WrongCode {
            left: MAX_FAILURES - r.failures,
        }));
    }
    // Single use: one code, one page, one connection.
    let r = g.requests.remove(i);
    g.pairing = None;
    let code = rnd.token().map_err(internal)?;
    let mut scopes = r.scopes.clone();
    if act && !scopes.iter().any(|x| x == SCOPE_ACT) {
        scopes.push(SCOPE_ACT.to_string());
    }
    g.codes.push(AuthCode {
        code_hash: grants::hash(&code),
        client_id: r.client_id.clone(),
        redirect_uri: r.redirect_uri.clone(),
        challenge: r.challenge.clone(),
        scopes,
        expires_at: now + CODE_TTL,
    });
    let mut url = r.redirect_uri.clone();
    url.push(if url.contains('?') { '&' } else { '?' });
    url.push_str("code=");
    url.push_str(&super::http::encode(&code));
    if let Some(state) = &r.state {
        url.push_str("&state=");
        url.push_str(&super::http::encode(state));
    }
    Ok(Ok(url))
}

/// The approval pages open now, oldest first.
pub fn open_pages(g: &Grants, now: OffsetDateTime) -> Vec<Pending> {
    let mut open: Vec<Pending> = g
        .requests
        .iter()
        .filter(|r| r.expires_at > now)
        .cloned()
        .collect();
    open.sort_by_key(|r| r.created_at);
    open
}

/// The host a callback sends the browser back to: `claude.ai` for
/// `https://claude.ai/api/mcp/auth_callback`.
pub fn redirect_host(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.rsplit('@').next().unwrap_or(authority)
}

/// A client's name as it gave it, or a stand-in for none.
pub fn shown_name(name: &str) -> &str {
    if name.is_empty() {
        "(unnamed client)"
    } else {
        name
    }
}

/// One approval page as `mcp pair` lists it: its id, the name the client
/// gave itself (anyone can register as "Claude", so it is marked as the
/// client's own claim), where the page sends the browser back to, and how
/// long it has been open. The callback host is the part a stranger cannot
/// choose — it has to be on `mcp.redirect_uris`.
pub fn describe_page(p: &Pending, now: OffsetDateTime) -> String {
    format!(
        "{}  \"{}\" (self-declared), returns to {}, opened {}",
        p.id,
        shown_name(&p.client_name),
        redirect_host(&p.redirect_uri),
        tower_text::age(p.created_at, now)
    )
}

/// Issues a pairing code for the open request named by `request`, replacing
/// any other code, and returns it with the request it is for.
///
/// The id is required. With no id and one page open, the code used to go to
/// that page — and with the person's own page not loaded yet, or expired, the
/// one page open could be a stranger's calling itself "Claude" (#25). Picking
/// a page is now the caller's job, with the person looking at it (`mcp pair`
/// asks before it uses the only page open).
pub fn issue_pairing(
    g: &mut Grants,
    request: &str,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Result<(String, Pending), String> {
    let open = open_pages(g, now);
    let id = request.trim().to_ascii_uppercase();
    let target = open.iter().find(|r| r.id == id).cloned().ok_or_else(|| {
        if open.is_empty() {
            format!("no approval page {id} is open — add the connector (or reload its page) first")
        } else {
            let list: Vec<String> = open
                .iter()
                .map(|p| format!("  {}", describe_page(p, now)))
                .collect();
            format!(
                "no approval page {id} is open; these are:\n{}",
                list.join("\n")
            )
        }
    })?;
    let code = rnd.code().map_err(|e| e.to_string())?;
    g.pairing = Some(Pairing {
        request: target.id.clone(),
        code_hash: grants::hash(&code),
        expires_at: now + PAIRING_TTL,
    });
    Ok((code, target))
}

/// Revokes one grant by id, or every grant with `--all`, and returns how
/// many went.
///
/// What the client has in flight goes too: an authorization code it has not
/// exchanged yet, the approval pages it has open and a pairing code issued
/// for one of them. Otherwise a code landed a moment before the revocation
/// still turned into a fresh grant at `/token` (#112), and `mcp pair` could
/// hand a code to a page for a client just disconnected. The client's
/// registration stays: it may be approved again, with a new pairing code.
pub fn revoke(g: &mut Grants, which: &str) -> usize {
    let before = g.grants.len();
    if which == "--all" {
        g.grants.clear();
        g.codes.clear();
        g.requests.clear();
        g.pairing = None;
        return before;
    }
    let Some(client) = g
        .grants
        .iter()
        .find(|x| x.id == which)
        .map(|x| x.client_id.clone())
    else {
        return 0;
    };
    g.grants.retain(|x| x.id != which);
    g.codes.retain(|c| c.client_id != client);
    g.requests.retain(|r| r.client_id != client);
    let requests = &g.requests;
    if g.pairing
        .as_ref()
        .is_some_and(|p| !requests.iter().any(|r| r.id == p.request))
    {
        g.pairing = None;
    }
    before - g.grants.len()
}

// ------------------------------------------------------------------ token

/// Client credentials from an `Authorization: Basic` header, if any.
pub type Basic = Option<(String, String)>;

/// The token endpoint: an authorization code or a refresh token in, a pair of
/// tokens out.
pub fn token(
    g: &mut Grants,
    form: &HashMap<String, String>,
    basic: Basic,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Result<Value, OAuthError> {
    let get = |k: &str| form.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let (client_id, secret) = match &basic {
        Some((id, s)) => (Some(id.as_str()), Some(s.as_str())),
        None => (get("client_id"), get("client_secret")),
    };
    let client_id =
        client_id.ok_or_else(|| OAuthError::new(401, "invalid_client", "client_id is missing"))?;
    let client = g
        .client(client_id)
        .ok_or_else(|| OAuthError::new(401, "invalid_client", "unknown client"))?;
    if !client.secret_hash.is_empty()
        && secret.map(grants::hash) != Some(client.secret_hash.clone())
    {
        return Err(OAuthError::new(
            401,
            "invalid_client",
            "client authentication failed",
        ));
    }
    let client_name = client.client_name.clone();
    let invalid = |d: &str| OAuthError::new(400, "invalid_grant", d);

    match get("grant_type") {
        Some("authorization_code") => {
            let code = get("code").ok_or_else(|| invalid("code is missing"))?;
            let hash = grants::hash(code);
            // Taken out whether or not the rest checks out: a code is tried once.
            let i = g
                .codes
                .iter()
                .position(|c| c.code_hash == hash)
                .ok_or_else(|| invalid("unknown or used code"))?;
            let c = g.codes.remove(i);
            if c.client_id != client_id || c.expires_at <= now {
                return Err(invalid("unknown or expired code"));
            }
            if get("redirect_uri").is_some_and(|u| u != c.redirect_uri) {
                return Err(invalid("redirect_uri does not match"));
            }
            let verifier =
                get("code_verifier").ok_or_else(|| invalid("code_verifier is missing"))?;
            if !pkce_ok(verifier, &c.challenge) {
                return Err(invalid("code_verifier does not match the challenge"));
            }
            let access = rnd.token().map_err(internal)?;
            let refresh = rnd.token().map_err(internal)?;
            let id = grants::hash(&rnd.token().map_err(internal)?)[..8].to_string();
            g.grants.push(Grant {
                id,
                client_id: client_id.to_string(),
                client_name,
                scopes: c.scopes.clone(),
                created_at: now,
                last_used: None,
                access_hash: grants::hash(&access),
                access_expires: now + ACCESS_TTL,
                refresh_hash: grants::hash(&refresh),
                refresh_expires: now + REFRESH_TTL,
                spent_refresh: Vec::new(),
            });
            Ok(token_body(&access, &refresh, &c.scopes))
        }
        Some("refresh_token") => {
            let presented =
                get("refresh_token").ok_or_else(|| invalid("refresh_token is missing"))?;
            let hash = grants::hash(presented);
            if let Some(i) = g
                .grants
                .iter()
                .position(|x| x.spent_refresh.contains(&hash))
            {
                // A refresh token used twice was copied. Whoever holds the
                // live one loses it too — the person reconnects with a code.
                g.grants.remove(i);
                return Err(invalid("refresh token reused; the grant is revoked"));
            }
            let grant = g
                .grants
                .iter_mut()
                .find(|x| x.refresh_hash == hash && x.client_id == client_id)
                .ok_or_else(|| invalid("unknown refresh token"))?;
            let access = rnd.token().map_err(internal)?;
            let refresh = rnd.token().map_err(internal)?;
            grant
                .spent_refresh
                .push(std::mem::take(&mut grant.refresh_hash));
            // A grant refreshes hourly for as long as it lives, so the history
            // is capped. A token older than this is still refused — it is
            // simply not recognised as a reuse any more.
            let over = grant.spent_refresh.len().saturating_sub(MAX_SPENT_REFRESH);
            grant.spent_refresh.drain(..over);
            grant.access_hash = grants::hash(&access);
            grant.access_expires = now + ACCESS_TTL;
            grant.refresh_hash = grants::hash(&refresh);
            grant.refresh_expires = now + REFRESH_TTL;
            Ok(token_body(&access, &refresh, &grant.scopes.clone()))
        }
        Some(other) => Err(OAuthError::new(
            400,
            "unsupported_grant_type",
            format!("{other} is not supported"),
        )),
        None => Err(OAuthError::new(
            400,
            "invalid_request",
            "grant_type is missing",
        )),
    }
}

fn token_body(access: &str, refresh: &str, scopes: &[String]) -> Value {
    json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL.whole_seconds(),
        "refresh_token": refresh,
        "scope": scopes.join(" "),
    })
}

/// RFC 7636 S256: `BASE64URL(SHA256(verifier)) == challenge`, with the
/// verifier's length and alphabet checked first.
pub fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    let shape = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    shape
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()))
            == challenge
}

// ------------------------------------------------------------------ bearer

/// A caller the server recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub grant: String,
    pub client: String,
    pub scopes: Vec<String>,
    /// Whether `last_used` is old enough to be worth writing.
    pub touch: bool,
}

/// Looks a bearer token up. Read-only, so a request does not write the file;
/// the caller writes `last_used` when [`Caller::touch`] says so.
pub fn authenticate(g: &Grants, bearer: &str, now: OffsetDateTime) -> Option<Caller> {
    let hash = grants::hash(bearer);
    let grant = g
        .grants
        .iter()
        .find(|x| x.access_hash == hash && x.access_expires > now)?;
    Some(Caller {
        grant: grant.id.clone(),
        client: grant.client_name.clone(),
        scopes: grant.scopes.clone(),
        touch: grant
            .last_used
            .is_none_or(|t| now - t >= Duration::minutes(1)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_on_this_machine_is_told_from_one_elsewhere() {
        for ok in [
            "http://127.0.0.1:53123/callback",
            "http://localhost:3118/oauth/callback",
            "http://[::1]:9000/cb",
            "http://localhost/cb",
        ] {
            assert!(is_loopback_callback(ok), "{ok}");
        }
        for bad in [
            "https://127.0.0.1:1/cb",
            "http://127.0.0.1.evil.com/cb",
            "http://localhost:80@evil.com/cb",
            "http://evil.com/cb",
            "http://localhost:x/cb",
        ] {
            assert!(!is_loopback_callback(bad), "{bad}");
        }
    }

    #[test]
    fn a_local_client_registers_only_while_the_server_serves_this_machine() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let req = json!({"client_name":"Claude Code","redirect_uris":["http://localhost:53123/callback"],"token_endpoint_auth_method":"none"});
        assert!(register(&mut g, &req, &allowed(), false, now(), &mut rnd).is_err());
        assert!(register(&mut g, &req, &allowed(), true, now(), &mut rnd).is_ok());
        let far = json!({"redirect_uris":["https://evil.example/cb"]});
        assert!(register(&mut g, &far, &allowed(), true, now(), &mut rnd).is_err());
    }

    /// Tokens `t1`, `t2`, … and codes from a fixed list, so a test can name
    /// what it expects to be issued.
    struct Seq {
        n: u32,
        codes: Vec<&'static str>,
    }

    impl Seq {
        fn new() -> Self {
            Seq {
                n: 0,
                codes: vec!["123456", "654321"],
            }
        }
    }

    impl Random for Seq {
        fn token(&mut self) -> io::Result<String> {
            self.n += 1;
            Ok(format!("t{}", self.n))
        }
        fn code(&mut self) -> io::Result<String> {
            Ok(self.codes.remove(0).to_string())
        }
    }

    const CB: &str = "https://claude.ai/api/mcp/auth_callback";
    // RFC 7636 appendix B.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn allowed() -> Vec<String> {
        vec![CB.to_string()]
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn public_client(g: &mut Grants, rnd: &mut Seq) -> String {
        let r = register(
            g,
            &json!({"client_name":"Claude","redirect_uris":[CB],"token_endpoint_auth_method":"none"}),
            &allowed(),
            false,
            now(),
            rnd,
        )
        .unwrap();
        r["client_id"].as_str().unwrap().to_string()
    }

    fn auth_params(client: &str) -> HashMap<String, String> {
        map(&[
            ("response_type", "code"),
            ("client_id", client),
            ("redirect_uri", CB),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("state", "s t&ate"),
            ("scope", "tower.read tower.act"),
        ])
    }

    /// Register, pair, approve, exchange: the whole happy path, returning the
    /// token response.
    fn connect(g: &mut Grants, rnd: &mut Seq) -> (String, Value) {
        let client = public_client(g, rnd);
        let req = check_authorize(g, &auth_params(&client)).unwrap();
        let rid = open_request(g, &req, now(), rnd).unwrap();
        let code = issue_pairing(g, &rid, now(), rnd).unwrap().0;
        let url = approve(g, &rid, &code, false, now(), rnd).unwrap().unwrap();
        let issued = url
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        let tok = token(
            g,
            &map(&[
                ("grant_type", "authorization_code"),
                ("code", issued),
                ("redirect_uri", CB),
                ("client_id", &client),
                ("code_verifier", VERIFIER),
            ]),
            None,
            now(),
            rnd,
        )
        .unwrap();
        (client, tok)
    }

    #[test]
    fn pkce_matches_the_rfc_vector_only() {
        assert!(pkce_ok(VERIFIER, CHALLENGE));
        assert!(!pkce_ok(
            VERIFIER,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cN"
        ));
        assert!(!pkce_ok("short", CHALLENGE), "under 43 characters");
        assert!(
            !pkce_ok(&format!("{VERIFIER}!"), CHALLENGE),
            "outside the alphabet"
        );
    }

    #[test]
    fn registration_accepts_only_allowed_callbacks() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let err = register(
            &mut g,
            &json!({"redirect_uris":["https://evil.example/cb"]}),
            &allowed(),
            false,
            now(),
            &mut rnd,
        )
        .unwrap_err();
        assert_eq!(err.error, "invalid_redirect_uri");
        let err = register(&mut g, &json!({}), &allowed(), false, now(), &mut rnd).unwrap_err();
        assert_eq!(err.error, "invalid_redirect_uri");
        let err = register(
            &mut g,
            &json!({"redirect_uris":[CB],"grant_types":["client_credentials"]}),
            &allowed(),
            false,
            now(),
            &mut rnd,
        )
        .unwrap_err();
        assert_eq!(err.error, "invalid_client_metadata");
        assert!(g.clients.is_empty());
    }

    #[test]
    fn an_absent_auth_method_gets_a_secret_and_must_use_it() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let r = register(
            &mut g,
            &json!({"redirect_uris":[CB]}),
            &allowed(),
            false,
            now(),
            &mut rnd,
        )
        .unwrap();
        assert_eq!(r["token_endpoint_auth_method"], "client_secret_basic");
        let secret = r["client_secret"].as_str().unwrap().to_string();
        let id = r["client_id"].as_str().unwrap().to_string();
        let form = map(&[("grant_type", "refresh_token"), ("refresh_token", "x")]);
        let wrong = token(
            &mut g,
            &form,
            Some((id.clone(), "nope".into())),
            now(),
            &mut rnd,
        );
        assert_eq!(wrong.unwrap_err().error, "invalid_client");
        let right = token(&mut g, &form, Some((id, secret)), now(), &mut rnd);
        assert_eq!(
            right.unwrap_err().error,
            "invalid_grant",
            "authenticated, then refused"
        );
    }

    #[test]
    fn a_full_registry_drops_its_oldest_pending_client_not_the_newcomer() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        // A connected client, which no amount of registering may displace.
        let (connected, _) = connect(&mut g, &mut rnd);
        let first = public_client(&mut g, &mut rnd);
        for _ in 1..MAX_PENDING_CLIENTS {
            public_client(&mut g, &mut rnd);
        }
        let newest = public_client(&mut g, &mut rnd);
        assert!(g.client(&newest).is_some(), "the newcomer is registered");
        assert!(
            g.client(&first).is_none(),
            "the oldest pending one made room"
        );
        assert!(g.client(&connected).is_some());
        assert_eq!(g.clients.len(), MAX_PENDING_CLIENTS + 1);
    }

    #[test]
    fn an_authorize_request_is_checked_before_anything_is_shown() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let client = public_client(&mut g, &mut rnd);
        let ok = check_authorize(&g, &auth_params(&client)).unwrap();
        assert_eq!(ok.client_name, "Claude");
        assert_eq!(ok.scopes, vec![SCOPE_READ], "act is not granted yet");
        assert_eq!(ok.state.as_deref(), Some("s t&ate"));

        let with = |k: &str, v: &str| {
            let mut p = auth_params(&client);
            p.insert(k.into(), v.into());
            check_authorize(&g, &p)
        };
        assert!(with("redirect_uri", "https://evil.example/cb").is_err());
        assert!(with("client_id", "c-unknown").is_err());
        assert!(with("code_challenge_method", "plain").is_err());
        assert!(with("response_type", "token").is_err());
        let mut p = auth_params(&client);
        p.remove("code_challenge");
        assert!(check_authorize(&g, &p).is_err());
        let mut p = auth_params(&client);
        p.remove("redirect_uri");
        assert_eq!(
            check_authorize(&g, &p).unwrap().redirect_uri,
            CB,
            "the only one registered"
        );
    }

    #[test]
    fn the_whole_flow_issues_tokens_that_authenticate() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let (_, tok) = connect(&mut g, &mut rnd);
        assert_eq!(tok["token_type"], "Bearer");
        assert_eq!(tok["expires_in"], 3600);
        assert_eq!(tok["scope"], "tower.read");
        let access = tok["access_token"].as_str().unwrap();
        let caller = authenticate(&g, access, now()).unwrap();
        assert_eq!(caller.scopes, vec![SCOPE_READ]);
        assert!(caller.touch);
        assert!(
            authenticate(&g, access, now() + ACCESS_TTL).is_none(),
            "expired"
        );
        assert!(authenticate(&g, "t999", now()).is_none());
        assert!(g.pairing.is_none(), "the code was single use");
        assert!(g.codes.is_empty(), "the authorization code was single use");
    }

    #[test]
    fn act_is_granted_only_when_the_person_ticks_it() {
        for act in [false, true] {
            let mut g = Grants::default();
            let mut rnd = Seq::new();
            let client = public_client(&mut g, &mut rnd);
            let req = check_authorize(&g, &auth_params(&client)).unwrap();
            let rid = open_request(&mut g, &req, now(), &mut rnd).unwrap();
            assert_eq!(req.scopes, vec![SCOPE_READ], "asking for act is not enough");
            let code = issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap().0;
            approve(&mut g, &rid, &code, act, now(), &mut rnd)
                .unwrap()
                .unwrap();
            let tok = token(
                &mut g,
                &map(&[
                    ("grant_type", "authorization_code"),
                    ("code", "t3"),
                    ("client_id", &client),
                    ("code_verifier", VERIFIER),
                ]),
                None,
                now(),
                &mut rnd,
            )
            .unwrap();
            let want = if act {
                "tower.read tower.act"
            } else {
                "tower.read"
            };
            assert_eq!(tok["scope"], want);
        }
    }

    #[test]
    fn the_redirect_carries_the_code_and_the_escaped_state() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let client = public_client(&mut g, &mut rnd);
        let req = check_authorize(&g, &auth_params(&client)).unwrap();
        let rid = open_request(&mut g, &req, now(), &mut rnd).unwrap();
        let code = issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap().0;
        let url = approve(&mut g, &rid, &format!(" {code} "), false, now(), &mut rnd)
            .unwrap()
            .unwrap();
        assert_eq!(url, format!("{CB}?code=t3&state=s%20t%26ate"));
    }

    #[test]
    fn an_authorization_code_is_tried_once_and_bound_to_its_verifier() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let client = public_client(&mut g, &mut rnd);
        let req = check_authorize(&g, &auth_params(&client)).unwrap();
        let rid = open_request(&mut g, &req, now(), &mut rnd).unwrap();
        let code = issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap().0;
        approve(&mut g, &rid, &code, false, now(), &mut rnd)
            .unwrap()
            .unwrap();
        let form = |verifier: &str| {
            map(&[
                ("grant_type", "authorization_code"),
                ("code", "t3"),
                ("client_id", &client),
                ("code_verifier", verifier),
            ])
        };
        let wrong = "x".repeat(43);
        assert_eq!(
            token(&mut g, &form(&wrong), None, now(), &mut rnd)
                .unwrap_err()
                .error,
            "invalid_grant"
        );
        assert!(
            token(&mut g, &form(VERIFIER), None, now(), &mut rnd).is_err(),
            "a failed attempt used the code up"
        );
    }

    #[test]
    fn an_expired_authorization_code_is_refused() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let client = public_client(&mut g, &mut rnd);
        let req = check_authorize(&g, &auth_params(&client)).unwrap();
        let rid = open_request(&mut g, &req, now(), &mut rnd).unwrap();
        let code = issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap().0;
        approve(&mut g, &rid, &code, false, now(), &mut rnd)
            .unwrap()
            .unwrap();
        let form = map(&[
            ("grant_type", "authorization_code"),
            ("code", "t3"),
            ("client_id", &client),
            ("code_verifier", VERIFIER),
        ]);
        let late = now() + CODE_TTL;
        assert_eq!(
            token(&mut g, &form, None, late, &mut rnd)
                .unwrap_err()
                .error,
            "invalid_grant"
        );
    }

    #[test]
    fn refresh_rotates_and_a_reused_refresh_token_revokes_the_grant() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let (client, tok) = connect(&mut g, &mut rnd);
        let first = tok["refresh_token"].as_str().unwrap().to_string();
        let refresh = |g: &mut Grants, rnd: &mut Seq, t: &str| {
            token(
                g,
                &map(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", t),
                    ("client_id", &client),
                ]),
                None,
                now(),
                rnd,
            )
        };
        let second = refresh(&mut g, &mut rnd, &first).unwrap();
        let new_access = second["access_token"].as_str().unwrap();
        assert!(authenticate(&g, new_access, now()).is_some());
        assert!(
            authenticate(&g, tok["access_token"].as_str().unwrap(), now()).is_none(),
            "the old access token is replaced"
        );
        assert_eq!(
            refresh(&mut g, &mut rnd, &first).unwrap_err().error,
            "invalid_grant"
        );
        assert!(g.grants.is_empty(), "reuse revoked the whole grant");
    }

    #[test]
    fn the_spent_refresh_history_is_capped() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let (client, tok) = connect(&mut g, &mut rnd);
        let mut refresh = tok["refresh_token"].as_str().unwrap().to_string();
        for _ in 0..MAX_SPENT_REFRESH + 5 {
            let form = map(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", &client),
            ]);
            let r = token(&mut g, &form, None, now(), &mut rnd).unwrap();
            refresh = r["refresh_token"].as_str().unwrap().to_string();
        }
        assert_eq!(g.grants[0].spent_refresh.len(), MAX_SPENT_REFRESH);
    }

    /// Opens an approval page for a freshly registered client.
    fn page(g: &mut Grants, rnd: &mut Seq) -> String {
        let client = public_client(g, rnd);
        let req = check_authorize(g, &auth_params(&client)).unwrap();
        open_request(g, &req, now(), rnd).unwrap()
    }

    /// #103: wrong codes close the page they were typed on and nothing else.
    /// There is no global lockout: a new page pairs as usual right after.
    #[test]
    fn wrong_codes_close_only_their_own_page() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let mine = page(&mut g, &mut rnd);
        let code = issue_pairing(&mut g, &mine, now(), &mut rnd).unwrap().0;
        for left in (1..MAX_FAILURES).rev() {
            assert_eq!(
                approve(&mut g, &mine, "000000", false, now(), &mut rnd).unwrap(),
                Err(Refusal::WrongCode { left })
            );
        }
        assert_eq!(
            approve(&mut g, &mine, "000000", false, now(), &mut rnd).unwrap(),
            Err(Refusal::Exhausted)
        );
        assert!(g.pairing.is_none(), "the code went with its page");
        assert_eq!(
            approve(&mut g, &mine, &code, false, now(), &mut rnd).unwrap(),
            Err(Refusal::UnknownRequest),
            "the right code is too late for a closed page"
        );
        // Nothing is locked: another page pairs straight away.
        let next = page(&mut g, &mut rnd);
        let code = issue_pairing(&mut g, &next, now(), &mut rnd).unwrap().0;
        assert!(approve(&mut g, &next, &code, false, now(), &mut rnd)
            .unwrap()
            .is_ok());
    }

    /// #103: a stranger hammering their own page has nothing to guess — no
    /// code is ever issued for it — and cannot touch anyone else's.
    #[test]
    fn guesses_on_a_page_without_a_code_count_for_nothing() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let theirs = page(&mut g, &mut rnd);
        for _ in 0..100 {
            assert_eq!(
                approve(&mut g, &theirs, "000000", false, now(), &mut rnd).unwrap(),
                Err(Refusal::NoPairing {
                    request: theirs.clone()
                })
            );
        }
        let mine = page(&mut g, &mut rnd);
        let code = issue_pairing(&mut g, &mine, now(), &mut rnd).unwrap().0;
        // Their page cannot use my code, and trying costs mine nothing.
        assert_eq!(
            approve(&mut g, &theirs, &code, false, now(), &mut rnd).unwrap(),
            Err(Refusal::NoPairing {
                request: theirs.clone()
            })
        );
        assert!(approve(&mut g, &mine, &code, false, now(), &mut rnd)
            .unwrap()
            .is_ok());
        assert_eq!(
            approve(&mut g, "NOSUCH", "000000", false, now(), &mut rnd).unwrap(),
            Err(Refusal::UnknownRequest)
        );
    }

    /// #25: a code is issued only for the page named by its id, whatever is
    /// open — never for "the only page" by default.
    #[test]
    fn a_code_is_issued_only_for_the_page_named() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let none = issue_pairing(&mut g, "ABC123", now(), &mut rnd).unwrap_err();
        assert!(none.contains("no approval page ABC123 is open"), "{none}");
        let a = page(&mut g, &mut rnd);
        let b = page(&mut g, &mut rnd);
        let wrong = issue_pairing(&mut g, "ZZZZZZ", now(), &mut rnd).unwrap_err();
        assert!(wrong.contains(&a) && wrong.contains(&b), "{wrong}");
        assert!(wrong.contains("(self-declared)"), "{wrong}");
        assert!(wrong.contains("returns to claude.ai"), "{wrong}");
        assert!(g.pairing.is_none());
        let typed = format!(" {} ", b.to_ascii_lowercase());
        let (_, req) = issue_pairing(&mut g, &typed, now(), &mut rnd).unwrap();
        assert_eq!(req.id, b, "ids are matched however they are typed");
        assert_eq!(g.pairing.as_ref().unwrap().request, b);
    }

    #[test]
    fn a_page_is_described_by_what_a_stranger_cannot_choose() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let id = page(&mut g, &mut rnd);
        let later = now() + Duration::minutes(3);
        let open = open_pages(&g, later);
        assert_eq!(open.len(), 1);
        assert_eq!(
            describe_page(&open[0], later),
            format!("{id}  \"Claude\" (self-declared), returns to claude.ai, opened 3m ago")
        );
        assert!(open_pages(&g, now() + REQUEST_TTL).is_empty(), "expired");
        assert_eq!(
            redirect_host("http://127.0.0.1:53123/cb"),
            "127.0.0.1:53123"
        );
        assert_eq!(redirect_host("https://u@claude.ai/x?y"), "claude.ai");
        assert_eq!(shown_name(""), "(unnamed client)");
    }

    /// Codes and pages both expire.
    #[test]
    fn a_pairing_code_and_its_page_expire() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let rid = page(&mut g, &mut rnd);
        let code = issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap().0;
        assert_eq!(
            approve(&mut g, &rid, &code, false, now() + PAIRING_TTL, &mut rnd).unwrap(),
            Err(Refusal::NoPairing {
                request: rid.clone()
            })
        );
        assert_eq!(
            approve(&mut g, &rid, &code, false, now() + REQUEST_TTL, &mut rnd).unwrap(),
            Err(Refusal::UnknownRequest)
        );
    }

    /// #24: past the cap the oldest page without a code makes room. A new
    /// page is never refused, and the page a code was issued for stays
    /// however many others are opened.
    #[test]
    fn a_full_cap_pushes_out_the_oldest_unpaired_page_and_refuses_nothing() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let paired = page(&mut g, &mut rnd);
        let code = issue_pairing(&mut g, &paired, now(), &mut rnd).unwrap().0;
        let second = page(&mut g, &mut rnd);
        let mut last = String::new();
        for i in 0..MAX_PENDING_REQUESTS * 3 {
            let client = public_client(&mut g, &mut rnd);
            let req = check_authorize(&g, &auth_params(&client)).unwrap();
            let at = now() + Duration::seconds(i as i64 + 1);
            last = open_request(&mut g, &req, at, &mut rnd).expect("never refused");
        }
        assert_eq!(g.requests.len(), MAX_PENDING_REQUESTS);
        assert!(
            g.requests.iter().any(|r| r.id == paired),
            "the paired page stays"
        );
        assert!(
            !g.requests.iter().any(|r| r.id == second),
            "an unpaired page is what a flood can push out"
        );
        assert!(
            g.requests.iter().any(|r| r.id == last),
            "the newest is open"
        );
        assert!(approve(&mut g, &paired, &code, false, now(), &mut rnd)
            .unwrap()
            .is_ok());
    }

    /// One client opening page after page only pushes out its own.
    #[test]
    fn one_client_cannot_crowd_out_the_others() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let mine = page(&mut g, &mut rnd);
        let noisy = public_client(&mut g, &mut rnd);
        let req = check_authorize(&g, &auth_params(&noisy)).unwrap();
        for i in 0..100 {
            open_request(&mut g, &req, now() + Duration::seconds(i + 1), &mut rnd).unwrap();
        }
        assert!(g.requests.iter().any(|r| r.id == mine));
        let theirs = g.requests.iter().filter(|r| r.client_id == noisy).count();
        assert_eq!(theirs, MAX_REQUESTS_PER_CLIENT);
    }

    #[test]
    fn metadata_names_the_public_endpoints() {
        let base = "https://mac.tail1.ts.net";
        let a = authorization_server_metadata(base);
        assert_eq!(a["issuer"], base);
        assert_eq!(a["token_endpoint"], "https://mac.tail1.ts.net/token");
        assert_eq!(a["code_challenge_methods_supported"], json!(["S256"]));
        assert_eq!(a["authorization_response_iss_parameter_supported"], true);
        let r = protected_resource_metadata(base);
        assert_eq!(r["resource"], "https://mac.tail1.ts.net/mcp");
        assert_eq!(r["authorization_servers"], json!([base]));
    }

    /// #112: revoking a client's grant also takes back what it has in flight:
    /// an authorization code not yet exchanged must not become a new grant.
    #[test]
    fn revoke_takes_back_a_code_in_flight() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let (client, _) = connect(&mut g, &mut rnd);
        let grant = g.grants[0].id.clone();
        // The same client comes back for another approval, and gets its code...
        let req = check_authorize(&g, &auth_params(&client)).unwrap();
        let rid = open_request(&mut g, &req, now(), &mut rnd).unwrap();
        let code = issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap().0;
        let url = approve(&mut g, &rid, &code, true, now(), &mut rnd)
            .unwrap()
            .unwrap();
        let issued = url
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();
        // ...and is revoked before it exchanges it.
        assert_eq!(revoke(&mut g, &grant), 1);
        let form = map(&[
            ("grant_type", "authorization_code"),
            ("code", &issued),
            ("client_id", &client),
            ("code_verifier", VERIFIER),
        ]);
        assert_eq!(
            token(&mut g, &form, None, now(), &mut rnd)
                .unwrap_err()
                .error,
            "invalid_grant",
            "the code died with the revocation"
        );
        assert!(g.grants.is_empty());
    }

    /// #112: an open page and its code go too, so `mcp pair` cannot hand a
    /// code to a page for the client just disconnected.
    #[test]
    fn revoke_closes_the_clients_pages_and_their_code() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let (client, _) = connect(&mut g, &mut rnd);
        let grant = g.grants[0].id.clone();
        let req = check_authorize(&g, &auth_params(&client)).unwrap();
        let rid = open_request(&mut g, &req, now(), &mut rnd).unwrap();
        issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap();
        // Another client's page is none of its business.
        let other = public_client(&mut g, &mut rnd);
        let oreq = check_authorize(&g, &auth_params(&other)).unwrap();
        let orid = open_request(&mut g, &oreq, now(), &mut rnd).unwrap();
        revoke(&mut g, &grant);
        assert!(g.requests.iter().all(|r| r.client_id != client));
        assert!(g.pairing.is_none());
        assert!(g.requests.iter().any(|r| r.id == orid));
        assert_eq!(revoke(&mut g, "no-such-grant"), 0);
    }

    /// A stranger's flood: `clients` fresh clients, each opening as many
    /// pages as it may, starting at `from` seconds past [`now`].
    fn flood(g: &mut Grants, rnd: &mut Seq, clients: usize, from: usize) {
        for c in 0..clients {
            let client = public_client(g, rnd);
            let req = check_authorize(g, &auth_params(&client)).unwrap();
            for i in 0..MAX_REQUESTS_PER_CLIENT {
                let at = now() + Duration::seconds((from + c * 10 + i) as i64 + 1);
                open_request(g, &req, at, rnd).expect("a new page is never refused");
            }
        }
    }

    /// #24: a stranger opening pages without end neither refuses the person a
    /// page nor touches the one they are pairing, nor its client.
    #[test]
    fn a_stranger_cannot_lock_out_pairing_or_evict_a_paired_page() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        flood(&mut g, &mut rnd, MAX_PENDING_REQUESTS, 0);
        assert_eq!(g.requests.len(), MAX_PENDING_REQUESTS);
        // The person's page opens on a full server...
        let client = public_client(&mut g, &mut rnd);
        let req = check_authorize(&g, &auth_params(&client)).unwrap();
        let mine = open_request(&mut g, &req, now() + Duration::hours(1), &mut rnd).unwrap();
        // ...gets its code, and outlives another flood, pages and
        // registrations alike.
        let code = issue_pairing(&mut g, &mine, now(), &mut rnd).unwrap().0;
        flood(&mut g, &mut rnd, MAX_PENDING_CLIENTS, 100_000);
        assert!(
            g.requests.iter().any(|r| r.id == mine),
            "my page is still open"
        );
        assert!(g.client(&client).is_some(), "and my client registered");
        assert!(g.requests.len() <= MAX_PENDING_REQUESTS);
        assert!(g.clients.len() <= MAX_PENDING_CLIENTS);
        assert!(approve(&mut g, &mine, &code, false, now(), &mut rnd)
            .unwrap()
            .is_ok());
    }

    /// #24: registering without end never drops a client in the middle of
    /// connecting — one with an open page, or with an authorization code it
    /// has not exchanged yet — and that client still gets its tokens.
    #[test]
    fn registrations_never_push_out_a_client_mid_flow() {
        let mut g = Grants::default();
        let mut rnd = Seq::new();
        let paging = public_client(&mut g, &mut rnd);
        let req = check_authorize(&g, &auth_params(&paging)).unwrap();
        open_request(&mut g, &req, now(), &mut rnd).unwrap();
        let coded = public_client(&mut g, &mut rnd);
        let req = check_authorize(&g, &auth_params(&coded)).unwrap();
        let rid = open_request(&mut g, &req, now(), &mut rnd).unwrap();
        let code = issue_pairing(&mut g, &rid, now(), &mut rnd).unwrap().0;
        let url = approve(&mut g, &rid, &code, false, now(), &mut rnd)
            .unwrap()
            .unwrap();
        let issued = url
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();
        let idle = public_client(&mut g, &mut rnd);
        for _ in 0..MAX_PENDING_CLIENTS * 2 {
            public_client(&mut g, &mut rnd);
        }
        assert!(g.client(&paging).is_some(), "a client with a page stays");
        assert!(g.client(&coded).is_some(), "a client with a code stays");
        assert!(g.client(&idle).is_none(), "an idle one made room");
        assert_eq!(g.clients.len(), MAX_PENDING_CLIENTS);
        let form = map(&[
            ("grant_type", "authorization_code"),
            ("code", &issued),
            ("client_id", &coded),
            ("code_verifier", VERIFIER),
        ]);
        assert!(token(&mut g, &form, None, now(), &mut rnd).is_ok());
    }

    /// Whether a client is mid-flow: an open page or an unexchanged code.
    fn in_flight(g: &Grants, client: &str) -> bool {
        g.requests.iter().any(|r| r.client_id == client)
            || g.codes.iter().any(|k| k.client_id == client)
    }

    /// Property over random sequences of register / authorize / pair /
    /// approve / exchange (seeded, several runs): whatever the order,
    ///
    /// - (a) the page holding a live pairing code is never pushed out by a
    ///   registration or by another page opening;
    /// - (b) a client with an open page or a pending code is never pushed
    ///   out by a registration, and a registration is refused only when
    ///   every pending client is in flight;
    /// - (c) pages never exceed [`MAX_PENDING_REQUESTS`] overall or
    ///   [`MAX_REQUESTS_PER_CLIENT`] per client, and clients without a grant
    ///   never exceed [`MAX_PENDING_CLIENTS`].
    #[test]
    fn eviction_invariants_hold_over_random_operation_sequences() {
        for seed in 0..3u64 {
            let mut rng = tower_testrng::Rng::new(0x0A07_E71C + seed);
            let mut g = Grants::default();
            let mut rnd = Seq {
                n: 0,
                codes: vec!["246810"; 20_000],
            };
            let mut t = now();
            let (mut registered, mut opened, mut paired, mut exchanged) = (0, 0, 0, 0);
            for step in 0..2000 {
                t += Duration::milliseconds(rng.below(500) as i64);
                let pairing_page = g.pairing.as_ref().map(|p| p.request.clone());
                let busy_before: Vec<String> = g
                    .clients
                    .iter()
                    .map(|c| c.client_id.clone())
                    .filter(|c| in_flight(&g, c))
                    .collect();
                let op = rng.below(10);
                match op {
                    // Registrations, most often: the flood the caps are for.
                    0..=3 => {
                        let pending_before = g
                            .clients
                            .iter()
                            .filter(|c| !g.grants.iter().any(|x| x.client_id == c.client_id))
                            .count();
                        let all_busy = g
                            .clients
                            .iter()
                            .filter(|c| !g.grants.iter().any(|x| x.client_id == c.client_id))
                            .all(|c| in_flight(&g, &c.client_id));
                        let r = register(
                            &mut g,
                            &json!({"client_name":"x","redirect_uris":[CB],"token_endpoint_auth_method":"none"}),
                            &allowed(),
                            false,
                            t,
                            &mut rnd,
                        );
                        match r {
                            Ok(_) => registered += 1,
                            Err(e) => {
                                assert_eq!(e.status, 503, "step {step}");
                                assert!(pending_before >= MAX_PENDING_CLIENTS && all_busy);
                            }
                        }
                    }
                    // A page for some registered client.
                    4..=6 => {
                        if let Some(c) = g.clients.get(rng.below(g.clients.len())) {
                            let client = c.client_id.clone();
                            let req = check_authorize(&g, &auth_params(&client)).unwrap();
                            open_request(&mut g, &req, t, &mut rnd).expect("never refused");
                            opened += 1;
                        }
                    }
                    // `mcp pair` for some open page.
                    7 => {
                        if let Some(p) = g.requests.get(rng.below(g.requests.len())) {
                            let id = p.id.clone();
                            issue_pairing(&mut g, &id, t, &mut rnd).unwrap();
                            paired += 1;
                        }
                    }
                    // The person types the code (or a wrong one), and the
                    // client sometimes exchanges what it got.
                    _ => {
                        if let Some(rid) = pairing_page.clone() {
                            let typed = if rng.one_in(3) { "000000" } else { "246810" };
                            if let Ok(url) =
                                approve(&mut g, &rid, typed, false, t, &mut rnd).unwrap()
                            {
                                let issued = url
                                    .split("code=")
                                    .nth(1)
                                    .unwrap()
                                    .split('&')
                                    .next()
                                    .unwrap()
                                    .to_string();
                                let client = g.codes.last().unwrap().client_id.clone();
                                if rng.one_in(2) {
                                    let form = map(&[
                                        ("grant_type", "authorization_code"),
                                        ("code", &issued),
                                        ("client_id", &client),
                                        ("code_verifier", VERIFIER),
                                    ]);
                                    token(&mut g, &form, None, t, &mut rnd).unwrap();
                                    exchanged += 1;
                                }
                            }
                        }
                    }
                }
                // (a) Only the page's own approve (or a new `pair`) ends it.
                if op <= 6 {
                    if let Some(rid) = &pairing_page {
                        assert!(
                            g.requests.iter().any(|r| &r.id == rid),
                            "step {step}: the paired page {rid} was pushed out"
                        );
                        assert_eq!(g.pairing.as_ref().map(|p| &p.request), Some(rid));
                    }
                }
                // (b) Nobody in flight is dropped by a registration.
                if op <= 3 {
                    for c in &busy_before {
                        assert!(g.client(c).is_some(), "step {step}: {c} was pushed out");
                    }
                }
                // (c) The caps.
                assert!(g.requests.len() <= MAX_PENDING_REQUESTS, "step {step}");
                for c in &g.clients {
                    let n = g
                        .requests
                        .iter()
                        .filter(|r| r.client_id == c.client_id)
                        .count();
                    assert!(n <= MAX_REQUESTS_PER_CLIENT, "step {step}: {n}");
                }
                let pending = g
                    .clients
                    .iter()
                    .filter(|c| !g.grants.iter().any(|x| x.client_id == c.client_id))
                    .count();
                assert!(pending <= MAX_PENDING_CLIENTS, "step {step}: {pending}");
                assert!(
                    g.requests.iter().all(|r| g.client(&r.client_id).is_some()),
                    "step {step}: a page outlived its client"
                );
            }
            assert!(
                registered > 500 && opened > 300 && paired > 50 && exchanged > 5,
                "seed {seed}: {registered} {opened} {paired} {exchanged}"
            );
        }
    }
}
