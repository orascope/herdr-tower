// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Making places for agents to live, and driving what lives in them.
//!
//! A control tower's vocabulary maps onto herdr as decided on 2026-08-27: a
//! workspace takes the seat of a session, a tab that of a window. A herdr tab
//! is already a shell pane, so a command is typed into its shell rather than
//! made the tab's process — and the tab survives a command that dies
//! instantly, error message and all.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{deadline_for, AgentStatus, Error, Herdr, Pane, Result, Workspace};

/// One herdr tab — the seat a control tower gives to a "window".
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "TabWire")]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    /// Semantic name, without the workspace display prefix. Keepers and
    /// in-flight creation requests compare this name.
    pub label: String,
    /// Exact server label, for display reconciliation only.
    pub display_label: String,
    pub focused: bool,
    pub pane_count: u32,
    /// Stable public tab number; it does not change when tabs are reordered.
    pub number: u32,
}

#[derive(Deserialize)]
struct TabWire {
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    pane_count: u32,
    #[serde(default)]
    number: u32,
}

impl From<TabWire> for Tab {
    fn from(w: TabWire) -> Self {
        Self {
            tab_id: w.tab_id,
            workspace_id: w.workspace_id,
            label: strip_workspace_prefix(&w.label).to_string(),
            display_label: w.label,
            focused: w.focused,
            pane_count: w.pane_count,
            number: w.number,
        }
    }
}

/// Herdr's uppercase base-32 public-ID alphabet, in counting order.
const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";

/// Herdr's workspace handle uses its uppercase base-32 public-ID alphabet.
/// Ordinary brackets such as `[wip]` are a person's name, not a coordinate.
pub fn is_workspace_id(id: &str) -> bool {
    id.strip_prefix('w').is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| PUBLIC_ID_ALPHABET.contains(&b))
    })
}

/// Where a pane comes in its workspace's count: `w2:pJY` before `w2:pJZ`
/// before `w2:pJ0` before `w2:pK2`.
///
/// herdr counts pane IDs in [`PUBLIC_ID_ALPHABET`] with no zero digit — `1`
/// is the first digit and `0` the last, and `p1B` is a two-digit number,
/// not a padded one — so a shorter ID is always the smaller, and IDs of one
/// length compare digit by digit in alphabet order. Checked against herdr
/// 0.9.5's `pane list`, which lists a workspace's panes in that order.
///
/// A sort key, not a number: anything that is not a herdr pane coordinate
/// sorts after every one that is, by its text.
pub fn pane_order(pane_id: &str) -> (bool, usize, Vec<u8>) {
    let digits = pane_id
        .split_once(":p")
        .map(|(_, n)| n)
        .filter(|n| !n.is_empty())
        .and_then(|n| {
            n.bytes()
                .map(|b| {
                    PUBLIC_ID_ALPHABET
                        .iter()
                        .position(|&a| a == b)
                        .map(|i| i as u8)
                })
                .collect::<Option<Vec<u8>>>()
        });
    match digits {
        Some(d) => (false, d.len(), d),
        None => (true, 0, pane_id.as_bytes().to_vec()),
    }
}

/// Remove compact workspace display prefixes while preserving ordinary brackets.
/// Also accept the single separator space written by earlier versions.
pub fn strip_workspace_prefix(mut label: &str) -> &str {
    loop {
        let Some(rest) = label.strip_prefix('[') else {
            return label;
        };
        if rest.strip_suffix(']').is_some_and(is_workspace_id) {
            return "";
        }
        match rest.split_once(']') {
            Some((id, name)) if is_workspace_id(id) => {
                label = name.strip_prefix(' ').unwrap_or(name);
            }
            _ => return label,
        }
    }
}

/// Plain display name, including removal of legacy tab/pane prefixes.
pub fn plain_display_label(mut label: &str) -> &str {
    loop {
        let plain = strip_id_prefix(strip_workspace_prefix(label));
        if plain == label {
            return label;
        }
        label = plain;
    }
}

/// Workspace coordinate directly before a sidebar or first-tab name.
/// ASCII spaces at that boundary are normalized away so the result is stable.
pub fn workspace_id_label(id: &str, label: &str) -> String {
    let name = plain_display_label(label);
    if !is_workspace_id(id) {
        return name.to_string();
    }
    let name = name.trim_start_matches(' ');
    if name.is_empty() {
        format!("[{id}]")
    } else {
        format!("[{id}]{name}")
    }
}

