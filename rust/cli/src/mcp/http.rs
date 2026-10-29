// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The HTTP side: which path does what, and the listener that feeds it.
//!
//! [`route`] is the whole server as a function from a request to a response,
//! so every endpoint is tested without a socket. [`serve`] is the thin loop
//! around it: a loopback listener, a thread per request, a ceiling on how many
//! run at once and on how big a body may be.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use time::OffsetDateTime;

use super::grants;
use super::limit::Limiter;
use super::oauth::{self, Random};
use super::rpc;
use super::tools::{Call, Source};

/// Request bodies larger than this are refused.
pub const BODY_CAP: usize = 64 * 1024;
/// Tool calls (`POST /mcp`) handled at once; the rest get `503`. A `tower`
/// run or a wait may hold one for close to a minute.
pub const MAX_CONCURRENT: usize = 8;
/// Every other route — the approval page, `/token`, `/register`, the
/// metadata, `/` — has slots of its own, so busy tools never shut the way to
/// pair or to check the server (#114). These answer at once.
pub const MAX_CONTROL: usize = 8;

/// What the server needs to know besides the request.
pub struct Settings {
    /// The public base URL, `https://<mac>.<tailnet>.ts.net`.
    pub base: String,
    pub redirect_uris: Vec<String>,
    /// Where `mcp-grants.json` lives.
    pub state_dir: PathBuf,
    /// Whether tools that write to an agent are offered at all
    /// (`mcp.act`). Off hides them from every grant, and from the approval page.
    pub act: bool,
    pub limiter: Limiter,
    /// How long a connection has to deliver its whole request
    /// ([`READ_DEADLINE`] in the server).
    pub read_deadline: Duration,
    /// Whether a client may register a callback on this machine
    /// ([`oauth::is_loopback_callback`]): when there is no public URL and the
    /// server serves this machine alone.
    pub loopback_callbacks: bool,
}

pub struct Request<'a> {
    pub method: &'a str,
    /// Without the query string.
    pub path: &'a str,
    pub query: &'a str,
    /// Names in lower case.
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

