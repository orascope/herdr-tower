// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Every setting `config.json` can hold, as `herdr-tower config` names it.
//!
//! One table, so the command can refuse a key that does not exist (a typo in
//! a JSON file is silently ignored forever), parse a value by its type rather
//! than by guessing, keep secrets off the screen, and say when a change takes
//! effect. A test serializes a fully populated [`Config`] and checks this table
//! against it, so a new field cannot be added without a row here.

use std::time::Duration;

/// How a value is written on the command line and stored in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Stored as a JSON string.
    Text,
    /// `true` / `false` (also `on` / `off`, `yes` / `no`).
    Bool,
    /// A duration like `500ms`, `2s`, `15m`.
    Duration,
    /// A duration, or `off`.
    DurationOrOff,
    /// A share in 1% steps from 1 to 100 (`83`, `83%`), or `off` / `0`.
    PercentOrOff,
    /// A JSON array of strings, or strings separated by commas.
    TextList,
    /// A JSON object of strings.
    TextMap,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Bool => "true|false",
            Kind::Duration => "duration",
            Kind::DurationOrOff => "duration|off",
            Kind::PercentOrOff => "0..100%|off",
            Kind::TextList => "list",
            Kind::TextMap => "object",
        }
    }
}

/// One setting.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    /// Dotted for a nested one: `mcp.public_url`.
    pub name: &'static str,
    pub kind: Kind,
    /// Kept off the screen unless asked for.
    pub secret: bool,
    /// When a change takes effect.
    pub applies: &'static str,
    pub about: &'static str,
}

const NEXT_USE: &str = "on its next use";
const TOWER_START: &str = "the next time the tower opens";
const MCP_RESTART: &str = "when the supervisor restarts (herdr-tower mcp on, or daemon install)";