/// The name a tab or pane wears: its herdr ID, then its name — a tab
/// `[w2:t3] hero-dark-codex`, its pane `[w2:p1] hero-dark-codex`, and a pane
/// with no name of its own just `[w2:p1]`.
///
/// The ID is what every tab- or pane-taking command wants and what the tower's
/// PANE column shows, so putting it in the tab bar and on the pane border
/// answers "which one is this" without opening anything. A `label` that
/// already carries a prefix has it replaced rather than stacked, so renaming a
/// place whose label names another ID (a pane moved to another tab, a label
/// herdr restored) can never produce `[w2:t9] [w2:t3] name`.
///
/// An `id` that does not look like a herdr tab or pane ID leaves the label
/// bare: a prefix [`strip_id_prefix`] would not recognise is worse than none,
/// because every reader that finds a tab by its name would stop finding it.
/// So `strip_id_prefix(&id_label(id, l))` is `strip_id_prefix(l)` for every
/// `id`, and `l` itself whenever `l` carried no prefix.
pub fn id_label(id: &str, label: &str) -> String {
    let name = strip_id_prefix(label);
    if !is_herdr_id(id) {
        name.to_string()
    } else if name.is_empty() {
        format!("[{id}]")
    } else {
        format!("[{id}] {name}")
    }
}

/// A tab's or pane's name without the `[<id>] ` prefix [`id_label`] adds —
/// and empty for a label that is only `[<id>]`, which is a place with no name
/// of its own.
///
/// Every place that finds or recognises a tab or pane by its name compares
/// this, so one named before the prefix existed and one named after both
/// answer, and a pane named only `[w2:p1]` counts as unnamed wherever the
/// tower falls back to its terminal title. Conservative on purpose — only a
/// bracketed herdr ID (`w…:t…` or `w…:p…`, ASCII letters and digits either
/// side of the one colon) that is the whole label or is followed by exactly
/// one space counts. `[wip] fix`, `[w2:p1]nospace` and a name that merely
/// starts with a bracket are somebody's own and come back untouched.
pub fn strip_id_prefix(label: &str) -> &str {
    let Some(rest) = label.strip_prefix('[') else {
        return label;
    };
    if let Some(id) = rest.strip_suffix(']') {
        if is_id_prefix(id) {
            return "";
        }
    }
    rest.split_once("] ")
        .filter(|(id, _)| is_id_prefix(id))
        .map_or(label, |(_, name)| name)
}

/// What a prefix to strip may hold: one herdr ID, or one workspace with a
/// comma list of its places (`w5:p1,p4`, `w2:t3,p1`).
///
/// Wider than [`is_herdr_id`] on purpose, and only on the reading side. Tabs
/// named by hand before the tower did it carry lists like `[w5:p1,p4] 1`; read
/// as somebody's own bracket they would stay behind ours for good
/// (`[w5:t1] [w5:p1,p4] 1`). The tower itself only ever writes one ID.
fn is_id_prefix(id: &str) -> bool {
    if is_herdr_id(id) {
        return true;
    }
    let Some((ws, places)) = id.split_once(':') else {
        return false;
    };
    let alnum = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    ws.strip_prefix('w').is_some_and(alnum)
        && places.split(',').enumerate().all(|(i, part)| {
            match part.strip_prefix(['t', 'p']) {
                Some(rest) => alnum(rest),
                // After the first, a bare place number reads as the same kind.
                None => i > 0 && alnum(part),
            }
        })
        && places.contains(',')
}

/// Whether `id` has the shape of a herdr tab or pane ID: `w<alnum>+:t<alnum>+`
/// or `w<alnum>+:p<alnum>+` (`w2:t3`, `w3B:p1`, `w7:p1C`).
pub fn is_herdr_id(id: &str) -> bool {
    let part = |s: &str, lead: &[char]| {
        s.strip_prefix(lead)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric()))
    };
    id.split_once(':')
        .is_some_and(|(ws, place)| part(ws, &['w']) && part(place, &['t', 'p']))
}

/// The renames that put every tab's and pane's ID in front of its name:
/// `(id, current label)` in, `(id, new label)` out for each one that is not
/// already [`id_label`]`(id, current)`.
///
/// Every place, named or not — an unnamed pane becomes `[w2:p1]` — and only
/// the ones that differ, so a second run over the result renames nothing. The
/// supervisor's reconcile and every writer that keeps one particular name (the
/// dock's tab, the usage tab) compute the same [`id_label`], so none of them
/// undoes another. A label with a stale ID gets the current one; a bracket of
/// somebody's own stays behind ours (`[w2:t3] [wip] x`). An ID that does not
/// look like one is skipped rather than stripped.
pub fn id_label_renames<'a>(
    places: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<(String, String)> {
    places
        .into_iter()
        .filter(|(id, _)| is_herdr_id(id))
        .filter_map(|(id, current)| {
            let want = id_label(id, current);
            (want != current).then(|| (id.to_string(), want))
        })
        .collect()
}

/// One process in the pane's server-observed foreground process group.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ForegroundProcess {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub argv0: String,
    #[serde(default)]
    pub cmdline: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub pid: i64,
}

