// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! User configuration: `~/.config/herdr-tower/config.json`.
//!
//! Every setting is optional, and a missing or unreadable file is not an error
//! — the tower runs on defaults. Configuration that can break the tool by being
//! absent is configuration that should not exist.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod keys;

/// How often the TUI refreshes its pane preview when unset.
pub const DEFAULT_PREVIEW_TICK: Duration = Duration::from_secs(1);

/// [`Config::wait_reminder`] when nothing is set.
pub const DEFAULT_WAIT_RENOTIFY: Duration = Duration::from_secs(15 * 60);

/// The shortest [`Config::wait_reminder`]: a reminder a few seconds after the
/// first notification is only a duplicate of it.
pub const MIN_WAIT_RENOTIFY: Duration = Duration::from_secs(60);

/// A floor on the preview interval. Each tick reads a pane, so a
/// mistyped setting could otherwise spin the control tower against the backend.
pub const MIN_PREVIEW_TICK: Duration = Duration::from_millis(200);

/// The user's settings. Absent fields fall back to defaults, and the accessors
/// below are the only sanctioned way to read the ones with defaults — reading
/// the field directly gets you the raw `Option` and loses the fallback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Where the status card is posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_webhook_url: Option<String>,
    /// Where transition notifications go. Falls back to the card webhook when
    /// unset, so a single webhook still works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_webhook_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_macos: Option<bool>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub notify_discord: bool,
    /// Discord channel ID to a human-readable label.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub channel_labels: HashMap<String, String>,
    /// A duration string such as `"500ms"`. Kept as text so an unparseable
    /// value degrades to the default instead of refusing to load the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_interval: Option<String>,
    /// Whether the resident supervisor puts the dock back after something
    /// closed it. herdr's tab and sidebar menus both carry *Close* and no
    /// client setting removes that entry, so this is what makes the dock's tab
    /// hard to lose by accident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dock_keep: Option<bool>,
    /// Whether the supervisor keeps a usage tab (`herdr-tower usage --watch`)
    /// beside the dock's own, in the dock workspace. Off unless set: the
    /// tower shows the board on `u`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_tab: Option<bool>,
    /// Whether pane names carry their pane IDs (default on). Workspace names
    /// and the first tab always carry the workspace ID, independently of this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_labels: Option<bool>,
    /// Whether the supervisor reads Parallax Browser's open handoffs — its
    /// agents asking for the person — and the tower shows them beside its
    /// own. On unless set: nothing is asked of a machine without Parallax,
    /// and a person with it has one more thing that waits on them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallax: Option<bool>,
    /// Whether the supervisor asks GitHub a few times a day for the newest
    /// release, so the tower's header can offer it. On unless set; the
    /// request carries nothing but the user agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_check: Option<bool>,
    /// How much of claude's and codex's five-hour window has to be left
    /// before the tower holds what it sends them: `"25"` (1% steps) or
    /// `"off"` (`"0"` also means off). Unset is [`DEFAULT_USAGE_GUARD`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_guard_5h: Option<String>,
    /// The same for the seven-day window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_guard_7d: Option<String>,
    /// Per AI, over the shared floors above: `"25"`, `"off"`, or unset to
    /// take the shared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_guard_claude_5h: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_guard_claude_7d: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_guard_codex_5h: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_guard_codex_7d: Option<String>,
    /// How long a session may sit in WAIT before it is announced a second
    /// time (`"15m"`), or `"off"`. See [`Config::wait_reminder`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_renotify: Option<String>,
    /// The remote MCP server (`herdr-tower mcp`). Absent means it has never
    /// been set up; `mcp serve` refuses to start without a public URL.
    #[serde(default, skip_serializing_if = "McpConfig::is_unset")]
    pub mcp: McpConfig,
}

/// Where the remote MCP server listens and who may connect to it.
///
/// Every field is optional and a wrong one degrades to its default through the
/// accessors, like the rest of this file. The server runs inside the resident
/// supervisor, on unless `enabled` is `false` (`herdr-tower mcp off`). Until
/// 2026-09 it was a LaunchAgent of its own and there was no such flag — the
/// service being loaded was the switch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfig {
    /// `false` keeps the supervisor from serving it. See [`McpConfig::enabled`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// `host:port`. Only a loopback address is accepted; see [`McpConfig::listen`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// The HTTPS URL the tunnel publishes (`https://<mac>.<tailnet>.ts.net`).
    /// The OAuth metadata has to name it. Unset, the supervisor asks Tailscale
    /// for this machine's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// The OAuth callbacks a client may register. Empty means claude.ai's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirect_uris: Vec<String>,
    /// `false` takes every tool that writes to an agent off the server,
    /// whatever a client was granted. See [`McpConfig::act_enabled`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act: Option<bool>,
    /// `false` stops posting each remote action to the notification channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_acts: Option<bool>,
}

