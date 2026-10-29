// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The four operations a control tower needs, plus the pieces around them.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;

use crate::{deadline_for, types::*, Client, Error, Result};

/// The herdr backend.
#[derive(Debug, Clone, Default)]
pub struct Herdr {
    pub client: Client,
}

#[derive(serde::Deserialize)]
struct PaneList {
    #[serde(default)]
    panes: Vec<Pane>,
}

/// The live pane facts the tower needs from one atomic Herdr snapshot.
///
/// `tab_roots` contains the pane occupying the first (top-left) leaf of every
/// tab layout. Herdr pane IDs are reusable coordinates, not creation counters,
/// so the layout is the only reliable answer to "which pane opens this tab?".
#[derive(Debug, Clone, Default)]
pub struct PaneSnapshot {
    pub panes: Vec<Pane>,
    pub workspace_labels: HashMap<String, String>,
    /// Workspace IDs in sidebar order, including workspaces without agents.
    pub workspace_order: Vec<String>,
    /// Exact sidebar labels, including empty semantic names and display prefixes.
    pub workspace_display_labels: HashMap<String, String>,
    /// Tab ID (`"w2:tY"`) to its label. herdr always gives a tab one — its
    /// number, until somebody names it.
    pub tab_labels: HashMap<String, String>,
    pub tab_roots: HashSet<String>,
}

#[derive(serde::Deserialize)]
struct SnapshotResult {
    snapshot: Snapshot,
}

#[derive(serde::Deserialize, Default)]
struct Snapshot {
    #[serde(default)]
    panes: Vec<Pane>,
    #[serde(default)]
    workspaces: Vec<Workspace>,
    #[serde(default)]
    tabs: Vec<crate::Tab>,
    #[serde(default)]
    layouts: Vec<SnapshotLayout>,
}

#[derive(serde::Deserialize, Default)]
struct SnapshotLayout {
    #[serde(default)]
    panes: Vec<SnapshotLayoutPane>,
}

#[derive(serde::Deserialize)]
struct SnapshotLayoutPane {
    pane_id: String,
    rect: SnapshotRect,
}

#[derive(serde::Deserialize)]
struct SnapshotRect {
    x: u16,
    y: u16,
}

#[derive(serde::Deserialize)]
struct PaneReadResult {
    read: ReadBody,
}

#[derive(serde::Deserialize)]
struct ReadBody {
    #[serde(default)]
    text: String,
}

/// How long `prompt` should wait. `None` sends and returns immediately.
#[derive(Debug, Clone, Default)]
pub struct PromptOptions {
    /// Empty means herdr's default: the first settled idle, done, or blocked.
    pub until: Vec<AgentStatus>,
    pub timeout: Option<Duration>,
}

impl Herdr {
    /// A backend talking to a specific socket.
    pub fn at(socket: impl Into<std::path::PathBuf>) -> Self {
        Herdr {
            client: Client::at(socket),
        }
    }

    /// Checks the server is alive and reports its version and protocol.
    pub fn ping(&self) -> Result<ServerInfo> {
        self.client.call("ping", json!({}))
    }

    /// Every pane on the server.
    ///
    /// Where a bare multiplexer hands back panes and leaves classification to
    /// the caller, herdr also reports `agent`, `agent_status`, and
    /// `agent_session`.
    pub fn list_panes(&self) -> Result<Vec<Pane>> {
        let res: PaneList = self.client.call("pane.list", json!({}))?;
        Ok(res.panes)
    }

    /// Panes, workspace and tab labels, and tab roots from one server
    /// generation.
    ///
    /// Only rightward and downward splits exist, so the first pane of a tab is
    /// the leaf with the smallest `(y, x)` coordinate. Reading the rectangles
    /// rather than trusting array order also keeps this true if the wire listing
    /// changes its iteration order.
    pub fn pane_snapshot(&self) -> Result<PaneSnapshot> {
        let res: SnapshotResult = self.client.call("session.snapshot", json!({}))?;
        let workspace_order = res
            .snapshot
            .workspaces
            .iter()
            .map(|w| w.workspace_id.clone())
            .collect();
        let workspace_display_labels = res
            .snapshot
            .workspaces
            .iter()
            .map(|w| (w.workspace_id.clone(), w.display_label.clone()))
            .collect();
        let workspace_labels = res
            .snapshot
            .workspaces
            .iter()
            .filter(|w| !w.label.is_empty())
            .map(|w| (w.workspace_id.clone(), w.label.clone()))
            .collect();
        let tab_labels = res
            .snapshot
            .tabs
            .iter()
            .filter(|t| !t.label.is_empty())
            .map(|t| (t.tab_id.clone(), t.label.clone()))
            .collect();
        let tab_roots = res
            .snapshot
            .layouts
            .iter()
            .filter_map(|layout| {
                layout
                    .panes
                    .iter()
                    .min_by_key(|pane| (pane.rect.y, pane.rect.x, pane.pane_id.as_str()))
                    .map(|pane| pane.pane_id.clone())
            })
            .collect();
        Ok(PaneSnapshot {
            panes: res.snapshot.panes,
            workspace_labels,
            workspace_order,
            workspace_display_labels,
            tab_labels,
            tab_roots,
        })
    }