/// What herdr itself sees running in a pane.
///
/// Unlike a client-side lock, this view is the same when the client connects
/// from a container or another mount namespace.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProcessInfo {
    #[serde(default)]
    pub foreground_process_group_id: i64,
    #[serde(default)]
    pub foreground_processes: Vec<ForegroundProcess>,
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub shell_pid: i64,
}

/// Move an existing terminal without closing or starting its process.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneMoveDestination {
    NewTab { workspace_id: String },
    NewWorkspace { label: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneMoveResult {
    pub changed: bool,
    pub previous_pane_id: String,
    pub previous_workspace_id: String,
    pub pane: Pane,
    #[serde(default)]
    pub created_workspace: Option<Workspace>,
}

/// Which way a pane splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    #[default]
    Right,
    Down,
}

/// Who receives a right-click inside a pane.
///
/// `Herdr` is the server's own default: the click opens herdr's context menu,
/// whose entries include *Close pane*. `Pane` forwards the button to the
/// program running there instead, which is how a pane opts out of that menu
/// altogether.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RightClick {
    #[default]
    Herdr,
    Pane,
}

/// Configures a pane split.
#[derive(Debug, Clone, Default)]
pub struct SplitOptions {
    pub target_pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub direction: SplitDirection,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    pub ratio: Option<f64>,
    /// Defaults to false, following herdr's rule that background work must not
    /// steal the user's view.
    pub focus: bool,
}

/// Configures tab creation.
#[derive(Debug, Clone, Default)]
pub struct TabOptions {
    pub workspace_id: Option<String>,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    /// Same default and same reason as [`SplitOptions::focus`].
    pub focus: bool,
}

/// herdr's own `agent.start` default, mirrored so a caller that leaves the
/// timeout unset still gets a socket deadline covering the startup wait.
const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Configures starting an agent.
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    /// The unique name herdr assigns: `[a-z][a-z0-9_-]{0,31}`.
    pub name: String,
    /// The herdr agent kind (`claude`, `codex`, `gemini`, `agy`, …).
    pub kind: String,
    /// Must be an existing **shell pane**. `agent.start` never creates, splits,
    /// or moves layout — make the seat first with [`Herdr::split_pane`] or
    /// [`Herdr::new_tab`].
    pub pane_id: String,
    /// Native agent arguments.
    pub args: Vec<String>,
    /// `None` uses herdr's own 30s readiness default.
    pub timeout: Option<Duration>,
}

#[derive(Deserialize)]
struct SplitResult {
    pane: Pane,
}

#[derive(Deserialize)]
struct TabCreateResult {
    tab: Tab,
    root_pane: Pane,
}

#[derive(Deserialize)]
struct TabListResult {
    tabs: Vec<Tab>,
}

#[derive(Deserialize)]
struct WorkspaceCreateResult {
    workspace: Workspace,
    tab: Tab,
    root_pane: Pane,
}

#[derive(Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

impl Herdr {
    /// Lists every tab visible to this server.
    pub fn tabs(&self) -> Result<Vec<Tab>> {
        let res: TabListResult = self.client.call("tab.list", json!({}))?;
        Ok(res.tabs)
    }