impl Request<'_> {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, v: &Value) -> Self {
        Response {
            status,
            headers: vec![
                ("Content-Type", "application/json".into()),
                ("Cache-Control", "no-store".into()),
            ],
            body: serde_json::to_vec(v).unwrap_or_default(),
        }
    }

    fn html(status: u16, body: String) -> Self {
        Response {
            status,
            headers: vec![
                ("Content-Type", "text/html; charset=utf-8".into()),
                ("Cache-Control", "no-store".into()),
                // The approval page must not be framed by another site, where
                // a click could be steered onto it.
                ("X-Frame-Options", "DENY".into()),
                (
                    "Content-Security-Policy",
                    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' https:; frame-ancestors 'none'".into(),
                ),
            ],
            body: body.into_bytes(),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Response {
            status,
            headers: vec![("Content-Type", "text/plain; charset=utf-8".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Response {
            status,
            headers: vec![],
            body: vec![],
        }
    }

    pub fn with(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }
}

/// The whole server: one request in, one response out.
pub fn route(
    s: &Settings,
    src: &dyn Source,
    req: &Request,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Response {
    match (req.method, req.path) {
        ("GET", "/.well-known/oauth-protected-resource")
        | ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            Response::json(200, &oauth::protected_resource_metadata(&s.base))
        }
        ("GET", "/.well-known/oauth-authorization-server") => {
            Response::json(200, &oauth::authorization_server_metadata(&s.base))
        }
        ("POST", "/register") => register(s, req, now, rnd),
        ("GET", "/authorize") => authorize_page(s, &decode_form(req.query), now, rnd),
        ("POST", "/authorize") => authorize_submit(s, req, now, rnd),
        ("POST", "/token") => token(s, src, req, now, rnd),
        ("POST", "/mcp") => mcp(s, src, req, now),
        (_, "/mcp") => Response::empty(405).with("Allow", "POST".into()),
        ("GET", "/") => Response::text(200, "herdr-tower MCP server\n"),
        _ => Response::text(404, "not found\n"),
    }
}

fn internal(e: io::Error) -> Response {
    Response::json(
        500,
        &json!({"error":"server_error","error_description": e.to_string()}),
    )
}

fn register(s: &Settings, req: &Request, now: OffsetDateTime, rnd: &mut dyn Random) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(req.body) else {
        return Response::json(
            400,
            &json!({"error":"invalid_client_metadata","error_description":"the body is not JSON"}),
        );
    };
    match grants::update(&s.state_dir, now, |g| {
        oauth::register(g, &body, &s.redirect_uris, s.loopback_callbacks, now, rnd)
    }) {
        Ok(Ok(v)) => Response::json(201, &v),
        Ok(Err(e)) => Response::json(e.status, &e.body()),
        Err(e) => internal(e),
    }
}

/// Opens an approval request for a checked authorization request and shows
/// its page. Every visit opens a request of its own; `mcp pair` issues a code
/// for one of them.
fn authorize_page(
    s: &Settings,
    params: &HashMap<String, String>,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Response {
    let opened = grants::update(&s.state_dir, now, |g| {
        let ar = oauth::check_authorize(g, params).map_err(|why| (400, why))?;
        let id = oauth::open_request(g, &ar, now, rnd).map_err(|e| (e.status, e.description))?;
        Ok::<_, (u16, String)>(asking(g, &id))
    });
    match opened {
        Err(e) => internal(e),
        Ok(Err((status, why))) => Response::html(status, error_page(&why)),
        Ok(Ok(None)) => Response::html(400, error_page(&oauth::Refusal::UnknownRequest.message())),
        Ok(Ok(Some(who))) => Response::html(200, page(&who, now, s.act, false, None)),
    }
}

/// Who an approval page is for, as the page describes them.
struct Asking {
    request: String,
    /// The name the client registered with — its own claim, nothing more.
    client: String,
    /// Where approving sends the browser: the one thing on the page a
    /// stranger cannot choose, since a callback has to be on
    /// `mcp.redirect_uris`.
    redirect_host: String,
    /// When the client registered, if it still is.
    registered: Option<OffsetDateTime>,
}

/// The open request `id`, as its page shows it.
fn asking(g: &grants::Grants, id: &str) -> Option<Asking> {
    let r = g.requests.iter().find(|r| r.id == id)?;
    Some(Asking {
        request: r.id.clone(),
        client: r.client_name.clone(),
        redirect_host: oauth::redirect_host(&r.redirect_uri).to_string(),
        registered: g.client(&r.client_id).map(|c| c.created_at),
    })
}

fn authorize_submit(
    s: &Settings,
    req: &Request,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Response {
    let form = decode_form(&String::from_utf8_lossy(req.body));
    let request = form.get("request_id").cloned().unwrap_or_default();
    let typed = form.get("pairing_code").cloned().unwrap_or_default();
    let ticked = form.get("act").is_some_and(|v| v == "on");
    // Only a ticked box grants act, and only while the server offers it.
    let act = s.act && ticked;
    let outcome = grants::update(&s.state_dir, now, |g| {
        let decided = oauth::approve(g, &request, &typed, act, now, rnd)?;
        // The page can be shown again only while its request is still open.
        Ok::<_, oauth::OAuthError>((asking(g, &request), decided))
    });
    match outcome {
        Err(e) => internal(e),
        Ok(Err(e)) => Response::json(e.status, &e.body()),
        // RFC 9207: the client checks who issued the code before redeeming it.
        Ok(Ok((_, Ok(location)))) => {
            Response::empty(302).with("Location", format!("{location}&iss={}", encode(&s.base)))
        }
        Ok(Ok((Some(who), Err(refusal)))) => Response::html(
            401,
            page(&who, now, s.act, ticked, Some(&refusal.message())),
        ),
        Ok(Ok((None, Err(refusal)))) => Response::html(400, error_page(&refusal.message())),
    }
}

/// The token endpoint. A code redeemed into a new grant is announced
/// ([`Source::grant_created`]): a connection nobody expected — a code
/// intercepted, a page approved by mistake — shows up in the notification
/// channel the moment it exists, not the next time someone runs `mcp grants`
/// (#26). A refresh renews a grant and says nothing.
fn token(
    s: &Settings,
    src: &dyn Source,
    req: &Request,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Response {
    let form = decode_form(&String::from_utf8_lossy(req.body));
    let basic = req.header("authorization").and_then(basic_credentials);
    match grants::update(&s.state_dir, now, |g| {
        let before = g.grants.len();
        let out = oauth::token(g, &form, basic, now, rnd);
        let created = g
            .grants
            .last()
            .filter(|_| out.is_ok() && g.grants.len() > before)
            .map(|x| {
                let act = x.scopes.iter().any(|sc| sc == oauth::SCOPE_ACT);
                (x.client_name.clone(), act)
            });
        out.map(|v| (v, created))
    }) {
        Ok(Ok((v, created))) => {
            if let Some((client, act)) = created {
                src.grant_created(&client, act);
            }
            Response::json(200, &v)
        }
        Ok(Err(e)) => {
            let r = Response::json(e.status, &e.body());
            if e.status == 401 {
                r.with("WWW-Authenticate", "Basic".into())
            } else {
                r
            }
        }
        Err(e) => internal(e),
    }
}

/// `Authorization: Basic` for a client (RFC 6749 §2.3.1: both halves are
/// form-encoded before they are joined).
fn basic_credentials(header: &str) -> Option<(String, String)> {
    let b64 = header.strip_prefix("Basic ")?.trim();
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let raw = String::from_utf8(raw).ok()?;
    let (id, secret) = raw.split_once(':')?;
    Some((decode(id), decode(secret)))
}

/// Origins a browser-borne request may come from: each allowed callback's
/// origin, and this server's own.
fn allowed_origin(s: &Settings, origin: &str) -> bool {
    let of = |url: &str| -> String {
        let rest = url.strip_prefix("https://").unwrap_or("");
        format!("https://{}", rest.split('/').next().unwrap_or(""))
    };
    origin == s.base || s.redirect_uris.iter().any(|u| of(u) == origin)
}

fn unauthorized(s: &Settings, why: &str) -> Response {
    Response::json(
        401,
        &json!({"error":"invalid_token","error_description": why}),
    )
    .with(
        "WWW-Authenticate",
        format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            s.base
        ),
    )
}

fn mcp(s: &Settings, src: &dyn Source, req: &Request, now: OffsetDateTime) -> Response {
    // DNS rebinding and cross-site calls from a browser: a request that says
    // where it came from has to come from somewhere expected.
    if let Some(origin) = req.header("origin") {
        if !allowed_origin(s, origin) {
            return Response::text(403, "origin not allowed\n");
        }
    }
    let Some(bearer) = req
        .header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
    else {
        return unauthorized(s, "a bearer token is required");
    };
    let g = match grants::load(&s.state_dir) {
        Ok(g) => g,
        Err(e) => return internal(e),
    };
    let Some(caller) = oauth::authenticate(&g, bearer, now) else {
        return unauthorized(s, "the token is unknown or expired");
    };
    if caller.touch {
        let _ = grants::update(&s.state_dir, now, |g| {
            if let Some(x) = g.grants.iter_mut().find(|x| x.id == caller.grant) {
                x.last_used = Some(now);
            }
        });
    }
    let msg: Value = match serde_json::from_slice(req.body) {
        Ok(v) => v,
        Err(_) => return Response::json(400, &rpc::error(Value::Null, -32700, "parse error")),
    };
    if msg.is_array() {
        return Response::json(
            400,
            &rpc::error(Value::Null, -32600, "batches are not supported"),
        );
    }
    // `mcp.act: false` takes act away from every grant, whatever it holds.
    let scopes: Vec<String> = caller
        .scopes
        .iter()
        .filter(|x| s.act || x.as_str() != oauth::SCOPE_ACT)
        .cloned()
        .collect();
    let call = Call {
        grant: &caller.grant,
        client: &caller.client,
        scopes: &scopes,
        limiter: &s.limiter,
    };
    let mirrored = rpc::Mirrored {
        version: req.header("mcp-protocol-version"),
        method: req.header("mcp-method"),
        name: req.header("mcp-name"),
    };
    if rpc::is_modern(&msg, mirrored.version) {
        let (status, v) = rpc::modern(&msg, &mirrored, src, &call);
        return Response::json(status, &v);
    }
    match rpc::handle(&msg, src, &call) {
        Some(v) => Response::json(200, &v),
        None => Response::empty(202),
    }
}

// ------------------------------------------------------------------ pages

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const STYLE: &str = "\
:root{--bg:#f6f7f8;--card:#fff;--fg:#1d2126;--muted:#5b6570;--line:#d9dee3;--accent:#0f7b6c;--bad:#b3261e}\
@media (prefers-color-scheme:dark){:root{--bg:#15191d;--card:#1f252b;--fg:#e8ecef;--muted:#9aa6b1;--line:#333c45;--accent:#48c9b0;--bad:#f2b8b5}}\
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 -apple-system,system-ui,sans-serif;padding:24px 16px}\
main{max-width:420px;margin:0 auto;background:var(--card);border:1px solid var(--line);border-radius:12px;padding:24px}\
h1{font-size:20px;margin:0 0 8px}p{margin:8px 0;color:var(--muted)}b{color:var(--fg)}\
input[type=text]{width:100%;font-size:28px;letter-spacing:.3em;text-align:center;padding:10px;border:1px solid var(--line);border-radius:8px;background:var(--bg);color:var(--fg)}\
button{width:100%;margin-top:16px;padding:12px;font-size:16px;border:0;border-radius:8px;background:var(--accent);color:#fff}\
.bad{color:var(--bad)}code{font-size:14px}.note{font-size:14px}\
.act{display:flex;gap:8px;align-items:flex-start;margin-top:16px;color:var(--muted)}.act input{margin-top:4px}\
.req b{font-family:ui-monospace,monospace;font-size:20px;letter-spacing:.1em}";

fn shell(body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>herdr-tower</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>"
    )
}

/// The approval page.
///
/// The client's name is whatever it registered with, so the page says so: a
/// stranger's client can call itself "Claude" as easily as claude.ai does
/// (#25). Beside it the page shows what a stranger cannot choose — the host
/// approving sends the browser back to, which has to be on
/// `mcp.redirect_uris` — and how long ago the client registered: a real
/// connector registers seconds before its page opens.
fn page(a: &Asking, now: OffsetDateTime, act: bool, ticked: bool, error: Option<&str>) -> String {
    let who = if a.client.is_empty() {
        "An MCP client (it gave no name)".to_string()
    } else {
        format!(
            "<b>{}</b> <span class=\"note\">(the name it gave itself — any client can claim any name)</span>",
            esc(&a.client)
        )
    };
    let registered = a.registered.map_or_else(
        || "It is no longer registered.".to_string(),
        |t| format!("It registered {}.", esc(&tower_text::age(t, now))),
    );
    let host = esc(&a.redirect_host);
    let request = esc(&a.request);
    let error = error
        .map(|e| format!("<p class=\"bad\">{}</p>", esc(e)))
        .unwrap_or_default();
    // Unticked unless the person ticked it before a wrong code sent the page back.
    let act_box = if act {
        let checked = if ticked { " checked" } else { "" };
        format!(
            "<label class=\"act\"><input type=\"checkbox\" name=\"act\" value=\"on\"{checked}> \
Also let it <b>send instructions</b> to the agents. They run with full permissions on this Mac.</label>"
        )
    } else {
        String::new()
    };
    shell(&format!(
        "<h1>Connect to herdr-tower</h1>\
<p>{who} asks to <b>see</b> the coding agents on this Mac: their states, their screens, and what they did.</p>\
<p>Approving sends you back to <b>{host}</b>. {registered}</p>\
<p class=\"req\">Request <b>{request}</b></p>\
<p>Run <code>herdr-tower mcp pair {request}</code> on the Mac and enter the code it shows.</p>{error}\
<form method=\"post\" action=\"/authorize\"><input type=\"hidden\" name=\"request_id\" value=\"{request}\">\
<input type=\"text\" name=\"pairing_code\" inputmode=\"numeric\" autocomplete=\"one-time-code\" maxlength=\"6\" required autofocus>\
{act_box}<button type=\"submit\">Approve</button></form>"
    ))
}

fn error_page(why: &str) -> String {
    shell(&format!(
        "<h1>Cannot connect</h1><p class=\"bad\">{}</p>",
        esc(why)
    ))
}

// ------------------------------------------------------------------ encoding

/// Percent-encodes everything but RFC 3986's unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes one `application/x-www-form-urlencoded` component.
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                match (
                    b.get(i + 1).copied().and_then(hex),
                    b.get(i + 2).copied().and_then(hex),
                ) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A query string or form body. The first of a repeated name wins.
pub fn decode_form(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in s.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.entry(decode(k)).or_insert_with(|| decode(v));
    }
    out
}

// ------------------------------------------------------------------ serve

/// [`route`], with a panic answered as `500` instead of taking the request
/// thread down. The panic itself still reaches the log through the default
/// hook.
pub fn route_safely(
    s: &Settings,
    src: &dyn Source,
    req: &Request,
    now: OffsetDateTime,
    rnd: &mut dyn Random,
) -> Response {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        route(s, src, req, now, rnd)
    }))
    .unwrap_or_else(|_| Response::text(500, "internal error\n"))
}