pub const KEYS: &[Key] = &[
    Key {
        name: "discord_webhook_url",
        kind: Kind::Text,
        secret: true,
        applies: NEXT_USE,
        about: "where the status card is posted",
    },
    Key {
        name: "notify_webhook_url",
        kind: Kind::Text,
        secret: true,
        applies: NEXT_USE,
        about: "where transition one-liners go (falls back to the card's webhook)",
    },
    Key {
        name: "notify_macos",
        kind: Kind::Bool,
        secret: false,
        applies: NEXT_USE,
        about: "desktop notifications (default true)",
    },
    Key {
        name: "notify_discord",
        kind: Kind::Bool,
        secret: false,
        applies: NEXT_USE,
        about: "one-liners to Discord (default false)",
    },
    Key {
        name: "channel_labels",
        kind: Kind::TextMap,
        secret: false,
        applies: NEXT_USE,
        about: "Discord channel ID to a readable label",
    },
    Key {
        name: "preview_interval",
        kind: Kind::Duration,
        secret: false,
        applies: TOWER_START,
        about: "how often the tower's preview refreshes (default 1s, floor 200ms)",
    },
    Key {
        name: "dock_keep",
        kind: Kind::Bool,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "reopen the dock after something closed it (default true)",
    },
    Key {
        name: "usage_tab",
        kind: Kind::Bool,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "a usage tab beside the dock's, besides the tower's `u` (default false)",
    },
    Key {
        name: "id_labels",
        kind: Kind::Bool,
        secret: false,
        applies: "on the supervisor's next pass, and for every pane made after",
        about: "put each herdr pane's ID in front of its name, `[w2:p1] name` (default true)",
    },
    Key {
        name: "parallax",
        kind: Kind::Bool,
        secret: false,
        applies: "on the supervisor's next poll",
        about: "show Parallax Browser's handoffs that wait on you, read-only (default true)",
    },
    Key {
        name: "usage_guard_5h",
        kind: Kind::PercentOrOff,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "hold the tower's sends to claude/codex once only this much of their 5h window is left, and ask working ones to wrap up (default 25%)",
    },
    Key {
        name: "usage_guard_7d",
        kind: Kind::PercentOrOff,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "the same for the 7-day window (default 25%)",
    },
    Key {
        name: "usage_guard_claude_5h",
        kind: Kind::PercentOrOff,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "claude's own 5-hour floor, over usage_guard_5h (unset: the shared one)",
    },
    Key {
        name: "usage_guard_claude_7d",
        kind: Kind::PercentOrOff,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "claude's own 7-day floor, over usage_guard_7d (unset: the shared one)",
    },
    Key {
        name: "usage_guard_codex_5h",
        kind: Kind::PercentOrOff,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "codex's own 5-hour floor, over usage_guard_5h (unset: the shared one)",
    },
    Key {
        name: "usage_guard_codex_7d",
        kind: Kind::PercentOrOff,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "codex's own 7-day floor, over usage_guard_7d (unset: the shared one)",
    },
    Key {
        name: "wait_renotify",
        kind: Kind::DurationOrOff,
        secret: false,
        applies: "on the supervisor's next pass",
        about: "announce an unanswered WAIT once more after this long (default 15m, floor 1m)",
    },
    Key {
        name: "mcp.enabled",
        kind: Kind::Bool,
        secret: false,
        applies: MCP_RESTART,
        about: "serve the remote MCP server from the supervisor (default true; mcp on/off sets it)",
    },
    Key {
        name: "mcp.public_url",
        kind: Kind::Text,
        secret: false,
        applies: MCP_RESTART,
        about: "the https:// origin Tailscale Funnel publishes (default: this machine's Tailscale name)",
    },
    Key {
        name: "mcp.listen",
        kind: Kind::Text,
        secret: false,
        applies: MCP_RESTART,
        about: "host:port on loopback (default 127.0.0.1:8787)",
    },
    Key {
        name: "mcp.redirect_uris",
        kind: Kind::TextList,
        secret: false,
        applies: MCP_RESTART,
        about: "OAuth callbacks a client may register (default claude.ai's)",
    },
    Key {
        name: "mcp.act",
        kind: Kind::Bool,
        secret: false,
        applies: MCP_RESTART,
        about: "offer the act tools at all (default true)",
    },
    Key {
        name: "mcp.notify_acts",
        kind: Kind::Bool,
        secret: false,
        applies: "on the next remote action",
        about: "post each remote action to the notification channel (default true)",
    },
];

pub fn find(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.name == name)
}

