// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Wire types for the herdr socket API.

use serde::{Deserialize, Serialize};

/// The agent lifecycle state as judged by herdr.
///
/// It corresponds to a control tower's own state model without being identical,
/// so this crate carries the raw value and leaves the mapping to callers:
///
/// | herdr     | tower          |
/// |-----------|----------------|
/// | `Working` | WORKING        |
/// | `Blocked` | WAITING (herdr recognized an approval or question UI) |
/// | `Done`    | DONE_UNREAD (idle after unseen background work) |
/// | `Idle`    | IDLE           |
/// | `Unknown` | nothing. herdr's docs are explicit that unknown does **not** prove completion, so translating it would invent a fact. |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

/// A sixth status must not blind the tower.
///
/// The derived decoder rejected anything outside the five, which fails the whole
/// `Pane`, which fails `pane.list` — and `tower_scan::sync_backend` reads a failed
/// listing as an unreachable backend and leaves every record untouched. A herdr
/// release adding one lifecycle value would therefore freeze the status output
/// with no error anywhere, and nothing checks the protocol version on this path.
///
/// `Unknown` is the right landing place rather than a guess: this crate's own
/// rule is that `unknown` does not prove completion, so an unrecognized value
/// stops at the same wall instead of inventing a state.
///
/// Hand-written because `#[serde(other)]` is only allowed on tagged enums, not
/// on a plain string one.
impl<'de> Deserialize<'de> for AgentStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "idle" => AgentStatus::Idle,
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::Blocked,
            "done" => AgentStatus::Done,
            _ => AgentStatus::Unknown,
        })
    }
}

/// Selects what `pane.read` returns.
///
/// Careful: the socket spells these with underscores (`recent_unwrapped`) while
/// the CLI flag uses hyphens (`--source recent-unwrapped`). This crate speaks
/// the socket spelling, which serde enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
}

/// The agent's native session identity, which herdr learns from an official
/// integration. `value` is the very claude/codex session ID that a resume flow
/// would otherwise have to scrape out of rollout headers.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentSession {
    pub agent: String,
    pub kind: String,
    pub source: String,
    pub value: String,
}

/// A pane's viewport scroll state.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Scroll {
    #[serde(default)]
    pub offset_from_bottom: u64,
    #[serde(default)]
    pub max_offset_from_bottom: u64,
    #[serde(default)]
    pub viewport_rows: u64,
}

/// One herdr pane, as reported by `pane.list`, `pane.get`, and `pane.updated`.
///
/// Unlike a raw terminal multiplexer's pane it already carries the agent kind,
/// status, and session ID — herdr has finished the classification that a
/// control tower would otherwise have to guess at.
#[derive(Debug, Clone, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub foreground_cwd: String,
    /// `"claude"`, `"codex"`, `"gemini"`, … or absent for a plain shell.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    /// The name a person gave this pane with `herdr agent rename` (`/rename`).
    ///
    /// The wire key is `label`. This was declared as `name` and, being
    /// `#[serde(default)]`, deserialized to `None` on every pane ever listed —
    /// silently, because a missing optional field is indistinguishable from an
    /// unnamed pane. Checked against a live server: `pane.list` sends `label`
    /// and no `name` at all.
    #[serde(default, rename = "label")]
    pub label: Option<String>,
    #[serde(default, rename = "terminal_title_stripped")]
    pub title: String,
    #[serde(default, rename = "terminal_title")]
    pub raw_title: String,
    /// Sidebar tokens planted through `pane.report_metadata`.
    #[serde(default)]
    pub tokens: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub scroll: Scroll,
}

fn unknown_status() -> AgentStatus {
    AgentStatus::Unknown
}

impl Pane {
    /// Whether herdr recognized an agent living in this pane.
    pub fn has_agent(&self) -> bool {
        self.agent.as_deref().is_some_and(|a| !a.is_empty())
    }