    /// Tab ID (`"w2:tY"`) to its label.
    pub fn tab_labels(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .tabs()?
            .into_iter()
            .filter(|t| !t.label.is_empty())
            .map(|t| (t.tab_id, t.label))
            .collect())
    }

    /// Returns the processes herdr's host-side pane observer sees.
    pub fn process_info(&self, pane_id: &str) -> Result<ProcessInfo> {
        let res: ProcessInfoResult = self
            .client
            .call("pane.process_info", json!({ "pane_id": pane_id }))?;
        Ok(res.process_info)
    }

    /// Rehomes the existing terminal. Herdr may give the pane a new ID when
    /// its workspace changes; the returned pane is authoritative after this.
    pub fn move_pane(
        &self,
        pane_id: &str,
        destination: &PaneMoveDestination,
    ) -> Result<PaneMoveResult> {
        #[derive(Deserialize)]
        struct R {
            move_result: PaneMoveResult,
        }
        let res: R = self.client.call(
            "pane.move",
            json!({ "pane_id": pane_id, "destination": destination, "focus": false }),
        )?;
        Ok(res.move_result)
    }

    /// Routes right-clicks in one pane, and says nothing about any other.
    ///
    /// herdr keeps this per pane and does not report it back in `pane.get`, so
    /// the setting is written rather than reconciled: sending it again is
    /// harmless and is how a caller makes sure of it after a restart.
    pub fn set_right_click(&self, pane_id: &str, target: RightClick) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "pane.input.set",
                json!({ "pane_id": pane_id, "right_click": target }),
            )
            .map(|_| ())
    }

    /// Splits a pane and returns the new one.
    pub fn split_pane(&self, opts: &SplitOptions) -> Result<Pane> {
        let mut params = serde_json::Map::new();
        params.insert("direction".into(), serde_json::to_value(opts.direction)?);
        params.insert("focus".into(), json!(opts.focus));
        insert_some(
            &mut params,
            "target_pane_id",
            opts.target_pane_id.as_deref(),
        );
        insert_some(&mut params, "workspace_id", opts.workspace_id.as_deref());
        insert_some(&mut params, "cwd", opts.cwd.as_deref());
        if !opts.env.is_empty() {
            params.insert("env".into(), json!(opts.env));
        }
        if let Some(r) = opts.ratio.filter(|r| *r > 0.0) {
            params.insert("ratio".into(), json!(r));
        }
        let res: SplitResult = self
            .client
            .call("pane.split", serde_json::Value::Object(params))?;
        Ok(res.pane)
    }

    /// Creates a tab and returns it with its first pane. This is how a worker
    /// gets a place to live.
    pub fn new_tab(&self, opts: &TabOptions) -> Result<(Tab, Pane)> {
        let mut params = serde_json::Map::new();
        params.insert("focus".into(), json!(opts.focus));
        insert_some(&mut params, "workspace_id", opts.workspace_id.as_deref());
        insert_some(&mut params, "label", opts.label.as_deref());
        insert_some(&mut params, "cwd", opts.cwd.as_deref());
        if !opts.env.is_empty() {
            params.insert("env".into(), json!(opts.env));
        }
        let res: TabCreateResult = self
            .client
            .call("tab.create", serde_json::Value::Object(params))?;
        Ok((res.tab, res.root_pane))
    }

    /// Creates a workspace and returns it with its first tab and pane.
    ///
    /// `focus` is a parameter rather than a default because creating a place in
    /// the background and moving the user into it are genuinely different acts;
    /// a control tower doing setup work wants the former.
    pub fn new_workspace(
        &self,
        label: &str,
        cwd: &str,
        focus: bool,
    ) -> Result<(Workspace, Tab, Pane)> {
        let mut params = serde_json::Map::new();
        params.insert("focus".into(), json!(focus));
        insert_some(&mut params, "label", non_empty(label));
        insert_some(&mut params, "cwd", non_empty(cwd));
        let res: WorkspaceCreateResult = self
            .client
            .call("workspace.create", serde_json::Value::Object(params))?;
        Ok((res.workspace, res.tab, res.root_pane))
    }

    /// Replaces a workspace's human-readable label.
    pub fn rename_workspace(&self, workspace_id: &str, label: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "workspace.rename",
                json!({ "workspace_id": workspace_id, "label": label }),
            )
            .map(|_| ())
    }

    /// Replaces a tab's human-readable label.
    pub fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>("tab.rename", json!({ "tab_id": tab_id, "label": label }))
            .map(|_| ())
    }

    /// Replaces a pane's own name — the one herdr draws on the pane's border.
    ///
    /// Not the agent's name: `agent.rename` is a separate call with a stricter
    /// alphabet, and checked against a live 0.9.1 server a pane renamed under
    /// a running agent keeps answering to the agent's name, and a pane named
    /// before `agent.start` keeps its name through the start.
    pub fn rename_pane(&self, pane_id: &str, label: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "pane.rename",
                json!({ "pane_id": pane_id, "label": label }),
            )
            .map(|_| ())
    }

    /// Takes a pane's own name away, so herdr draws what runs in it again.
    /// `label: null` is the socket's spelling of `herdr pane rename --clear`.
    pub fn clear_pane_name(&self, pane_id: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "pane.rename",
                json!({ "pane_id": pane_id, "label": null }),
            )
            .map(|_| ())
    }

    /// Names the root pane of a place the tower just made after the ID herdr
    /// gave it: `[<pane_id>] label` ([`id_label`]). The tab keeps the plain
    /// name it was created with — tab ids in tab names read as one more
    /// coordinate to decode next to the panes' and were taken back out
    /// (2026-09-26).
    ///
    /// A call of its own because `tab.create` and `workspace.create` only
    /// learn the ID by making the place, so the label they were given cannot
    /// carry it. Best effort, and deliberately without a result: a tab or pane that
    /// keeps its bare name is still the right one — every reader recognises it
    /// through [`strip_id_prefix`] — so no launch fails over a name.
    pub fn name_with_id(&self, pane_id: &str, label: &str) {
        let _ = self.rename_pane(pane_id, &id_label(pane_id, label));
    }

    /// Closes one pane, and whatever runs in it. herdr closes the tab too when
    /// it was the tab's last pane.
    pub fn close_pane(&self, pane_id: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>("pane.close", json!({ "pane_id": pane_id }))
            .map(|_| ())
    }

    /// Closes one tab, and whatever runs in it.
    pub fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>("tab.close", json!({ "tab_id": tab_id }))
            .map(|_| ())
    }

    /// Closes one workspace by its stable server ID.
    pub fn close_workspace(&self, workspace_id: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "workspace.close",
                json!({ "workspace_id": workspace_id }),
            )
            .map(|_| ())
    }

    /// Types text into a pane without submitting it.
    pub fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "pane.send_text",
                json!({ "pane_id": pane_id, "text": text }),
            )
            .map(|_| ())
    }

    /// Sends logical keys (`"enter"`, `"esc"`, `"ctrl+c"`).
    ///
    /// There are no byte sequences to assemble: herdr validates every key first
    /// and writes nothing at all if one of them is invalid, so a typo cannot
    /// deliver half a chord into somebody's session.
    pub fn send_keys(&self, pane_id: &str, keys: &[&str]) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "pane.send_keys",
                json!({ "pane_id": pane_id, "keys": keys }),
            )
            .map(|_| ())
    }

    /// Types a command into a shell pane and submits it.
    ///
    /// There is no `pane.run` on the socket — the CLI's `herdr pane run` is
    /// built on `pane.send_input`, which carries text and keys in a single
    /// call. That removes a chronic problem of the send-keys era outright: text
    /// and Enter leave together, so a TUI swallowing an Enter that follows text
    /// too closely cannot happen and no send delay needs tuning.
    ///
    /// To put an instruction into an *agent's* input use [`Herdr::prompt`]
    /// instead — refusing to write into an approval dialog is `agent.prompt`'s
    /// job, and this call does not do it.
    pub fn run(&self, pane_id: &str, command: &str) -> Result<()> {
        self.client
            .call::<_, serde_json::Value>(
                "pane.send_input",
                json!({ "pane_id": pane_id, "text": command, "keys": ["enter"] }),
            )
            .map(|_| ())
    }

    /// Launches an agent in a shell pane and returns once herdr has recognized
    /// it and considers it ready for interactive input.
    ///
    /// It replaces a hand-rolled sequence: create a window, type the command,
    /// answer the "do you trust this folder?" prompt with Enter, then watch the
    /// screen to confirm it came up. If the agent is blocked during startup the
    /// call reports `agent_not_ready` immediately, and the name stays usable.
    pub fn start_agent(&self, opts: &StartOptions) -> Result<()> {
        if opts.name.is_empty() || opts.kind.is_empty() || opts.pane_id.is_empty() {
            return Err(Error::Invalid(
                "starting an agent needs a name, a kind, and a pane".into(),
            ));
        }
        let mut params = serde_json::Map::new();
        params.insert("name".into(), json!(opts.name));
        params.insert("kind".into(), json!(opts.kind));
        params.insert("pane_id".into(), json!(opts.pane_id));
        if !opts.args.is_empty() {
            params.insert("args".into(), json!(opts.args));
        }
        if let Some(t) = opts.timeout {
            params.insert("timeout_ms".into(), json!(t.as_millis() as u64));
        }
        // agent.start answers only once herdr has seen the agent come up, which
        // defaults to 30s and can be set as high as 300s — far past the
        // socket's ordinary deadline.
        let wait = opts.timeout.unwrap_or(DEFAULT_START_TIMEOUT);
        self.client
            .call_within::<_, serde_json::Value>(
                deadline_for(wait),
                "agent.start",
                serde_json::Value::Object(params),
            )
            .map(|_| ())
    }

    /// Blocks until the agent settles into one of `until`.
    ///
    /// An empty `until` means herdr's own default: the first settled idle,
    /// done, or blocked. This is what replaces "dispatch, then poll for DONE" —
    /// the server answers once instead of the caller asking repeatedly.
    ///
    /// `timeout` is `Option` rather than a plain duration because the two cases
    /// need different socket deadlines, and getting that wrong is invisible:
    /// herdr waits **indefinitely** when no `timeout_ms` is sent, so a padded
    /// three-second deadline would abort every unbounded wait locally while the
    /// server was still waiting, and the caller could not tell that apart from
    /// a failure. `None` therefore sets no deadline at all.
    pub fn wait(
        &self,
        target: &str,
        until: &[AgentStatus],
        timeout: Option<Duration>,
    ) -> Result<()> {
        let mut params = serde_json::Map::new();
        params.insert("target".into(), json!(target));
        if !until.is_empty() {
            params.insert("until".into(), serde_json::to_value(until)?);
        }
        match timeout {
            Some(t) => {
                params.insert("timeout_ms".into(), json!(t.as_millis() as u64));
                self.client
                    .call_within::<_, serde_json::Value>(
                        deadline_for(t),
                        "agent.wait",
                        serde_json::Value::Object(params),
                    )
                    .map(|_| ())
            }
            None => self
                .client
                .call_blocking::<_, serde_json::Value>(
                    "agent.wait",
                    serde_json::Value::Object(params),
                )
                .map(|_| ()),
        }
    }

    /// Reports whether a workspace carries exactly that label.
    ///
    /// A `Result` rather than a bare bool on purpose: an unreachable server and
    /// "no such workspace" are different answers, and collapsing them makes a
    /// caller create a second workspace with a name that already exists.
    pub fn has_workspace(&self, label: &str) -> Result<bool> {
        Ok(self.workspaces()?.iter().any(|w| names(w, label)))
    }

    /// Resolves a stored session name to a workspace ID.
    ///
    /// Callers hold labels — they are what a control tower stores as the session
    /// name — while the API speaks IDs. `None` means nothing answers to that
    /// name, which is a fact rather than a failure.
    pub fn workspace_id_by_label(&self, label: &str) -> Result<Option<String>> {
        Ok(self
            .workspaces()?
            .into_iter()
            .find(|w| names(w, label))
            .map(|w| w.workspace_id))
    }
}