/// A value typed on the command line, as it will be stored.
pub fn parse(kind: Kind, raw: &str) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    let raw_t = raw.trim();
    match kind {
        Kind::Text => Ok(Value::String(raw.to_string())),
        Kind::Bool => match raw_t.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Ok(Value::Bool(true)),
            "false" | "off" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("{raw:?} is not true or false")),
        },
        Kind::Duration => match super::parse_duration(raw_t) {
            Some(d) if d > Duration::ZERO => Ok(Value::String(raw_t.to_string())),
            _ => Err(format!("{raw:?} is not a duration like 500ms, 2s or 15m")),
        },
        Kind::DurationOrOff => {
            if raw_t.eq_ignore_ascii_case("off") {
                Ok(Value::String("off".into()))
            } else {
                parse(Kind::Duration, raw_t)
                    .map_err(|_| format!("{raw:?} is not a duration like 15m, or off"))
            }
        }
        Kind::PercentOrOff => {
            if raw_t.eq_ignore_ascii_case("off") {
                return Ok(Value::String("off".into()));
            }
            if raw_t.trim_end_matches('%').trim().parse::<u8>() == Ok(0) {
                return Ok(Value::String("off".into()));
            }
            match super::parse_percent_step(raw_t) {
                Some(p) => Ok(Value::String(p.to_string())),
                None => Err(format!(
                    "{raw:?} must be a whole percentage from 0 to 100, or off (0 means off)"
                )),
            }
        }
        Kind::TextList => {
            if raw_t.starts_with('[') {
                let v: Vec<String> = serde_json::from_str(raw_t)
                    .map_err(|e| format!("not a JSON list of strings: {e}"))?;
                Ok(Value::from(v))
            } else {
                Ok(Value::from(
                    raw_t
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect::<Vec<_>>(),
                ))
            }
        }
        Kind::TextMap => {
            let v: std::collections::BTreeMap<String, String> = serde_json::from_str(raw_t)
                .map_err(|e| format!("not a JSON object of strings: {e}"))?;
            Ok(serde_json::to_value(v).expect("a map of strings serializes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, McpConfig};
    use serde_json::json;

    /// Every field of `Config`, set — a struct literal with no `..Default`, so
    /// a new field fails to compile here until it is given a row in `KEYS`.
    fn full() -> Config {
        Config {
            discord_webhook_url: Some("u".into()),
            notify_webhook_url: Some("u".into()),
            notify_macos: Some(true),
            notify_discord: true,
            channel_labels: [("1".to_string(), "a".to_string())].into(),
            preview_interval: Some("1s".into()),
            dock_keep: Some(true),
            usage_tab: Some(true),
            id_labels: Some(true),
            parallax: Some(true),
            usage_guard_5h: Some("25".into()),
            usage_guard_7d: Some("off".into()),
            usage_guard_claude_5h: Some("20".into()),
            usage_guard_claude_7d: Some("off".into()),
            usage_guard_codex_5h: Some("30".into()),
            usage_guard_codex_7d: Some("35".into()),
            wait_renotify: Some("15m".into()),
            mcp: McpConfig {
                enabled: Some(true),
                listen: Some("127.0.0.1:1".into()),
                public_url: Some("https://x".into()),
                redirect_uris: vec!["https://x/cb".into()],
                act: Some(true),
                notify_acts: Some(true),
            },
        }
    }

    #[test]
    fn the_table_names_every_field_and_nothing_else() {
        let v = serde_json::to_value(full()).unwrap();
        let mut fields: Vec<String> = Vec::new();
        for (k, val) in v.as_object().unwrap() {
            if k == "mcp" {
                for sub in val.as_object().unwrap().keys() {
                    fields.push(format!("mcp.{sub}"));
                }
            } else {
                fields.push(k.clone());
            }
        }
        fields.sort();
        let mut names: Vec<String> = KEYS.iter().map(|k| k.name.to_string()).collect();
        names.sort();
        assert_eq!(fields, names);
    }

    #[test]
    fn values_are_parsed_by_their_kind() {
        assert_eq!(parse(Kind::Bool, "On").unwrap(), json!(true));
        assert_eq!(parse(Kind::Bool, "no").unwrap(), json!(false));
        assert!(parse(Kind::Bool, "maybe").is_err());
        assert_eq!(parse(Kind::Duration, "500ms").unwrap(), json!("500ms"));
        assert!(parse(Kind::Duration, "soon").is_err());
        assert!(parse(Kind::Duration, "0s").is_err());
        assert_eq!(parse(Kind::DurationOrOff, "OFF").unwrap(), json!("off"));
        assert_eq!(parse(Kind::DurationOrOff, "20m").unwrap(), json!("20m"));
        assert_eq!(
            parse(Kind::TextList, "https://a/cb, https://b/cb").unwrap(),
            json!(["https://a/cb", "https://b/cb"])
        );
        assert_eq!(
            parse(Kind::TextList, r#"["https://a/cb"]"#).unwrap(),
            json!(["https://a/cb"])
        );
        assert_eq!(
            parse(Kind::TextMap, r#"{"123":"general"}"#).unwrap(),
            json!({"123":"general"})
        );
        assert!(parse(Kind::TextMap, "123=general").is_err());
        assert_eq!(
            parse(Kind::Text, " true ").unwrap(),
            json!(" true "),
            "text is kept as typed"
        );
    }

    #[test]
    fn only_the_webhooks_are_secret() {
        let secret: Vec<&str> = KEYS.iter().filter(|k| k.secret).map(|k| k.name).collect();
        assert_eq!(secret, ["discord_webhook_url", "notify_webhook_url"]);
    }
}
