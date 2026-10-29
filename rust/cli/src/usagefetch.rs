// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Gathering the usage board: claude from its OAuth usage endpoint, codex
//! from `codex app-server` (its session rollouts as the fallback), each
//! asked at most every five minutes and kept in the state directory.
//!
//! Shared by `herdr-tower usage` and the resident supervisor's usage guard.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use time::OffsetDateTime;
use tower_usage::Limits;

use crate::usagecmd::{Board, Vendor};

/// The endpoint claude's own `/usage` reads.
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The header that endpoint wants beside an OAuth token.
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// How old a live answer (claude's endpoint, codex's app server) may be
/// before it is asked for again: no more often than a person checking
/// `/usage` or `/status` would.
const EVERY: time::Duration = time::Duration::minutes(5);
/// After a failed ask, the wait before the next one.
const RETRY: time::Duration = time::Duration::minutes(1);
/// How long `codex app-server` gets to answer.
const CODEX_TIMEOUT: Duration = Duration::from_secs(15);
/// Where codex's last live answer is kept, in the state directory.
const CODEX_LIVE_CACHE: &str = "usage-codex-live.json";
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(10);
/// Where claude's last answer is kept, in the state directory.
const CLAUDE_CACHE: &str = "usage-claude.json";
/// Where codex's last reading is kept, in the state directory.
const CODEX_CACHE: &str = "usage-codex.json";

/// The board as it stands: each source asked only when its kept answer is due.
pub fn board(home: &Path, state_dir: &Path) -> Board {
    board_with(home, state_dir, false)
}

/// The board with every source asked again now, whatever its pace: the
/// usage tab's `r`. The tab keeps its own limit on how often (see
/// `cmd::usage`), since each ask is a request to the vendor.
pub fn board_refreshed(home: &Path, state_dir: &Path) -> Board {
    board_with(home, state_dir, true)
}

fn board_with(home: &Path, state_dir: &Path, force: bool) -> Board {
    let now = OffsetDateTime::now_utc();
    let mut vendors = vec![
        claude(home, state_dir, now, force),
        codex(home, state_dir, now, force),
    ];
    if home.join(".gemini").is_dir() {
        vendors.push(Vendor {
            name: "gemini",
            limits: Limits::default(),
            note: Some("no 5h or 7d windows — Gemini's limits are per day".into()),
        });
    }
    Board {
        vendors,
        at: now,
        guard: crate::usageguard::Guard::from_config(&tower_config::load()),
    }
}

/// codex's latest reading. Rollouts are only read back to the last reading
/// found, which is kept in the state directory: codex may go days without
/// recording one, and searching every rollout on every redraw would not do.
fn codex(home: &Path, state_dir: &Path, now: OffsetDateTime, force: bool) -> Vendor {
    // Live first: what codex's own `/status` shows. The rollouts only carry
    // limits while turns run, so a reached limit freezes them at the figure
    // before it.
    let account = std::fs::read(home.join(".codex").join("auth.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|a| {
            a.pointer("/tokens/account_id")
                .and_then(Value::as_str)
                .map(fingerprint)
        });
    let live = cached(
        &state_dir.join(CODEX_LIVE_CACHE),
        now,
        account.as_deref(),
        force,
        fetch_codex_live,
    );
    if let Some((mut limits, resets)) = live
        .body
        .as_ref()
        .and_then(tower_usage::parse_codex_rate_limits)
        .filter(|(l, _)| !l.is_empty())
    {
        limits.as_of = live.fetched_at;
        let mut notes = Vec::new();
        if limits.reached {
            notes.push("plan limit reached — paid credits may allow continued use".to_string());
        }
        if resets > 0 {
            notes.push(format!(
                "{resets} free full reset{} available (codex: Reset usage)",
                if resets == 1 { "" } else { "s" }
            ));
        }
        if let Some(e) = live.error {
            notes.push(format!("last refresh failed: {e}"));
        }
        return Vendor {
            name: "codex",
            limits,
            note: (!notes.is_empty()).then(|| notes.join("; ")),
        };
    }
    codex_from_rollouts(home, state_dir, live.error)
}

