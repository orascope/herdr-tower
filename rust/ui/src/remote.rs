// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Agents on the machines herdr has saved (`herdr machine add`), read-only.
//!
//! herdr's own `prefix+g` list shows them; the tower used to read only the
//! local sockets and so never did. They are fetched through the herdr CLI —
//! one `herdr --machine <label> api snapshot`, the same snapshot herdr's own
//! navigator walks — and drawn under the local rows. The
//! tower does not send to, focus or track them: their pane ids (`w2:p6`) are
//! only unique per machine, and the store keys by pane.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long one `herdr` call may take before it is given up on. A machine
/// that is asleep makes ssh hang far longer than this.
const CALL_TIMEOUT: Duration = Duration::from_secs(8);

/// One agent pane on a saved machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgent {
    /// The workspace's label there, `[w2]` for an unnamed one.
    pub workspace: String,
    pub pane_id: String,
    /// `claude`, `codex`, …
    pub agent: String,
    /// herdr's own word: `idle`, `working`, `done`, `blocked`.
    pub status: String,
    /// The id that machine's herdr-tower knows it by: `claude-w2:p6`.
    pub id: String,
    /// The pane's own name, else its terminal title — what herdr's navigator
    /// shows for it.
    pub title: String,
    /// The tab's label, without herdr's `[w2:t1]` id prefix.
    pub tab: String,
    /// The agent's own session id, whole.
    pub session_id: String,
    /// Where the agent is, on that machine.
    pub cwd: String,
    /// What its idle loop is doing there (`idle=30m  sent=3 …`), if it has one.
    pub loop_state: Option<String>,
    /// The messages that machine's herdr-tower has scheduled for it.
    pub schedules: Vec<RemoteSchedule>,
}

/// A pane on a saved machine whose title reads like an agent's, but which
/// herdr there does not list as one — a `claude` started under `sudo su -`
/// runs on a terminal of its own, where herdr cannot see it. Shown dimmed
/// under its machine and never selected: there is nothing there to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnseenPane {
    /// The workspace's label there, `[w2]` for an unnamed one.
    pub workspace: String,
    pub pane_id: String,
    /// The pane's own name, else its terminal title.
    pub title: String,
    /// The tab's label, without herdr's `[w2:t1]` id prefix.
    pub tab: String,
    /// herdr could not read the folder of what runs in front there: it
    /// belongs to another user, as it does under `sudo` or `su`.
    pub other_user: bool,
}

/// One scheduled message on a saved machine, as its `schedule` list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSchedule {
    pub id: String,
    /// When it goes next, in that machine's words: `19:00`, `sending`, `—`.
    pub next: String,
}

/// A saved machine and what was read from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteMachine {
    pub label: String,
    /// The ssh target saved for it, for the commands that run its herdr-tower.
    pub target: String,
    pub agents: Vec<RemoteAgent>,
    /// Its panes that look like agents but are not listed as any.
    pub unseen: Vec<UnseenPane>,
    /// Why nothing could be read, for a machine that did not answer.
    pub error: Option<String>,
    /// Since when the agents shown are the last ones read rather than live:
    /// a machine that stops answering keeps its rows, dimmed, the way herdr's
    /// navigator keeps a stale endpoint's snapshot.
    pub stale_since: Option<time::OffsetDateTime>,
    /// Its processor counters and memory, from its `herdr-tower host`.
    /// `None` from a machine whose herdr-tower is older than the command, or
    /// that could not be reached over ssh.
    pub cpu: Option<tower_os::host::CpuTicks>,
    pub memory: Option<tower_os::host::Memory>,
    /// The counters of the reading before, for the busy share between the
    /// two. Set by the feed's remote thread, which remembers them.
    pub cpu_before: Option<tower_os::host::CpuTicks>,
    /// What it runs, from the same `herdr-tower host`; `None` from one too
    /// old to say.
    pub system: Option<tower_os::host::System>,
    /// Why its herdr-tower could not be read over ssh — its agents still
    /// come through herdr, but no load, loops or schedules: see
    /// [`ssh_trouble`].
    pub tower_trouble: Option<String>,
}

impl RemoteMachine {
    /// How busy it is, as the header shows this machine.
    pub fn load(&self) -> crate::app::HostLoad {
        crate::app::HostLoad {
            cpu_pct: self
                .cpu
                .zip(self.cpu_before)
                .and_then(|(now, before)| now.busy_pct_since(&before)),
            memory: self.memory,
        }
    }
}

/// The label and ssh target of each enabled saved machine, from
/// `herdr machine list --json`.
pub fn parse_machines(json: &str) -> Vec<(String, String)> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|m| m["enabled"].as_bool().unwrap_or(true))
        .filter_map(|m| {
            let label = m["label"].as_str().filter(|l| !l.is_empty())?;
            let target = m["target"].as_str().unwrap_or_default();
            Some((label.to_string(), target.to_string()))
        })
        .collect()
}

