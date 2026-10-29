// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What the control tower does with herdr that herdr does not do by itself.
//!
//! This used to be a seam: four role interfaces (`Observer`, `Navigator`,
//! `Spawner`, `Sender`) plus five optional ones, so a second backend could be
//! plugged in. herdr is the only backend, and one implementation behind an
//! interface is indirection with nothing on the other side of it, so the seam
//! is gone and what is left is what the seam was carrying:
//!
//!   - **selection and discovery** — the current server plus every standard
//!     named-session socket ([`select`], [`discover_backends`])
//!   - **compositions** — several herdr calls that only make sense together:
//!     resolve a label then create a tab then run a command ([`spawn_in`]),
//!     ask what lives in a pane before choosing how to write to it
//!     ([`send_text`]), attach this terminal ([`attach_cmd`])
//!   - **translation** — the tower's state vocabulary into herdr's
//!     ([`wait_for_agent`])
//!   - **policy** — turning a firehose of revisions into signals a redraw loop
//!     can live with ([`Watch`])
//!
//! Everything that is simply more herdr API — subscribing, planting sidebar
//! tokens, creating tabs, starting agents — lives in `herdrx`, and the pane
//! conversion and observation mapping live in `tower_scan`. Neither is repeated
//! here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use herdrx::{AgentStatus, Herdr, TabOptions};
use tower_state::AgentState;

mod watch;
pub use watch::{Stopper, Wait, Watch, COALESCE_WINDOW};

/// A Herdr server managed by the one resident supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    /// Empty for the default socket; otherwise a stable, short namespace.
    pub id: String,
    pub socket: PathBuf,
}

impl Backend {
    pub fn client(&self) -> Herdr {
        Herdr::at(&self.socket)
    }
}