/// The rollout reading to show, and what to keep for next time, from what was
/// kept last time and one `scan` of the rollouts newer than it.
///
/// Kept: `mtime` and `limits`, the last reading and its rollout's time; and
/// `seen`, the newest rollout looked at, found or not. The next scan starts
/// after the later of the two, so rollouts without limits are read once — not
/// on every board read, as when only a found reading was kept. `None` to
/// write when nothing new was looked at.
fn next_rollout_reading(
    kept: Option<&Value>,
    scan: impl FnOnce(Option<SystemTime>) -> tower_usage::CodexScan,
) -> (Limits, Option<Value>) {
    // `try_`: a negative or huge value in the file must not panic.
    let time = |key: &str| {
        let secs = kept?.get(key)?.as_f64()?;
        Some(UNIX_EPOCH + Duration::try_from_secs_f64(secs).ok()?)
    };
    let secs = |t: SystemTime| {
        t.duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    };
    let found_at = time("mtime");
    let kept_limits: Option<Limits> =
        found_at.and_then(|_| serde_json::from_value(kept?.get("limits")?.clone()).ok());
    let found_at = kept_limits.as_ref().and(found_at);
    let after = found_at.max(time("seen"));
    let got = scan(after);
    if let Some((mtime, found)) = got.found {
        let seen = got.newest.max(Some(mtime)).map(secs);
        let v = json!({ "mtime": secs(mtime), "limits": found, "seen": seen });
        return (found, Some(v));
    }
    let limits = kept_limits.clone().unwrap_or_default();
    match got.newest.filter(|n| Some(*n) > after) {
        Some(newest) => {
            let mut v = json!({ "seen": secs(newest) });
            if let (Some(at), Some(l)) = (found_at, kept_limits) {
                v["mtime"] = json!(secs(at));
                v["limits"] = json!(l);
            }
            (limits, Some(v))
        }
        None => (limits, None),
    }
}

/// codex's last reading from its rollouts, when the live one is not to be had.
fn codex_from_rollouts(home: &Path, state_dir: &Path, live_error: Option<String>) -> Vendor {
    let root = tower_usage::codex_sessions_root(home);
    let cache_path = state_dir.join(CODEX_CACHE);
    let kept = std::fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let (limits, write) = next_rollout_reading(kept.as_ref(), |after| {
        tower_usage::codex_limits_scan(&root, after)
    });
    if let Some(v) = write {
        write_cache(&cache_path, &v);
    }
    let note = if limits.is_empty() {
        Some("no reading yet — codex records its limits on each turn".to_string())
    } else {
        Some(format!(
            "from codex's session logs; live reading unavailable{}",
            live_error.map(|e| format!(": {e}")).unwrap_or_default()
        ))
    };
    Vendor {
        name: "codex",
        limits,
        note,
    }
}