    /// One pane record.
    pub fn pane(&self, pane_id: &str) -> Result<Pane> {
        #[derive(serde::Deserialize)]
        struct R {
            pane: Pane,
        }
        let res: R = self
            .client
            .call("pane.get", json!({ "pane_id": pane_id }))?;
        Ok(res.pane)
    }

    /// Pane content from the chosen source, limited to `lines` rows.
    pub fn read(&self, pane_id: &str, source: ReadSource, lines: u32) -> Result<String> {
        let res: PaneReadResult = self.client.call(
            "pane.read",
            json!({ "pane_id": pane_id, "source": source, "lines": lines, "format": "text" }),
        )?;
        Ok(res.read.text)
    }

    /// The last `lines` rows of a pane, for display only.
    ///
    /// Reads the visible source because agent TUIs run on the alternate screen,
    /// and rows that scroll off there never enter herdr's host scrollback — the
    /// recent sources return the same thing for an agent pane anyway (measured:
    /// all four returned an identical 654 bytes), so the unambiguous one wins.
    ///
    /// herdr already returns exactly that many rows, so only trailing blank
    /// lines need tidying.
    pub fn capture_pane(&self, pane_id: &str, lines: u32) -> Result<String> {
        let text = self.read(pane_id, ReadSource::Visible, lines)?;
        Ok(trim_trailing_blank(&text))
    }

    /// Moves the user's view to that pane — the whole way there.
    ///
    /// `target` is a pane ID (`"w2:p4S"`) or a herdr agent name (`"codex1"`).
    /// A pane with no agent is rejected by `agent.focus`, so those fall through
    /// to `pane.focus`. The fallback covers "no agent" only — swallowing other
    /// errors would make failures silent.
    ///
    /// The workspace and tab go first, and they are the difference between this
    /// working and appearing to. From herdr 0.9 each client holds its own view —
    /// "Multiple clients can now view different workspaces and tabs
    /// independently" — so focusing a pane moves the active pane *within the tab
    /// that client is already showing* and does nothing at all when the target
    /// lives in another tab. The server's `focused` flag moves either way, which
    /// is what made this look like it worked: measured on 0.9.0, a cross-tab
    /// `agent.focus` flipped the flag and left the screen where it was, while
    /// the same call inside the current tab moved the selection. Pressing enter
    /// in the tower did nothing for exactly this reason.
    ///
    /// The lookup is best effort and the moves are not. A target that is an
    /// agent name rather than a pane ID cannot be looked up as a pane, and that
    /// is the ordinary case rather than a failure — the jump proceeds exactly as
    /// it did before this step existed. But once a tab *is* known, failing to
    /// reach it is the whole error: the client stays where it was, and a caller
    /// told the focus succeeded goes on to mark the completion read and quit
    /// (see `tower_ui::App::attached`), so the person loses the unread marker for a
    /// pane they never saw. A herdr too old for these calls is the one thing
    /// swallowed, since half a jump still beats none.
    pub fn focus(&self, target: &str) -> Result<()> {
        self.reveal_container(target)?;
        match self
            .client
            .call::<_, serde_json::Value>("agent.focus", json!({ "target": target }))
        {
            Ok(_) => Ok(()),
            Err(e) if e.is_code("agent_not_found") || e.is_code("not_found") => {
                self.focus_pane(target)
            }
            Err(e) => Err(e),
        }
    }