/// Serves until the listener fails. Binds loopback only; the caller has
/// already refused anything else, and this checks again.
///
/// A plain HTTP/1.1 server over `std::net`, one request per connection. It
/// replaced tiny_http, which gives no way to put a read timeout on a
/// connection: a client that sent a `Content-Length` and then nothing held a
/// slot and a thread for as long as it liked, and a handful of them, with no
/// credentials, left every route unanswered (#116). Here a connection has
/// [`Settings::read_deadline`] to deliver its whole request, and until it has,
/// it holds a slot of the reading pool only — the tool and control pools are
/// taken once a request is complete.
pub fn serve(listen: SocketAddr, s: Settings, src: Arc<dyn Source>) -> io::Result<()> {
    let listener = bind(listen)?;
    eprintln!(
        "herdr-tower mcp: listening on http://{listen}, public at {}",
        s.base
    );
    serve_on(listener, Arc::new(Remote { s, src }))
}

/// A server on the loop [`serve`] runs: which requests are tool calls, and
/// what each request is answered with.
pub trait Handler: Send + Sync {
    /// The log prefix, `herdr-tower <label>:`.
    fn label(&self) -> &'static str;
    /// How long a connection has to deliver its whole request.
    fn read_deadline(&self) -> Duration;
    /// Whether this request takes a slot of the tool pool rather than the
    /// control pool.
    fn is_tool(&self, method: &str, path: &str) -> bool;
    /// The answer. A panic in here is answered as `500` ([`serve_on`]).
    fn handle(&self, req: &Request, now: OffsetDateTime) -> Response;
    /// Whether every request gets a log line, or only a refused or failed
    /// one. A server whose log nothing trims, called on every click, says
    /// only what went wrong.
    fn logs_every_request(&self) -> bool {
        true
    }
    /// Whether a request that came in on one of the reserved reading slots
    /// ([`RESERVED_READING`]) may go on to send its body: it carries
    /// credentials this server accepts. Asked once the head is in, before
    /// any of the body is read. By default no one may, and a reserved slot
    /// serves only a request whose head brought the whole of it.
    fn admits_reserved(&self, _headers: &[(String, String)], _now: OffsetDateTime) -> bool {
        false
    }
}

/// The remote MCP server as a [`Handler`].
struct Remote {
    s: Settings,
    src: Arc<dyn Source>,
}

impl Handler for Remote {
    fn label(&self) -> &'static str {
        "mcp"
    }
    fn read_deadline(&self) -> Duration {
        self.s.read_deadline
    }
    fn is_tool(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == "/mcp"
    }
    fn handle(&self, req: &Request, now: OffsetDateTime) -> Response {
        route_safely(&self.s, self.src.as_ref(), req, now, &mut oauth::OsRandom)
    }
    /// A live access token: an existing connection's tool calls get through
    /// however many strangers are trickling bodies in. The grants file is
    /// read, not locked or written.
    fn admits_reserved(&self, headers: &[(String, String)], now: OffsetDateTime) -> bool {
        let Some(bearer) = headers
            .iter()
            .find(|(n, _)| n == "authorization")
            .and_then(|(_, v)| v.strip_prefix("Bearer "))
            .map(str::trim)
        else {
            return false;
        };
        grants::load(&self.s.state_dir)
            .is_ok_and(|g| oauth::authenticate(&g, bearer, now).is_some())
    }
}

/// Binds a loopback listener. Anything else is refused: every server here is
/// reached through the machine itself or a tunnel that ends on it.
pub fn bind(listen: SocketAddr) -> io::Result<std::net::TcpListener> {
    if !listen.ip().is_loopback() {
        return Err(io::Error::other(format!(
            "{listen} is not a loopback address"
        )));
    }
    std::net::TcpListener::bind(listen)
}

/// Serves `h` on `listener` until the listener fails.
pub fn serve_on(listener: std::net::TcpListener, h: Arc<dyn Handler>) -> io::Result<()> {
    let label = h.label();
    let pools = Arc::new(Pools::default());
    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(c) => c,
            // A failed accept (the peer gave up, descriptors ran short) is
            // that connection's problem, not the listener's: go on.
            Err(e) => {
                eprintln!("herdr-tower {label}: accept failed: {e}");
                continue;
            }
        };
        let taken = pools.reading.fetch_add(1, Ordering::SeqCst);
        if taken >= MAX_READING {
            pools.reading.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let _ = write_response(&mut stream, &busy());
            take_what_arrived(&mut stream);
            continue;
        }
        // The last few slots are held back: see [`RESERVED_READING`].
        let reserved = taken >= MAX_READING - RESERVED_READING;
        // Held until the whole request is read, then given back.
        let reading = PoolSlot {
            pools: pools.clone(),
            which: Pool::Reading,
        };
        let (h, pools) = (h.clone(), pools.clone());
        let spawned = std::thread::Builder::new().spawn(move || {
            serve_one(h.as_ref(), &pools, stream, reading, reserved);
        });
        // Out of threads: the connection is dropped (its slot with it) and
        // the loop goes on.
        if let Err(e) = spawned {
            eprintln!("herdr-tower {label}: could not start a request thread: {e}");
        }
    }
    Ok(())
}

/// The whole request has to arrive within this, head and body together.
pub const READ_DEADLINE: Duration = Duration::from_secs(10);
/// Connections still sending their request, at most.
pub const MAX_READING: usize = 32;
/// Of [`MAX_READING`], the slots kept for requests with credentials.
///
/// Anyone can open connections that send a head with a `Content-Length` and
/// then trickle the body, and there is no telling strangers apart by address
/// behind the tunnel, where every connection comes from 127.0.0.1. With one
/// pool for everybody, 32 of them answered every route `503` — `/mcp` for
/// connected clients included — for as long as they were renewed (#27).
///
/// A connection that arrives with only these slots left gets
/// [`RESERVED_HEAD_DEADLINE`] for its head, and then goes on to read its body
/// only if [`Handler::admits_reserved`] accepts its credentials — a live
/// bearer token, for the remote MCP server. A request without them is
/// answered `503` before a byte of its body is read, unless its head
/// brought the whole request (a `GET`, a short form), which is served as
/// usual: there is nothing left to trickle.
pub const RESERVED_READING: usize = 8;
/// How long a connection on a reserved slot has to send its head. A real
/// client sends it at once; a stranger trickling heads into these slots has
/// to open a new connection this often to keep one.
pub const RESERVED_HEAD_DEADLINE: Duration = Duration::from_secs(2);
/// A request head larger than this is refused.
pub const HEAD_CAP: usize = 16 * 1024;
/// How long writing an answer may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Pools {
    reading: AtomicUsize,
    tools: AtomicUsize,
    control: AtomicUsize,
}

#[derive(Clone, Copy)]
enum Pool {
    Reading,
    Tools,
    Control,
}

impl Pools {
    fn counter(&self, which: Pool) -> &AtomicUsize {
        match which {
            Pool::Reading => &self.reading,
            Pool::Tools => &self.tools,
            Pool::Control => &self.control,
        }
    }
}