/// The agents of one machine from its `api snapshot`, in the sidebar's order:
/// workspace, then tab, then pane. `None` when the reply is not a snapshot.
pub fn parse_snapshot(json: &str) -> Option<Vec<RemoteAgent>> {
    let v: Value = serde_json::from_str(json).ok()?;
    let snap = v.get("result")?.get("snapshot")?;
    let agents = snap.get("agents")?.as_array()?;
    let places = Places::of(snap);

    let mut found: Vec<((usize, u64, usize), RemoteAgent)> = agents
        .iter()
        .filter_map(|a| {
            let pane_id = a["pane_id"].as_str()?.to_string();
            let ws = a["workspace_id"].as_str().unwrap_or_default();
            let (rank, workspace, tab, pane) = places.place(ws, a, &pane_id);
            let name = herdrx::strip_id_prefix(pane["label"].as_str().unwrap_or_default()).trim();
            let title = if name.is_empty() {
                a["terminal_title_stripped"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
            } else {
                name
            };
            let cwd = a["foreground_cwd"]
                .as_str()
                .or(a["cwd"].as_str())
                .unwrap_or_default();
            let agent = a["agent"].as_str().unwrap_or("?").to_string();
            Some((
                rank,
                RemoteAgent {
                    workspace,
                    id: format!("{agent}-{pane_id}"),
                    pane_id,
                    agent,
                    status: a["agent_status"].as_str().unwrap_or("unknown").to_string(),
                    loop_state: None,
                    schedules: Vec::new(),
                    title: title.to_string(),
                    tab,
                    session_id: a["agent_session"]["value"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    cwd: cwd.to_string(),
                },
            ))
        })
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    Some(found.into_iter().map(|(_, a)| a).collect())
}

/// The panes of one machine's `api snapshot` whose title reads like an
/// agent's ([`looks_like_agent`]) but that its herdr does not list among the
/// agents, in the sidebar's order. Empty when the reply is not a snapshot.
pub fn parse_unseen(json: &str) -> Vec<UnseenPane> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(snap) = v.get("result").and_then(|r| r.get("snapshot")) else {
        return Vec::new();
    };
    let listed: std::collections::HashSet<&str> = snap["agents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a["pane_id"].as_str())
        .collect();
    let places = Places::of(snap);
    let mut found: Vec<((usize, u64, usize), UnseenPane)> = snap["panes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| {
            let pane_id = p["pane_id"].as_str()?;
            let raw = p["terminal_title"].as_str().unwrap_or_default();
            let stripped = p["terminal_title_stripped"].as_str().unwrap_or_default();
            if listed.contains(pane_id) || !looks_like_agent(raw, stripped) {
                return None;
            }
            let ws = p["workspace_id"].as_str().unwrap_or_default();
            let (rank, workspace, tab, _) = places.place(ws, p, pane_id);
            let name = herdrx::strip_id_prefix(p["label"].as_str().unwrap_or_default()).trim();
            let title = if name.is_empty() {
                stripped.trim()
            } else {
                name
            };
            Some((
                rank,
                UnseenPane {
                    workspace,
                    pane_id: pane_id.to_string(),
                    title: title.to_string(),
                    tab,
                    other_user: p.get("foreground_cwd").is_none(),
                },
            ))
        })
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, p)| p).collect()
}

/// Whether a pane's terminal title reads like an agent's. Claude Code puts a
/// status glyph before its title (`✳ build`, `◑ build`), which herdr strips;
/// a shell's prompt title (`alice@desk: ~`) has nothing to strip. A title
/// that starts with an agent's name counts too.
fn looks_like_agent(raw: &str, stripped: &str) -> bool {
    let (raw, stripped) = (raw.trim(), stripped.trim());
    if stripped.is_empty() {
        return false;
    }
    let lower = stripped.to_lowercase();
    raw != stripped
        || ["claude", "codex", "gemini"]
            .iter()
            .any(|name| lower.starts_with(name))
}

/// Where a snapshot's workspaces and tabs sit in herdr's sidebar, and what
/// they are called there: what both its agents and its other panes are
/// placed and labelled by.
struct Places<'a> {
    workspaces: HashMap<&'a str, (usize, String)>,
    tabs: HashMap<&'a str, (u64, String)>,
    panes: HashMap<&'a str, (usize, &'a Value)>,
}

impl<'a> Places<'a> {
    fn of(snap: &'a Value) -> Self {
        let list = |key: &str| {
            snap.get(key)
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
        };
        let workspaces = list("workspaces")
            .iter()
            .enumerate()
            .filter_map(|(i, w)| {
                let id = w["workspace_id"].as_str()?;
                let label = w["label"].as_str().unwrap_or_default();
                Some((id, (i, label.to_string())))
            })
            .collect();
        let tabs = list("tabs")
            .iter()
            .filter_map(|t| {
                let id = t["tab_id"].as_str()?;
                // herdr names a tab `[w2:t1] build`, and a tab its user named in
                // an unnamed workspace `[w2]top`: neither prefix is the name.
                let label = herdrx::strip_id_prefix(t["label"].as_str().unwrap_or_default()).trim();
                let ws = t["workspace_id"].as_str().unwrap_or_default();
                let label = label
                    .strip_prefix(&format!("[{ws}]"))
                    .unwrap_or(label)
                    .trim();
                Some((
                    id,
                    (t["number"].as_u64().unwrap_or(u64::MAX), label.to_string()),
                ))
            })
            .collect();
        let panes = list("panes")
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((p["pane_id"].as_str()?, (i, p))))
            .collect();
        Places {
            workspaces,
            tabs,
            panes,
        }
    }

    /// For `item` (an agent or a pane) in workspace `ws`: its rank in the
    /// sidebar's order, its workspace's label (`[w2]` for an unnamed one),
    /// its tab's label and its pane.
    fn place(
        &self,
        ws: &str,
        item: &Value,
        pane_id: &str,
    ) -> ((usize, u64, usize), String, String, &'a Value) {
        let (ws_rank, ws_label) = self
            .workspaces
            .get(ws)
            .cloned()
            .unwrap_or((usize::MAX, String::new()));
        let (tab_rank, tab) = self
            .tabs
            .get(item["tab_id"].as_str().unwrap_or_default())
            .cloned()
            .unwrap_or((u64::MAX, String::new()));
        let (pane_rank, pane) = self
            .panes
            .get(pane_id)
            .copied()
            .unwrap_or((usize::MAX, &Value::Null));
        let workspace = if ws_label.is_empty() {
            format!("[{ws}]")
        } else {
            ws_label
        };
        ((ws_rank, tab_rank, pane_rank), workspace, tab, pane)
    }
}

