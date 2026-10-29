// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `mcp serve` as a remote client meets it: the real binary on a loopback
//! port, walked from an unauthenticated request to a tool call.
//!
//! The pairing code comes from a second run of the binary (`mcp pair`), the
//! way a person gets one, so the two processes sharing the grants file is
//! part of what is tested. No herdr and no network beyond loopback: the store
//! is the recorded fixture, and Discord is off.

#![cfg(unix)]

use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BASE: &str = "https://mac.example.ts.net";
const CB: &str = "https://claude.ai/api/mcp/auth_callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

fn tower(dir: &Path, args: &[&str]) -> Command {
    let bin = std::env::var_os("HERDR_TOWER_TEST_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_herdr-tower").into());
    let mut c = Command::new(bin);
    c.args(args)
        .env("HOME", dir)
        .env("HERDR_TOWER_STATE_DIR", dir.join("state"))
        .env("HERDR_TOWER_CONFIG", dir.join("config.json"))
        .env("HERDR_SOCKET_PATH", dir.join("no-herdr.sock"))
        // Never this machine's Tailscale: its name is a public URL, and with
        // one `mcp serve` would bind the real port and serve.
        .env("HERDR_TOWER_TAILSCALE", "none")
        .current_dir(dir);
    c
}

fn start(dir: &Path) -> Server {
    let port = free_port();
    std::fs::write(
        dir.join("config.json"),
        json!({"mcp": {"listen": format!("127.0.0.1:{port}"), "public_url": BASE}}).to_string(),
    )
    .unwrap();
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../expected/fixture"),
        &dir.join("state"),
    );
    let child = tower(dir, &["mcp", "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the binary starts");
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the server never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, url }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

struct Reply {
    status: u16,
    location: Option<String>,
    www_authenticate: Option<String>,
    body: String,
}

fn send(
    a: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<(&str, String)>,
) -> Reply {
    let mut resp = match (method, body) {
        ("GET", _) => {
            let mut r = a.get(url);
            for (n, v) in headers {
                r = r.header(*n, *v);
            }
            r.call()
        }
        (_, Some((ctype, body))) => {
            let mut r = a.post(url).header("content-type", ctype);
            for (n, v) in headers {
                r = r.header(*n, *v);
            }
            r.send(body.as_bytes())
        }
        _ => unreachable!(),
    }
    .expect("a response");
    let h = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let location = h("location");
    let www_authenticate = h("www-authenticate");
    Reply {
        status: resp.status().as_u16(),
        location,
        www_authenticate,
        body: resp.body_mut().read_to_string().unwrap_or_default(),
    }
}