/// [`McpConfig::listen`] when unset.
pub const DEFAULT_MCP_LISTEN: &str = "127.0.0.1:8787";

/// The callbacks claude.ai and the Claude apps register with.
pub const DEFAULT_MCP_REDIRECT_URIS: [&str; 2] = [
    "https://claude.ai/api/mcp/auth_callback",
    "https://claude.com/api/mcp/auth_callback",
];

impl McpConfig {
    fn is_unset(&self) -> bool {
        *self == McpConfig::default()
    }

    /// The address to bind: the configured one when it parses and is a
    /// loopback address, the default when unset.
    ///
    /// An address that is set but unusable is an error, not a fallback. The
    /// server is reached through a tunnel, and binding `0.0.0.0` would put it
    /// on every network the machine joins; quietly binding the default instead
    /// would hide the mistake while the tunnel points at a port nothing serves.
    pub fn listen(&self) -> Result<std::net::SocketAddr, String> {
        let raw = self
            .listen
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MCP_LISTEN);
        let addr: std::net::SocketAddr = raw
            .parse()
            .map_err(|_| format!("mcp.listen {raw:?} is not host:port"))?;
        if !addr.ip().is_loopback() {
            return Err(format!(
                "mcp.listen {raw:?} is not a loopback address — the tunnel is the only way in"
            ));
        }
        Ok(addr)
    }

    /// The public base URL without a trailing slash, when it is an `https://` URL.
    pub fn public_url(&self) -> Option<String> {
        let raw = self.public_url.as_deref()?.trim().trim_end_matches('/');
        let host = raw.strip_prefix("https://")?;
        (!host.is_empty() && !host.contains(['/', '?', '#', ' '])).then(|| raw.to_string())
    }

    /// Whether the supervisor serves the remote MCP server. On unless turned
    /// off: it listens on loopback only and a client needs a pairing code, and
    /// nothing reaches it from outside until `tailscale funnel` publishes it.
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Whether the server offers tools that write to an agent. On unless
    /// turned off; a client still has to be granted them at approval.
    pub fn act_enabled(&self) -> bool {
        self.act.unwrap_or(true)
    }

    /// Whether each remote action is also posted to the notification channel.
    /// On unless turned off: an action nobody expected should show up where
    /// the WAITs arrive.
    pub fn notify_acts_enabled(&self) -> bool {
        self.notify_acts.unwrap_or(true)
    }

    /// The callbacks a client may register.
    pub fn redirect_uris(&self) -> Vec<String> {
        let set: Vec<String> = self
            .redirect_uris
            .iter()
            .map(|u| u.trim().to_string())
            .filter(|u| u.starts_with("https://"))
            .collect();
        if set.is_empty() {
            DEFAULT_MCP_REDIRECT_URIS.map(String::from).to_vec()
        } else {
            set
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Config {
    /// The preview refresh interval.
    ///
    /// An unparseable or non-positive value falls back to the default, and
    /// anything under the floor is raised to it — a settings mistake should
    /// slow the preview down, never stall or flood the tower.
    pub fn preview_tick(&self) -> Duration {
        let Some(raw) = self.preview_interval.as_deref() else {
            return DEFAULT_PREVIEW_TICK;
        };
        match parse_duration(raw) {
            Some(d) if d >= MIN_PREVIEW_TICK => d,
            Some(_) => MIN_PREVIEW_TICK,
            None => DEFAULT_PREVIEW_TICK,
        }
    }

    /// After how long a WAIT is announced again, or `None` for never.
    ///
    /// A WAIT is announced once, when it starts; miss that one notification
    /// and the agent sits blocked on a question for as long as nobody looks.
    /// One reminder per wait, not one per interval: a second nudge rescues a
    /// missed first one, and a stream of them would be muted. Unset or
    /// unreadable is the default; `"off"` (or `"0"`) turns it off; anything
    /// shorter than a minute is raised to one.
    pub fn wait_reminder(&self) -> Option<Duration> {
        let Some(raw) = self.wait_renotify.as_deref() else {
            return Some(DEFAULT_WAIT_RENOTIFY);
        };
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("off") || raw == "0" || raw.eq_ignore_ascii_case("false") {
            return None;
        }
        match parse_duration(raw) {
            Some(d) => Some(d.max(MIN_WAIT_RENOTIFY)),
            None => Some(DEFAULT_WAIT_RENOTIFY),
        }
    }

    /// Whether desktop notifications fire. On unless turned off — the point of
    /// the tower is to stop watching, which needs something to interrupt with.
    pub fn macos_enabled(&self) -> bool {
        self.notify_macos.unwrap_or(true)
    }

    /// Whether a closed dock is opened again by the resident supervisor.
    pub fn dock_keep_enabled(&self) -> bool {
        self.dock_keep.unwrap_or(true)
    }

    /// The five-hour guard: the share that has to be left (1–100), or `None`
    /// when off. Unset is [`DEFAULT_USAGE_GUARD`]; anything unreadable is
    /// off, since a guard that misreads its setting must not hold the
    /// tower's sends.
    pub fn usage_guard_5h(&self) -> Option<u8> {
        guard_floor(self.usage_guard_5h.as_deref())
    }

    /// The seven-day guard, as [`Config::usage_guard_5h`].
    pub fn usage_guard_7d(&self) -> Option<u8> {
        guard_floor(self.usage_guard_7d.as_deref())
    }

    /// One AI's floor for one window (`"5h"` or `"7d"`): its own setting when
    /// it has one (`off` included), else the shared one.
    pub fn usage_guard_for(&self, vendor: &str, window: &str) -> Option<u8> {
        let own = match (vendor, window) {
            ("claude", "5h") => &self.usage_guard_claude_5h,
            ("claude", "7d") => &self.usage_guard_claude_7d,
            ("codex", "5h") => &self.usage_guard_codex_5h,
            ("codex", "7d") => &self.usage_guard_codex_7d,
            _ => return None,
        };
        match own.as_deref() {
            Some(raw) => parse_percent_step(raw),
            None if window == "5h" => self.usage_guard_5h(),
            None => self.usage_guard_7d(),
        }
    }

    /// Whether Parallax Browser's handoffs are read and shown.
    pub fn parallax_enabled(&self) -> bool {
        self.parallax.unwrap_or(true)
    }

    /// Whether the supervisor looks for a newer release.
    pub fn update_check_enabled(&self) -> bool {
        self.update_check.unwrap_or(true)
    }

    /// Whether the dock workspace carries the usage tab.
    pub fn usage_tab_enabled(&self) -> bool {
        self.usage_tab.unwrap_or(false)
    }

    /// Whether tabs and panes carry their herdr IDs in their names.
    pub fn id_labels_enabled(&self) -> bool {
        self.id_labels.unwrap_or(true)
    }

    /// Where notifications go: the dedicated webhook if there is one, else the
    /// card's. Splitting them is opt-in, so an existing single-webhook setup
    /// keeps working.
    pub fn notify_url(&self) -> Option<&str> {
        self.notify_webhook_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.discord_webhook_url.as_deref())
            .filter(|s| !s.is_empty())
    }
}

/// The config file path, honoring `HERDR_TOWER_CONFIG`.
pub fn path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HERDR_TOWER_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    // An empty `HOME` counts as no home, the same as `tower_state::default_dir`.
    // A service or container that exports `HOME=` would otherwise build the
    // *relative* path `.config/herdr-tower/config.json`, resolved against each
    // process's working directory — so a hook (which inherits the agent's cwd) and
    // the tower would load different webhooks and different notification settings,
    // and a checkout containing that path could supply the hook's configuration.
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("herdr-tower")
            .join("config.json"),
    )
}