/// The agents of one machine from its `workspace list` and `agent list`
/// replies. `None` when the agent list cannot be read: a missing workspace
/// list only costs the labels.
pub fn parse_agents(workspaces: Option<&str>, agents: &str) -> Option<Vec<RemoteAgent>> {
    let result = |s: &str| -> Option<Value> {
        let v: Value = serde_json::from_str(s).ok()?;
        v.get("result").cloned()
    };
    let agents = result(agents)?;
    let list = agents.get("agents")?.as_array()?;

    let spaces = workspaces
        .and_then(result)
        .and_then(|r| r.get("workspaces").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let labels: HashMap<String, String> = spaces
        .iter()
        .filter_map(|w| {
            Some((
                w["workspace_id"].as_str()?.to_string(),
                w["label"].as_str()?.to_string(),
            ))
        })
        .collect();
    // The sidebar's order, so the rows come out grouped by workspace the way
    // the local list is.
    let order: HashMap<&str, usize> = spaces
        .iter()
        .enumerate()
        .filter_map(|(i, w)| Some((w["workspace_id"].as_str()?, i)))
        .collect();

    let mut found: Vec<(usize, RemoteAgent)> = list
        .iter()
        .filter_map(|a| {
            let ws = a["workspace_id"].as_str().unwrap_or_default();
            let workspace = match labels.get(ws) {
                Some(l) if !l.is_empty() => l.clone(),
                _ => format!("[{ws}]"),
            };
            let pane_id = a["pane_id"].as_str()?.to_string();
            let agent = a["agent"].as_str().unwrap_or("?").to_string();
            Some(RemoteAgent {
                workspace,
                id: format!("{agent}-{pane_id}"),
                pane_id,
                agent,
                status: a["agent_status"].as_str().unwrap_or("unknown").to_string(),
                loop_state: None,
                schedules: Vec::new(),
                title: a["terminal_title_stripped"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                tab: String::new(),
                session_id: a["agent_session"]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                cwd: a["cwd"].as_str().unwrap_or_default().to_string(),
            })
            .map(|agent| (order.get(ws).copied().unwrap_or(usize::MAX), agent))
        })
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    Some(found.into_iter().map(|(_, a)| a).collect())
}

/// How the commands reach a machine: herdr's own CLI, and a shell command on
/// the machine over ssh. Each returns stdout, or why it failed.
pub struct Runner<'a> {
    pub herdr: &'a (dyn Fn(&[&str]) -> Result<String, String> + Sync),
    pub ssh: &'a (dyn Fn(&str, &str) -> Result<String, String> + Sync),
}

/// The line between `loop list` and `schedule` in the one ssh call.
const SCHEDULES_MARK: &str = "--herdr-tower-schedules--";
/// The line between `host` and `loop list` in it.
const HOST_MARK: &str = "--herdr-tower-host--";

/// The schedules aimed at one session, keyed by the short session id the
/// `TO` column shows (`session 840316f8`), from `herdr-tower schedule`.
///
/// The columns are padded (`{:<8} {:<17} {:<24} {:<12}`), but a long `WHEN`
/// (a one-off with its date) pushes the rest along, so nothing is read at a
/// fixed place: `TO` is found by its `session ` word, and `NEXT` starts one
/// past its 24 columns and runs to the next double space.
pub fn parse_schedules(text: &str) -> HashMap<String, Vec<RemoteSchedule>> {
    let mut out: HashMap<String, Vec<RemoteSchedule>> = HashMap::new();
    for line in text.lines() {
        // `session <id>`, not the word inside `every handoff session`.
        let Some(at) = line.match_indices(" session ").map(|(i, _)| i).find(|&i| {
            line[i + " session ".len()..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace())
        }) else {
            continue;
        };
        let id = line.split_whitespace().next().unwrap_or_default();
        if id.is_empty() || id == "ID" {
            continue;
        }
        let to = &line[at + 1..];
        let sid = to["session ".len()..]
            .split_whitespace()
            .next()
            .unwrap_or_default();
        let to_width = "session ".len() + sid.len();
        let next_from = to
            .char_indices()
            .nth(to_width.max(24) + 1)
            .map_or(to.len(), |(i, _)| i);
        let next = to[next_from..]
            .split("  ")
            .next()
            .unwrap_or_default()
            .trim();
        if sid.is_empty() {
            continue;
        }
        out.entry(sid.to_string())
            .or_default()
            .push(RemoteSchedule {
                id: id.to_string(),
                next: next.to_string(),
            });
    }
    out
}

/// `id` to the rest of its line, from `herdr-tower loop list`.
pub fn parse_loops(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (id, rest) = l.trim().split_once(char::is_whitespace)?;
            id.contains('-')
                .then(|| (id.to_string(), rest.trim().to_string()))
        })
        .collect()
}