/// Whether a workspace answers to `name`, by label **or** by ID.
///
/// An unlabeled workspace is stored under its own ID: `tower_scan::sync` writes
/// `workspace_id` as the session name when there is no label, because a record
/// has to say *something*. Matching on the label alone therefore failed to find
/// a workspace that plainly exists — `resume` treated it as gone and `restore`
/// created a duplicate beside it. The ID is only consulted as a fallback, so a
/// workspace deliberately labeled `w2` still wins over the workspace whose ID
/// that is.
fn names(w: &Workspace, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    w.label == name || (w.label.is_empty() && w.workspace_id == name)
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Omits a parameter entirely when it has no value, rather than sending an
/// empty string — herdr reads an empty `cwd` or `label` as a value, not as
/// "unset", and would create a tab with a blank name.
fn insert_some(
    params: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    v: Option<&str>,
) {
    if let Some(v) = v.filter(|v| !v.is_empty()) {
        params.insert(key.to_string(), json!(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panes_sort_in_herdrs_own_count() {
        // As herdr 0.9.5 lists one workspace's panes.
        let listed = ["w2:pEJ", "w2:pHC", "w2:pJY", "w2:pJZ", "w2:pJ0", "w2:pK2"];
        let mut shuffled = listed;
        shuffled.reverse();
        shuffled.sort_by_key(|p| pane_order(p));
        assert_eq!(shuffled, listed);
        // A shorter count is smaller, `1` leading or not.
        let mut ids = ["w7C:pQ", "w60:p1B", "w7C:p3", "w7C:pC", "wF:p1C", "w7C:pJ"];
        ids.sort_by_key(|p| pane_order(p));
        assert_eq!(
            ids,
            ["w7C:p3", "w7C:pC", "w7C:pJ", "w7C:pQ", "w60:p1B", "wF:p1C"]
        );
        // `0` is the last digit, not the first.
        assert!(pane_order("w2:pZ") < pane_order("w2:p0"));
    }

    /// herdr's count written out on its own: bijective base 32 over the
    /// public-ID alphabet, `1` for one and no zero digit.
    fn herdr_pane_id(mut n: u64) -> String {
        let mut digits = Vec::new();
        while n > 0 {
            n -= 1;
            digits.push(PUBLIC_ID_ALPHABET[(n % 32) as usize]);
            n /= 32;
        }
        digits.reverse();
        format!("w1:p{}", String::from_utf8(digits).unwrap())
    }

    /// Cross-check and round trip: for herdr's first panes, sorting by
    /// [`pane_order`] is sorting by the number herdr counted, and the key's
    /// digits read back as that number.
    #[test]
    fn the_order_is_herdrs_count_for_every_pane_it_numbers() {
        let ids: Vec<(u64, String)> = (1..=40_000).map(|n| (n, herdr_pane_id(n))).collect();
        assert_eq!(ids[0].1, "w1:p1");
        assert_eq!(ids[31].1, "w1:p0", "thirty-two is the last one-digit id");
        assert_eq!(ids[32].1, "w1:p11");
        for pair in ids.windows(2) {
            assert!(
                pane_order(&pair[0].1) < pane_order(&pair[1].1),
                "{} before {}",
                pair[0].1,
                pair[1].1
            );
        }
        for (n, id) in &ids {
            let (odd, len, digits) = pane_order(id);
            assert!(!odd && len == digits.len(), "{id}");
            let back = digits
                .iter()
                .fold(0u64, |acc, &d| acc * 32 + u64::from(d) + 1);
            assert_eq!(back, *n, "{id}");
        }
    }

    /// Edge: herdr writes its IDs in upper case; a lower-case or confusable
    /// letter is not one of its digits.
    #[test]
    fn letters_outside_the_alphabet_are_not_a_pane_count() {
        for odd in ["w2:pjz", "w2:pO", "w2:pL", "w2:pU", "w2:p 1", "w2:p1:"] {
            assert!(pane_order(odd).0, "{odd:?}");
        }
    }

    #[test]
    fn anything_that_is_not_a_pane_coordinate_sorts_last() {
        for odd in ["", "w2", "w2:p", "w2:pI", "%3", "w2:t1"] {
            assert!(pane_order("w9:pZZZZ") < pane_order(odd), "{odd:?}");
        }
        assert!(pane_order("%3") < pane_order("%4"), "by their text");
    }

    #[test]
    fn workspace_prefix_preserves_ordinary_brackets_and_wire_names() {
        for (raw, name) in [
            ("", ""),
            ("[w2]", ""),
            ("[wF] 개발", "개발"),
            ("[w2] [w3B] project", "project"),
            ("[wip] fix", "[wip] fix"),
            ("[wI] name", "[wI] name"),
            ("[w2]nospace", "nospace"),
            ("[w2]  spaces", " spaces"),
        ] {
            assert_eq!(strip_workspace_prefix(raw), name);
            let w: Workspace = serde_json::from_value(json!({ "workspace_id": "wF", "label": raw,
                "active_tab_id": null, "tokens": {"owner": "request"} }))
            .unwrap();
            let t: Tab =
                serde_json::from_value(json!({ "tab_id": "wF:t1", "label": raw })).unwrap();
            assert_eq!(w.label, name);
            assert_eq!(t.label, name);
            assert_eq!(w.display_label, raw);
            assert_eq!(t.display_label, raw);
            assert_eq!(w.tokens["owner"], "request");
        }
        for id in ["w2", "wF", "w3B", "w0", "wZZ"] {
            for name in ["", "项目", "[wip] fix", "two spaces  ", "🗼 HERDR-TOWER 🗼"] {
                let label = workspace_id_label(id, name);
                assert_eq!(strip_workspace_prefix(&label), name);
                assert_eq!(workspace_id_label(id, &label), label);
                assert_eq!(
                    workspace_id_label(id, &format!("[w2:t1] [w3B] {name}")),
                    label
                );
            }
        }
    }

    #[test]
    fn workspace_prefix_has_no_gap_and_migrates_legacy_labels_once() {
        for (raw, want, name) in [
            ("project", "[wF]project", "project"),
            ("[w2] project", "[wF]project", "project"),
            ("[w2]project", "[wF]project", "project"),
            ("[w2][w3B]project", "[wF]project", "project"),
            ("[w2] [w3B]project", "[wF]project", "project"),
            ("[wip] fix", "[wF][wip] fix", "[wip] fix"),
            ("개발", "[wF]개발", "개발"),
            ("  project", "[wF]project", "project"),
            ("", "[wF]", ""),
            ("  ", "[wF]", ""),
        ] {
            let label = workspace_id_label("wF", raw);
            assert_eq!(label, want, "{raw:?}");
            assert_eq!(workspace_id_label("wF", &label), want, "no second rename");
            let ws: Workspace =
                serde_json::from_value(json!({"workspace_id":"wF","label":label})).unwrap();
            let tab: Tab = serde_json::from_value(json!({"tab_id":"wF:t1","label":label})).unwrap();
            assert_eq!(ws.label, name);
            assert_eq!(tab.label, name);
        }
    }

    /// Hand-made list prefixes are replaced, never kept behind ours.
    #[test]
    fn a_hand_made_id_list_prefix_is_replaced_not_stacked() {
        for (label, want) in [
            ("[w5:p1,p4] 1", "1"),
            ("[wB:p1,p2] 1", "1"),
            ("[w2:t3,p1] x", "x"),
            ("[w5:p1,4] y", "y"),
            ("[w5:p1,p4]", ""),
            ("[w5:,p4] z", "[w5:,p4] z"),
            ("[w5:p1,] z", "[w5:p1,] z"),
            ("[w5:x1,p4] z", "[w5:x1,p4] z"),
            ("[wip,p1] z", "[wip,p1] z"),
        ] {
            assert_eq!(strip_id_prefix(label), want, "{label:?}");
        }
        assert_eq!(id_label("w5:t1", "[w5:p1,p4] 1"), "[w5:t1] 1");
    }

    /// A split with no direction must still name one; herdr has no default.
    #[test]
    fn split_direction_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SplitDirection::default()).unwrap(),
            "\"right\""
        );
        assert_eq!(
            serde_json::to_string(&SplitDirection::Down).unwrap(),
            "\"down\""
        );
    }

    /// The wire spelling herdr's `PaneRightClickTarget` accepts.
    #[test]
    fn right_click_targets_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&RightClick::default()).unwrap(),
            "\"herdr\""
        );
        assert_eq!(
            serde_json::to_string(&RightClick::Pane).unwrap(),
            "\"pane\""
        );
    }

    /// An empty string is not "unset": sending one names a tab "".
    #[test]
    fn empty_optional_parameters_are_omitted() {
        let mut params = serde_json::Map::new();
        insert_some(&mut params, "label", Some(""));
        insert_some(&mut params, "cwd", None);
        insert_some(&mut params, "workspace_id", Some("w2"));
        assert_eq!(params.len(), 1);
        assert_eq!(params["workspace_id"], "w2");
    }

    #[test]
    fn an_id_prefix_round_trips_and_never_stacks() {
        for id in ["w2:t3", "w3B:t1", "w2:p1", "w3B:p1", "w7:p1C", "w10:p12"] {
            for name in ["hero-dark-codex", "🗼 HERDR-TOWER 🗼", "[wip] fix", "a ] b"] {
                let labelled = id_label(id, name);
                assert_eq!(labelled, format!("[{id}] {name}"));
                assert_eq!(strip_id_prefix(&labelled), name, "{labelled:?}");
            }
            // No name of its own: the ID alone, which reads back as no name.
            assert_eq!(id_label(id, ""), format!("[{id}]"));
            assert_eq!(strip_id_prefix(&id_label(id, "")), "");
        }
        assert_eq!(
            id_label("w2:t9", "[w2:t3] name"),
            "[w2:t9] name",
            "a new ID replaces the old one"
        );
        assert_eq!(id_label("w2:p1", "[w2:t3] name"), "[w2:p1] name");
        assert_eq!(id_label("w2:p9", "[w2:p1]"), "[w2:p9]");
        assert_eq!(id_label("w2:t3", &id_label("w2:t3", "x")), "[w2:t3] x");
    }

    #[test]
    fn only_a_well_formed_id_prefix_is_stripped() {
        for label in [
            "[wip] fix",
            "[wip]",
            "[w2:p1]nospace",
            "[w2] x",
            "[p1:w2] x",
            "[w2:x1] x",
            "[w2:x1]",
            "[w2:p1:p2] x",
            "[w 2:p1] x",
            "[w:p1] x",
            "[w2:p] x",
            "[w2:t-1] x",
            "[] x",
            " [w2:p1] x",
            "[w2:p1] ]",
            "🗼 HERDR-TOWER 🗼",
            "",
            "[",
            "plain",
        ] {
            let want = if label == "[w2:p1] ]" { "]" } else { label };
            assert_eq!(strip_id_prefix(label), want, "{label:?}");
        }
        assert_eq!(strip_id_prefix("[w2:p1] "), "");
        assert_eq!(strip_id_prefix("[w2:t1]"), "", "the ID alone is no name");
        assert_eq!(strip_id_prefix("[w2:t1]  two"), " two", "exactly one space");
        assert_eq!(
            strip_id_prefix("[w2:t1] [w2:p1] x"),
            "[w2:p1] x",
            "one prefix, not every one"
        );
    }

    #[test]
    fn an_id_that_is_not_one_leaves_the_label_bare() {
        assert_eq!(id_label("", "name"), "name");
        assert_eq!(id_label("remote/w2:p1", "name"), "name");
        assert_eq!(id_label("term_65c4e6", "name"), "name");
        assert_eq!(id_label("", "[w2:p1] name"), "name");
    }

    #[test]
    fn the_reconcile_renames_only_what_is_not_already_right() {
        let places = [
            ("w2:t3", "hero-dark-codex"), // missing
            ("w2:t4", "[w2:t1] review"),  // stale ID
            ("w2:t5", "[w2:t5] done"),    // already right
            ("w2:p1", ""),                // unnamed pane
            ("w2:p2", "[w2:p1]"),         // only a stale prefix
            ("w2:p3", "[w2:p3]"),         // only its own prefix
            ("w2:t6", "[wip] x"),         // somebody's bracket
            ("remote/w2:t7", "x"),        // not an ID: skipped
        ];
        let got = id_label_renames(places);
        let want: Vec<(String, String)> = [
            ("w2:t3", "[w2:t3] hero-dark-codex"),
            ("w2:t4", "[w2:t4] review"),
            ("w2:p1", "[w2:p1]"),
            ("w2:p2", "[w2:p2]"),
            ("w2:t6", "[w2:t6] [wip] x"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(got, want);

        // Applying them leaves nothing to do.
        let applied: Vec<(&str, &str)> = places
            .iter()
            .map(|(id, label)| {
                let now = got
                    .iter()
                    .find(|(i, _)| i == id)
                    .map_or(*label, |(_, l)| l.as_str());
                (*id, now)
            })
            .collect();
        assert!(id_label_renames(applied).is_empty());
    }
}