fn rpc(a: &ureq::Agent, s: &Server, token: &str, msg: Value) -> Reply {
    send(
        a,
        "POST",
        &format!("{}/mcp", s.url),
        &[
            ("authorization", &format!("Bearer {token}")),
            ("accept", "application/json, text/event-stream"),
        ],
        Some(("application/json", msg.to_string())),
    )
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Register, pair (through a second run of the binary, as a person would),
/// approve — ticking the act box or not — and exchange. Returns the token.
fn connect(dir: &Path, s: &Server, a: &ureq::Agent, act: bool) -> String {
    // Register.
    let r = send(
        a,
        "POST",
        &format!("{}/register", s.url),
        &[],
        Some((
            "application/json",
            json!({"client_name":"Claude","redirect_uris":[CB],"token_endpoint_auth_method":"none"}).to_string(),
        )),
    );
    assert_eq!(r.status, 201, "{}", r.body);
    let client = serde_json::from_str::<Value>(&r.body).unwrap()["client_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The approval page, as the connector opens it: it names its request.
    let query = format!(
        "response_type=code&client_id={client}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=st",
        enc(CB)
    );
    let page = send(a, "GET", &format!("{}/authorize?{query}", s.url), &[], None);
    assert_eq!(page.status, 200, "{}", page.body);
    let marker = "name=\"request_id\" value=\"";
    let at = page
        .body
        .find(marker)
        .expect("the page carries its request")
        + marker.len();
    let request = page.body[at..at + 6].to_string();

    // #25: with no id and no terminal to confirm on, even the only page open
    // is not picked; its id is shown instead.
    let out = tower(dir, &["mcp", "pair"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains(&format!("herdr-tower mcp pair {request}"))
            && said.contains("\"Claude\" (self-declared), returns to claude.ai"),
        "{said}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("pairing code"));

    // A pairing code for that page, the way a person gets one. `--no-discord`
    // is the default now, and still accepted.
    let out = tower(dir, &["mcp", "pair", &request, "--no-discord"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains(&format!("for request {request}")),
        "{stdout}"
    );
    let code = stdout
        .lines()
        .find_map(|l| l.strip_prefix("pairing code: "))
        .expect("the code is printed")
        .to_string();

    // Approve.
    let form = format!(
        "request_id={request}&pairing_code={code}{}",
        if act { "&act=on" } else { "" }
    );
    let r = send(
        a,
        "POST",
        &format!("{}/authorize", s.url),
        &[],
        Some(("application/x-www-form-urlencoded", form)),
    );
    assert_eq!(r.status, 302, "{}", r.body);
    let location = r.location.unwrap();
    let issued = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string();

    // Exchange.
    let r = send(
        a,
        "POST",
        &format!("{}/token", s.url),
        &[],
        Some((
            "application/x-www-form-urlencoded",
            format!(
                "grant_type=authorization_code&code={issued}&redirect_uri={}&client_id={client}&code_verifier={VERIFIER}",
                enc(CB)
            ),
        )),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    serde_json::from_str::<Value>(&r.body).unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_client_connects_with_a_pairing_code_and_lists_the_agents() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path());
    let a = agent();

    // Unauthenticated: pointed at the metadata.
    let r = rpc(
        &a,
        &s,
        "nothing",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    );
    assert_eq!(r.status, 401);
    assert!(r
        .www_authenticate
        .unwrap()
        .contains(&format!("{BASE}/.well-known/oauth-protected-resource")));
    let meta: Value = serde_json::from_str(
        &send(
            &a,
            "GET",
            &format!("{}/.well-known/oauth-authorization-server", s.url),
            &[],
            None,
        )
        .body,
    )
    .unwrap();
    assert_eq!(meta["registration_endpoint"], format!("{BASE}/register"));

    let token = connect(dir.path(), &s, &a, false);

    // MCP.
    let r = rpc(
        &a,
        &s,
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let init: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    let r = rpc(
        &a,
        &s,
        &token,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    assert_eq!(r.status, 202);

    let r = rpc(
        &a,
        &s,
        &token,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_agents","arguments":{}}}),
    );
    let call: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(call["result"]["isError"], false, "{}", r.body);
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    let listed: Value = serde_json::from_str(text).unwrap();
    assert!(
        listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == "claude-w2:pA"),
        "{text}"
    );

    // This grant has read scope only. Waiting reaches the tool handler,
    // whereas an act tool is not available to this connection.
    let r = rpc(
        &a,
        &s,
        &token,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wait_agent","arguments":{"agent":"missing"}}}),
    );
    let wait: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(wait["result"]["isError"], true, "{}", r.body);
    assert!(wait["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("no agent"));

    // The grant is listed, then revoked, and the token stops working.
    let out = tower(dir.path(), &["mcp", "grants"]).output().unwrap();
    let listing = String::from_utf8(out.stdout).unwrap();
    assert!(listing.contains("Claude"), "{listing}");
    let out = tower(dir.path(), &["mcp", "revoke", "--all"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let r = rpc(
        &a,
        &s,
        &token,
        json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
    );
    assert_eq!(r.status, 401);
}

/// With no public URL — no Tailscale name, no `mcp.public_url` — the server
/// still starts, naming itself by its loopback address and serving this
/// machine alone.
#[test]
fn serve_without_a_public_url_serves_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    std::fs::write(
        dir.path().join("config.json"),
        json!({"mcp": {"listen": format!("127.0.0.1:{port}")}}).to_string(),
    )
    .unwrap();
    let child = tower(dir.path(), &["mcp", "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server {
        child,
        url: format!("http://127.0.0.1:{port}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the server never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    let meta: Value = serde_json::from_str(
        &agent()
            .get(format!(
                "{}/.well-known/oauth-authorization-server",
                server.url
            ))
            .call()
            .unwrap()
            .into_body()
            .read_to_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(meta["issuer"], server.url);
}

#[test]
fn serve_refuses_a_public_url_that_is_not_https() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"mcp": {"public_url": "http://mac.example"}}"#,
    )
    .unwrap();
    let out = tower(dir.path(), &["mcp", "serve"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("mcp.public_url"));
}

#[test]
fn serve_refuses_a_listen_address_beyond_loopback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        json!({"mcp": {"listen": "0.0.0.0:8787", "public_url": BASE}}).to_string(),
    )
    .unwrap();
    let out = tower(dir.path(), &["mcp", "serve"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
}

fn call_tool(a: &ureq::Agent, s: &Server, token: &str, name: &str, args: Value) -> Value {
    let r = rpc(
        a,
        s,
        token,
        json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":name,"arguments":args}}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    serde_json::from_str::<Value>(&r.body).unwrap()["result"].clone()
}

#[test]
fn an_act_grant_sends_through_herdr_and_every_attempt_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path());
    let a = agent();

    // Read only: send_prompt does not exist for this connection.
    let read = connect(dir.path(), &s, &a, false);
    let r = rpc(
        &a,
        &s,
        &read,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"send_prompt","arguments":{"agent":"claude-w2:pA","text":"go"}}}),
    );
    let body: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(body["error"]["code"], -32602, "{}", r.body);

    // Ticked: it exists, and with no herdr running the send fails — honestly,
    // and on the record.
    let act = connect(dir.path(), &s, &a, true);
    let res = call_tool(
        &a,
        &s,
        &act,
        "send_prompt",
        json!({"agent":"claude-w2:pA","text":"go ahead"}),
    );
    assert_eq!(res["isError"], true, "{res}");
    let audit = std::fs::read_to_string(dir.path().join("state/mcp-audit.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
    assert_eq!(entry["tool"], "send_prompt");
    assert_eq!(entry["agent"], "claude-w2:pA");
    assert_eq!(entry["text"], "go ahead");
    assert!(
        entry["outcome"].as_str().unwrap().starts_with("failed"),
        "{entry}"
    );

    // And `report` shows it.
    let out = tower(dir.path(), &["report", "--json"]).output().unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["remote_actions"][0]["text"], "go ahead", "{report}");
}

#[test]
fn machine_add_is_listed_for_act_and_runs_the_local_herdr_cli() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let herdr = bin.join("herdr");
    std::fs::write(
        &herdr,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/herdr-args\"\nprintf 'machine saved\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o755)).unwrap();

    let s = start(dir.path());
    let a = agent();
    let read = connect(dir.path(), &s, &a, false);
    let res = rpc(
        &a,
        &s,
        &read,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
    );
    let body: Value = serde_json::from_str(&res.body).unwrap();
    assert!(body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["name"] != "machine_add"));
    let act = connect(dir.path(), &s, &a, true);
    let res = rpc(
        &a,
        &s,
        &act,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let body: Value = serde_json::from_str(&res.body).unwrap();
    assert!(body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "machine_add"));

    let res = call_tool(
        &a,
        &s,
        &act,
        "machine_add",
        json!({
            "ssh_target":"user@example.test", "label":"build box", "remote_session":"dev"
        }),
    );
    assert_eq!(res["isError"], false, "{res}");
    assert_eq!(res["content"][0]["text"], "machine saved");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("herdr-args")).unwrap(),
        "machine\nadd\n--label\nbuild box\n--remote-session\ndev\nuser@example.test\n"
    );
    let audit = std::fs::read_to_string(dir.path().join("state/mcp-audit.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
    assert_eq!(entry["tool"], "machine_add");
}

#[test]
fn the_tower_tool_runs_any_command_line_through_the_real_binary() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path());
    let a = agent();
    let token = connect(dir.path(), &s, &a, true);

    let res = call_tool(&a, &s, &token, "tower", json!({"args":["status","--json"]}));
    assert_eq!(res["isError"], false, "{res}");
    let text = res["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("$ herdr-tower status --json\nexit 0"),
        "{text}"
    );
    assert!(
        text.contains("claude-w2:pA"),
        "the fixture store, through the CLI: {text}"
    );

    // A failing command is a result with its exit code, not a broken call.
    let res = call_tool(&a, &s, &token, "tower", json!({"args":["info","nobody"]}));
    let text = res["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\nexit 1"), "{text}");
    assert!(text.contains("no agent"), "{text}");

    // #26: the server's own commands are for the Mac, and refused here.
    let res = call_tool(&a, &s, &token, "tower", json!({"args":["mcp","grants"]}));
    assert_eq!(res["isError"], true, "{res}");
    let text = res["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("on the Mac"), "{text}");
    let res = call_tool(
        &a,
        &s,
        &token,
        "tower",
        json!({"args":["config","set","mcp.act","false"]}),
    );
    assert_eq!(res["isError"], true, "{res}");
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("config.json")).unwrap()).unwrap();
    assert!(cfg["mcp"].get("act").is_none(), "nothing was changed");

    let audit = std::fs::read_to_string(dir.path().join("state/mcp-audit.jsonl")).unwrap();
    let lines: Vec<Value> = audit
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let runs: Vec<(&str, &str)> = lines
        .iter()
        .filter(|e| e["tool"] == "tower")
        .map(|e| (e["text"].as_str().unwrap(), e["outcome"].as_str().unwrap()))
        .collect();
    assert_eq!(
        runs,
        [
            ("status --json", "exit 0"),
            ("info nobody", "exit 1"),
            ("mcp grants", "refused: for the Mac only"),
            ("config set mcp.act <redacted>", "refused: for the Mac only")
        ]
    );
}

#[test]
fn settings_can_be_changed_over_mcp_through_tower() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path());
    let a = agent();
    let token = connect(dir.path(), &s, &a, true);
    let run = |args: Value| {
        let res = call_tool(&a, &s, &token, "tower", json!({ "args": args }));
        res["content"][0]["text"].as_str().unwrap().to_string()
    };

    let out = run(json!(["config", "set", "wait_renotify", "20m"]));
    assert!(
        out.contains("exit 0") && out.contains("wait_renotify = 20m"),
        "{out}"
    );
    let out = run(json!(["config", "get", "wait_renotify"]));
    assert!(out.contains("\n20m\n"), "{out}");

    // The server's own section survived the edit, and a typo changed nothing.
    let out = run(json!(["config", "set", "wait_renotfy", "5m"]));
    assert!(
        out.contains("exit 1") && out.contains("no setting"),
        "{out}"
    );
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(cfg["wait_renotify"], "20m");
    assert_eq!(cfg["mcp"]["public_url"], BASE);
}

/// #101 + #107: the read-scope `report` tool with a window too large to
/// represent answers an error, and after more such calls than the server has
/// slots it still answers everything else.
#[test]
fn huge_report_windows_neither_panic_nor_exhaust_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path());
    let a = agent();
    let token = connect(dir.path(), &s, &a, false);
    for _ in 0..12 {
        let res = call_tool(
            &a,
            &s,
            &token,
            "report",
            json!({"since":"999999999999999999999d"}),
        );
        assert_eq!(res["isError"], true, "{res}");
    }
    let r = send(&a, "GET", &format!("{}/", s.url), &[], None);
    assert_eq!(r.status, 200, "the server still has its slots");
    let res = call_tool(&a, &s, &token, "report", json!({"since":"1h"}));
    assert_eq!(res["isError"], false, "{res}");
}