/// A slot in one of the pools, given back when dropped — however the thread
/// holding it ends.
struct PoolSlot {
    pools: Arc<Pools>,
    which: Pool,
}

impl Drop for PoolSlot {
    fn drop(&mut self) {
        self.pools
            .counter(self.which)
            .fetch_sub(1, Ordering::SeqCst);
    }
}

fn busy() -> Response {
    Response::empty(503).with("Retry-After", "1".into())
}

fn serve_one(
    h: &dyn Handler,
    pools: &Arc<Pools>,
    mut stream: std::net::TcpStream,
    reading: PoolSlot,
    reserved: bool,
) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let admits =
        |headers: &[(String, String)]| h.admits_reserved(headers, OffsetDateTime::now_utc());
    let gate: Option<Gate> = if reserved { Some(&admits) } else { None };
    let parsed = read_request(&mut stream, h.read_deadline(), gate);
    drop(reading);
    let now = OffsetDateTime::now_utc();
    let (method, path, resp) = match parsed {
        // Timed out or went away: there is nobody to answer.
        Err(ReadError::Gone) => return,
        Err(ReadError::Refused(503, _)) => ("-".to_string(), "-".to_string(), busy()),
        Err(ReadError::Refused(status, why)) => (
            "-".to_string(),
            "-".to_string(),
            Response::text(status, why),
        ),
        Ok(raw) => {
            let which = if h.is_tool(&raw.method, &raw.path) {
                (Pool::Tools, MAX_CONCURRENT)
            } else {
                (Pool::Control, MAX_CONTROL)
            };
            let counter = pools.counter(which.0);
            let resp = if counter.fetch_add(1, Ordering::SeqCst) >= which.1 {
                counter.fetch_sub(1, Ordering::SeqCst);
                busy()
            } else {
                let _slot = PoolSlot {
                    pools: pools.clone(),
                    which: which.0,
                };
                let req = Request {
                    method: &raw.method,
                    path: &raw.path,
                    query: &raw.query,
                    headers: &raw.headers,
                    body: &raw.body,
                };
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.handle(&req, now)))
                    .unwrap_or_else(|_| Response::text(500, "internal error\n"))
            };
            (raw.method, raw.path, resp)
        }
    };
    // Never the query: it carries codes and state.
    if h.logs_every_request() || resp.status >= 400 {
        eprintln!(
            "{} {method} {path} {}",
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            resp.status
        );
    }
    let _ = write_response(&mut stream, &resp);
    // A refusal is answered before the body is read; see `take_what_arrived`.
    if method == "-" {
        take_what_arrived(&mut stream);
    }
}

/// A request as it came off the wire.
struct Raw {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

enum ReadError {
    /// The peer went away or ran out of time; nothing to answer.
    Gone,
    /// A request that is answered with this status and text.
    Refused(u16, &'static str),
}

/// Whether a request's headers let it read its body on a reserved slot
/// ([`Handler::admits_reserved`]).
type Gate<'a> = &'a dyn Fn(&[(String, String)]) -> bool;

/// One read, within what is left of the deadline.
fn read_some(
    stream: &mut std::net::TcpStream,
    until: std::time::Instant,
    buf: &mut [u8],
) -> Result<usize, ReadError> {
    use std::io::Read as _;
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(ReadError::Gone);
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| ReadError::Gone)?;
        match stream.read(buf) {
            Ok(0) => return Err(ReadError::Gone),
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ReadError::Gone),
        }
    }
}