/// Reads every enabled machine, one thread each so a slow one does not hold
/// the others.
pub fn fetch(run: &Runner) -> Vec<RemoteMachine> {
    let Ok(list) = (run.herdr)(&["machine", "list", "--json"]) else {
        return Vec::new();
    };
    let saved = parse_machines(&list);
    std::thread::scope(|s| {
        let handles: Vec<_> = saved
            .iter()
            .map(|(label, target)| {
                s.spawn(move || {
                    // One snapshot, as herdr's navigator reads; the two
                    // listings only for a server that does not answer it.
                    let snapshot = (run.herdr)(&["--machine", label, "api", "snapshot"]).ok();
                    let unseen = snapshot.as_deref().map(parse_unseen).unwrap_or_default();
                    let parsed = snapshot
                        .as_deref()
                        .and_then(parse_snapshot)
                        .or_else(|| {
                            let agents =
                                (run.herdr)(&["--machine", label, "agent", "list"]).ok()?;
                            let workspaces =
                                (run.herdr)(&["--machine", label, "workspace", "list"]).ok();
                            parse_agents(workspaces.as_deref(), &agents)
                        });
                    let Some(mut agents) = parsed else {
                        return RemoteMachine {
                            label: label.clone(),
                            target: target.clone(),
                            agents: Vec::new(),
                            error: Some("not reachable".to_string()),
                            ..RemoteMachine::default()
                        };
                    };
                    // A machine without herdr-tower, or without ssh access,
                    // still lists its agents; it just has no loops to show.
                    let (mut cpu, mut memory, mut system) = (None, None, None);
                    let mut tower_trouble = None;
                    if !valid_target(target) {
                        tower_trouble = Some("no ssh target".to_string());
                    } else {
                        // The machine's load, the loops and the schedules in
                        // one round trip. `host` goes first: the call's
                        // outcome is the last command's, and a herdr-tower
                        // older than `host` must not cost the loops.
                        let command = format!(
                            "herdr-tower host; echo {HOST_MARK}; herdr-tower loop list; echo {SCHEDULES_MARK}; herdr-tower schedule"
                        );
                        let reply = (run.ssh)(target, &command);
                        if let Err(said) = &reply {
                            tower_trouble = Some(ssh_trouble(said));
                        }
                        if let Ok(text) = reply {
                            let (host, rest) = text.split_once(HOST_MARK).unwrap_or(("", &text));
                            (cpu, memory) = tower_os::host::parse_report(host);
                            system = tower_os::host::parse_system(host);
                            let (loops, schedules) =
                                rest.split_once(SCHEDULES_MARK).unwrap_or((rest, ""));
                            let loops = parse_loops(loops);
                            let schedules = parse_schedules(schedules);
                            for a in &mut agents {
                                a.loop_state = loops.get(&a.id).cloned();
                                let short = tower_state::short_session_id(&a.session_id);
                                if !short.is_empty() {
                                    a.schedules = schedules.get(short).cloned().unwrap_or_default();
                                }
                            }
                        }
                    }
                    RemoteMachine {
                        label: label.clone(),
                        target: target.clone(),
                        agents,
                        unseen,
                        error: None,
                        stale_since: None,
                        cpu,
                        memory,
                        cpu_before: None,
                        system,
                        tower_trouble,
                    }
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// The preview's key for a pane on a saved machine. `@` never starts a local
/// target, and a tab never appears in a machine label or a pane id.
pub fn preview_key(label: &str, pane_id: &str) -> String {
    format!("@{label}\t{pane_id}")
}

/// The machine and pane a [`preview_key`] names; `None` for a local target.
pub fn parse_preview_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix('@')?.split_once('\t')
}

/// One thing to do on a saved machine: who, and what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRequest {
    pub machine: RemoteMachine,
    pub agent: RemoteAgent,
    pub action: RemoteAction,
}

impl RemoteRequest {
    /// Carries it out — blocking, so on a thread of its own — and says how it
    /// went as the event that reports it.
    pub fn run(self, tower: &dyn crate::tower::Tower) -> crate::Event {
        let result = tower.remote_act(&self.machine, &self.agent, &self.action);
        self.done(result)
    }

    /// The event that reports `result`, for a request that could not even be
    /// started as well as for one that ran.
    pub fn done(self, result: Result<String, String>) -> crate::Event {
        crate::Event::RemoteDone {
            label: self.machine.label,
            id: self.agent.id,
            action: self.action,
            result,
        }
    }
}

/// What can be done to an agent on a saved machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    LoopStart,
    LoopStop,
    /// Types the text into the agent and submits it.
    Send(String),
    /// Presses Escape in a working agent.
    Interrupt,
    /// A message at a time, as the `T` field takes it: the time as typed
    /// (`19:00`, `+30m`), read on that machine in its own local time.
    Schedule {
        time: String,
        message: String,
        repeat: tower_state::Repeat,
    },
    /// Cancels these schedules there.
    Unschedule(Vec<String>),
}

/// `text` as one word for a POSIX shell: in single quotes, each `'` closed,
/// escaped and reopened. Nothing inside single quotes is special.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A time as the `T` field takes it, with nothing a shell would read.
fn valid_time(time: &str) -> bool {
    !time.is_empty()
        && !time.starts_with('-')
        && time
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+:-".contains(c))
}

/// An ssh target that cannot be read as an option and carries no shell syntax.
pub fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('-')
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@.-_:[]".contains(c))
}

/// An id or pane id that is safe inside a remote shell command. They come from
/// another machine, so they are checked, not quoted.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_:.".contains(c))
}

/// Does `action` to `agent` on `machine`, and says what happened.
pub fn act(
    run: &Runner,
    machine: &RemoteMachine,
    agent: &RemoteAgent,
    action: &RemoteAction,
) -> Result<String, String> {
    if !valid_id(&agent.id) || !valid_id(&agent.pane_id) {
        return Err(format!("refusing an unusual agent id {:?}", agent.id));
    }
    let on = format!("{} · {}", machine.label, agent.id);
    let over_ssh = |command: String| {
        if !valid_target(&machine.target) {
            return Err(format!("{} has no usable ssh target", machine.label));
        }
        (run.ssh)(&machine.target, &command)
    };
    match action {
        RemoteAction::LoopStart => {
            over_ssh(format!("herdr-tower loop start {}", agent.id))?;
            Ok(format!("idle loop on: {on}"))
        }
        RemoteAction::LoopStop => {
            over_ssh(format!("herdr-tower loop stop {}", agent.id))?;
            Ok(format!("idle loop off: {on}"))
        }
        RemoteAction::Interrupt => {
            over_ssh(format!("herdr-tower interrupt {}", agent.id))?;
            Ok(format!("interrupted: {on}"))
        }
        RemoteAction::Schedule {
            time,
            message,
            repeat,
        } => {
            let message = message.trim();
            if !valid_time(time) {
                return Err(format!("not a time this can send: {time:?}"));
            }
            if message.is_empty() {
                return Err("after the time, the message to send".into());
            }
            // A message that reads as one of `schedule add`'s flags would be
            // taken for it there.
            if message.starts_with("--") {
                return Err("a message cannot start with --".into());
            }
            if agent.session_id.is_empty() || !valid_id(&agent.session_id) {
                return Err("a schedule needs the agent's session id; it has none yet".into());
            }
            let flag = match repeat {
                tower_state::Repeat::Once => "",
                tower_state::Repeat::Daily => " --daily",
                tower_state::Repeat::Weekdays => " --weekdays",
            };
            over_ssh(format!(
                "herdr-tower schedule add {time} --session {}{flag} {}",
                agent.session_id,
                shell_quote(message)
            ))?;
            Ok(format!("⏰ {time} {}: {on}", repeat.label()))
        }
        RemoteAction::Unschedule(ids) => {
            if ids.is_empty() || !ids.iter().all(|id| valid_id(id)) {
                return Err("no schedule to cancel".into());
            }
            over_ssh(format!("herdr-tower schedule rm {}", ids.join(" ")))?;
            Ok(format!("⏰ cancelled {} for {on}", ids.len()))
        }
        RemoteAction::Send(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Err("nothing to send".into());
            }
            (run.herdr)(&[
                "--machine",
                &machine.label,
                "agent",
                "prompt",
                &agent.pane_id,
                text,
            ])?;
            Ok(format!("sent to {on}"))
        }
    }
}

/// Runs `program args` and returns its stdout, or the first line of what it
/// said on stderr; a call that outlasts [`CALL_TIMEOUT`] is killed.
pub fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let mut err = child.stderr.take().ok_or("no stderr")?;
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let body = out_reader.join().unwrap_or_default();
                let said = err_reader.join().unwrap_or_default();
                return if status.success() {
                    Ok(body)
                } else {
                    let line = said
                        .lines()
                        .chain(body.lines())
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("failed");
                    Err(line.trim().to_string())
                };
            }
            Ok(None) if start.elapsed() < CALL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(30));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out"));
            }
        }
    }
}

