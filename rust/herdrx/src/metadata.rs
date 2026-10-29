// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Display values a control tower plants on herdr's own sidebar.
//!
//! This is the one place the two tools complement each other instead of
//! overlapping: herdr knows the agent kind and lifecycle state, but nothing
//! about context usage, which model is answering, or whether a Discord bot is
//! driving the session. The tower computes those already, so it lays them onto
//! the surface the user is looking at anyway.
//!
//! Display, not judgement — nothing here overrides herdr's own `agent_status`.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;

use crate::{Error, Herdr, Result};

/// Who planted a value. herdr keeps every source's tokens apart and remembers
/// at most 32 distinct sources per pane lifetime, so this stays one fixed
/// value: writing under our own name means tokens planted by herdr's own
/// integrations are never disturbed, and clearing ours never touches theirs.
pub const SOURCE: &str = "herdr-tower";

/// Sidebar token names. herdr's `config.toml` refers to them as `"$ctx"`:
///
/// ```toml
/// [ui.sidebar.agents.rows_by_agent]
/// claude = [["state_icon","workspace","tab"], ["agent"], [{ token = "$ctx" }]]
/// ```
pub const TOKEN_CTX: &str = "ctx";
/// The model the agent reported, e.g. `"Opus 5"`.
pub const TOKEN_MODEL: &str = "model";
/// `"⌁"` — the session is driven by a Discord bot.
pub const TOKEN_WIRED: &str = "wired";
/// `"⎇ main"` — the worktree branch.
pub const TOKEN_BRANCH: &str = "branch";

/// Every token this tool plants, for clearing them in one call.
pub const ALL_TOKENS: [&str; 4] = [TOKEN_CTX, TOKEN_MODEL, TOKEN_WIRED, TOKEN_BRANCH];

/// herdr schema: `maxProperties` on the token map.
const MAX_TOKENS: usize = 16;
/// herdr schema: the `ttl_ms` upper bound (86_400_000).
const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// The CLI docs cap title text at 80 characters.
const MAX_TITLE: usize = 80;

/// Display-only data attached to one pane.
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub pane_id: String,
    /// Override the default [`SOURCE`] when a separate ordered contribution is
    /// needed. `None` keeps ordinary sidebar decoration under the fixed source.
    pub source: Option<String>,
    /// Sidebar tokens. An empty value clears that token.
    pub tokens: HashMap<String, String>,
    /// Overrides the pane title. `None` leaves it alone.
    pub title: Option<String>,
    /// Lets herdr expire the values on its own.
    ///
    /// A control tower that dies must not strand stale numbers on screen, and
    /// nothing else will come along to clear them. Two or three times the
    /// refresh interval is the right order of magnitude: long enough that a
    /// skipped refresh does not make the value blink, short enough that a
    /// corpse's numbers do not outlive the session.
    pub ttl: Option<Duration>,
    /// Orders updates, so a late-arriving stale write cannot overwrite a newer
    /// one. Zero leaves the ordering to arrival.
    pub seq: u64,
}

/// Ordered, expiring tokens attached to one workspace.
///
/// Workspace metadata is useful for server-wide coordination because it is
/// independent of panes moving, splitting, or being replaced.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceMetadata {
    pub workspace_id: String,
    pub source: String,
    pub tokens: HashMap<String, String>,
    pub ttl: Option<Duration>,
    pub seq: u64,
}

/// herdr's schema constraint on a token name: 1-32 of `[A-Za-z0-9_-]`.
fn valid_token_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Metadata {
    /// Checks the schema constraints before anything goes over the wire.
    ///
    /// Catching these here points at the cause far better than a server-side
    /// rejection does — the offending token name is right in the message.
    pub fn validate(&self) -> Result<()> {
        if self.pane_id.is_empty() {
            return Err(Error::Invalid("metadata needs a pane_id".into()));
        }
        if self.tokens.len() > MAX_TOKENS {
            return Err(Error::Invalid(format!(
                "at most {MAX_TOKENS} tokens are allowed, got {}",
                self.tokens.len()
            )));
        }
        if let Some(bad) = self.tokens.keys().find(|k| !valid_token_name(k)) {
            return Err(Error::Invalid(format!(
                "token name {bad:?} must be 1-32 characters of [A-Za-z0-9_-]"
            )));
        }
        if let Some(ttl) = self.ttl {
            if ttl > MAX_TTL {
                return Err(Error::Invalid(format!("TTL must be at most {MAX_TTL:?}")));
            }
            // herdr counts in whole milliseconds, so anything shorter would
            // round to zero and read as "no TTL at all" — the opposite of what
            // the caller asked for.
            if ttl < Duration::from_millis(1) {
                return Err(Error::Invalid("TTL must be at least 1ms".into()));
            }
        }
        if self
            .title
            .as_deref()
            .is_some_and(|t| t.chars().count() > MAX_TITLE)
        {
            return Err(Error::Invalid(format!(
                "title must be at most {MAX_TITLE} characters"
            )));
        }
        Ok(())
    }
}