    /// Brings the workspace and tab holding `pane_id` into view.
    ///
    /// A target that is *not a pane* is not an error — see [`Herdr::focus`] —
    /// but that is the only thing the lookup may excuse. A socket that dropped
    /// or a malformed answer says nothing about where the target lives, and
    /// reading it as "an agent name, carry on" hides the same failure the tab
    /// step was added to stop hiding: the client stays on its tab while the
    /// caller is told the jump worked. Measured against a live server, the
    /// answer for a name is `pane_not_found`.
    fn reveal_container(&self, pane_id: &str) -> Result<()> {
        let p = match self.pane(pane_id) {
            Ok(p) => p,
            Err(e) if e.is_code("pane_not_found") || e.is_code("not_found") => return Ok(()),
            Err(e) => return Err(e),
        };
        if !p.workspace_id.is_empty() {
            self.client.call::<_, serde_json::Value>(
                "workspace.focus",
                json!({ "workspace_id": p.workspace_id }),
            )?;
        }
        if !p.tab_id.is_empty() {
            self.client
                .call::<_, serde_json::Value>("tab.focus", json!({ "tab_id": p.tab_id }))?;
        }
        Ok(())
    }

    /// Grabs a pane regardless of whether an agent lives in it.
    pub fn focus_pane(&self, pane_id: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>("pane.focus", json!({ "pane_id": pane_id }))
            .map(|_| ())
    }

    /// Submits an instruction into an agent's input.
    ///
    /// herdr absorbs work a raw send-keys path has to handle by hand: bracketed
    /// paste honored against the pane's live state, the delay before Enter, and
    /// — most importantly — an agent sitting at an approval dialog is not
    /// written to at all, but rejected with `agent_blocked`. On a raw path that
    /// is invisible, so instructions end up typed into the approval prompt.
    pub fn prompt(&self, target: &str, text: &str, opts: Option<&PromptOptions>) -> Result<()> {
        let Some(opts) = opts else {
            return self
                .client
                .call::<_, serde_json::Value>(
                    "agent.prompt",
                    json!({ "target": target, "text": text }),
                )
                .map(|_| ());
        };

        let mut wait = serde_json::Map::new();
        if !opts.until.is_empty() {
            wait.insert("until".into(), serde_json::to_value(&opts.until)?);
        }
        if let Some(t) = opts.timeout {
            wait.insert("timeout_ms".into(), json!(t.as_millis() as u64));
        }
        // A waited prompt blocks server-side until the agent settles, so the
        // socket deadline has to outlast the wait that was requested.
        self.client
            .call_within::<_, serde_json::Value>(
                deadline_for(opts.timeout.unwrap_or_default()),
                "agent.prompt",
                json!({ "target": target, "text": text, "wait": wait }),
            )
            .map(|_| ())
    }
}

fn trim_trailing_blank(text: &str) -> String {
    let mut lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Serializing `AgentStatus` in a request body needs the same lowercase form.
const _: fn() = || {
    fn assert_serialize<T: Serialize>() {}
    assert_serialize::<AgentStatus>();
};

#[allow(dead_code)]
fn _unused(_: Error) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_only_trailing_blanks() {
        assert_eq!(trim_trailing_blank("a\nb\n\n   \n"), "a\nb");
        assert_eq!(trim_trailing_blank("a\n\nb\n"), "a\n\nb");
        assert_eq!(trim_trailing_blank(""), "");
    }

    /// The socket spells read sources with underscores; the CLI uses hyphens.
    #[test]
    fn read_source_uses_socket_spelling() {
        let v = serde_json::to_string(&ReadSource::RecentUnwrapped).unwrap();
        assert_eq!(v, "\"recent_unwrapped\"");
    }

    /// unknown must not become a completion state.
    #[test]
    fn agent_status_round_trips() {
        for (s, want) in [
            (AgentStatus::Working, "\"working\""),
            (AgentStatus::Blocked, "\"blocked\""),
            (AgentStatus::Done, "\"done\""),
            (AgentStatus::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&s).unwrap(), want);
        }
    }

    /// A herdr tab counter is base36; decimal parsing would collide tabs.
    #[test]
    fn tab_index_decodes_base36() {
        assert_eq!(tab_index("w2:t7"), 7);
        assert_eq!(tab_index("w2:tC"), 12); // would be 0 under decimal parsing
        assert_eq!(tab_index("w2:tZ"), 35);
        assert_eq!(tab_index("nonsense"), 0);
    }

    #[test]
    fn socket_path_resolution_order() {
        let explicit = |k: &str| match k {
            "HERDR_SOCKET_PATH" => Some("/tmp/x.sock".into()),
            "HERDR_SESSION" => Some("lab".into()),
            _ => None,
        };
        assert_eq!(
            crate::socket_path(explicit, "/home/u"),
            std::path::PathBuf::from("/tmp/x.sock")
        );

        // herdr's folder: `~/.config/herdr`, or `~\AppData\Roaming\herdr`
        // on Windows with no `APPDATA` set.
        let herdr = if cfg!(windows) {
            std::path::Path::new("/home/u")
                .join("AppData")
                .join("Roaming")
                .join("herdr")
        } else {
            std::path::PathBuf::from("/home/u/.config/herdr")
        };
        let named = |k: &str| (k == "HERDR_SESSION").then(|| "lab".to_string());
        assert_eq!(
            crate::socket_path(named, "/home/u"),
            herdr.join("sessions").join("lab").join("herdr.sock")
        );

        let none = |_: &str| None;
        assert_eq!(
            crate::socket_path(none, "/home/u"),
            herdr.join("herdr.sock")
        );

        // With APPDATA set, Windows takes it, as herdr does.
        if cfg!(windows) {
            let appdata = |k: &str| (k == "APPDATA").then(|| r"D:\roam".to_string());
            assert_eq!(
                crate::socket_path(appdata, "/home/u"),
                std::path::Path::new(r"D:\roam\herdr\herdr.sock")
            );
        }
    }
}