/// What a band says when the ssh call to a machine's herdr-tower fails,
/// from the first line the failure printed:
///
/// - `herdr-tower not installed` when the shell there found no such command,
///   as bash (`command not found`), zsh, sh (`not found`) and Windows (`not
///   recognized`) each say it;
/// - `ssh failed: …` when ssh itself could not get there — no such host, no
///   key, a timeout — with its reason;
/// - `herdr-tower failed: …` when it got there and the command failed some
///   other way.
///
/// The reason is cut short: it shares the band with the machine's name.
pub fn ssh_trouble(said: &str) -> String {
    let lower = said.to_lowercase();
    if lower.contains("herdr-tower")
        && ["not found", "not recognized", "no such file"]
            .iter()
            .any(|w| lower.contains(w))
    {
        return "herdr-tower not installed".to_string();
    }
    let short = |s: &str| {
        s.trim()
            .chars()
            .take(60)
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    // ssh's reason starts at its own words; what comes before them — the
    // `ssh: ` it prefixes, a `user@host: ` — the band already says.
    let ssh_reason = [
        "permission denied",
        "host key verification failed",
        "connection refused",
        "connection timed out",
        "connection closed",
        "could not resolve",
        "no route to host",
        "timed out",
    ]
    .iter()
    .filter_map(|w| lower.find(w))
    .min();
    if let Some(at) = ssh_reason {
        format!("ssh failed: {}", short(&said[at..]))
    } else if said.trim().is_empty() || said.trim() == "failed" {
        "herdr-tower failed".to_string()
    } else {
        format!("herdr-tower failed: {}", short(said))
    }
}

/// `herdr <args>`.
pub fn run_herdr(herdr: &str, args: &[&str]) -> Result<String, String> {
    run(herdr, args)
}

/// `command` on `target` over ssh, never asking for a password, with the
/// directories herdr-tower installs into on the path.
pub fn run_ssh(ssh: &str, target: &str, command: &str) -> Result<String, String> {
    let script = format!("export PATH=\"$HOME/.local/bin:/opt/homebrew/bin:$PATH\"; {command}");
    run(
        ssh,
        &[
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=6",
            "--",
            target,
            &script,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_failed_ssh_call_says_why_on_the_band() {
        let cases = [
            ("bash: line 1: herdr-tower: command not found", "herdr-tower not installed"),
            ("zsh:1: command not found: herdr-tower", "herdr-tower not installed"),
            ("sh: 1: herdr-tower: not found", "herdr-tower not installed"),
            (
                "herdr-tower : The term 'herdr-tower' is not recognized as the name of a cmdlet",
                "herdr-tower not installed",
            ),
            (
                "'herdr-tower' is not recognized as an internal or external command,",
                "herdr-tower not installed",
            ),
            (
                "ssh: Could not resolve hostname nosuchhost.invalid: nodename nor servname provided, or not known",
                "ssh failed: Could not resolve hostname nosuchhost.invalid: nodename nor",
            ),
            (
                "nobody@desk: Permission denied (publickey,password).",
                "ssh failed: Permission denied (publickey,password).",
            ),
            ("Host key verification failed.", "ssh failed: Host key verification failed."),
            ("ssh timed out", "ssh failed: timed out"),
            ("error: the store is locked", "herdr-tower failed: error: the store is locked"),
            ("failed", "herdr-tower failed"),
            ("", "herdr-tower failed"),
        ];
        for (said, want) in cases {
            assert_eq!(ssh_trouble(said), want, "{said:?}");
        }
        // However long the reason, the band gets a short one.
        let long = format!("ssh: connection timed out {}", "x".repeat(200));
        assert_eq!(
            ssh_trouble(&long).chars().count(),
            "ssh failed: ".len() + 60
        );
    }

    /// The machine's agents still come through herdr; its band says why
    /// there is nothing from its herdr-tower.
    #[test]
    fn a_machine_without_herdr_tower_lists_its_agents_and_says_so() {
        let herdr = |args: &[&str]| -> Result<String, String> {
            match args {
                ["machine", "list", "--json"] => Ok(
                    r#"[{"label":"bare","target":"u@bare"},{"label":"nokey","target":"u@nokey"},{"label":"none"}]"#.into(),
                ),
                [_, _, "agent", "list"] => {
                    Ok(r#"{"result":{"agents":[{"pane_id":"w2:p6","agent":"claude","workspace_id":"w2"}]}}"#.into())
                }
                _ => Err("no".into()),
            }
        };
        let ssh = |target: &str, _: &str| -> Result<String, String> {
            Err(match target {
                "u@bare" => "bash: line 1: herdr-tower: command not found".into(),
                _ => "u@nokey: Permission denied (publickey).".into(),
            })
        };
        let got = fetch(&Runner {
            herdr: &herdr,
            ssh: &ssh,
        });
        let trouble = |label: &str| {
            got.iter()
                .find(|m| m.label == label)
                .and_then(|m| m.tower_trouble.clone())
        };
        assert!(got.iter().all(|m| m.agents.len() == 1 && m.error.is_none()));
        assert_eq!(
            trouble("bare").as_deref(),
            Some("herdr-tower not installed")
        );
        assert_eq!(
            trouble("nokey").as_deref(),
            Some("ssh failed: Permission denied (publickey).")
        );
        assert_eq!(trouble("none").as_deref(), Some("no ssh target"));
    }

    #[test]
    fn machine_list_keeps_enabled_labels_and_targets() {
        let json = r#"[{"label":"a","target":"u@a","enabled":true},{"label":"b","enabled":false},{"label":"c"}]"#;
        assert_eq!(
            parse_machines(json),
            vec![("a".into(), "u@a".into()), ("c".into(), String::new())]
        );
        assert!(parse_machines("not json").is_empty());
    }

    #[test]
    fn agents_take_the_workspace_label_or_the_id() {
        let ws = r#"{"result":{"workspaces":[{"workspace_id":"w2","label":"diskann"}]}}"#;
        let ag = r#"{"result":{"agents":[
            {"pane_id":"w2:p6","agent":"claude","agent_status":"idle","workspace_id":"w2"},
            {"pane_id":"w9:p1","agent":"codex","agent_status":"working","workspace_id":"w9"}]}}"#;
        let got = parse_agents(Some(ws), ag).unwrap();
        assert_eq!(got[0].workspace, "diskann");
        assert_eq!(got[0].status, "idle");
        assert_eq!(got[0].id, "claude-w2:p6");
        assert_eq!(got[1].workspace, "[w9]");
        // No workspace list: ids only, and still an answer.
        assert_eq!(parse_agents(None, ag).unwrap()[0].workspace, "[w2]");
        assert!(parse_agents(None, "garbage").is_none());
    }

    #[test]
    fn loop_list_lines_are_keyed_by_agent_id() {
        let text = "claude-w2:p6  (desc)  idle=30m  sent=11  expires=119h  paused: agent is dead\nno idle loops are configured.\n";
        let got = parse_loops(text);
        assert_eq!(
            got["claude-w2:p6"],
            "(desc)  idle=30m  sent=11  expires=119h  paused: agent is dead"
        );
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn a_machine_that_does_not_answer_is_kept_with_a_reason() {
        let herdr = |args: &[&str]| -> Result<String, String> {
            match args {
                ["machine", "list", "--json"] => {
                    Ok(r#"[{"label":"up","target":"u@up"},{"label":"down","target":"u@down"}]"#.into())
                }
                ["--machine", "up", "agent", "list"] => {
                    Ok(r#"{"result":{"agents":[{"pane_id":"w2:p6","agent":"claude","workspace_id":"w2"}]}}"#.into())
                }
                _ => Err("no".into()),
            }
        };
        let ssh = |target: &str, command: &str| -> Result<String, String> {
            assert_eq!(target, "u@up");
            assert!(command.starts_with("herdr-tower host; echo "), "{command}");
            Ok(format!(
                "cpu busy=5 total=20\nmem used=1 total=4\nsys arch=aarch64 tower=v0.57.4 os=Ubuntu 26.04\n{HOST_MARK}\nclaude-w2:p6  x  idle=10m  sent=0  expires=1h\n{SCHEDULES_MARK}\nnothing scheduled\n"
            ))
        };
        let got = fetch(&Runner {
            herdr: &herdr,
            ssh: &ssh,
        });
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].error, None);
        assert_eq!(
            got[0].agents[0].loop_state.as_deref(),
            Some("x  idle=10m  sent=0  expires=1h")
        );
        assert_eq!(
            got[0].cpu,
            Some(tower_os::host::CpuTicks { busy: 5, total: 20 })
        );
        assert_eq!(got[0].memory.map(|m| m.used_pct()), Some(25.0));
        assert_eq!(
            got[0].system.as_ref().map(|s| s.label()).as_deref(),
            Some("Ubuntu 26.04 arm64 (herdr-tower v0.57.4)")
        );
        assert_eq!(got[1].label, "down");
        assert!(got[1].error.is_some());
        assert_eq!((got[1].cpu, got[1].memory), (None, None));
        assert_eq!(got[1].system, None);
    }

    /// A machine whose herdr-tower predates `host` says only "unknown
    /// command" for it, on stderr: its loops and schedules still come through,
    /// and it simply has no load to show.
    #[test]
    fn a_machine_without_host_still_shows_its_loops() {
        let herdr = |args: &[&str]| -> Result<String, String> {
            match args {
                ["machine", "list", "--json"] => Ok(r#"[{"label":"old","target":"u@old"}]"#.into()),
                ["--machine", "old", "agent", "list"] => {
                    Ok(r#"{"result":{"agents":[{"pane_id":"w2:p6","agent":"claude","workspace_id":"w2"}]}}"#.into())
                }
                _ => Err("no".into()),
            }
        };
        let ssh = |_: &str, _: &str| -> Result<String, String> {
            Ok(format!(
                "{HOST_MARK}\nclaude-w2:p6  x  idle=10m  sent=0  expires=1h\n{SCHEDULES_MARK}\nnothing scheduled\n"
            ))
        };
        let got = fetch(&Runner {
            herdr: &herdr,
            ssh: &ssh,
        });
        assert_eq!(
            got[0].agents[0].loop_state.as_deref(),
            Some("x  idle=10m  sent=0  expires=1h")
        );
        assert_eq!((got[0].cpu, got[0].memory), (None, None));
        assert_eq!(got[0].load(), crate::app::HostLoad::default());
        assert_eq!(got[0].system, None, "nothing to say what it runs");
    }

    #[test]
    fn the_busy_share_is_between_this_reading_and_the_one_before() {
        use tower_os::host::{CpuTicks, Memory};
        let mut m = RemoteMachine {
            cpu: Some(CpuTicks {
                busy: 30,
                total: 200,
            }),
            memory: Some(Memory { used: 3, total: 4 }),
            ..RemoteMachine::default()
        };
        assert_eq!(m.load().cpu_pct, None, "one reading is not a share");
        assert_eq!(m.load().memory, Some(Memory { used: 3, total: 4 }));
        m.cpu_before = Some(CpuTicks {
            busy: 10,
            total: 100,
        });
        assert_eq!(m.load().cpu_pct, Some(20.0));
    }

    fn machine() -> (RemoteMachine, RemoteAgent) {
        let agent = RemoteAgent {
            workspace: "w".into(),
            pane_id: "w2:p6".into(),
            agent: "claude".into(),
            status: "idle".into(),
            id: "claude-w2:p6".into(),
            loop_state: None,
            schedules: Vec::new(),
            title: String::new(),
            tab: String::new(),
            session_id: String::new(),
            cwd: String::new(),
        };
        let m = RemoteMachine {
            label: "gpu".into(),
            target: "alice@gpu".into(),
            agents: vec![agent.clone()],
            error: None,
            stale_since: None,
            ..Default::default()
        };
        (m, agent)
    }

    #[test]
    fn actions_run_the_machines_own_commands() {
        let (m, a) = machine();
        let calls = Mutex::new(Vec::<String>::new());
        let herdr = |args: &[&str]| -> Result<String, String> {
            calls
                .lock()
                .unwrap()
                .push(format!("herdr {}", args.join("|")));
            Ok(String::new())
        };
        let ssh = |target: &str, command: &str| -> Result<String, String> {
            calls
                .lock()
                .unwrap()
                .push(format!("ssh {target} {command}"));
            Ok(String::new())
        };
        let run = Runner {
            herdr: &herdr,
            ssh: &ssh,
        };
        act(&run, &m, &a, &RemoteAction::LoopStart).unwrap();
        act(&run, &m, &a, &RemoteAction::LoopStop).unwrap();
        act(&run, &m, &a, &RemoteAction::Interrupt).unwrap();
        act(&run, &m, &a, &RemoteAction::Send(" go on ".into())).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "ssh alice@gpu herdr-tower loop start claude-w2:p6",
                "ssh alice@gpu herdr-tower loop stop claude-w2:p6",
                "ssh alice@gpu herdr-tower interrupt claude-w2:p6",
                "herdr --machine|gpu|agent|prompt|w2:p6|go on",
            ]
        );
        assert!(act(&run, &m, &a, &RemoteAction::Send("  ".into())).is_err());
    }

    #[test]
    fn nothing_odd_reaches_a_remote_shell() {
        let (mut m, mut a) = machine();
        let herdr = |_: &[&str]| -> Result<String, String> { panic!("must not run") };
        let ssh = |_: &str, _: &str| -> Result<String, String> { panic!("must not run") };
        let run = Runner {
            herdr: &herdr,
            ssh: &ssh,
        };
        a.id = "claude-w2:p6; rm -rf ~".into();
        assert!(act(&run, &m, &a, &RemoteAction::LoopStart).is_err());
        a.id = "claude-w2:p6".into();
        m.target = "-oProxyCommand=x".into();
        assert!(act(&run, &m, &a, &RemoteAction::LoopStart).is_err());
        assert!(!valid_target("a b"));
        assert!(valid_target("alice@carol-gpu.tail1a2b3c.ts.net"));
        assert!(!valid_id("--force"));
    }
    #[test]
    fn a_snapshot_is_read_in_sidebar_order_with_the_panes_names() {
        let json = r#"{"result":{"snapshot":{
            "workspaces":[{"workspace_id":"w9","label":"second"},{"workspace_id":"w2","label":"first"}],
            "tabs":[{"tab_id":"w2:t2","label":"[w2:t2] build","number":2},{"tab_id":"w2:t1","workspace_id":"w2","label":"[w2]top","number":1},
                    {"tab_id":"w9:t1","label":"1","number":1}],
            "panes":[{"pane_id":"w2:p7","label":"[w2:p7] v400-build-gap"},{"pane_id":"w2:p6","label":"[w2:p6]"},
                     {"pane_id":"w9:p1"}],
            "agents":[
              {"pane_id":"w2:p6","agent":"claude","agent_status":"idle","workspace_id":"w2","tab_id":"w2:t2",
               "terminal_title_stripped":"Dbstore build","cwd":"/home/q","foreground_cwd":"/home/q/work",
               "agent_session":{"value":"b8120ca6-e8d0"}},
              {"pane_id":"w9:p1","agent":"codex","agent_status":"working","workspace_id":"w9","tab_id":"w9:t1"},
              {"pane_id":"w2:p7","agent":"codex","agent_status":"done","workspace_id":"w2","tab_id":"w2:t2"},
              {"pane_id":"w2:p1","agent":"codex","agent_status":"idle","workspace_id":"w2","tab_id":"w2:t1"}]}}}"#;
        let got = parse_snapshot(json).unwrap();
        let order: Vec<_> = got.iter().map(|a| a.pane_id.as_str()).collect();
        // Workspace in sidebar order, then tab number, then pane order.
        assert_eq!(order, ["w9:p1", "w2:p1", "w2:p7", "w2:p6"]);
        let p6 = got.iter().find(|a| a.pane_id == "w2:p6").unwrap();
        assert_eq!(p6.tab, "build", "the tab's id prefix is dropped");
        assert_eq!(
            p6.title, "Dbstore build",
            "an id-only pane name gives way to the title"
        );
        assert_eq!(p6.cwd, "/home/q/work", "the foreground folder first");
        assert_eq!(p6.session_id, "b8120ca6-e8d0");
        let p7 = got.iter().find(|a| a.pane_id == "w2:p7").unwrap();
        assert_eq!(p7.title, "v400-build-gap", "the pane's own name first");
        let p1 = got.iter().find(|a| a.pane_id == "w2:p1").unwrap();
        assert_eq!(p1.tab, "top", "a workspace-id prefix is dropped too");
        assert!(parse_snapshot(r#"{"result":{"agents":[]}}"#).is_none());
    }

    /// What desk said on 2026-10-22: a `claude` under `sudo su -` titled its
    /// pane but was not among the agents, beside two shells and a root shell
    /// herdr could not read the folder of either.
    #[test]
    fn a_pane_titled_like_an_agent_but_not_listed_is_unseen() {
        let json = r#"{"result":{"snapshot":{
            "workspaces":[{"workspace_id":"w4","label":"dock"},{"workspace_id":"w3","label":"[w3]~"}],
            "tabs":[{"tab_id":"w3:t1","workspace_id":"w3","label":"[w3]1","number":1}],
            "panes":[
              {"pane_id":"w4:p1","workspace_id":"w4","terminal_title":"alice@desk: ~","terminal_title_stripped":"alice@desk: ~","foreground_cwd":"/home/alice"},
              {"pane_id":"w3:p2","workspace_id":"w3","tab_id":"w3:t1","terminal_title":"root@desk:/tmp","terminal_title_stripped":"root@desk:/tmp"},
              {"pane_id":"w3:p1","workspace_id":"w3","tab_id":"w3:t1","label":"[w3:p1]","terminal_title":"✳ Claude Code","terminal_title_stripped":"Claude Code"},
              {"pane_id":"w3:p3","workspace_id":"w3","tab_id":"w3:t1","terminal_title":"◑ Dbstore build","terminal_title_stripped":"Dbstore build","foreground_cwd":"/home/alice"},
              {"pane_id":"w3:p4","workspace_id":"w3","tab_id":"w3:t1","terminal_title":"codex","terminal_title_stripped":"codex","foreground_cwd":"/home/alice"},
              {"pane_id":"w3:p5","workspace_id":"w3","tab_id":"w3:t1","terminal_title":"✳ Listed","terminal_title_stripped":"Listed","foreground_cwd":"/home/alice"}],
            "agents":[{"pane_id":"w3:p5","agent":"claude","workspace_id":"w3","tab_id":"w3:t1"}]}}}"#;
        let got = parse_unseen(json);
        let panes: Vec<_> = got.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(
            panes,
            ["w3:p1", "w3:p3", "w3:p4"],
            "titled like agents, not listed, in pane order"
        );
        let p1 = &got[0];
        assert_eq!(
            p1.title, "Claude Code",
            "an id-only name gives way to the title"
        );
        assert_eq!(p1.workspace, "[w3]~");
        assert_eq!(p1.tab, "1");
        assert!(p1.other_user, "no folder herdr could read: another user's");
        assert!(!got[1].other_user);
        assert!(parse_unseen("not json").is_empty());
        assert!(parse_unseen(r#"{"result":{"agents":[]}}"#).is_empty());
    }

    #[test]
    fn a_title_reads_like_an_agent_by_its_glyph_or_its_name() {
        assert!(looks_like_agent("✳ Claude Code", "Claude Code"));
        assert!(looks_like_agent("⠂ fix it", "fix it"));
        assert!(looks_like_agent("Gemini - ~/x", "Gemini - ~/x"));
        assert!(!looks_like_agent(
            "alice@desk: ~/claude",
            "alice@desk: ~/claude"
        ));
        assert!(!looks_like_agent("vim", "vim"));
        assert!(!looks_like_agent("✳", ""), "nothing left to show");
        assert!(!looks_like_agent("", ""));
    }

    #[test]
    fn one_snapshot_is_all_a_machine_is_asked_for() {
        let calls = Mutex::new(Vec::<String>::new());
        let herdr = |args: &[&str]| -> Result<String, String> {
            calls.lock().unwrap().push(args.join(" "));
            match args {
                ["machine", "list", "--json"] => Ok(r#"[{"label":"up","target":"u@up"}]"#.into()),
                ["--machine", "up", "api", "snapshot"] => Ok(
                    r#"{"result":{"snapshot":{"agents":[{"pane_id":"w2:p6","agent":"claude","workspace_id":"w2"}]}}}"#
                        .into(),
                ),
                _ => Err("no".into()),
            }
        };
        let ssh = |_: &str, _: &str| -> Result<String, String> { Ok(String::new()) };
        let got = fetch(&Runner {
            herdr: &herdr,
            ssh: &ssh,
        });
        assert_eq!(got[0].agents.len(), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            ["machine list --json", "--machine up api snapshot"]
        );
    }

    /// A `schedule` line laid out exactly as that command writes it.
    fn schedule_line(id: &str, when: &str, to: &str, next: &str, message: &str) -> String {
        format!("{id:<8} {when:<17} {to:<24} {next:<12} {:>4}  {message}", 0)
    }

    #[test]
    fn schedules_are_keyed_by_the_short_session_id_they_name() {
        let text = [
            format!(
                "{:<8} {:<17} {:<24} {:<12} {:>4}  MESSAGE",
                "ID", "WHEN", "TO", "NEXT", "SENT"
            ),
            schedule_line(
                "t3f2a91",
                "daily 19:00",
                "session 840316f8",
                "19:00",
                "wrap up  now",
            ),
            schedule_line(
                "t9",
                "once 2026-10-11 09:00",
                "session 840316f8",
                "tomorrow 09:00",
                "x",
            ),
            schedule_line(
                "t4",
                "weekdays 09:00",
                "every handoff session",
                "09:00",
                "go",
            ),
            schedule_line("t5", "daily 08:00", "workspace dbstore", "08:00", "go"),
        ]
        .join("\n");
        let got = parse_schedules(&text);
        assert_eq!(got.len(), 1, "only session targets: {got:?}");
        let mine = &got["840316f8"];
        assert_eq!(
            mine[0],
            RemoteSchedule {
                id: "t3f2a91".into(),
                next: "19:00".into()
            }
        );
        assert_eq!(mine[1].id, "t9");
        assert!(parse_schedules("nothing scheduled — ...").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_quoted_message_reaches_the_shell_as_it_was_typed() {
        for text in [
            "it's done",
            "$(touch /tmp/x) `id` \\ \"q\"",
            "two\nlines",
            "'",
        ] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(text)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), text);
        }
    }

    #[test]
    fn a_schedule_runs_that_machines_schedule_add_and_refuses_what_it_cannot_send() {
        let (m, mut a) = machine();
        a.session_id = "b8120ca6-e8d0".into();
        let calls = Mutex::new(Vec::<String>::new());
        let herdr = |_: &[&str]| -> Result<String, String> { Ok(String::new()) };
        let ssh = |_: &str, command: &str| -> Result<String, String> {
            calls.lock().unwrap().push(command.to_string());
            Ok(String::new())
        };
        let run = Runner {
            herdr: &herdr,
            ssh: &ssh,
        };
        let schedule = |time: &str, message: &str, repeat| RemoteAction::Schedule {
            time: time.into(),
            message: message.into(),
            repeat,
        };
        act(
            &run,
            &m,
            &a,
            &schedule("19:00", "it's time", tower_state::Repeat::Daily),
        )
        .unwrap();
        act(
            &run,
            &m,
            &a,
            &RemoteAction::Unschedule(vec!["t3f2a91".into(), "t9".into()]),
        )
        .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "herdr-tower schedule add 19:00 --session b8120ca6-e8d0 --daily 'it'\\''s time'",
                "herdr-tower schedule rm t3f2a91 t9",
            ]
        );
        for (time, message) in [
            ("19:00;rm", "x"),
            ("-1", "x"),
            ("19:00", "--daily"),
            ("19:00", " "),
        ] {
            assert!(act(
                &run,
                &m,
                &a,
                &schedule(time, message, tower_state::Repeat::Once)
            )
            .is_err());
        }
        a.session_id.clear();
        assert!(act(
            &run,
            &m,
            &a,
            &schedule("19:00", "x", tower_state::Repeat::Once)
        )
        .is_err());
        assert_eq!(calls.lock().unwrap().len(), 2, "nothing refused was sent");
    }
}