impl Herdr {
    /// Plants display values on a pane, under this tool's own source name.
    pub fn report_metadata(&self, m: &Metadata) -> Result<()> {
        m.validate()?;

        let mut params = serde_json::Map::new();
        params.insert("pane_id".into(), json!(m.pane_id));
        params.insert(
            "source".into(),
            json!(m.source.as_deref().unwrap_or(SOURCE)),
        );
        if !m.tokens.is_empty() {
            // An empty value goes out as null, which is herdr's "delete this
            // token". Sending "" instead would plant an empty string that
            // occupies a sidebar slot forever.
            let tokens: serde_json::Map<String, serde_json::Value> = m
                .tokens
                .iter()
                .map(|(k, v)| {
                    let value = if v.is_empty() {
                        serde_json::Value::Null
                    } else {
                        json!(v)
                    };
                    (k.clone(), value)
                })
                .collect();
            params.insert("tokens".into(), serde_json::Value::Object(tokens));
        }
        if let Some(t) = &m.title {
            params.insert("title".into(), json!(t));
        }
        if let Some(ttl) = m.ttl {
            params.insert("ttl_ms".into(), json!(ttl.as_millis() as u64));
        }
        if m.seq > 0 {
            params.insert("seq".into(), json!(m.seq));
        }
        self.client
            .call::<_, serde_json::Value>("pane.report_metadata", serde_json::Value::Object(params))
            .map(|_| ())
    }

    /// Plants ordered tokens on a workspace under an explicit source.
    pub fn report_workspace_metadata(&self, m: &WorkspaceMetadata) -> Result<()> {
        if m.workspace_id.is_empty() {
            return Err(Error::Invalid(
                "workspace metadata needs a workspace_id".into(),
            ));
        }
        if m.source.is_empty() {
            return Err(Error::Invalid("workspace metadata needs a source".into()));
        }
        if m.tokens.len() > MAX_TOKENS {
            return Err(Error::Invalid(format!(
                "at most {MAX_TOKENS} tokens are allowed, got {}",
                m.tokens.len()
            )));
        }
        if let Some(bad) = m.tokens.keys().find(|key| !valid_token_name(key)) {
            return Err(Error::Invalid(format!(
                "token name {bad:?} must be 1-32 characters of [A-Za-z0-9_-]"
            )));
        }
        if let Some(ttl) = m.ttl {
            if ttl > MAX_TTL {
                return Err(Error::Invalid(format!("TTL must be at most {MAX_TTL:?}")));
            }
            if ttl < Duration::from_millis(1) {
                return Err(Error::Invalid("TTL must be at least 1ms".into()));
            }
        }

        let tokens: serde_json::Map<String, serde_json::Value> = m
            .tokens
            .iter()
            .map(|(key, value)| {
                let value = if value.is_empty() {
                    serde_json::Value::Null
                } else {
                    json!(value)
                };
                (key.clone(), value)
            })
            .collect();
        let mut params = serde_json::Map::from_iter([
            ("workspace_id".into(), json!(m.workspace_id)),
            ("source".into(), json!(m.source)),
            ("tokens".into(), serde_json::Value::Object(tokens)),
        ]);
        if let Some(ttl) = m.ttl {
            params.insert("ttl_ms".into(), json!(ttl.as_millis() as u64));
        }
        if m.seq > 0 {
            params.insert("seq".into(), json!(m.seq));
        }
        self.client
            .call::<_, serde_json::Value>(
                "workspace.report_metadata",
                serde_json::Value::Object(params),
            )
            .map(|_| ())
    }

    /// Removes planted tokens. An empty `names` clears every token this tool
    /// plants, so a control tower shutting down leaves nothing behind even when
    /// it never set a TTL.
    pub fn clear_tokens(&self, pane_id: &str, names: &[&str]) -> Result<()> {
        let names: &[&str] = if names.is_empty() { &ALL_TOKENS } else { names };
        self.report_metadata(&Metadata {
            pane_id: pane_id.to_string(),
            tokens: names
                .iter()
                .map(|n| (n.to_string(), String::new()))
                .collect(),
            ..Metadata::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// herdr's schema rejects the whole request on a bad name, so a typo would
    /// silently drop every other token in the same call.
    #[test]
    fn token_names_follow_the_schema() {
        assert!(valid_token_name("ctx"));
        assert!(valid_token_name("a-b_C9"));
        assert!(!valid_token_name(""));
        assert!(!valid_token_name("has space"));
        assert!(!valid_token_name("dot.ted"));
        assert!(!valid_token_name(&"x".repeat(33)));
    }

    /// A TTL under a millisecond would round to zero and read as "no TTL",
    /// stranding the value on screen — the opposite of what was asked.
    #[test]
    fn a_sub_millisecond_ttl_is_refused() {
        let m = Metadata {
            pane_id: "w2:p1".into(),
            ttl: Some(Duration::from_micros(500)),
            ..Metadata::default()
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn a_ttl_past_the_schema_bound_is_refused() {
        let m = Metadata {
            pane_id: "w2:p1".into(),
            ttl: Some(Duration::from_secs(25 * 60 * 60)),
            ..Metadata::default()
        };
        assert!(m.validate().is_err());
    }

    /// Counted in characters, not bytes: a Korean title would fail far too
    /// early under a byte count.
    #[test]
    fn the_title_cap_counts_characters() {
        let m = Metadata {
            pane_id: "w2:p1".into(),
            title: Some("가".repeat(80)),
            ..Metadata::default()
        };
        assert!(m.validate().is_ok());
    }
}