fn claude(home: &Path, state_dir: &Path, now: OffsetDateTime, force: bool) -> Vendor {
    let file = std::fs::read(home.join(".claude").join(".credentials.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let creds = pick_credentials(keychain_kept(now), file, now);
    let plan = creds.as_ref().and_then(tower_usage::claude_plan);
    let login = creds.as_ref().and_then(|c| {
        let o = c.get("claudeAiOauth")?;
        let t = o
            .get("refreshToken")
            .or_else(|| o.get("accessToken"))?
            .as_str()?;
        Some(fingerprint(t))
    });
    let got = cached(
        &state_dir.join(CLAUDE_CACHE),
        now,
        login.as_deref(),
        force,
        || {
            let token = creds
                .as_ref()
                .and_then(|c| tower_usage::claude_token(c, now))
                .ok_or("no current claude login — run claude once to refresh it")?;
            fetch_claude(&token)
        },
    );
    let mut note = got.error;
    let mut limits = Limits {
        plan,
        ..Limits::default()
    };
    if let Some(body) = got.body {
        match tower_usage::parse_claude_usage(body.to_string().as_bytes()) {
            Some((five, seven)) => {
                limits.five_hour = five;
                limits.seven_day = seven;
                limits.as_of = got.fetched_at;
            }
            None => note = Some("the usage endpoint answered in an unknown shape".into()),
        }
    }
    Vendor {
        name: "claude",
        limits,
        note,
    }
}

/// What a cached fetch has to show: the last answer, when it came, and why
/// the last try failed, if it did.
struct Cached {
    body: Option<Value>,
    fetched_at: Option<OffsetDateTime>,
    error: Option<String>,
}

/// Asks `fetch` when the kept answer is older than [`EVERY`] and the last try
/// is older than [`RETRY`]; otherwise answers from the file at `path`. A
/// failed ask keeps the old answer and says why.
fn cached(
    path: &Path,
    now: OffsetDateTime,
    login: Option<&str>,
    force: bool,
    fetch: impl FnOnce() -> std::result::Result<Value, String>,
) -> Cached {
    let rfc = &time::format_description::well_known::Rfc3339;
    let mut cache = read_cache(path);
    // Another login than the one the kept answer was for: that answer is
    // someone else's usage. It is dropped and asked for again now, whatever
    // the pace — a switched account must not show the old one's figures.
    if login.is_some() && cache.get("login").and_then(Value::as_str) != login {
        cache = json!({ "login": login });
    }
    let when = |cache: &Value, key: &str| {
        cache
            .get(key)
            .and_then(Value::as_str)
            .and_then(|t| OffsetDateTime::parse(t, rfc).ok())
    };
    let fresh = when(&cache, "fetched_at").is_some_and(|t| now - t < EVERY);
    let waited = when(&cache, "attempted_at").is_none_or(|t| now - t >= RETRY);
    if force || (!fresh && waited) {
        let stamp = now.format(rfc).unwrap_or_default();
        cache["attempted_at"] = json!(stamp);
        match fetch() {
            Ok(body) => {
                cache["fetched_at"] = json!(stamp);
                cache["body"] = body;
                cache["error"] = Value::Null;
            }
            Err(e) => cache["error"] = json!(e),
        }
        write_cache(path, &cache);
    }
    Cached {
        body: cache.get("body").filter(|b| !b.is_null()).cloned(),
        fetched_at: when(&cache, "fetched_at"),
        error: cache.get("error").and_then(Value::as_str).map(String::from),
    }
}

/// The credentials to read claude's plan and usage with. On macOS claude
/// keeps its login in the keychain, and `~/.claude/.credentials.json` can be
/// an older login left behind — after switching accounts it still held the
/// old one's token, unexpired, and the board showed the old account. So the
/// keychain comes first whenever it has a current token; the file serves
/// where there is no keychain (Linux), or when only it has one. When both are
/// current the later `expiresAt` wins, so a login written to the file alone
/// is not shadowed by the account before it left in the keychain.
pub fn pick_credentials(
    keychain: Option<Value>,
    file: Option<Value>,
    now: OffsetDateTime,
) -> Option<Value> {
    let current = |c: &Option<Value>| {
        c.as_ref()
            .is_some_and(|c| tower_usage::claude_token(c, now).is_some())
    };
    // Both current: the one issued last is the login in use. A login can
    // land in the file alone, leaving an unexpired keychain entry of the
    // account before, so a tie (or no `expiresAt`) keeps the keychain.
    let expires = |c: &Option<Value>| {
        c.as_ref()
            .and_then(|c| c.pointer("/claudeAiOauth/expiresAt"))
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    };
    match (current(&keychain), current(&file)) {
        (true, true) if expires(&file) > expires(&keychain) => file,
        (true, _) => keychain,
        (false, true) => file,
        (false, false) => keychain.or(file),
    }
}

/// A short, one-way name for a login, kept beside a cached answer to tell
/// whose it is. Never the token itself.
fn fingerprint(secret: &str) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(secret.as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// How long a keychain read is reused before the keychain is asked again.
///
/// Every board read asked it — one `/usr/bin/security` process every fifteen
/// seconds from each open tower, and every minute from the supervisor's usage
/// guard — for a login that changes when a person switches accounts. A
/// minute is how long a switch can take to show. A kept login whose token
/// has expired since it was read is not used: the keychain is asked again at
/// once, for the refreshed one. One that was expired already when read is
/// kept the minute like any other — asking again would find the same.
const KEYCHAIN_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The keychain's claude login, read at most once per [`KEYCHAIN_TTL`] in
/// this process.
fn keychain_kept(now: OffsetDateTime) -> Option<Value> {
    static KEPT: std::sync::Mutex<Option<Kept>> = std::sync::Mutex::new(None);
    let mut kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    kept_or_read(
        &mut kept,
        std::time::Instant::now(),
        now,
        keychain_credentials,
    )
}

/// A keychain reading: when it was taken, what it said, and whether its
/// token was current then.
type Kept = (std::time::Instant, Option<Value>, bool);

fn token_current(value: &Option<Value>, now: OffsetDateTime) -> bool {
    value
        .as_ref()
        .is_some_and(|c| tower_usage::claude_token(c, now).is_some())
}

/// The kept reading while it is younger than [`KEYCHAIN_TTL`], unless its
/// token was current when read and has expired since; otherwise `read`
/// again, and keep that.
fn kept_or_read(
    kept: &mut Option<Kept>,
    at: std::time::Instant,
    now: OffsetDateTime,
    read: impl FnOnce() -> Option<Value>,
) -> Option<Value> {
    if let Some((when, value, was_current)) = kept.as_ref() {
        let young = at.saturating_duration_since(*when) < KEYCHAIN_TTL;
        let expired_since = *was_current && !token_current(value, now);
        if young && !expired_since {
            return value.clone();
        }
    }
    let value = read();
    *kept = Some((at, value.clone(), token_current(&value, now)));
    value
}

#[cfg(target_os = "macos")]
fn keychain_credentials() -> Option<Value> {
    let out = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

#[cfg(not(target_os = "macos"))]
fn keychain_credentials() -> Option<Value> {
    None
}

/// Asks claude's usage endpoint. The token goes to the host that issued it
/// and nowhere else, and is never written down.
fn fetch_claude(token: &str) -> std::result::Result<Value, String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(CLAUDE_TIMEOUT))
            .http_status_as_error(false)
            .build(),
    );
    let resp = agent
        .get(CLAUDE_USAGE_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", CLAUDE_OAUTH_BETA)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("could not reach the usage endpoint ({e})"))?;
    let status = resp.status().as_u16();
    let body = resp
        .into_body()
        .read_to_string()
        .map_err(|e| format!("the usage endpoint's answer could not be read ({e})"))?;
    match status {
        200 => serde_json::from_str(&body)
            .map_err(|_| "the usage endpoint did not answer in JSON".to_string()),
        401 | 403 => Err("claude's login was refused — run claude once to refresh it".into()),
        429 => Err("the usage endpoint asked to slow down; trying again shortly".into()),
        s => Err(format!("the usage endpoint answered {s}")),
    }
}

/// `codex`, found the way the tower finds every tool it did not ship: on
/// `PATH`, then in the usual install locations. The resident supervisor reads
/// usage for the guard, and a LaunchAgent's `PATH` is `/usr/bin:/bin`, where a
/// bare `codex` is never found. The child gets the widened `PATH` too, so a
/// codex installed through npm finds its `node`.
fn codex_command(tools: &tower_usage::ToolSearch) -> Option<std::process::Command> {
    let program = tools.lookup("codex")?;
    let mut cmd = std::process::Command::new(program);
    cmd.env("PATH", tools.extended_path());
    // `codex app-server` can leave a shared daemon behind that runs every
    // codex session's hooks with *its* environment. Started from the tower's
    // pane, that daemon carried the tower's `HERDR_PANE_ID`, and every codex
    // hook landed on that pane's record (#45). It is nobody's pane.
    for var in [
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
    ] {
        cmd.env_remove(var);
    }
    Some(cmd)
}

/// Asks `codex app-server` for the account's limits (`account/rateLimits/read`),
/// the way codex's own `/status` does, with codex's own login. The server is
/// stopped as soon as it has answered, or when [`CODEX_TIMEOUT`] runs out.
fn fetch_codex_live() -> std::result::Result<Value, String> {
    use std::io::BufRead;
    use std::process::Stdio;
    let mut child = codex_command(&tower_usage::ToolSearch::from_env())
        .ok_or("codex was not found on PATH or in the usual install locations")?
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("codex could not be started ({e})"))?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("codex's pipes could not be opened".into());
    };
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(|l| l.ok())
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let asked = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"herdr-tower","version":env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc":"2.0","method":"initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"account/rateLimits/read"}),
    ]
    .iter()
    .try_for_each(|m| writeln!(stdin, "{m}"))
    .and_then(|()| stdin.flush());
    let deadline = std::time::Instant::now() + CODEX_TIMEOUT;
    let answer = match asked {
        Err(e) => Err(format!("codex app-server could not be asked ({e})")),
        Ok(()) => loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if v.get("id") != Some(&json!(2)) {
                        continue;
                    }
                    break match (v.get("result"), v.pointer("/error/message")) {
                        (Some(r), _) => Ok(r.clone()),
                        (None, Some(m)) => Err(format!("codex answered: {m}")),
                        (None, None) => Err("codex answered without a result".into()),
                    };
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    break Err("codex app-server did not answer in time".into())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Err("codex app-server ended without answering".into())
                }
            }
        },
    };
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    answer
}