    /// The agent session ID to resume with.
    pub fn session_id(&self) -> Option<&str> {
        let s = self.agent_session.as_ref()?;
        (s.kind == "id").then_some(s.value.as_str())
    }
}

/// Decodes the counter in a herdr tab ID (`"w2:t7"` → 7, `"w2:tC"` → 12).
///
/// The suffix is base36, verified against a live server: tab and pane IDs draw
/// from `0-9A-Z` (`w2:tC`, `w2:p3X`, `w2:p1W`). Reading it as decimal would map
/// every lettered tag to 0, and a control tower that groups split panes by
/// session:window would then collide distinct tabs and silently drop agents.
pub fn tab_index(tab_id: &str) -> u32 {
    tab_id
        .split_once(":t")
        .and_then(|(_, tail)| u32::from_str_radix(tail, 36).ok())
        .unwrap_or(0)
}

/// The `ping` response, used to check the server is alive and speaks the
/// expected protocol generation.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerInfo {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub protocol: i64,
    #[serde(default)]
    /// Capability values are deliberately opaque. Herdr 0.9 added the numeric
    /// `endpoint_protocol_generation` beside boolean feature flags, so treating
    /// this as `HashMap<String, bool>` made an otherwise healthy ping fail to
    /// decode and the resident supervisor report every socket disconnected.
    pub capabilities: std::collections::HashMap<String, serde_json::Value>,
}

impl ServerInfo {
    /// Whether the server's protocol generation matches this client.
    pub fn compatible(&self) -> bool {
        self.protocol == super::PROTOCOL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_capabilities_accept_boolean_and_numeric_values() {
        let wire = r#"{
            "type":"pong",
            "version":"0.9.0",
            "protocol":22,
            "capabilities":{
                "live_handoff":true,
                "endpoint_protocol_generation":1
            }
        }"#;
        let info: ServerInfo = serde_json::from_str(wire).unwrap();
        assert_eq!(info.r#type, "pong");
        assert_eq!(info.capabilities["live_handoff"], true);
        assert_eq!(info.capabilities["endpoint_protocol_generation"], 1);
    }

    /// A field whose name does not match the wire is invisible, not loud.
    ///
    /// `label` was declared as `name` and, being `#[serde(default)]`, produced
    /// `None` for every pane the server ever sent — so the `wt` collision guard
    /// that reads it answered "free" for every name, and no test noticed because
    /// nothing compared a decoded pane against bytes a server really wrote.
    /// These are those bytes, trimmed from a live `pane.list`.
    #[test]
    fn a_renamed_pane_carries_the_name_a_person_gave_it() {
        let wire = r#"{
            "agent": "claude",
            "agent_status": "working",
            "cwd": "/Users/alice/herdr-tower",
            "focused": true,
            "label": "herdr-rust",
            "pane_id": "w2:p4S",
            "tab_id": "w2:tC",
            "terminal_title": "◑ herdr-tower",
            "terminal_title_stripped": "herdr-tower",
            "workspace_id": "w2"
        }"#;
        let p: Pane = serde_json::from_str(wire).expect("decode");
        assert_eq!(p.label.as_deref(), Some("herdr-rust"));
        // The title is the agent's own summary, and the stripped form is the one
        // without the status glyph that changes on every turn.
        assert_eq!(p.title, "herdr-tower");
        assert_eq!(p.raw_title, "◑ herdr-tower");
    }

    /// Most panes have no name, and that has to stay distinguishable from the
    /// bug above rather than looking the same.
    #[test]
    fn an_unnamed_pane_says_so() {
        let wire = r#"{
            "agent": "claude",
            "pane_id": "w2:p4C",
            "terminal_title_stripped": "2550 pg_stat_activity"
        }"#;
        let p: Pane = serde_json::from_str(wire).expect("decode");
        assert_eq!(p.label, None);
        assert_eq!(p.title, "2550 pg_stat_activity");
    }
}