/// One herdr workspace — the place-level concept a control tower shows as a
/// session name.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(from = "WorkspaceWire")]
pub struct Workspace {
    pub workspace_id: String,
    pub number: u32,
    /// Semantic name, without the sidebar's workspace display prefix.
    pub label: String,
    /// Exact server label, for display reconciliation only.
    pub display_label: String,
    pub focused: bool,
    pub pane_count: u32,
    pub tab_count: u32,
    pub active_tab_id: Option<String>,
    pub tokens: std::collections::HashMap<String, String>,
}

#[derive(serde::Deserialize)]
struct WorkspaceWire {
    workspace_id: String,
    #[serde(default)]
    number: u32,
    #[serde(default)]
    label: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    pane_count: u32,
    #[serde(default)]
    tab_count: u32,
    // Herdr returns null when there is no active tab.
    #[serde(default)]
    active_tab_id: Option<String>,
    #[serde(default)]
    tokens: std::collections::HashMap<String, String>,
}

impl From<WorkspaceWire> for Workspace {
    fn from(w: WorkspaceWire) -> Self {
        Self {
            workspace_id: w.workspace_id,
            number: w.number,
            label: crate::strip_workspace_prefix(&w.label).to_string(),
            display_label: w.label,
            focused: w.focused,
            pane_count: w.pane_count,
            tab_count: w.tab_count,
            active_tab_id: w.active_tab_id,
            tokens: w.tokens,
        }
    }
}

#[derive(serde::Deserialize)]
struct WorkspaceList {
    #[serde(default)]
    workspaces: Vec<Workspace>,
}

impl Herdr {
    /// The workspaces.
    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        let res: WorkspaceList = self.client.call("workspace.list", json!({}))?;
        Ok(res.workspaces)
    }

    /// Workspace ID to human-readable label.
    ///
    /// `pane.list` reports only the ID (`"w2"`), so putting a name on screen
    /// costs this one extra call.
    pub fn workspace_labels(&self) -> Result<std::collections::HashMap<String, String>> {
        Ok(self
            .workspaces()?
            .into_iter()
            .filter(|w| !w.label.is_empty())
            .map(|w| (w.workspace_id, w.label))
            .collect())
    }

    /// Brings a workspace into view, showing whichever of its tabs is active.
    pub fn focus_workspace(&self, workspace_id: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "workspace.focus",
                json!({ "workspace_id": workspace_id }),
            )
            .map(|_| ())
    }

    /// Moves a workspace to `insert_index` in the sidebar order, 0 being the top.
    pub fn move_workspace(&self, workspace_id: &str, insert_index: usize) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "workspace.move",
                json!({ "workspace_id": workspace_id, "insert_index": insert_index }),
            )
            .map(|_| ())
    }

    /// The workspace the user is looking at.
    pub fn focused_workspace(&self) -> Result<Option<Workspace>> {
        Ok(self.workspaces()?.into_iter().find(|w| w.focused))
    }
}