fn read_cache(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Written whole or not at all, `0600`: it holds nothing secret, but it is
/// the account's usage.
fn write_cache(path: &Path, cache: &Value) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let name = path
        .file_name()
        .map_or("usage".into(), |n| n.to_string_lossy());
    let tmp: PathBuf = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut f = tower_os::private(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
        )
        .open(&tmp)?;
        f.write_all(cache.to_string().as_bytes())?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::{kept_or_read, KEYCHAIN_TTL};
    use serde_json::{json, Value};
    use std::cell::Cell;
    use std::time::{Duration, Instant};
    use time::OffsetDateTime;

    fn login(token: &str, expires_ms: i64) -> Value {
        json!({ "claudeAiOauth": { "accessToken": token, "expiresAt": expires_ms } })
    }

    #[test]
    fn a_cached_reached_plan_does_not_claim_paid_work_is_impossible() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let body = json!({"rateLimits": {
            "planType": "pro", "rateLimitReachedType": "rate_limit_reached",
            "primary": {"usedPercent": 100, "windowDurationMins": 10080,
                        "resetsAt": now.unix_timestamp() + 86400}
        }});
        super::cached(
            &state.join(super::CODEX_LIVE_CACHE),
            now,
            None,
            false,
            || Ok(body),
        );
        let v = super::codex(&home, &state, now, false);
        assert!(v.limits.reached);
        assert_eq!(v.limits.seven_day.unwrap().used_pct, 100.0);
        assert_eq!(
            v.note.as_deref(),
            Some("plan limit reached — paid credits may allow continued use")
        );
    }

    /// The keychain is asked once per [`KEYCHAIN_TTL`]: again only when the
    /// kept reading is that old, or its token has expired — never to hand
    /// out an expired login.
    #[test]
    fn the_keychain_is_read_once_a_minute_and_at_once_when_the_token_expires() {
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let now_ms = now.unix_timestamp() * 1000;
        let t0 = Instant::now();
        let reads = Cell::new(0);
        let mut kept = None;
        let mut ask = |at: Instant, now: OffsetDateTime, v: Value| {
            kept_or_read(&mut kept, at, now, || {
                reads.set(reads.get() + 1);
                Some(v)
            })
        };

        let a = login("a", now_ms + 3_600_000);
        assert_eq!(ask(t0, now, a.clone()), Some(a.clone()));
        // Within the minute: the kept one, even if the keychain now says b.
        let b = login("b", now_ms + 3_600_000);
        let later = t0 + KEYCHAIN_TTL - Duration::from_millis(1);
        assert_eq!(ask(later, now, b.clone()), Some(a.clone()));
        assert_eq!(reads.get(), 1);
        // A minute on: asked again.
        assert_eq!(ask(t0 + KEYCHAIN_TTL, now, b.clone()), Some(b.clone()));
        assert_eq!(reads.get(), 2);
        // The kept token expires: asked again at once, inside the minute.
        let past_expiry = now + time::Duration::hours(2);
        let c = login("c", now_ms + 10_800_000);
        assert_eq!(ask(t0 + KEYCHAIN_TTL, past_expiry, c.clone()), Some(c));
        assert_eq!(reads.get(), 3);
    }

    fn at_secs(secs: u64) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn reading(pct: f64) -> tower_usage::Limits {
        tower_usage::Limits {
            five_hour: Some(tower_usage::Window {
                used_pct: pct,
                resets_at: None,
            }),
            ..Default::default()
        }
    }

    /// Rollouts without limits are looked at once: what was kept last time is
    /// fed back, and the next scan starts after the newest rollout looked at —
    /// found or not — while the last reading stays on show.
    #[test]
    fn rollouts_without_limits_are_not_read_again() {
        use super::next_rollout_reading;
        use tower_usage::CodexScan;

        // Nothing kept: scan everything; found at 100, looked up to 300.
        let (l, kept) = next_rollout_reading(None, |after| {
            assert_eq!(after, None);
            CodexScan {
                found: Some((at_secs(100), reading(20.0))),
                newest: Some(at_secs(300)),
            }
        });
        assert_eq!(l, reading(20.0));
        let kept = kept.expect("a first reading is kept");

        // Round trip: the next scan starts after 300, not after 100.
        let (l, again) = next_rollout_reading(Some(&kept), |after| {
            assert_eq!(after, Some(at_secs(300)));
            CodexScan::default()
        });
        assert_eq!(l, reading(20.0), "the kept reading stays on show");
        assert_eq!(again, None, "nothing new looked at, nothing written");

        // Newer rollouts, none with limits: only how far is moved on.
        let (l, moved) = next_rollout_reading(Some(&kept), |_| CodexScan {
            found: None,
            newest: Some(at_secs(500)),
        });
        assert_eq!(l, reading(20.0));
        let moved = moved.unwrap();
        let (_, _) = next_rollout_reading(Some(&moved), |after| {
            assert_eq!(after, Some(at_secs(500)));
            CodexScan::default()
        });

        // A file from before `seen` existed (mtime and limits only) still works.
        let old = json!({ "mtime": 100.0, "limits": reading(20.0) });
        let (l, _) = next_rollout_reading(Some(&old), |after| {
            assert_eq!(after, Some(at_secs(100)));
            CodexScan::default()
        });
        assert_eq!(l, reading(20.0));
    }

    /// A damaged kept file is read as nothing kept: never a panic, never a
    /// reading taken from garbage.
    #[test]
    fn a_damaged_rollout_cache_is_nothing_kept() {
        use super::next_rollout_reading;
        use tower_usage::CodexScan;
        for bad in [
            json!(null),
            json!({ "mtime": -5.0, "limits": reading(20.0) }),
            json!({ "mtime": 1e300, "seen": "x" }),
            json!({ "limits": reading(20.0) }),
            json!({ "mtime": 100.0, "limits": "nonsense" }),
        ] {
            let (l, _) = next_rollout_reading(Some(&bad), |_| CodexScan::default());
            assert_eq!(l, tower_usage::Limits::default(), "{bad}");
        }
    }

    /// No login in the keychain is an answer too, kept for the minute: a Mac
    /// without claude does not start a process on every board read.
    #[test]
    fn a_login_expired_when_read_is_kept_the_minute_too() {
        // Claude Code has not run for hours: the keychain holds an expired
        // token, and asking again every board read would find the same.
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let stale = login("old", now.unix_timestamp() * 1000 - 60_000);
        let t0 = Instant::now();
        let reads = Cell::new(0);
        let mut kept = None;
        for i in 0..5u64 {
            let got = kept_or_read(&mut kept, t0 + Duration::from_secs(i * 10), now, || {
                reads.set(reads.get() + 1);
                Some(stale.clone())
            });
            assert_eq!(got, Some(stale.clone()));
        }
        assert_eq!(reads.get(), 1, "once for the minute");
        kept_or_read(&mut kept, t0 + KEYCHAIN_TTL, now, || {
            reads.set(reads.get() + 1);
            Some(stale.clone())
        });
        assert_eq!(reads.get(), 2, "and again after it");
    }

    #[test]
    fn no_login_is_kept_for_the_minute_too() {
        let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let t0 = Instant::now();
        let reads = Cell::new(0);
        let mut kept = None;
        for i in 0..5u64 {
            let got = kept_or_read(&mut kept, t0 + Duration::from_secs(i * 10), now, || {
                reads.set(reads.get() + 1);
                None
            });
            assert_eq!(got, None);
        }
        assert_eq!(reads.get(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn codex_is_found_off_a_launch_agents_path() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tools = tower_usage::ToolSearch::new(Some(home.path().into()), "/usr/bin:/bin");
        // Off PATH, in ~/.local/bin, which is searched before /opt/homebrew/bin
        // and /usr/local/bin, so a codex installed there does not answer instead.
        let codex = bin.join("codex");
        std::fs::write(&codex, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cmd = super::codex_command(&tools).unwrap();
        assert_eq!(cmd.get_program(), codex.as_os_str());
        let path = cmd
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(path.starts_with("/usr/bin:/bin:"), "{path}");
        assert!(path.contains(&*bin.to_string_lossy()), "{path}");
        // Started from the tower's pane, a `codex app-server` daemon must not
        // carry that pane: codex runs every session's hooks with its
        // environment (#45).
        for var in [
            "HERDR_ENV",
            "HERDR_PANE_ID",
            "HERDR_TAB_ID",
            "HERDR_WORKSPACE_ID",
        ] {
            assert!(
                cmd.get_envs().any(|(k, v)| k == var && v.is_none()),
                "{var} is removed"
            );
        }
    }

    use super::*;

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    fn creds(token: &str, expires_ms: i64, tier: &str) -> Value {
        json!({"claudeAiOauth": {
            "accessToken": token, "refreshToken": format!("r-{token}"),
            "expiresAt": expires_ms, "subscriptionType": "max", "rateLimitTier": tier
        }})
    }

    /// After an account switch the file still held the old login, unexpired;
    /// the keychain held the new one. The keychain wins while it is current.
    #[test]
    fn the_keychain_login_wins_over_a_file_left_behind() {
        let now = at(1_000);
        let (future, past) = (10_000_000, 1);
        let new = creds("new", future, "default_claude_max_20x");
        let old = creds("old", future, "default_claude_max_5x");
        let got = pick_credentials(Some(new.clone()), Some(old.clone()), now).unwrap();
        assert_eq!(tower_usage::claude_plan(&got).as_deref(), Some("Max 20x"));
        // No keychain (Linux): the file.
        assert_eq!(
            pick_credentials(None, Some(old.clone()), now),
            Some(old.clone())
        );
        // Both current, the file's issued later: the file is the live login.
        let newer = creds("newer", future + 1, "default_claude_max_20x");
        assert_eq!(
            pick_credentials(Some(old.clone()), Some(newer), now).unwrap()["claudeAiOauth"]
                ["accessToken"],
            "newer"
        );
        // The keychain's token has expired and the file's has not: the file.
        let stale = creds("new", past, "default_claude_max_20x");
        assert_eq!(
            pick_credentials(Some(stale.clone()), Some(old), now).unwrap()["claudeAiOauth"]
                ["accessToken"],
            "old"
        );
        // Neither current: still something to read the plan from.
        assert!(pick_credentials(Some(stale), None, now).is_some());
        assert_eq!(pick_credentials(None, None, now), None);
    }

    /// A kept answer belongs to the login it was fetched for: another login
    /// drops it and asks again at once; the same login keeps it.
    #[test]
    fn a_switched_login_never_shows_the_old_one_s_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let now = at(1_790_000_000);
        let asked = std::cell::Cell::new(0);
        let answer = |v: i64| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                Ok(json!({ "v": v }))
            }
        };
        let got = cached(&path, now, Some("a"), false, answer(1));
        assert_eq!(got.body, Some(json!({"v": 1})));
        // Same login, a minute later: the kept answer, nothing asked.
        let got = cached(
            &path,
            now + time::Duration::minutes(1),
            Some("a"),
            false,
            answer(2),
        );
        assert_eq!((got.body, asked.get()), (Some(json!({"v": 1})), 1));
        // Another login a second later: asked at once, the old answer gone.
        let got = cached(
            &path,
            now + time::Duration::seconds(61),
            Some("b"),
            false,
            answer(3),
        );
        assert_eq!((got.body, asked.get()), (Some(json!({"v": 3})), 2));
        // Another login whose ask fails: no answer at all, not the old one.
        let failing = || Err::<Value, String>("down".into());
        let got = cached(
            &path,
            now + time::Duration::seconds(62),
            Some("c"),
            false,
            failing,
        );
        assert_eq!(got.body, None);
        assert_eq!(got.error.as_deref(), Some("down"));
        // Forced: asked again at once even though the answer is fresh.
        let before = asked.get();
        let got = cached(
            &path,
            now + time::Duration::seconds(63),
            Some("c"),
            true,
            answer(4),
        );
        assert_eq!((got.body, asked.get()), (Some(json!({"v": 4})), before + 1));
        // The fingerprint is short, stable and not the secret.
        assert_eq!(fingerprint("secret"), fingerprint("secret"));
        assert_eq!(fingerprint("secret").len(), 16);
        assert!(!fingerprint("secret").contains("secret"));
    }

    /// The rollout fallback's cache, with a bad `mtime`: negative, past what a
    /// `Duration` holds, not a number. None of them panics the usage thread;
    /// the file is passed over as if it were not there.
    #[test]
    fn a_bad_mtime_in_the_codex_cache_is_passed_over() {
        let limits = tower_usage::Limits {
            five_hour: Some(tower_usage::Window {
                used_pct: 42.0,
                resets_at: None,
            }),
            ..tower_usage::Limits::default()
        };
        for mtime in [
            json!(-1.0),
            json!(1e300),
            json!(f64::MAX),
            json!("soon"),
            json!(null),
        ] {
            let home = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            std::fs::write(
                state.path().join(CODEX_CACHE),
                serde_json::to_vec(&json!({ "mtime": mtime, "limits": limits })).unwrap(),
            )
            .unwrap();
            let v = codex_from_rollouts(home.path(), state.path(), None);
            assert!(v.limits.is_empty(), "{mtime}: {:?}", v.limits);
        }
    }

    /// A good cache comes back as it was written when no rollout is newer.
    #[test]
    fn the_codex_cache_round_trips() {
        let limits = tower_usage::Limits {
            five_hour: Some(tower_usage::Window {
                used_pct: 42.0,
                resets_at: None,
            }),
            ..tower_usage::Limits::default()
        };
        for mtime in [0.0, 1.5, 1_790_000_000.25] {
            let home = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            std::fs::write(
                state.path().join(CODEX_CACHE),
                serde_json::to_vec(&json!({ "mtime": mtime, "limits": limits })).unwrap(),
            )
            .unwrap();
            let v = codex_from_rollouts(home.path(), state.path(), Some("offline".into()));
            assert_eq!(v.limits, limits, "{mtime}");
            assert!(v.note.unwrap().ends_with(": offline"));
        }
    }
}