/// Loads the configuration, falling back to defaults.
///
/// A missing file is the normal case for a fresh install. A malformed one also
/// yields defaults rather than an error: the tower's job is to show what the
/// agents are doing, and it should keep doing that while somebody fixes their
/// JSON. [`load_checked`] says when that happened.
pub fn load() -> Config {
    load_checked().0
}

/// [`load`], plus one line for each thing in the file that was not used.
///
/// Falling back is right, falling back without a word is not: a stray comma
/// turned off the Discord webhook and every notification with nothing to say
/// why, and a misspelled key is ignored just as quietly. The lines are for
/// the supervisor's log and `doctor`, which are where somebody looks once the
/// notifications stop. Empty when the file is missing or fully understood.
pub fn load_checked() -> (Config, Vec<String>) {
    let Some(p) = path() else {
        return (Config::default(), Vec::new());
    };
    match std::fs::read(&p) {
        Ok(body) => parse_checked(&body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
        Err(e) => (
            Config::default(),
            vec![format!(
                "config.json could not be read ({e}) — every setting is at its default"
            )],
        ),
    }
}

/// The parsing half of [`load_checked`].
fn parse_checked(body: &[u8]) -> (Config, Vec<String>) {
    if let Some(problem) = shape_problem(body) {
        return (Config::default(), vec![problem]);
    }
    let mut unknown = Vec::new();
    let mut de = serde_json::Deserializer::from_slice(body);
    let parsed = serde_ignored::deserialize(&mut de, |path| unknown.push(path.to_string()))
        .and_then(|cfg| de.end().map(|()| cfg));
    match parsed {
        Ok(cfg) => {
            let problems = unknown
                .into_iter()
                .map(|key| format!("config.json: unknown key {key:?} is ignored"))
                .collect();
            (cfg, problems)
        }
        // Where and what kind, not serde's own message: that one quotes the
        // offending value, and a webhook URL pasted under the wrong key would
        // be copied into the supervisor's log.
        Err(e) => {
            let what = match e.classify() {
                serde_json::error::Category::Data => "a setting of the wrong type",
                _ => "broken JSON",
            };
            (
                Config::default(),
                vec![format!(
                    "config.json has {what} at line {} column {} — every setting is at its default until it is fixed",
                    e.line(),
                    e.column()
                )],
            )
        }
    }
}

/// A list where an object of settings belongs, which serde would accept.
///
/// serde reads a struct from a JSON array too, field by field in declaration
/// order, so `["https://…"]` loaded as a config whose Discord webhook was that
/// URL, and `[]` as an empty one, with nothing to say either was odd. The file
/// and its `mcp` section are objects; a list in either place is refused like
/// any other value of the wrong type. The file is judged by its first
/// character, so a broken list is still called a list; `mcp` only once the
/// JSON parses. `None` for anything else — the deserializer reports it, with
/// its position.
fn shape_problem(body: &[u8]) -> Option<String> {
    let first = body.iter().find(|b| !b.is_ascii_whitespace());
    let place = if first == Some(&b'[') {
        "the file itself"
    } else {
        match serde_json::from_slice::<serde_json::Value>(body).ok()? {
            serde_json::Value::Object(map) if map.get("mcp").is_some_and(|v| v.is_array()) => {
                "\"mcp\""
            }
            _ => return None,
        }
    };
    Some(format!(
        "config.json has a list where an object of settings belongs ({place}) — every setting is at its default until it is fixed"
    ))
}

/// The share of a window that has to be left before the usage guard holds
/// the tower's sends, when nothing is set.
pub const DEFAULT_USAGE_GUARD: u8 = 25;

fn guard_floor(raw: Option<&str>) -> Option<u8> {
    match raw {
        None => Some(DEFAULT_USAGE_GUARD),
        Some(r) => parse_percent_step(r),
    }
}

/// A share in 1% steps from 1 to 100: `"83"` or `"83%"`. Zero means off.
/// Anything else, `"off"` included, is `None`.
pub fn parse_percent_step(raw: &str) -> Option<u8> {
    let n: u8 = raw.trim().trim_end_matches('%').trim().parse().ok()?;
    (1..=100).contains(&n).then_some(n)
}

/// Parses the unit-suffixed duration strings the config file uses (`"1s"`,
/// `"500ms"`, `"2m"`). Only the units that appear in practice are accepted;
/// anything else is a mistake worth falling back on rather than guessing at.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = s.find(|c: char| c.is_alphabetic()).map(|i| s.split_at(i))?;
    let n: f64 = num.trim().parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    let secs = match unit {
        "ns" => n / 1e9,
        "us" | "µs" => n / 1e6,
        "ms" => n / 1e3,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return None,
    };
    // `try_from_secs_f64` rather than a hand-written bound. The obvious guard —
    // `secs <= Duration::MAX.as_secs_f64()` — does not hold at the boundary,
    // because `as_secs_f64` rounds *up* to 2^64: a value equal to it passes the
    // comparison and then panics inside `from_secs_f64`. Asking the constructor
    // is the only way to be right about its own range.
    Duration::try_from_secs_f64(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fully_understood_file_has_no_problems() {
        let (cfg, problems) = parse_checked(
            br#"{"notify_discord": true, "channel_labels": {"123": "ops"}, "mcp": {"act": false}}"#,
        );
        assert!(cfg.notify_discord);
        assert!(!cfg.mcp.act_enabled());
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn unknown_keys_are_named_and_the_rest_still_loads() {
        let (cfg, problems) = parse_checked(
            br#"{"notify_discrod": true, "notify_macos": false, "mcp": {"lisen": "x"}}"#,
        );
        assert_eq!(cfg.notify_macos, Some(false));
        assert_eq!(
            problems,
            [
                r#"config.json: unknown key "notify_discrod" is ignored"#,
                r#"config.json: unknown key "mcp.lisen" is ignored"#,
            ]
        );
    }

    #[test]
    fn a_malformed_file_falls_back_and_says_where() {
        let (cfg, problems) = parse_checked(b"{\"notify_discord\": true,}");
        assert!(!cfg.notify_discord);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("line 1"), "{}", problems[0]);
        assert!(problems[0].contains("default"), "{}", problems[0]);
    }

    #[test]
    fn a_wrong_type_falls_back_and_says_so() {
        let (cfg, problems) = parse_checked(br#"{"notify_discord": "yes", "notify_macos": false}"#);
        assert_eq!(cfg.notify_macos, None);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("wrong type"), "{}", problems[0]);
        assert!(
            !problems[0].contains("yes"),
            "the value stays out: {}",
            problems[0]
        );
    }

    #[test]
    fn trailing_content_is_not_a_valid_file() {
        let (_, problems) = parse_checked(br#"{"notify_discord": true} {}"#);
        assert_eq!(problems.len(), 1);
    }

    /// Edges of `parse_checked`: what is not an object, what is empty, and
    /// keys no setting has. Each row: input, how many problems, a piece of
    /// the first one.
    #[test]
    fn parse_checked_edges() {
        let cases: &[(&str, &[u8], usize, &str)] = &[
            ("empty object", b"{}", 0, ""),
            ("empty file", b"", 1, "broken JSON"),
            ("whitespace only", b" \n\t ", 1, "broken JSON"),
            ("null", b"null", 1, "wrong type"),
            ("string", b"\"x\"", 1, "wrong type"),
            ("an empty list", b"[]", 1, "(the file itself)"),
            (
                "a list of values",
                br#"["https://x", "https://y"]"#,
                1,
                "a list where an object",
            ),
            ("a list in whitespace", b" \n[ ] \n", 1, "(the file itself)"),
            ("mcp as a list", br#"{"mcp": [true]}"#, 1, "(\"mcp\")"),
            ("mcp as an empty list", br#"{"mcp": []}"#, 1, "(\"mcp\")"),
            (
                "a list as a setting value",
                br#"{"mcp": {"redirect_uris": []}}"#,
                0,
                "",
            ),
            ("a broken list", b"[1,", 1, "(the file itself)"),
            ("a byte-order mark", b"\xef\xbb\xbf{}", 1, "broken JSON"),
            ("not utf-8", b"{\"a\xff\": 1}", 1, "broken JSON"),
            (
                "an unknown key in Korean",
                "{\"알림\": 1}".as_bytes(),
                1,
                "\"알림\"",
            ),
            (
                "a quote in an unknown key",
                br#"{"a\"b": 1}"#,
                1,
                r#""a\"b""#,
            ),
            (
                "an unknown key holding an object",
                br#"{"x": {"y": [1, 2]}}"#,
                1,
                "\"x\"",
            ),
            (
                "channel labels take any key",
                br#"{"channel_labels": {"anything": "x"}}"#,
                0,
                "",
            ),
            ("mcp set to null", br#"{"mcp": null}"#, 1, "wrong type"),
            (
                "an unknown key under mcp",
                br#"{"mcp": {"x": 1}}"#,
                1,
                "\"mcp.x\"",
            ),
            (
                "the second line",
                b"{\n  \"notify_discord\": true,,\n}",
                1,
                "line 2",
            ),
        ];
        for (name, body, count, piece) in cases {
            let (_, problems) = parse_checked(body);
            assert_eq!(problems.len(), *count, "{name}: {problems:?}");
            if let Some(first) = problems.first() {
                assert!(first.contains(piece), "{name}: {first}");
            }
        }
    }

    /// The settings `parse_checked` reads are the ones plain deserializing
    /// reads — the rule `load()` followed before it — whatever unknown keys
    /// sit beside them, and every unknown key is named exactly once.
    #[test]
    fn parse_checked_agrees_with_plain_deserializing_and_names_each_unknown_key() {
        let known: &[(&str, serde_json::Value)] = &[
            ("notify_discord", serde_json::json!(true)),
            ("notify_macos", serde_json::json!(false)),
            ("preview_interval", serde_json::json!("500ms")),
            ("dock_keep", serde_json::json!(true)),
            ("id_labels", serde_json::json!(false)),
            ("usage_guard_5h", serde_json::json!("off")),
            ("channel_labels", serde_json::json!({"1": "ops"})),
            (
                "mcp",
                serde_json::json!({"act": false, "listen": "127.0.0.1:9"}),
            ),
        ];
        let mut rng = tower_testrng::Rng::new(0x006f_6e66_6967);
        for round in 0..200 {
            let mut obj = serde_json::Map::new();
            for (k, v) in known {
                if rng.one_in(2) {
                    obj.insert(k.to_string(), v.clone());
                }
            }
            let mut unknown = Vec::new();
            for n in 0..rng.below(4) {
                let key = format!("zz_unknown_{round}_{n}");
                obj.insert(key.clone(), serde_json::json!(n));
                unknown.push(key);
            }
            let body = serde_json::to_vec(&serde_json::Value::Object(obj)).unwrap();

            let (cfg, problems) = parse_checked(&body);
            let plain: Config = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                serde_json::to_value(&cfg).unwrap(),
                serde_json::to_value(&plain).unwrap(),
                "round {round}"
            );
            assert_eq!(problems.len(), unknown.len(), "round {round}: {problems:?}");
            for key in &unknown {
                assert_eq!(
                    problems
                        .iter()
                        .filter(|p| p.contains(&format!("{key:?}")))
                        .count(),
                    1,
                    "round {round}: {key} in {problems:?}"
                );
            }
        }
    }

    /// `shape_problem` against an independent oracle — parse the JSON and
    /// ask whether the file, or its `mcp`, is an array — over seeded inputs:
    /// lists behind any whitespace, objects with `mcp` absent, an object, a
    /// list or a scalar, and lists nested where lists are allowed.
    #[test]
    fn a_list_is_refused_exactly_where_the_oracle_finds_one() {
        let ws = [" ", "\n", "\t", "\r\n"];
        let mcp_values = [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({"redirect_uris": ["https://a"]})),
            Some(serde_json::json!([])),
            Some(serde_json::json!([true, "x"])),
            Some(serde_json::json!(null)),
            Some(serde_json::json!(3)),
        ];
        let mut rng = tower_testrng::Rng::new(0x5ba9_e000);
        for round in 0..300 {
            let pad: String = (0..rng.below(4)).map(|_| *rng.pick(&ws).unwrap()).collect();
            let body = if rng.one_in(3) {
                let items: Vec<serde_json::Value> = (0..rng.below(3))
                    .map(|n| serde_json::json!(format!("https://{n}")))
                    .collect();
                format!("{pad}{}", serde_json::Value::Array(items))
            } else {
                let mut obj = serde_json::Map::new();
                obj.insert("channel_labels".into(), serde_json::json!({"1": "a"}));
                if let Some(v) = rng.pick(&mcp_values).unwrap() {
                    obj.insert("mcp".into(), v.clone());
                }
                format!("{pad}{}", serde_json::Value::Object(obj))
            };
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            let oracle =
                value.is_array() || value.get("mcp").is_some_and(serde_json::Value::is_array);

            let found = shape_problem(body.as_bytes());
            assert_eq!(found.is_some(), oracle, "round {round}: {body:?}");
            let (cfg, problems) = parse_checked(body.as_bytes());
            if oracle {
                assert_eq!(problems, [found.unwrap()], "round {round}");
                assert!(cfg.channel_labels.is_empty(), "round {round}: defaults");
            }
        }
    }

    /// Prefixing a broken list with any whitespace still names it a list;
    /// the same bytes after an object's brace are left to the deserializer.
    #[test]
    fn a_broken_list_is_a_list_and_a_broken_object_is_broken_json() {
        for tail in ["", "1,", "\"a", "[[", "}"] {
            for pad in ["", " ", "\n\t "] {
                let list = format!("{pad}[{tail}");
                let (_, problems) = parse_checked(list.as_bytes());
                assert!(problems[0].contains("(the file itself)"), "{list:?}");
                let obj = format!("{pad}{{{tail}");
                assert!(shape_problem(obj.as_bytes()).is_none(), "{obj:?}");
            }
        }
    }

    /// What `config set` writes back is read without a complaint and to the
    /// same settings: serialize → `parse_checked` → serialize is the identity.
    #[test]
    fn a_written_config_reads_back_clean() {
        let bodies: &[&[u8]] = &[
            b"{}",
            br#"{"notify_discord": true, "discord_webhook_url": "https://x", "channel_labels": {"1": "a"}}"#,
            br#"{"wait_renotify": "off", "mcp": {"redirect_uris": ["https://a"], "enabled": false}}"#,
            br#"{"usage_guard_claude_5h": "40", "parallax": false, "dock_keep": true, "usage_tab": true}"#,
        ];
        for body in bodies {
            let (cfg, problems) = parse_checked(body);
            assert!(problems.is_empty(), "{problems:?}");
            let written = serde_json::to_vec(&cfg).unwrap();
            let (again, problems) = parse_checked(&written);
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(
                serde_json::to_value(&again).unwrap(),
                serde_json::to_value(&cfg).unwrap()
            );
        }
    }

    #[test]
    fn defaults_apply_when_unset() {
        let c = Config::default();
        assert_eq!(c.preview_tick(), DEFAULT_PREVIEW_TICK);
        assert!(c.macos_enabled(), "on unless turned off");
        assert!(c.dock_keep_enabled(), "on unless turned off");
        assert!(c.parallax_enabled(), "on unless turned off");
        assert!(c.id_labels_enabled(), "on unless turned off");
        assert_eq!(c.usage_guard_5h(), Some(25), "the guard holds at 25% left");
        assert_eq!(c.usage_guard_7d(), Some(25));
        assert_eq!(c.notify_url(), None);
    }

    /// Explicit `false` has to survive, or a user cannot turn a default off.
    #[test]
    fn explicit_false_is_honored() {
        let c: Config = serde_json::from_str(
            r#"{"notify_macos":false,"dock_keep":false,"parallax":false,"id_labels":false}"#,
        )
        .unwrap();
        assert!(!c.macos_enabled());
        assert!(!c.dock_keep_enabled());
        assert!(!c.parallax_enabled());
        assert!(!c.id_labels_enabled());
    }

    /// `worker_auto_approve` was retired on 2026-10-02 — agents always start
    /// without approval prompts — so a file an older build wrote with it still
    /// has to load, keeping every other setting.
    #[test]
    fn a_retired_worker_auto_approve_still_loads() {
        let c: Config =
            serde_json::from_str(r#"{"worker_auto_approve":false,"dock_keep":false}"#).unwrap();
        assert!(!c.dock_keep_enabled());
    }

    /// A settings mistake must slow the preview, never stall or flood it.
    #[test]
    fn preview_tick_is_clamped_and_forgiving() {
        let with = |v: &str| Config {
            preview_interval: Some(v.into()),
            ..Default::default()
        };
        assert_eq!(with("500ms").preview_tick(), Duration::from_millis(500));
        assert_eq!(with("2s").preview_tick(), Duration::from_secs(2));
        assert_eq!(
            with("1ms").preview_tick(),
            MIN_PREVIEW_TICK,
            "raised to the floor"
        );
        assert_eq!(with("nonsense").preview_tick(), DEFAULT_PREVIEW_TICK);
        assert_eq!(with("0s").preview_tick(), DEFAULT_PREVIEW_TICK);
        assert_eq!(with("-5s").preview_tick(), DEFAULT_PREVIEW_TICK);
    }

    #[test]
    fn wait_reminder_defaults_turns_off_and_floors() {
        let with = |v: &str| Config {
            wait_renotify: Some(v.into()),
            ..Default::default()
        };
        assert_eq!(
            Config::default().wait_reminder(),
            Some(DEFAULT_WAIT_RENOTIFY)
        );
        assert_eq!(with("30m").wait_reminder(), Some(Duration::from_secs(1800)));
        assert_eq!(with("5s").wait_reminder(), Some(MIN_WAIT_RENOTIFY));
        assert_eq!(with("off").wait_reminder(), None);
        assert_eq!(with("0").wait_reminder(), None);
        assert_eq!(with("soon").wait_reminder(), Some(DEFAULT_WAIT_RENOTIFY));
    }

    /// Splitting the notification webhook is opt-in: a single-webhook setup
    /// must keep working untouched.
    #[test]
    fn notify_falls_back_to_the_card_webhook() {
        let card_only = Config {
            discord_webhook_url: Some("https://card".into()),
            ..Default::default()
        };
        assert_eq!(card_only.notify_url(), Some("https://card"));

        let split = Config {
            discord_webhook_url: Some("https://card".into()),
            notify_webhook_url: Some("https://notify".into()),
            ..Default::default()
        };
        assert_eq!(split.notify_url(), Some("https://notify"));

        // An empty string is not a configured webhook.
        let empty = Config {
            discord_webhook_url: Some("https://card".into()),
            notify_webhook_url: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(empty.notify_url(), Some("https://card"));
    }

    /// Malformed JSON yields defaults: the tower keeps reporting while somebody
    /// fixes their file.
    #[test]
    fn broken_json_does_not_stop_the_tower() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"{ not json").unwrap();
        // load() reads the env, so exercise the same fallback directly.
        let parsed: Config = serde_json::from_slice(b"{ not json").unwrap_or_default();
        assert_eq!(parsed.preview_tick(), DEFAULT_PREVIEW_TICK);
        assert!(p.exists());
    }

    #[test]
    fn round_trips_without_inventing_fields() {
        let c: Config = serde_json::from_str(r#"{"dock_keep":false}"#).unwrap();
        assert!(!c.dock_keep_enabled());
        let back = serde_json::to_string(&c).unwrap();
        assert_eq!(
            back, r#"{"dock_keep":false}"#,
            "unset fields stay out of the file"
        );
    }

    #[test]
    fn mcp_listens_on_loopback_only() {
        let at = |listen: &str| McpConfig {
            listen: Some(listen.into()),
            ..McpConfig::default()
        };
        assert_eq!(
            McpConfig::default().listen().unwrap().to_string(),
            DEFAULT_MCP_LISTEN
        );
        assert_eq!(at("[::1]:9000").listen().unwrap().port(), 9000);
        let wide = at("0.0.0.0:8787").listen().unwrap_err();
        assert!(wide.contains("loopback"), "{wide}");
        assert!(at("192.168.0.5:8787").listen().is_err());
        assert!(at("localhost").listen().is_err(), "not host:port");
    }

    #[test]
    fn mcp_public_url_must_be_a_bare_https_origin() {
        let url = |u: &str| {
            McpConfig {
                public_url: Some(u.into()),
                ..McpConfig::default()
            }
            .public_url()
        };
        assert_eq!(
            url("https://mac.tail1.ts.net/").as_deref(),
            Some("https://mac.tail1.ts.net")
        );
        assert_eq!(
            url("http://mac.tail1.ts.net"),
            None,
            "TLS is the tunnel's job"
        );
        assert_eq!(
            url("https://mac.tail1.ts.net/mcp"),
            None,
            "an origin, not a path"
        );
        assert_eq!(url("https://"), None);
        assert_eq!(McpConfig::default().public_url(), None);
    }

    #[test]
    fn mcp_redirect_uris_default_to_claude_and_drop_plain_http() {
        assert_eq!(
            McpConfig::default().redirect_uris(),
            DEFAULT_MCP_REDIRECT_URIS.map(String::from).to_vec()
        );
        let c = McpConfig {
            redirect_uris: vec![
                "http://evil.example/cb".into(),
                " https://chatgpt.com/connector_platform_oauth_redirect ".into(),
            ],
            ..McpConfig::default()
        };
        assert_eq!(
            c.redirect_uris(),
            vec!["https://chatgpt.com/connector_platform_oauth_redirect".to_string()]
        );
    }

    #[test]
    fn mcp_act_and_its_notifications_are_on_unless_turned_off() {
        assert!(McpConfig::default().act_enabled());
        assert!(McpConfig::default().notify_acts_enabled());
        let c: Config =
            serde_json::from_str(r#"{"mcp":{"act":false,"notify_acts":false}}"#).unwrap();
        assert!(!c.mcp.act_enabled());
        assert!(!c.mcp.notify_acts_enabled());
    }

    #[test]
    fn an_unset_mcp_section_stays_out_of_the_file() {
        let back = serde_json::to_string(&Config::default()).unwrap();
        assert!(!back.contains("mcp"), "{back}");
    }
}

#[cfg(test)]
mod tolerance_tests {
    use super::*;

    /// A duration the accessor promises to fall back on must not panic instead.
    ///
    /// `Duration::from_secs_f64` panics on a negative, infinite, NaN or
    /// out-of-range value. `"100000000000000000000s"` parses as a perfectly
    /// finite `f64` and still overflows a `Duration`, so the guard has to be on
    /// the computed seconds rather than on the text.
    #[test]
    fn an_unrepresentable_interval_falls_back_rather_than_panicking() {
        for bad in [
            // The boundary value: `secs` here equals `Duration::MAX.as_secs_f64()`
            // exactly, because that method rounds up to 2^64. A hand-written
            // `secs <= Duration::MAX.as_secs_f64()` guard passes it and then
            // panics inside the constructor.
            "18446744073709551616s",
            "100000000000000000000s",
            "1e308h",
            "99999999999999999999999999h",
            "nonsense",
            "0s",
            "-5s",
            "",
        ] {
            let c: Config =
                serde_json::from_str(&format!(r#"{{"preview_interval":"{bad}"}}"#)).unwrap();
            assert_eq!(
                c.preview_tick(),
                DEFAULT_PREVIEW_TICK,
                "{bad:?} should fall back to the default"
            );
        }
        // The floor still applies to a value that is merely too eager.
        let fast: Config = serde_json::from_str(r#"{"preview_interval":"1ms"}"#).unwrap();
        assert_eq!(fast.preview_tick(), MIN_PREVIEW_TICK);
    }
}

#[cfg(test)]
mod percent_step_tests {
    use super::parse_percent_step;

    #[test]
    fn an_ai_s_own_floor_wins_over_the_shared_one() {
        let c: super::Config = serde_json::from_str(
            r#"{"usage_guard_5h":"40","usage_guard_codex_5h":"10","usage_guard_claude_7d":"off"}"#,
        )
        .unwrap();
        assert_eq!(c.usage_guard_for("codex", "5h"), Some(10), "its own");
        assert_eq!(
            c.usage_guard_for("claude", "5h"),
            Some(40),
            "the shared one"
        );
        assert_eq!(c.usage_guard_for("claude", "7d"), None, "its own off");
        assert_eq!(c.usage_guard_for("codex", "7d"), Some(25), "the default");
        assert_eq!(c.usage_guard_for("gemini", "5h"), None, "no such floor");
    }

    #[test]
    fn off_turns_the_guard_off_and_nonsense_does_too() {
        let c: super::Config =
            serde_json::from_str(r#"{"usage_guard_5h":"off","usage_guard_7d":"oops"}"#).unwrap();
        assert_eq!(c.usage_guard_5h(), None);
        assert_eq!(c.usage_guard_7d(), None, "unreadable: off, never a guess");
    }

    #[test]
    fn a_guard_share_is_one_to_one_hundred_in_steps_of_one() {
        for n in 1..=100 {
            assert_eq!(parse_percent_step(&n.to_string()), Some(n as u8));
            assert_eq!(parse_percent_step(&format!("{n}%")), Some(n as u8));
        }
        for bad in ["0", "0%", "101", "off", "", "-5", "85.0", "x"] {
            assert_eq!(parse_percent_step(bad), None, "{bad}");
        }
    }

    #[test]
    fn zero_disables_shared_and_vendor_guards() {
        let c: super::Config = serde_json::from_str(
            r#"{"usage_guard_5h":"0","usage_guard_7d":"0%","usage_guard_claude_5h":"0","usage_guard_codex_7d":"0%"}"#,
        ).unwrap();
        for name in ["claude", "codex"] {
            for window in ["5h", "7d"] {
                assert_eq!(c.usage_guard_for(name, window), None);
            }
        }
    }
}