/// Discovers the default Herdr server and every standard named server.
///
/// The default is retained even before its socket exists so a daemon started at
/// login begins supervising it as soon as Herdr comes up. Named sessions are
/// re-discovered on every pass from `sessions/*/herdr.sock`.
pub fn discover_backends(home: &Path) -> std::io::Result<Vec<Backend>> {
    // herdr's own folder: `~/.config/herdr`, or `%APPDATA%\herdr` on Windows.
    let root = herdrx::config_dir(&|k: &str| std::env::var(k).ok(), &home.to_string_lossy());
    let default = root.join("herdr.sock");
    let mut sockets = vec![default.clone()];
    let sessions = root.join("sessions");
    match std::fs::read_dir(sessions) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let socket = entry.path().join("herdr.sock");
                if socket.exists() {
                    sockets.push(socket);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    sockets.sort();
    sockets.dedup();
    Ok(sockets
        .into_iter()
        .map(|socket| Backend {
            id: herdrx::backend_id(&socket, &default),
            socket,
        })
        .collect())
}

/// A key unique across all Herdr servers in one state store.
pub fn target_key(backend: &str, pane_id: &str) -> String {
    if backend.is_empty() {
        pane_id.to_string()
    } else {
        format!("{backend}:{pane_id}")
    }
}

/// Reverses [`target_key`]. A raw default-server pane such as `w2:p3` is left
/// untouched; backend IDs have the fixed `b` plus sixteen-hex-digit shape.
pub fn split_target(target: &str) -> (&str, &str) {
    let Some((head, pane)) = target.split_once(':') else {
        return ("", target);
    };
    if head.len() == 17
        && head.starts_with('b')
        && head[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        (head, pane)
    } else {
        ("", target)
    }
}

/// The only backend's name. Status output and warnings refer to it.
pub const BACKEND_HERDR: &str = "herdr";

/// Anything that can go wrong driving the backend.
#[derive(Debug)]
pub enum Error {
    /// herdr refused, or could not be reached.
    Herdr(herdrx::Error),
    /// Text was handed to herdr for a pane and the exchange failed — see
    /// [`Error::may_have_landed`].
    Delivery(herdrx::Error),
    /// `herdr` is not on `PATH`, so this terminal has no way to attach.
    NoExecutable,
    /// No workspace carries that label.
    NoWorkspace(String),
    /// Every state the caller asked to wait for is one herdr has no word for.
    NothingToWaitFor,
    /// The place was made, but the command in it failed to start.
    ///
    /// The pane ID rides along because the pane is real: the tab stays open on
    /// purpose so somebody can attach and see what blocked. Collapsing this
    /// into a plain failure would leak a tab nobody can name.
    Spawned {
        pane_id: String,
        source: herdrx::Error,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Herdr(e) | Error::Delivery(e) => write!(f, "{e}"),
            Error::NoExecutable => write!(f, "herdr executable not found on PATH"),
            Error::NoWorkspace(l) => write!(f, "no workspace labeled {l:?}"),
            Error::NothingToWaitFor => write!(
                f,
                "herdr has no word for any of the requested states, so there is nothing to wait for"
            ),
            Error::Spawned { pane_id, source } => {
                write!(f, "the command in pane {pane_id} failed: {source}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Whether text whose sending failed may be in the pane anyway.
    ///
    /// Only a failure of the delivery itself can say yes: a failed lookup
    /// before it sent nothing. Resending what may have landed types it twice,
    /// so a caller that resends asks this first.
    pub fn may_have_landed(&self) -> bool {
        matches!(self, Error::Delivery(e) if e.may_have_acted())
    }
}

impl From<herdrx::Error> for Error {
    fn from(e: herdrx::Error) -> Self {
        Error::Herdr(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The backend to supervise with.
///
/// herdr sees what lives inside a pane and reports kind, lifecycle state, and
/// session ID directly, so the tower never guesses at process names or reads
/// screens. There is nothing to choose between, so this resolves the socket
/// and hands back a client.
///
/// `env` is a parameter so a caller can drive the resolution — and so it is the
/// *same* environment the rest of the selection consults, rather than a second
/// implicit lookup that can disagree with it.
pub fn select<F>(env: F) -> Herdr
where
    F: Fn(&str) -> Option<String>,
{
    match socket_from(env) {
        Some(p) => Herdr::at(p),
        // No resolvable home and no explicit socket: let the client fall back
        // to its own default, which fails with a message about the socket
        // rather than about a home directory.
        None => Herdr::default(),
    }
}

/// Resolves the herdr socket through the environment the selection is reading.
///
/// `HOME` comes from `env` too, so the injected environment stays
/// authoritative end to end: reading it from the process instead would make
/// the function untestable without mutating the real environment. Note the
/// empty-string guard: `$HOME` set to `""` is not a home directory, and
/// without the filter the socket resolves to `/.config/herdr/herdr.sock`
/// instead of failing.
pub fn socket_from<F>(env: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let home = env("HOME").filter(|h| !h.is_empty())?;
    Some(herdrx::socket_path(&env, &home))
}

/// The command that attaches **this** terminal to the herdr session.
///
/// herdr splits this into two gestures: attaching connects a terminal to a
/// session, focusing selects what it shows.
/// This builds the first gesture and performs neither — the caller selects the
/// pane once the attach is actually running, which is the only part of the
/// damage below that is ours to avoid.
///
/// The focus that follows is the session's, not this client's: every terminal
/// attached to the session follows it. There is no client-scoped alternative in
/// the API — `herdr api schema` carries no such parameter, `pane.focus` takes a
/// pane id and nothing else, and `herdr session attach` takes a name and nothing
/// else — so a tower running over ssh moves the pane on whatever desk is also
/// attached. Sending it from here, before the command had even been run, moved
/// that desk's view for an attach that might never start. See
/// `tower_ui::Tower::inside`.
///
/// The session name is deliberately not taken from a workspace label. A label
/// is a place *inside* a session, not the session itself, and passing one to
/// `herdr session attach` would name the wrong thing. The process attaches to
/// whatever this environment already points at: `HERDR_SESSION`, else the
/// default session, which bare `herdr` launches or attaches to.
pub fn attach_cmd<F>(env: F) -> Result<(PathBuf, Vec<String>)>
where
    F: Fn(&str) -> Option<String>,
{
    let bin = look_path("herdr", &env).ok_or(Error::NoExecutable)?;
    match env("HERDR_SESSION").filter(|s| !s.is_empty()) {
        Some(name) => Ok((bin, vec!["session".into(), "attach".into(), name])),
        None => Ok((bin, Vec::new())),
    }
}

/// Finds an executable on `PATH`.
fn look_path<F>(name: &str, env: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    // Executable *and* a file: a directory named `herdr` on PATH has the
    // execute bit set too, and running it fails with a confusing permission
    // error rather than "not found".
    tower_os::look_path(name, &env("PATH")?)
}

/// Creates a tab in the workspace with that label and, when `command` is given,
/// runs it there. Returns the new pane's ID.
///
/// `None` for `command` leaves a bare shell — the caller types into it later.
/// The command is typed into the tab's shell rather than made the tab's
/// process, so a command that dies instantly leaves its error message on screen
/// instead of taking the pane with it. A herdr tab is a shell pane already.
pub fn spawn_in(
    h: &Herdr,
    session: &str,
    label: &str,
    dir: &str,
    command: Option<&str>,
) -> Result<String> {
    let id = h
        .workspace_id_by_label(session)?
        .ok_or_else(|| Error::NoWorkspace(session.to_string()))?;
    spawn(h, &id, label, dir, command)
}

/// [`spawn_in`] for the workspace this process belongs to.
///
/// Our *own* workspace, not the focused one. Focus is user-interface state and
/// moves on its own: `wt new` run from workspace A while another herdr client
/// has B in front of it would put the worker tab in B, away from the session
/// that asked for it and away from the panes the coordinator is watching. That
/// is not hypothetical for this tool — the whole point is orchestrating agents
/// in the background while somebody works somewhere else.
///
/// The focused workspace stays as the fallback, because a caller outside herdr
/// has no pane of its own and "where the user is looking" is then the only
/// answer available.
pub fn spawn_in_focused(
    h: &Herdr,
    label: &str,
    dir: &str,
    command: Option<&str>,
) -> Result<String> {
    let id =
        own_workspace(h).or_else(|| h.focused_workspace().ok().flatten().map(|w| w.workspace_id));
    let id = id.ok_or_else(|| Error::NoWorkspace("<focused>".to_string()))?;
    spawn(h, &id, label, dir, command)
}

/// The workspace holding the pane this process runs in, when it runs in one.
///
/// A failure to resolve is `None` rather than an error: it only means the
/// fallback has to answer, and a `wt new` that refused to run because a
/// `pane.get` hiccupped would be worse than one that used the focused
/// workspace.
fn own_workspace(h: &Herdr) -> Option<String> {
    let me = herdrx::current_pane_id()?;
    let p = h.pane(&me).ok()?;
    (!p.workspace_id.is_empty()).then_some(p.workspace_id)
}

fn spawn(
    h: &Herdr,
    workspace_id: &str,
    label: &str,
    dir: &str,
    command: Option<&str>,
) -> Result<String> {
    let (_tab, pane) = h.new_tab(&TabOptions {
        workspace_id: Some(workspace_id.to_string()),
        label: Some(label.to_string()),
        cwd: Some(dir.to_string()),
        ..TabOptions::default()
    })?;
    name_new_place(h, &pane.pane_id, label);
    if let Some(cmd) = command.filter(|c| !c.is_empty()) {
        // The pane exists either way, and it is left open on purpose so
        // somebody can attach and see what blocked — so the failure has to
        // carry the ID, or the tab becomes unreachable.
        if let Err(source) = h.run(&pane.pane_id, cmd) {
            return Err(Error::Spawned {
                pane_id: pane.pane_id,
                source,
            });
        }
    }
    Ok(pane.pane_id)
}

/// Creates a workspace in the background and returns its first pane's ID.
///
/// Background is the point: yanking the user's view to a place being prepared
/// for someone else would be a regression, not a feature.
///
/// The workspace keeps `label` as it is — it is found by that name elsewhere,
/// and a sidebar style rule may match it exactly — while its first tab (herdr
/// numbers it "1") and that tab's pane are named `[<id>] <label>` the way
/// `spawn_in` names its own, best effort for the same reason.
pub fn new_session(h: &Herdr, label: &str, dir: &str) -> Result<String> {
    let (_ws, _tab, pane) = h.new_workspace(label, dir, false)?;
    name_new_place(h, &pane.pane_id, label);
    Ok(pane.pane_id)
}

/// Puts the new pane's ID in front of `label` ([`Herdr::name_with_id`]),
/// unless `id_labels` is off.
///
/// The bare label already went into the create call, so the place is named
/// even if these renames are lost; the IDs can only be added now that herdr
/// has said which tab and pane it made. Cosmetic, so never a failed launch —
/// and the supervisor's reconcile puts right whatever this missed.
fn name_new_place(h: &Herdr, pane_id: &str, label: &str) {
    if tower_config::load().id_labels_enabled() {
        h.name_with_id(pane_id, label);
    }
}

/// Delivers text to a pane, choosing the right door for whatever lives there.
///
/// herdr has two doors, and the difference matters. A pane hosting an agent
/// goes through `agent.prompt`, which honors bracketed paste, spaces the Enter
/// correctly, and **refuses to write to an agent sitting at an approval
/// dialog** — typing blindly cannot see that, and a broadcast would land inside
/// a permission prompt. A plain shell pane goes through `pane.send_input`,
/// which submits text and Enter atomically.
///
/// Deciding costs one `pane.get`. That is worth paying: guessing wrong means
/// typing an instruction into a confirmation dialog.
pub fn send_text(h: &Herdr, pane_id: &str, text: &str) -> Result<()> {
    let pane = h.pane(pane_id)?;
    if pane.has_agent() {
        h.prompt(pane_id, text, None).map_err(Error::Delivery)?;
    } else {
        h.run(pane_id, text).map_err(Error::Delivery)?;
    }
    Ok(())
}

/// Hands the waiting to herdr, which knows when its own agents settle. The
/// caller stops asking; the server answers once.
///
/// States herdr has no word for are dropped rather than approximated. If that
/// leaves nothing, the wait is refused: an empty `until` means herdr's own
/// default (the first settled idle, done, or blocked), so passing the empty
/// remainder through would silently widen "wait until DEAD" into "wait until
/// anything" and report success for a state that never happened.
///
/// `timeout` is `Option`: `None` waits as long as herdr does, which is forever.
pub fn wait_for_agent(
    h: &Herdr,
    pane_id: &str,
    until: &[AgentState],
    timeout: Option<Duration>,
) -> Result<()> {
    let statuses: Vec<AgentStatus> = until.iter().copied().filter_map(herdr_status).collect();
    if statuses.is_empty() && !until.is_empty() {
        return Err(Error::NothingToWaitFor);
    }
    h.wait(pane_id, &statuses, timeout)?;
    Ok(())
}

/// The tower's state vocabulary in herdr's words — the reverse of
/// `tower_scan::state_from`.
///
/// `Error` and `Dead` map to nothing: herdr reports on agents it can see, and
/// has no status meaning "the process died abnormally" or "the pane is gone".
/// Those are the tower's own conclusions, drawn from a pane no longer being
/// reported at all.
pub fn herdr_status(s: AgentState) -> Option<AgentStatus> {
    match s {
        AgentState::Working => Some(AgentStatus::Working),
        AgentState::Waiting => Some(AgentStatus::Blocked),
        AgentState::DoneUnread => Some(AgentStatus::Done),
        AgentState::Idle => Some(AgentStatus::Idle),
        AgentState::Error | AgentState::Dead => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    /// One of each herdr failure, labeled, and whether herdr may have acted.
    fn herdr_errors() -> Vec<(&'static str, herdrx::Error)> {
        let api = |code: &str| {
            herdrx::Error::Api(herdrx::ApiError {
                code: code.into(),
                message: "m".into(),
            })
        };
        vec![
            ("refused", api("agent_busy")),
            ("stalled", api("agent_prompt_stalled")),
            ("server timeout", api("timeout")),
            ("no socket", herdrx::Error::NoSocket),
            ("invalid", herdrx::Error::Invalid("bad".into())),
            ("not found", herdrx::Error::Io(ErrorKind::NotFound.into())),
            (
                "refused connection",
                herdrx::Error::Io(ErrorKind::ConnectionRefused.into()),
            ),
            ("late", herdrx::Error::Io(ErrorKind::WouldBlock.into())),
            ("timed out", herdrx::Error::Io(ErrorKind::TimedOut.into())),
            ("hung up", herdrx::Error::Io(ErrorKind::BrokenPipe.into())),
            (
                "undecodable",
                herdrx::Error::Decode(serde_json::from_str::<u8>("x").unwrap_err()),
            ),
        ]
    }

    /// Only the delivery call can have put text in the pane, and it has
    /// exactly when herdr may have acted on it. The same failure from the
    /// lookup before it never has, whatever its kind — that is what keeps a
    /// late `pane.get` retryable while a late `agent.prompt` is not.
    #[test]
    fn only_a_delivery_failure_may_have_landed() {
        for (name, e) in herdr_errors() {
            let acted = e.may_have_acted();
            let text = e.to_string();
            let delivery = Error::Delivery(e);
            assert_eq!(delivery.may_have_landed(), acted, "delivery: {name}");
            assert_eq!(delivery.to_string(), text, "delivery says herdr's words");
            let Error::Delivery(e) = delivery else {
                unreachable!()
            };
            let lookup = Error::from(e);
            assert!(matches!(lookup, Error::Herdr(_)), "{name}");
            assert!(!lookup.may_have_landed(), "lookup: {name}");
            assert_eq!(lookup.to_string(), text, "lookup says herdr's words");
        }
    }

    /// Failures that stop before anything is sent cannot have landed.
    #[test]
    fn the_failures_before_sending_never_landed() {
        for e in [
            Error::NoExecutable,
            Error::NoWorkspace("dev".into()),
            Error::NothingToWaitFor,
        ] {
            assert!(!e.may_have_landed(), "{e}");
        }
    }
}