/// Reads one HTTP/1.1 request: the head up to [`HEAD_CAP`], then a body of
/// exactly `Content-Length` bytes up to [`BODY_CAP`], all within `deadline`.
///
/// `reserved` is set for a connection on a reserved slot
/// ([`RESERVED_READING`]): its head has [`RESERVED_HEAD_DEADLINE`] at most,
/// and a body still to come is read only if the gate accepts the headers.
fn read_request(
    stream: &mut std::net::TcpStream,
    deadline: Duration,
    reserved: Option<Gate>,
) -> Result<Raw, ReadError> {
    use std::io::Write as _;
    let start = std::time::Instant::now();
    let until = start + deadline;
    let head_until = match reserved {
        Some(_) => start + deadline.min(RESERVED_HEAD_DEADLINE),
        None => until,
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
        if end.unwrap_or(buf.len()) > HEAD_CAP {
            return Err(ReadError::Refused(431, "request head too large\n"));
        }
        if let Some(end) = end {
            break end;
        }
        let n = read_some(stream, head_until, &mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end - 4])
        .map_err(|_| ReadError::Refused(400, "bad request\n"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err(ReadError::Refused(400, "bad request\n"));
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() || !target.starts_with('/') {
        return Err(ReadError::Refused(400, "bad request\n"));
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(ReadError::Refused(400, "bad request\n"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let header = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    };
    // No route takes a chunked body, and reading one would need its own
    // framing; a client that sends one is told to say how long it is.
    if header("transfer-encoding").is_some() {
        return Err(ReadError::Refused(411, "send a Content-Length\n"));
    }
    let length: usize = match header("content-length") {
        None => 0,
        Some(v) => v
            .parse()
            .map_err(|_| ReadError::Refused(400, "bad Content-Length\n"))?,
    };
    // Refused on the header alone: a body this size is never read.
    if length > BODY_CAP {
        return Err(ReadError::Refused(413, "body too large\n"));
    }
    let mut body = buf[head_end..].to_vec();
    if body.len() < length && reserved.is_some_and(|admits| !admits(&headers)) {
        return Err(ReadError::Refused(503, "busy\n"));
    }
    if body.len() < length
        && header("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
    {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    while body.len() < length {
        let n = read_some(stream, until, &mut chunk)?;
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Raw {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Reads, briefly, what the client already sent and nobody will look at.
///
/// Windows answers the close of a socket with unread bytes in it with a
/// reset, and a reset that reaches the client first throws away the `503`
/// it was sent: the client sees "connection reset" instead of "busy, retry".
/// Unix closes such a socket with the answer intact, so this is Windows only.
fn take_what_arrived(stream: &mut std::net::TcpStream) {
    if !cfg!(windows) {
        return;
    }
    use std::io::Read as _;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut buf = [0u8; 4096];
    let mut taken = 0;
    while taken < HEAD_CAP {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => taken += n,
        }
    }
}

/// Writes the answer and closes: one request per connection.
fn write_response(stream: &mut std::net::TcpStream, resp: &Response) -> io::Result<()> {
    use std::io::Write as _;
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        reason(resp.status),
        resp.body.len()
    );
    for (name, value) in &resp.headers {
        // A header value cannot carry a line break into the head.
        if value.contains(['\r', '\n']) {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::Source;
    use tower_state::Agent;

    const BASE: &str = "https://mac.tail1.ts.net";
    const CB: &str = "https://claude.ai/api/mcp/auth_callback";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[derive(Default)]
    struct Nobody {
        /// Every `grant_created`: the client, and whether it may act.
        granted: std::sync::Mutex<Vec<(String, bool)>>,
    }

    impl Source for Nobody {
        fn grant_created(&self, client: &str, act: bool) {
            self.granted.lock().unwrap().push((client.to_string(), act));
        }
        fn now(&self) -> OffsetDateTime {
            now()
        }
        fn agents(&self) -> Result<Vec<Agent>, String> {
            Ok(vec![])
        }
        fn ctx(&self, _: &[Agent]) -> HashMap<String, tower_usage::CtxInfo> {
            HashMap::new()
        }
        fn branch(&self, _: &str) -> Option<String> {
            None
        }
        fn read_pane(&self, _: &Agent, _: u32) -> Result<String, String> {
            Err("no pane".into())
        }
        fn report(&self, _: OffsetDateTime) -> Value {
            json!({})
        }
        fn schedules(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn loops(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn supervisor_fresh(&self) -> bool {
            true
        }
        fn prompt(
            &self,
            _: &Agent,
            _: &str,
            _: Option<std::time::Duration>,
        ) -> Result<crate::mcp::tools::Sent, crate::mcp::tools::SendError> {
            Err(crate::mcp::tools::SendError::Failed("no herdr".into()))
        }
        fn wait(
            &self,
            _: &Agent,
            _: &[tower_state::AgentState],
            _: std::time::Duration,
        ) -> Result<bool, String> {
            Err("no herdr".into())
        }
        fn live_state(&self, _: &Agent) -> Option<tower_state::AgentState> {
            None
        }
        fn audit(&self, _: &crate::mcp::audit::Entry) {}
        fn keys(&self, _: &Agent, _: &[&str]) -> Result<(), String> {
            Err("no herdr".into())
        }
        fn run_tower(
            &self,
            _: &[String],
            _: std::time::Duration,
        ) -> Result<crate::mcp::tools::Ran, String> {
            Err("no binary".into())
        }
    }

    struct Seq(u32);

    impl Random for Seq {
        fn token(&mut self) -> io::Result<String> {
            self.0 += 1;
            Ok(format!("tok{}", self.0))
        }
        fn code(&mut self) -> io::Result<String> {
            Ok("424242".into())
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    struct World {
        dir: tempfile::TempDir,
        rnd: Seq,
        act: bool,
        read_deadline: std::time::Duration,
        src: Nobody,
    }

    impl World {
        fn new() -> Self {
            World {
                dir: tempfile::tempdir().unwrap(),
                rnd: Seq(0),
                act: true,
                read_deadline: READ_DEADLINE,
                src: Nobody::default(),
            }
        }

        fn settings(&self) -> Settings {
            Settings {
                base: BASE.into(),
                redirect_uris: vec![CB.into()],
                state_dir: self.dir.path().to_path_buf(),
                act: self.act,
                limiter: Limiter::default(),
                read_deadline: self.read_deadline,
                loopback_callbacks: false,
            }
        }

        fn send(
            &mut self,
            method: &str,
            target: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> Response {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let headers: Vec<(String, String)> = headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect();
            let s = self.settings();
            route(
                &s,
                &self.src,
                &Request {
                    method,
                    path,
                    query,
                    headers: &headers,
                    body: body.as_bytes(),
                },
                now(),
                &mut self.rnd,
            )
        }

        fn pair(&mut self, rid: &str) {
            let rnd = &mut self.rnd;
            grants::update(self.dir.path(), now(), |g| {
                oauth::issue_pairing(g, rid, now(), rnd)
            })
            .unwrap()
            .unwrap();
        }
    }

    fn body(r: &Response) -> Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    fn header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn authorize_query(client: &str) -> String {
        format!(
            "response_type=code&client_id={client}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=xyz",
            encode(CB)
        )
    }

    /// Everything a client does, from nothing to a bearer token.
    fn connect(w: &mut World) -> String {
        connect_with(w, false)
    }

    /// [`connect`], ticking the act box or not.
    /// The request id an approval page carries.
    fn request_id(html: &str) -> String {
        let at = html
            .find("name=\"request_id\" value=\"")
            .expect("a request id")
            + 25;
        html[at..at + 6].to_string()
    }

    /// Registers a client and opens its approval page; returns the request id.
    fn open_page(w: &mut World) -> String {
        let r = w.send(
            "POST",
            "/register",
            &[],
            &json!({"redirect_uris":[CB],"token_endpoint_auth_method":"none"}).to_string(),
        );
        let client = body(&r)["client_id"].as_str().unwrap().to_string();
        let page = w.send(
            "GET",
            &format!("/authorize?{}", authorize_query(&client)),
            &[],
            "",
        );
        assert_eq!(page.status, 200);
        request_id(&String::from_utf8(page.body).unwrap())
    }

    fn connect_with(w: &mut World, act: bool) -> String {
        let r = w.send(
            "POST",
            "/register",
            &[],
            &json!({"client_name":"Claude <b>","redirect_uris":[CB],"token_endpoint_auth_method":"none"}).to_string(),
        );
        assert_eq!(r.status, 201);
        let client = body(&r)["client_id"].as_str().unwrap().to_string();

        let page = w.send(
            "GET",
            &format!("/authorize?{}", authorize_query(&client)),
            &[],
            "",
        );
        assert_eq!(page.status, 200);
        let html = String::from_utf8(page.body.clone()).unwrap();
        assert!(
            html.contains("Claude &lt;b&gt;"),
            "the client's name is escaped"
        );
        let rid = request_id(&html);
        assert!(
            html.contains(&format!("Request <b>{rid}</b>")),
            "the page names its request"
        );

        w.pair(&rid);
        let form = format!(
            "request_id={rid}&pairing_code=424242{}",
            if act { "&act=on" } else { "" }
        );
        let r = w.send("POST", "/authorize", &[], &form);
        assert_eq!(r.status, 302);
        let location = header(&r, "location").unwrap().to_string();
        assert!(location.starts_with(&format!("{CB}?code=")), "{location}");
        // RFC 9207: the issuer rides along, exactly as the metadata names it.
        assert!(
            location.ends_with(&format!("&state=xyz&iss={}", encode(BASE))),
            "{location}"
        );
        let code = location
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();

        let r = w.send(
            "POST",
            "/token",
            &[],
            &format!(
                "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client}&code_verifier={VERIFIER}",
                encode(CB)
            ),
        );
        assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
        assert_eq!(header(&r, "cache-control"), Some("no-store"));
        body(&r)["access_token"].as_str().unwrap().to_string()
    }

    #[test]
    fn metadata_is_public() {
        let mut w = World::new();
        let r = w.send("GET", "/.well-known/oauth-authorization-server", &[], "");
        assert_eq!(r.status, 200);
        assert_eq!(body(&r)["issuer"], BASE);
        let r = w.send("GET", "/.well-known/oauth-protected-resource/mcp", &[], "");
        assert_eq!(body(&r)["resource"], format!("{BASE}/mcp"));
    }

    #[test]
    fn mcp_without_a_token_points_at_the_metadata() {
        let mut w = World::new();
        let r = w.send("POST", "/mcp", &[], "{}");
        assert_eq!(r.status, 401);
        assert_eq!(
            header(&r, "www-authenticate").unwrap(),
            format!("Bearer resource_metadata=\"{BASE}/.well-known/oauth-protected-resource\"")
        );
        let r = w.send("POST", "/mcp", &[("authorization", "Bearer made-up")], "{}");
        assert_eq!(r.status, 401);
    }

    #[test]
    fn a_connected_client_speaks_mcp() {
        let mut w = World::new();
        let token = connect(&mut w);
        let auth = format!("Bearer {token}");
        let r = w.send(
            "POST",
            "/mcp",
            &[("authorization", &auth)],
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(r.status, 200);
        assert_eq!(body(&r)["result"]["serverInfo"]["name"], "herdr-tower");

        let r = w.send(
            "POST",
            "/mcp",
            &[("authorization", &auth)],
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert_eq!(r.status, 202);
        assert!(r.body.is_empty());

        let r = w.send(
            "POST",
            "/mcp",
            &[("authorization", &auth)],
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        assert_eq!(
            body(&r)["result"]["tools"].as_array().unwrap().len(),
            crate::mcp::tools::list(&[oauth::SCOPE_READ.to_string()]).len(),
            "read only: the box was not ticked"
        );

        let r = w.send("POST", "/mcp", &[("authorization", &auth)], "[]");
        assert_eq!(r.status, 400, "no batches");
        let r = w.send("POST", "/mcp", &[("authorization", &auth)], "{nope");
        assert_eq!(body(&r)["error"]["code"], -32700);

        let g = grants::load(w.dir.path()).unwrap();
        assert_eq!(g.grants[0].last_used, Some(now()), "use is recorded");
    }

    /// 2026-07-28: a stateless client needs no handshake, and a legacy
    /// client on the same endpoint is served as before.
    #[test]
    fn a_stateless_client_is_served_next_to_a_legacy_one() {
        let mut w = World::new();
        let token = connect(&mut w);
        let auth = format!("Bearer {token}");
        let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;
        let modern = [
            ("authorization", auth.as_str()),
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/list"),
        ];
        let r = w.send(
            "POST",
            "/mcp",
            &modern,
            &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{meta}}}}}"#),
        );
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let b = body(&r);
        assert_eq!(b["result"]["resultType"], "complete");
        assert_eq!(b["result"]["cacheScope"], "private");
        assert_eq!(
            b["result"]["tools"].as_array().unwrap().len(),
            crate::mcp::tools::list(&[oauth::SCOPE_READ.to_string()]).len(),
        );

        // The headers are checked against the body.
        let wrong = [
            ("authorization", auth.as_str()),
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "server/discover"),
        ];
        let r = w.send(
            "POST",
            "/mcp",
            &wrong,
            &format!(r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{{meta}}}}}"#),
        );
        assert_eq!(r.status, 400);
        assert_eq!(body(&r)["error"]["code"], -32020);

        // A legacy client, header and all, still gets the handshake-era answer.
        let r = w.send(
            "POST",
            "/mcp",
            &[
                ("authorization", &auth),
                ("mcp-protocol-version", "2025-06-18"),
            ],
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
        );
        assert_eq!(r.status, 200);
        assert!(body(&r)["result"].get("resultType").is_none());

        // Without a token, neither era gets anywhere.
        let r = w.send(
            "POST",
            "/mcp",
            &modern[1..],
            &format!(r#"{{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{{{meta}}}}}"#),
        );
        assert_eq!(r.status, 401);
    }

    fn tool_names(w: &mut World, token: &str) -> Vec<String> {
        let auth = format!("Bearer {token}");
        let r = w.send(
            "POST",
            "/mcp",
            &[("authorization", &auth)],
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        body(&r)["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_ticked_act_box_offers_the_act_tools() {
        let mut w = World::new();
        let token = connect_with(&mut w, true);
        let names = tool_names(&mut w, &token);
        assert!(names.contains(&"send_prompt".to_string()), "{names:?}");
        assert_eq!(names.len(), crate::mcp::tools::TOOLS.len());
    }

    #[test]
    fn act_turned_off_on_the_server_outranks_any_grant() {
        let mut w = World::new();
        let token = connect_with(&mut w, true);
        w.act = false;
        let names = tool_names(&mut w, &token);
        assert!(!names.contains(&"send_prompt".to_string()), "{names:?}");
        assert!(names.contains(&"read_agent".to_string()));
    }

    #[test]
    fn the_act_box_is_shown_only_when_act_is_offered() {
        let mut w = World::new();
        let r = w.send(
            "POST",
            "/register",
            &[],
            &json!({"redirect_uris":[CB],"token_endpoint_auth_method":"none"}).to_string(),
        );
        let client = body(&r)["client_id"].as_str().unwrap().to_string();
        let q = format!("/authorize?{}", authorize_query(&client));
        let html = String::from_utf8(w.send("GET", &q, &[], "").body).unwrap();
        assert!(html.contains("name=\"act\""));
        assert!(!html.contains(" checked"), "never ticked for the person");
        w.act = false;
        let html = String::from_utf8(w.send("GET", &q, &[], "").body).unwrap();
        assert!(!html.contains("name=\"act\""));
    }

    #[test]
    fn act_cannot_be_smuggled_in_when_the_server_does_not_offer_it() {
        let mut w = World::new();
        w.act = false;
        let token = connect_with(&mut w, true);
        let g = grants::load(w.dir.path()).unwrap();
        assert_eq!(g.grants[0].scopes, vec![oauth::SCOPE_READ], "{token}");
    }

    #[test]
    fn a_foreign_origin_is_refused_before_the_token_is_looked_at() {
        let mut w = World::new();
        let token = connect(&mut w);
        let auth = format!("Bearer {token}");
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let r = w.send(
            "POST",
            "/mcp",
            &[("authorization", &auth), ("origin", "https://evil.example")],
            ping,
        );
        assert_eq!(r.status, 403);
        let r = w.send(
            "POST",
            "/mcp",
            &[("authorization", &auth), ("origin", "https://claude.ai")],
            ping,
        );
        assert_eq!(r.status, 200);
        let r = w.send(
            "POST",
            "/mcp",
            &[("authorization", &auth), ("origin", BASE)],
            ping,
        );
        assert_eq!(r.status, 200);
    }

    #[test]
    fn a_wrong_pairing_code_shows_the_page_again_and_issues_nothing() {
        let mut w = World::new();
        let rid = open_page(&mut w);
        w.pair(&rid);
        let r = w.send(
            "POST",
            "/authorize",
            &[],
            &format!("request_id={rid}&pairing_code=000000&act=on"),
        );
        assert_eq!(r.status, 401);
        let html = String::from_utf8(r.body).unwrap();
        assert!(html.contains("4 attempt(s) left"), "{html}");
        assert!(
            html.contains(&format!("Request <b>{rid}</b>")),
            "the same page again"
        );
        assert!(
            html.contains(" checked"),
            "the box stays as the person left it"
        );
        assert!(grants::load(w.dir.path()).unwrap().codes.is_empty());
    }

    /// #103: a stranger's wrong codes on their own page — any number of them,
    /// on any number of pages — never stop someone else from pairing.
    #[test]
    fn a_strangers_guesses_do_not_lock_anyone_out() {
        let mut w = World::new();
        for _ in 0..5 {
            let theirs = open_page(&mut w);
            for _ in 0..10 {
                let r = w.send(
                    "POST",
                    "/authorize",
                    &[],
                    &format!("request_id={theirs}&pairing_code=000000"),
                );
                assert_eq!(r.status, 401, "no code exists for their page");
            }
        }
        let mine = open_page(&mut w);
        let rnd = &mut w.rnd;
        grants::update(w.dir.path(), now(), |g| {
            oauth::issue_pairing(g, &mine, now(), rnd)
        })
        .unwrap()
        .unwrap();
        let r = w.send(
            "POST",
            "/authorize",
            &[],
            &format!("request_id={mine}&pairing_code=424242"),
        );
        assert_eq!(r.status, 302, "{}", String::from_utf8_lossy(&r.body));
    }

    /// A closed or unknown page is refused plainly, never redirected.
    #[test]
    fn an_unknown_request_gets_an_error_page() {
        let mut w = World::new();
        let r = w.send(
            "POST",
            "/authorize",
            &[],
            "request_id=NOSUCH&pairing_code=424242",
        );
        assert_eq!(r.status, 400);
        assert!(header(&r, "location").is_none());
        assert!(String::from_utf8(r.body)
            .unwrap()
            .contains("expired or was closed"));
    }

    #[test]
    fn a_bad_authorize_request_is_never_redirected() {
        let mut w = World::new();
        let r = w.send(
            "GET",
            &format!(
                "/authorize?response_type=code&client_id=c-nobody&redirect_uri={}",
                encode("https://evil.example/cb")
            ),
            &[],
            "",
        );
        assert_eq!(r.status, 400);
        assert!(header(&r, "location").is_none());
    }

    #[test]
    fn the_approval_page_cannot_be_framed() {
        let mut w = World::new();
        let r = w.send("GET", "/authorize", &[], "");
        assert_eq!(header(&r, "x-frame-options"), Some("DENY"));
    }

    #[test]
    fn other_methods_on_mcp_and_unknown_paths() {
        let mut w = World::new();
        let r = w.send("GET", "/mcp", &[], "");
        assert_eq!(r.status, 405);
        assert_eq!(header(&r, "allow"), Some("POST"));
        assert_eq!(w.send("DELETE", "/mcp", &[], "").status, 405);
        assert_eq!(w.send("GET", "/admin", &[], "").status, 404);
    }

    #[test]
    fn form_encoding_round_trips() {
        assert_eq!(encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(decode("a+b%26c%3Dd%2F%C3%A9"), "a b&c=d/é");
        assert_eq!(decode("100%"), "100%", "a stray percent is kept");
        assert_eq!(decode("%zz"), "%zz");
        let f = decode_form("a=1&b=&c&a=2");
        assert_eq!(f["a"], "1", "the first of a repeated name wins");
        assert_eq!(f["b"], "");
        assert_eq!(f["c"], "");
    }

    #[test]
    fn basic_credentials_are_form_decoded() {
        let h = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("c-1:s%3Ae+t")
        );
        assert_eq!(basic_credentials(&h), Some(("c-1".into(), "s:e t".into())));
        assert_eq!(basic_credentials("Bearer x"), None);
    }

    /// Deterministic strings over an alphabet that exercises every branch of
    /// the encoder: unreserved, reserved, `+`, `%`, spaces, multi-byte.
    fn generated(n: usize) -> Vec<String> {
        const ALPHABET: &[&str] = &[
            "a", "Z", "0", "-", ".", "_", "~", " ", "+", "%", "&", "=", "?", "/", "#", "'", "\"",
            "é", "한", "🙂", "\n", "%2", "%zz",
        ];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        (0..n)
            .map(|i| {
                let len = i % 13;
                (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        ALPHABET[(seed % ALPHABET.len() as u64) as usize]
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn encode_then_decode_is_the_identity() {
        for s in generated(500) {
            let e = encode(&s);
            assert!(
                e.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b)),
                "only unreserved characters and escapes: {e:?}"
            );
            assert_eq!(decode(&e), s, "{s:?} → {e:?}");
        }
    }

    #[test]
    fn a_form_of_encoded_pairs_decodes_to_the_same_pairs() {
        let words = generated(120);
        for pair in words.chunks(2) {
            let (k, v) = (&pair[0], pair.get(1).cloned().unwrap_or_default());
            if k.is_empty() {
                continue; // an empty name is skipped by any form parser
            }
            let body = format!("{}={}&other=1", encode(k), encode(&v));
            let form = decode_form(&body);
            assert_eq!(form.get(k.as_str()), Some(&v), "{body}");
        }
    }

    /// Cross-check against the other encoder in the tree: a state value that
    /// survives `approve`'s redirect must come back from the query as sent.
    #[test]
    fn a_state_value_survives_the_redirect_query() {
        for s in generated(200).into_iter().filter(|s| !s.is_empty()) {
            let query = format!("code=abc&state={}", encode(&s));
            assert_eq!(decode_form(&query).get("state"), Some(&s));
        }
    }

    /// A source that panics on the first thing a tool reads.
    struct Panics;

    impl Source for Panics {
        fn now(&self) -> OffsetDateTime {
            now()
        }
        fn agents(&self) -> Result<Vec<Agent>, String> {
            panic!("a handler bug")
        }
        fn ctx(&self, _: &[Agent]) -> HashMap<String, tower_usage::CtxInfo> {
            HashMap::new()
        }
        fn branch(&self, _: &str) -> Option<String> {
            None
        }
        fn read_pane(&self, _: &Agent, _: u32) -> Result<String, String> {
            Err("no pane".into())
        }
        fn report(&self, _: OffsetDateTime) -> Value {
            json!({})
        }
        fn schedules(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn loops(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn supervisor_fresh(&self) -> bool {
            true
        }
        fn prompt(
            &self,
            _: &Agent,
            _: &str,
            _: Option<std::time::Duration>,
        ) -> Result<crate::mcp::tools::Sent, crate::mcp::tools::SendError> {
            Err(crate::mcp::tools::SendError::Failed("no herdr".into()))
        }
        fn wait(
            &self,
            _: &Agent,
            _: &[tower_state::AgentState],
            _: std::time::Duration,
        ) -> Result<bool, String> {
            Err("no herdr".into())
        }
        fn live_state(&self, _: &Agent) -> Option<tower_state::AgentState> {
            None
        }
        fn audit(&self, _: &crate::mcp::audit::Entry) {}
        fn keys(&self, _: &Agent, _: &[&str]) -> Result<(), String> {
            Err("no herdr".into())
        }
        fn run_tower(
            &self,
            _: &[String],
            _: std::time::Duration,
        ) -> Result<crate::mcp::tools::Ran, String> {
            Err("no binary".into())
        }
    }

    /// #107: a handler that panics is answered 500, and the next request is
    /// served as usual.
    #[test]
    fn a_panicking_handler_is_answered_500() {
        let mut w = World::new();
        let token = connect(&mut w);
        let s = w.settings();
        let auth = format!("Bearer {token}");
        let headers = vec![("authorization".to_string(), auth)];
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_agents","arguments":{}}}"#;
        let req = Request {
            method: "POST",
            path: "/mcp",
            query: "",
            headers: &headers,
            body,
        };
        let r = route_safely(&s, &Panics, &req, now(), &mut w.rnd);
        assert_eq!(r.status, 500);
        let ok = Request {
            method: "GET",
            path: "/",
            query: "",
            headers: &[],
            body: b"",
        };
        assert_eq!(
            route_safely(&s, &Panics, &ok, now(), &mut w.rnd).status,
            200
        );
    }

    /// #107: a slot comes back when its thread ends, however it ends.
    #[test]
    fn a_slot_is_returned_even_when_its_thread_panics() {
        let pools = Arc::new(Pools::default());
        for _ in 0..MAX_CONCURRENT * 2 {
            pools.tools.fetch_add(1, Ordering::SeqCst);
            let slot = PoolSlot {
                pools: pools.clone(),
                which: Pool::Tools,
            };
            let joined = std::thread::spawn(move || {
                let _slot = slot;
                panic!("a handler bug");
            })
            .join();
            assert!(joined.is_err());
        }
        assert_eq!(pools.tools.load(Ordering::SeqCst), 0);
    }

    /// Starts `serve` on a free loopback port and returns it, trying another
    /// port when a parallel test took the first.
    ///
    /// Ready means a request was *answered*, not that a connection was
    /// accepted. The readiness probe takes a reading slot like any other
    /// connection; a bare connect-and-drop left giving it back to a server
    /// thread that, under a full parallel run, could still be waiting to be
    /// scheduled while the test filled the pool — so the probe's slot was
    /// counted against the test's own connections and then freed in the
    /// middle of it, and the slot tests saw one slot too many (the flaky
    /// `stalled_bodies_cannot_take_the_slots_kept_for_connected_clients`).
    /// [`serve_on`] gives the reading slot back before it writes an answer, so
    /// once one has arrived the pool is empty again.
    fn start_server(settings: impl Fn() -> Settings, src: Arc<dyn Source>) -> SocketAddr {
        for _ in 0..10 {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let listen: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let (st, sr) = (settings(), src.clone());
            let server = std::thread::spawn(move || serve(listen, st, sr));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if server.is_finished() {
                    break;
                }
                if answered(listen) {
                    return listen;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the server never listened"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        panic!("no port to listen on");
    }

    /// Whether the server at `listen` answers a request — any status.
    fn answered(listen: SocketAddr) -> bool {
        use std::io::{Read as _, Write as _};
        let Ok(mut c) = std::net::TcpStream::connect(listen) else {
            return false;
        };
        let _ = c.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        if c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").is_err() {
            return false;
        }
        let mut answer = String::new();
        let _ = c.read_to_string(&mut answer);
        answer.starts_with("HTTP/1.1 ")
    }

    /// Sends raw bytes and reads the answer's status, or `None` when the
    /// server closed without one.
    fn exchange(listen: SocketAddr, raw: &str) -> Option<u16> {
        use std::io::{Read as _, Write as _};
        let mut c = std::net::TcpStream::connect(listen).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        c.write_all(raw.as_bytes()).unwrap();
        let mut answer = String::new();
        let _ = c.read_to_string(&mut answer);
        answer.get(9..12).and_then(|s| s.parse().ok())
    }

    /// Opens a connection that starts a request and then says nothing more.
    fn stall(listen: SocketAddr, raw: &str) -> std::net::TcpStream {
        use std::io::Write as _;
        let mut c = std::net::TcpStream::connect(listen).unwrap();
        c.write_all(raw.as_bytes()).unwrap();
        c
    }

    /// #116: connections that start a request body and stall — no token, no
    /// code, nothing — do not stop the other routes.
    #[test]
    fn stalled_bodies_do_not_shut_the_server() {
        let w = World::new();
        let listen = start_server(|| w.settings(), Arc::new(Nobody::default()));
        let stalled: Vec<_> = (0..MAX_CONTROL * 2)
            .map(|_| {
                stall(
                    listen,
                    "POST /token HTTP/1.1\r\nHost: x\r\nContent-Length: 100000\r\n\r\nx",
                )
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            exchange(
                listen,
                "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            ),
            Some(200)
        );
        drop(stalled);
    }

    fn quick_world() -> World {
        let mut w = World::new();
        w.read_deadline = std::time::Duration::from_millis(700);
        w
    }

    /// #116: a stalled connection is closed once its deadline passes.
    #[test]
    fn a_stalled_request_is_closed_at_its_deadline() {
        use std::io::Read as _;
        let w = quick_world();
        let listen = start_server(|| w.settings(), Arc::new(Nobody::default()));
        let mut c = stall(
            listen,
            "POST /token HTTP/1.1\r\nHost: x\r\nContent-Length: 50\r\n\r\nx",
        );
        c.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let started = std::time::Instant::now();
        let mut buf = Vec::new();
        let _ = c.read_to_end(&mut buf);
        assert!(buf.is_empty(), "no answer to a request never finished");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "closed at the deadline, not left open: {:?}",
            started.elapsed()
        );
    }

    /// #116: filling the reading pool gets a 503 at worst, and only until
    /// the stalled connections run out of time.
    #[test]
    fn a_full_reading_pool_recovers_at_the_deadline() {
        let w = quick_world();
        let listen = start_server(|| w.settings(), Arc::new(Nobody::default()));
        let stalled: Vec<_> = (0..MAX_READING)
            .map(|_| stall(listen, "GET / HTTP/1.1\r\n"))
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            exchange(listen, "GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(503)
        );
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert_eq!(
            exchange(listen, "GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(200)
        );
        drop(stalled);
    }

    /// Requests the server will not read are answered from the head alone.
    #[test]
    fn requests_are_refused_by_their_head() {
        let w = quick_world();
        let listen = start_server(|| w.settings(), Arc::new(Nobody::default()));
        let big = format!(
            "POST /token HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            BODY_CAP + 1
        );
        let started = std::time::Instant::now();
        assert_eq!(exchange(listen, &big), Some(413));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "the body was not waited for"
        );
        assert_eq!(
            exchange(
                listen,
                "POST /token HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            ),
            Some(411)
        );
        assert_eq!(exchange(listen, "NONSENSE\r\n\r\n"), Some(400));
        assert_eq!(
            exchange(
                listen,
                "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: nine\r\n\r\n"
            ),
            Some(400)
        );
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(HEAD_CAP + 10));
        assert_eq!(exchange(listen, &huge), Some(431));
    }

    /// #27: strangers trickling bodies can take the open slots, not the ones
    /// kept for connected clients. On a reserved slot a request with no
    /// valid token is refused on its head, before its body; one whose head
    /// brought all of it is served; a live token's call gets through.
    #[test]
    fn stalled_bodies_cannot_take_the_slots_kept_for_connected_clients() {
        use std::io::{Read as _, Write as _};
        let mut w = World::new();
        let token = connect(&mut w);
        // The listener runs on the wall clock, and `connect` on the tests'
        // fixed one: the grant is made current.
        let wall = OffsetDateTime::now_utc();
        grants::update(w.dir.path(), now(), |g| {
            g.grants[0].access_expires = wall + oauth::ACCESS_TTL;
            g.grants[0].refresh_expires = wall + oauth::REFRESH_TTL;
        })
        .unwrap();
        let listen = start_server(|| w.settings(), Arc::new(Nobody::default()));
        let slow = "POST /token HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\n\r\nx";
        let stalled: Vec<_> = (0..MAX_READING - RESERVED_READING)
            .map(|_| stall(listen, slow))
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let started = std::time::Instant::now();
        assert_eq!(exchange(listen, slow), Some(503), "a stranger's slow body");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "refused on its head, not after waiting for the body"
        );
        assert_eq!(
            exchange(
                listen,
                "POST /mcp HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer made-up\r\nContent-Length: 1000\r\n\r\nx"
            ),
            Some(503),
            "a made-up token is no credential"
        );
        assert_eq!(
            exchange(listen, "GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(200),
            "nothing left to trickle, so it is served"
        );

        // A connected client whose body arrives after its head.
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut c = std::net::TcpStream::connect(listen).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        write!(
            c,
            "POST /mcp HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
            ping.len()
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        c.write_all(ping.as_bytes()).unwrap();
        let mut answer = String::new();
        let _ = c.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 200 "), "{answer}");
        drop(stalled);
    }

    /// #27: a request that changes nothing — an unknown page, client, code
    /// or token — does not rewrite the grants file.
    #[cfg(unix)]
    #[test]
    fn requests_that_change_nothing_write_nothing() {
        use std::os::unix::fs::MetadataExt;
        let mut w = World::new();
        connect(&mut w);
        let inode = |w: &World| std::fs::metadata(grants::path(w.dir.path())).unwrap().ino();
        let before = inode(&w);
        let r = w.send(
            "POST",
            "/authorize",
            &[],
            "request_id=NOSUCH&pairing_code=000000",
        );
        assert_eq!(r.status, 400);
        let r = w.send(
            "POST",
            "/token",
            &[],
            "grant_type=authorization_code&code=x&client_id=c-nobody&code_verifier=x",
        );
        assert_eq!(r.status, 401);
        let r = w.send(
            "GET",
            "/authorize?response_type=code&client_id=c-nobody",
            &[],
            "",
        );
        assert_eq!(r.status, 400);
        let r = w.send(
            "POST",
            "/register",
            &[],
            &json!({"redirect_uris":["https://evil.example/cb"]}).to_string(),
        );
        assert_eq!(r.status, 400);
        let r = w.send("POST", "/mcp", &[("authorization", "Bearer made-up")], "{}");
        assert_eq!(r.status, 401);
        assert_eq!(inode(&w), before, "the grants file was rewritten");
    }

    /// #26: a code redeemed into a new grant is announced, with its rights;
    /// a refused exchange is not.
    #[test]
    fn a_new_grant_is_announced() {
        let mut w = World::new();
        connect_with(&mut w, true);
        let r = w.send(
            "POST",
            "/token",
            &[],
            "grant_type=authorization_code&code=nope&client_id=c-nobody",
        );
        assert_eq!(r.status, 401);
        assert_eq!(
            *w.src.granted.lock().unwrap(),
            vec![("Claude <b>".to_string(), true)]
        );
    }

    /// #25: the page says the name is the client's own claim, and shows
    /// what a stranger cannot choose: where approving returns to.
    #[test]
    fn the_approval_page_marks_the_name_as_self_declared() {
        let mut w = World::new();
        let r = w.send(
            "POST",
            "/register",
            &[],
            &json!({"client_name":"Claude","redirect_uris":[CB],"token_endpoint_auth_method":"none"}).to_string(),
        );
        let client = body(&r)["client_id"].as_str().unwrap().to_string();
        let q = format!("/authorize?{}", authorize_query(&client));
        let html = String::from_utf8(w.send("GET", &q, &[], "").body).unwrap();
        assert!(
            html.contains("<b>Claude</b> <span class=\"note\">(the name it gave itself"),
            "{html}"
        );
        assert!(
            html.contains("Approving sends you back to <b>claude.ai</b>. It registered just now."),
            "{html}"
        );
        let rid = request_id(&html);
        assert!(html.contains(&format!("<code>herdr-tower mcp pair {rid}</code>")));
    }

    /// #24: a full server never answers a new approval page `503`.
    #[test]
    fn a_full_server_still_opens_a_page() {
        let mut w = World::new();
        for _ in 0..oauth::MAX_PENDING_REQUESTS + 5 {
            open_page(&mut w);
        }
        let g = grants::load(w.dir.path()).unwrap();
        assert_eq!(g.requests.len(), oauth::MAX_PENDING_REQUESTS);
    }

    /// A client that waits for `100 Continue` before its body gets it.
    #[test]
    fn expect_continue_is_answered() {
        use std::io::{Read as _, Write as _};
        let w = quick_world();
        let listen = start_server(|| w.settings(), Arc::new(Nobody::default()));
        let body = json!({"redirect_uris":[CB],"token_endpoint_auth_method":"none"}).to_string();
        let mut c = std::net::TcpStream::connect(listen).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(
            c,
            "POST /register HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n",
            body.len()
        )
        .unwrap();
        let mut first = [0u8; 25];
        c.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"HTTP/1.1 100 Continue\r\n\r\n");
        c.write_all(body.as_bytes()).unwrap();
        let mut rest = String::new();
        c.read_to_string(&mut rest).unwrap();
        assert!(rest.starts_with("HTTP/1.1 201 Created\r\n"), "{rest}");
        assert!(rest.contains("Connection: close\r\n"));
    }
}
