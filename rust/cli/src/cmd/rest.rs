// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Rest of the command handlers: daemon, resume, restore, info, all, loop, wt.

use std::io::Write as _;
use time::OffsetDateTime;

use super::common::*;
use super::Result;

pub fn run_daemon(args: &[String]) -> Result<()> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Err("usage: herdr-tower daemon <install|start|status|run>".into());
    };
    if args.len() != 1 {
        return Err(format!("daemon {sub} takes no arguments").into());
    }
    let home = home()?;
    // Written into the service definition, so the link, not the versioned
    // file an upgrade removes — see `tower_cli::selfpath`.
    let exe = tower_cli::selfpath::current()?;
    let state_dir = tower_state::default_dir();
    match sub {
        "install" => tower_cli::daemoncmd::install(
            &mut std::io::stdout().lock(),
            &home,
            &exe,
            &state_dir,
            config_override()?.as_deref(),
            false,
        )?,
        "start" => tower_cli::daemoncmd::start(
            &mut std::io::stdout().lock(),
            &home,
            &exe,
            &state_dir,
            config_override()?.as_deref(),
        )?,
        "status" => {
            let snapshot = tower_state::DaemonStore::new(&state_dir).read()?;
            let Some(snapshot) = snapshot else {
                return Err("resident supervisor has not reported yet".into());
            };
            let freshness = if snapshot.fresh(OffsetDateTime::now_utc()) {
                "running"
            } else {
                "stale"
            };
            tower_cli::outln!(
                "{freshness}: pid={} version={} herdr={} backends={} updated={}",
                snapshot.pid,
                snapshot.version,
                if snapshot.connected {
                    "connected"
                } else {
                    "disconnected"
                },
                snapshot.backends.len(),
                snapshot.updated_at
            )?;
            if !snapshot.error.is_empty() {
                tower_cli::outln!("error: {}", snapshot.error)?;
            }
            // The snapshot is only the latest pass; what led up to it is here.
            tower_cli::outln!(
                "log: {}",
                state_dir.join(tower_cli::supervisorlog::LOG_FILE).display()
            )?;
        }
        "run" => {
            let store = store()?;
            let loops = tower_state::LoopStore::new(store.dir())?;
            let version = tower_cli::versioncmd::version_label(
                &tower_cli::versioncmd::VersionInfo::stamped(),
            );
            let sender = notify_sender();
            tower_cli::loopcmd::supervise(
                &store,
                &loops,
                &home,
                &version,
                &exe,
                tower_cli::loopcmd::Notices {
                    stopped: |stopped: &tower_cli::loopcmd::StopNotice| {
                        if let Some(send) = desktop_notifier() {
                            let _ = send(
                                &format!("↻ {} idle loop stopped", stopped.session),
                                &stopped.reason,
                            );
                        }
                    },
                    // `outbound_config`, as every other sender: a supervisor
                    // run against a sandbox store must not remind the user's
                    // real Discord of its test sessions.
                    still_waiting: |agent: &tower_state::Agent, waited| {
                        tower_notify::remind(&outbound_config(), &sender, agent, waited);
                    },
                    guard: |title: &str, body: &str| {
                        tower_notify::announce(&outbound_config(), &sender, title, body);
                    },
                    // The same path as the guard's: a desktop notice and the
                    // notification channel as the settings allow, with every
                    // mention switched off — a handoff's title is text an
                    // agent in another program wrote.
                    parallax: |title: &str, body: &str| {
                        tower_notify::announce(&outbound_config(), &sender, title, body);
                    },
                    // Item text is the advisor's words: the same path, with
                    // mentions off.
                    board: |title: &str, body: &str| {
                        tower_notify::announce(&outbound_config(), &sender, title, body);
                    },
                },
                // Started once this process holds the supervisor lock. A
                // thread that cannot start is said and the loop runs without
                // it; a port already taken ends only that thread.
                move || {
                    let spawned = [
                        // The Discord card, refreshed on a timer of this
                        // process instead of a LaunchAgent that starts one
                        // every five minutes.
                        std::thread::Builder::new()
                            .name("card".into())
                            .spawn(super::card::card_timer),
                        // The remote MCP server, unless `mcp off`: a thread of
                        // this process instead of a LaunchAgent of its own.
                        std::thread::Builder::new()
                            .name("remote-mcp".into())
                            .spawn(super::mcp::serve_in_supervisor),
                    ];
                    for e in spawned.into_iter().filter_map(|r| r.err()) {
                        eprintln!("herdr-tower daemon: a server thread did not start: {e}");
                    }
                },
            )?;
        }
        _ => return Err(format!("unknown daemon command: {sub}").into()),
    }
    Ok(())
}

/// How long `resume` waits for the revived CLI to appear before giving up on
/// confirming it.
///
/// Generous against a cold start and short against a person's patience. Nothing
/// is lost by timing out: the window is there, the record is there, and the
/// message says both.
const RESUME_START_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether herdr comes to report a CLI running in `pane`.
///
/// Polled rather than subscribed: `agent.wait` waits for a *status*, and a pane
/// that never becomes an agent has no status to report — the question here is
/// whether it becomes one at all.
fn agent_came_up(h: &herdrx::Herdr, pane: &str, within: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + within;
    loop {
        let up = h
            .list_panes()
            .is_ok_and(|panes| panes.iter().any(|p| p.pane_id == pane && p.agent.is_some()));
        if up {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// `herdr-tower resume [id]`
///
/// Rescues a conversation that died without the closing ritual. With no
/// argument it lists what can be rescued.
pub fn run_resume(args: &[String]) -> Result<()> {
    // Exactly one ID, or none — and asked *first*, before the store is
    // synchronized or anything is launched. Taking the first of several and
    // ignoring the rest meant a typo, `resume valid-id typo`, created a workspace
    // and started a conversation nobody had asked for.
    if args.len() > 1 {
        return Err(format!(
            "unknown argument: {:?} — resume takes exactly one id",
            args[1]
        )
        .into());
    }
    let store = store()?;
    let now = OffsetDateTime::now_utc();
    let h = sync_panes(&store, now)?;
    let roots = tower_ui::ResumePaths::from_env();
    let mut out = std::io::stdout().lock();
    let Some(id) = args.first() else {
        let agents = store.list()?;
        writeln!(out, "sessions that can be resumed (dead or errored first):")?;
        let mut found = false;
        for a in &agents {
            if tower_ui::resume_command(a, &roots).is_err() {
                continue;
            }
            let marker = if matches!(
                a.state,
                tower_state::AgentState::Dead | tower_state::AgentState::Error
            ) {
                "!"
            } else {
                " "
            };
            writeln!(
                out,
                "  {marker} {:<14} {:<8} {}  ({})",
                a.id,
                tower_cli::infocmd::state_word(a.state),
                tower_text::shorten_home(&a.cwd),
                a.short_session_id()
            )?;
            found = true;
        }
        if !found {
            writeln!(out, "  none — there is nothing to resume.")?;
        }
        writeln!(
            out,
            "\nUsage: herdr-tower resume <id>   (the session id in parentheses works too)"
        )?;
        return Ok(());
    };

    // The record ID the listing shows, or a session ID — the tower shows one
    // in SID and one whole in the preview heading.
    let agents = store.list()?;
    // By session ID only a record that has stopped: a live one answers to
    // the same ID, and reviving its conversation would run it twice — and the
    // record deleted afterwards would be the live agent's.
    let a = tower_cli::infocmd::find_resumable(&agents, id, |a| {
        tower_cli::infocmd::stopped(a) && tower_ui::resume_command(a, &roots).is_ok()
    })?
    .clone();
    let id = &a.id;
    let mut cmd = tower_ui::resume_command(&a, &roots)?;
    // Revived the way `wt new` and `open` start one: without approval prompts.
    cmd = tower_wt::with_auto_approve(&cmd);
    let name = format!("resume-{id}");
    // `new_session` hands back the new workspace's first *pane*, and the command
    // goes into that pane. Passing it to `spawn_in` as a workspace label — which
    // is what this did — resolved `w2:p1` against the labels, found nothing, and
    // failed with `NoWorkspace` after creating an empty workspace. `restore` has
    // always done it this way.
    let pane = tower_mux::new_session(&h, &name, &a.cwd)?;
    // Typed into a live shell rather than being the pane's process, so the window
    // survives this command exiting and the agent starts under the user's own
    // shell environment.
    tower_mux::send_text(&h, &pane, &cmd)?;
    writeln!(
        out,
        "starting in a new window {name:?}… ({})",
        tower_text::shorten_home(&a.cwd)
    )?;
    // The record goes only once herdr reports a CLI in the new pane. `send_text`
    // returning `Ok` means the shell took the keystrokes, not that the agent
    // started: a missing binary, or a resume ID the CLI refuses, fails a moment
    // later — and deleting here destroyed the only record carrying the
    // conversation ID while reporting it revived.
    //
    // `restore` leaves this to the scanner, which drops a dead row as soon as a
    // new agent takes its `kind|session|cwd` slot. That cannot work here: this
    // revival lands in a *new* workspace, so the slot never matches. The wait is
    // bounded and the record survives it either way.
    if !agent_came_up(&h, &pane, RESUME_START_WAIT) {
        writeln!(
            out,
            "the CLI did not come up within {}s — check the window. The record is kept, so `herdr-tower resume {id}` can be run again.",
            RESUME_START_WAIT.as_secs()
        )?;
        return Ok(());
    }
    // No double rows: a dead record that was actually revived goes.
    let _ = store.delete(id);
    writeln!(out, "picking the conversation back up.")?;
    Ok(())
}

/// `herdr-tower restore [--resume] [--dry-run] [id…]`
pub fn run_restore(args: &[String]) -> Result<()> {
    let store = store()?;
    // Sync first: with no server at all — the moment just after a reboot, which
    // is what this command is for — every stored record falls to DEAD and
    // becomes a restore candidate.
    let h = sync_panes(&store, OffsetDateTime::now_utc())?;
    let mut out = std::io::stdout().lock();
    let picker = tower_cli::restorepick::terminal_picker();
    tower_cli::restorecmd::run_restore(&mut out, &store, &h, args, Some(&picker))?;
    Ok(())
}

/// `herdr-tower info <session|id>`
pub fn run_info(args: &[String]) -> Result<()> {
    let [key] = args else {
        return Err("Usage: herdr-tower info <session name|id>".into());
    };
    let store = store()?;
    let now = OffsetDateTime::now_utc();
    sync_panes(&store, now)?;
    let agents = store.list()?;
    let Some(a) = tower_cli::infocmd::find_agent(&agents, key) else {
        let same = tower_cli::infocmd::by_session(&agents, key);
        if same.len() > 1 {
            let ids: Vec<&str> = same.iter().map(|a| a.id.as_str()).collect();
            return Err(format!(
                "{key:?} names more than one session ({}) — give the whole session id or the record id",
                ids.join(", ")
            )
            .into());
        }
        return Err(format!("no agent {key:?} — check with herdr-tower status").into());
    };
    let home = home()?;
    let cfg = tower_config::load();
    let paths =
        tower_wiring::Paths::detect().unwrap_or_else(|| tower_wiring::Paths::for_home(&home));
    let ctx = tower_usage::agent_ctx(
        std::slice::from_ref(a),
        &tower_usage::codex_sessions_root(&home),
        &tower_usage::gemini_dir(&home),
        &tower_usage::claude_dir(&home),
    );
    let branch = tower_wt::list_metas(&tower_state::default_dir())
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.path == a.cwd)
        .map(|m| m.live_branch());
    let wiring = tower_wiring::collect(&paths, &a.cwd, &a.pane.session, &cfg.channel_labels);

    let d = tower_ui::InfoData {
        agent: a,
        wiring: &wiring,
        ctx: ctx.get(&a.id),
        labels: &cfg.channel_labels,
        branch: branch.as_deref(),
        unit: tower_cli::infocmd::unit_word(),
    };
    tower_cli::out!("{}", tower_cli::infocmd::render_info(&d, now))?;
    Ok(())
}

/// `herdr-tower wake-all` / `close-all` / `broadcast <message>`
pub fn run_all(cmd: &str, args: &[String]) -> Result<()> {
    // Asked before the warning, the backend or the store: `wake-all` and
    // `close-all` carry their own message, so a positional word belongs to
    // neither — and `close-all --yes typo` shut every session down regardless.
    // Only `broadcast` has something to do with the rest of the line.
    {
        let (_, rest) = tower_cli::allcmd::parse_all_flags(cmd, args, false)?;
        if cmd != "broadcast" {
            if let Some(word) = rest.first() {
                return Err(format!("unknown argument: {word:?} — {cmd} takes no message").into());
            }
        }
    }
    let h = tower_cli::muxsel::default_mux();
    // `close-all` is the one that ends turns, so it watches for them to finish
    // by default; the others do not.
    let (o, rest) = tower_cli::allcmd::parse_all_flags(cmd, args, cmd == "close-all")?;
    let message = match cmd {
        "wake-all" => tower_ui::WAKE_MESSAGE.to_string(),
        "close-all" => tower_ui::CLOSE_MESSAGE.to_string(),
        _ if rest.is_empty() => {
            return Err("broadcast needs a message: herdr-tower broadcast \"<message>\"".into())
        }
        _ => rest.join(" "),
    };
    let store = store()?;
    let now = OffsetDateTime::now_utc();
    let backend = tower_cli::muxsel::default_backend_id();
    tower_scan::sync_named_backend(&store, &h, &backend, now)?;
    let self_pane = herdrx::current_pane_id().unwrap_or_default();
    let mut out = std::io::stdout().lock();
    tower_cli::allcmd::run_all(
        &mut out,
        &store,
        &h,
        &self_pane,
        &message,
        &o,
        cmd != "broadcast",
        &backend,
        confirm_on_tty,
    )?;
    Ok(())
}

/// `herdr-tower host` — this machine's processor counters and memory, for a
/// tower on another machine to read over ssh. See [`tower_os::host::report`].
pub fn run_host(args: &[String]) -> Result<()> {
    if let Some(extra) = args.first() {
        return Err(format!("host takes no arguments, not {extra:?}").into());
    }
    let version =
        tower_cli::versioncmd::version_label(&tower_cli::versioncmd::VersionInfo::stamped());
    write!(
        std::io::stdout().lock(),
        "{}",
        tower_os::host::report(&version)
    )?;
    Ok(())
}

/// `herdr-tower schedule …` — see [`tower_cli::schedulecmd`].
pub fn run_schedule(args: &[String]) -> Result<()> {
    let store = store()?;
    let schedules = tower_state::ScheduleStore::new(store.dir())?;
    // The records, for turning a session ID someone typed into the whole one.
    let agents = store.list()?;
    let now = OffsetDateTime::now_utc().to_offset(tower_state::local_offset());
    let exe = std::env::current_exe()?;
    tower_cli::schedulecmd::run_schedule(
        &mut std::io::stdout().lock(),
        &schedules,
        &agents,
        args,
        now,
        &|| tower_cli::loopcmd::spawn_supervisor(&exe),
    )?;
    Ok(())
}

/// `herdr-tower loop start|stop|list|done|blocked`.
pub fn run_loop(args: &[String]) -> Result<()> {
    use tower_state::{LoopLease, LoopStore, DEFAULT_IDLE_FOR, DEFAULT_LOOP_TTL};

    let Some(sub) = args.first().map(String::as_str) else {
        return Err("usage: herdr-tower loop <start|stop|list|done|blocked> ...".into());
    };
    let h = tower_cli::muxsel::default_mux();

    if matches!(sub, "done" | "blocked") {
        if args.len() != 1 {
            return Err(format!("loop {sub} takes no arguments").into());
        }
        let pane_id = herdrx::current_pane_id()
            .ok_or_else(|| format!("loop {sub} must run inside a herdr pane"))?;
        tower_cli::loopcmd::report_control(&h, &pane_id, sub)?;
        tower_cli::outln!("idle loop marked {sub} for this session.")?;
        return Ok(());
    }

    let store = store()?;
    let loops = LoopStore::new(store.dir())?;
    let backend = tower_cli::muxsel::default_backend_id();

    let now = OffsetDateTime::now_utc();
    match sub {
        "list" => {
            if args.len() != 1 {
                return Err("loop list takes no arguments".into());
            }
            tower_cli::loopcmd::render_list(&mut std::io::stdout(), &loops.list()?, now)?;
        }
        "start" => {
            let mut idle_for = DEFAULT_IDLE_FOR;
            let mut ttl = DEFAULT_LOOP_TTL;
            let mut max_turns = None;
            let mut instruction = String::new();
            let mut selector: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                let arg = &args[i];
                let take = |i: &mut usize, name: &str| -> Result<&str> {
                    *i += 1;
                    args.get(*i)
                        .map(String::as_str)
                        .ok_or_else(|| format!("{name} needs a value").into())
                };
                match arg.as_str() {
                    "--idle-for" | "-idle-for" => {
                        let raw = take(&mut i, arg)?;
                        idle_for = tower_cli::helpcmd::parse_duration(raw)
                            .ok_or_else(|| format!("invalid duration for --idle-for: {raw:?}"))?;
                    }
                    "--ttl" | "-ttl" => {
                        let raw = take(&mut i, arg)?;
                        ttl = tower_cli::helpcmd::parse_duration(raw)
                            .ok_or_else(|| format!("invalid duration for --ttl: {raw:?}"))?;
                    }
                    "--max-turns" | "-max-turns" => {
                        let raw = take(&mut i, arg)?;
                        max_turns = if raw == "unlimited" {
                            None
                        } else {
                            Some(raw.parse::<u64>().map_err(|_| {
                                "--max-turns must be a positive integer or unlimited"
                            })?)
                        };
                        if max_turns == Some(0) {
                            return Err("--max-turns must be greater than zero".into());
                        }
                    }
                    "--instruction" | "-instruction" => {
                        instruction = take(&mut i, arg)?.to_string();
                    }
                    s if s.starts_with('-') => return Err(format!("unknown flag: {s}").into()),
                    s if selector.is_none() => selector = Some(s.to_string()),
                    s => return Err(format!("unexpected argument: {s:?}").into()),
                }
                i += 1;
            }
            if idle_for.is_zero() || ttl.is_zero() {
                return Err("--idle-for and --ttl must be greater than zero".into());
            }
            let selector = selector.ok_or("loop start needs a session name or agent id")?;
            tower_scan::sync_named_backend(&store, &h, &backend, now)?;
            let agents: Vec<_> = store
                .list()?
                .into_iter()
                .filter(|a| a.pane.backend == backend && a.state != tower_state::AgentState::Dead)
                .collect();
            let agent = tower_cli::loopcmd::resolve(&agents, &selector)?;
            if agent.session_id.is_empty() {
                return Err(
                    "idle loop needs a native agent session id; retry after Herdr detects it"
                        .into(),
                );
            }
            tower_cli::loopcmd::clear_control(&h, &agent.pane.pane_id);
            loops.arm(LoopLease::new(
                agent,
                now,
                idle_for,
                ttl,
                max_turns,
                instruction,
            ))?;
            let exe = std::env::current_exe()?;
            if let Err(e) = tower_cli::loopcmd::spawn_supervisor(&exe) {
                let _ = loops.disarm(&agent.id);
                return Err(format!("could not start idle-loop supervisor: {e}").into());
            }
            tower_cli::outln!(
                "idle loop armed for {} ({}) — idle {}, lifetime {}, transmissions {}",
                agent.pane.session,
                agent.id,
                tower_cli::helpcmd::format_duration(idle_for),
                tower_cli::helpcmd::format_duration(ttl),
                max_turns.map_or_else(|| "unlimited".into(), |n| n.to_string())
            )?;
        }
        "stop" => {
            let all = args.iter().skip(1).any(|a| a == "--all" || a == "-all");
            if all {
                if args.len() != 2 {
                    return Err("usage: herdr-tower loop stop --all".into());
                }
                let old = loops.disarm_backend(&backend)?;
                for l in &old {
                    tower_cli::loopcmd::clear_display(&h, &l.pane_id);
                }
                tower_cli::outln!("stopped {} idle loops.", old.len())?;
            } else {
                if args.len() != 2 {
                    return Err("loop stop needs one session name or agent id".into());
                }
                let selector = &args[1];
                let found: Vec<_> = loops
                    .list()?
                    .into_iter()
                    .filter(|l| {
                        l.backend == backend
                            && (l.agent_id == *selector
                                || l.pane_id == *selector
                                || l.session == *selector)
                    })
                    .collect();
                match found.as_slice() {
                    [] => return Err(format!("no idle loop matches {selector:?}").into()),
                    [l] => {
                        loops.disarm(&l.agent_id)?;
                        tower_cli::loopcmd::clear_display(&h, &l.pane_id);
                        tower_cli::outln!("idle loop stopped for {} ({}).", l.session, l.agent_id)?;
                    }
                    many => {
                        return Err(format!(
                            "{selector:?} matches multiple idle loops: {}",
                            many.iter()
                                .map(|l| l.agent_id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                        .into())
                    }
                }
            }
        }
        _ => return Err(format!("unknown loop command: {sub}").into()),
    }
    Ok(())
}

/// `herdr-tower wt <sub> …`
pub fn run_wt(args: &[String]) -> Result<()> {
    let store = store()?;
    let now = OffsetDateTime::now_utc();
    let h = sync_panes(&store, now)?;
    // `Stdout`, not a lock on it: `wt test` streams a child's stdout and stderr
    // from two reader threads, so the writer has to be `Send`. A `StdoutLock` is
    // not, and `Stdout` locks per write anyway.
    let mut out = std::io::stdout();
    tower_cli::wtcmd::run_wt(&mut out, &tower_state::default_dir(), &store, &h, args, now)?;
    Ok(())
}

/// `herdr-tower open <folder>` — see [`tower_cli::opencmd`].
pub fn run_open(args: &[String]) -> Result<()> {
    use tower_cli::opencmd;
    let a = opencmd::parse_args(args)?;
    let h = tower_cli::muxsel::default_mux();
    let o = opencmd::OpenOptions {
        dir: &a.dir,
        agent: &a.agent,
        workspace: &a.workspace,
        prompt: &a.prompt,
        wait: None,
    };
    let nonce = OffsetDateTime::now_utc().unix_timestamp_nanos() as u64;
    let opened = opencmd::open(&h, &tower_cli::muxsel::default_backend_id(), &o, nonce)?;
    if a.json {
        tower_cli::outln!(
            "{}",
            serde_json::to_string_pretty(&opencmd::to_json(&opened))?
        )?;
    } else {
        tower_cli::out!("{}", opencmd::describe(&opened))?;
    }
    Ok(())
}

/// The one live record `key` names, after a sync with the backend.
fn named_agent(key: &str) -> Result<(herdrx::Herdr, tower_state::Agent)> {
    let store = store()?;
    let h = sync_panes(&store, OffsetDateTime::now_utc())?;
    let agents = store.list()?;
    let a = tower_cli::infocmd::find_agent(&agents, key)
        .cloned()
        .ok_or_else(|| format!("no agent {key:?} — `herdr-tower status` lists them"))?;
    Ok((h, a))
}

/// `herdr-tower close <agent>` — see [`tower_cli::agentctl`].
pub fn run_close(args: &[String]) -> Result<()> {
    use tower_cli::agentctl;
    const USAGE: &str = "usage: herdr-tower close <agent> [--force] [--yes]";
    let fs = tower_cli::helpcmd::FlagSet::new("close")
        .bool(
            "force",
            false,
            "close it even while it is working or waiting on you",
        )
        .bool("yes", false, "close without asking");
    let (key, p) = agentctl::one_target(&fs, args, USAGE)?;
    let (h, a) = named_agent(&key)?;
    let self_pane = herdrx::current_pane_id().unwrap_or_default();
    let live = agentctl::live_state(&h, &a);
    if let Some(why) = agentctl::close_refusal(&a, live, &self_pane, p.bool("force")) {
        return Err(why.into());
    }
    if !p.bool("yes") {
        tower_cli::out!(
            "close {} ({}, {}) in {}? its pane and whatever runs there go too [y/N] ",
            a.id,
            a.kind,
            a.pane.session,
            tower_text::shorten_home(&a.cwd)
        )?;
        std::io::stdout().flush()?;
        if !confirm_on_tty() {
            tower_cli::outln!("nothing closed")?;
            return Ok(());
        }
    }
    agentctl::close(&h, &a)?;
    tower_cli::outln!("closed {}", a.id)?;
    Ok(())
}

/// `herdr-tower interrupt <agent>` — see [`tower_cli::agentctl`].
pub fn run_interrupt(args: &[String]) -> Result<()> {
    use tower_cli::agentctl;
    const USAGE: &str = "usage: herdr-tower interrupt <agent>";
    let fs = tower_cli::helpcmd::FlagSet::new("interrupt");
    let (key, _) = agentctl::one_target(&fs, args, USAGE)?;
    let (h, a) = named_agent(&key)?;
    if let Some(why) = agentctl::interrupt_refusal(&a, agentctl::live_state(&h, &a)) {
        return Err(why.into());
    }
    agentctl::interrupt(&h, &a)?;
    tower_cli::outln!("interrupted {} (pressed Escape)", a.id)?;
    Ok(())
}

/// `herdr-tower workspaces` — see [`tower_cli::agentctl`].
pub fn run_workspaces(args: &[String]) -> Result<()> {
    let fs = tower_cli::helpcmd::FlagSet::new("workspaces").bool("json", false, "print JSON");
    let p = fs.parse(args).map_err(|e| fs.diagnostic(&e))?;
    if let Some(extra) = p.args().first() {
        return Err(format!("unknown argument: {extra:?} — workspaces takes only --json").into());
    }
    let store = store()?;
    let h = sync_panes(&store, OffsetDateTime::now_utc())?;
    let list = h.workspaces()?;
    let v = tower_cli::agentctl::workspaces_json(
        &list,
        &store.list()?,
        &tower_cli::muxsel::default_backend_id(),
    );
    if p.bool("json") {
        tower_cli::outln!("{}", serde_json::to_string_pretty(&v)?)?;
    } else {
        tower_cli::out!("{}", tower_cli::agentctl::workspaces_table(&v))?;
    }
    Ok(())
}

/// `herdr-tower doctor` — see [`tower_cli::doctorcmd`].
///
/// Fails when it found a problem, so a script can gate on it.
pub fn run_doctor(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return Err("doctor takes no arguments".into());
    }
    use tower_cli::doctorcmd as doc;
    let home = home()?;
    let this = tower_cli::selfpath::current()?;
    let bin = this.to_string_lossy().into_owned();
    let opencode_dir =
        tower_cli::opencodeinit::config_dir(&home, std::env::var_os("XDG_CONFIG_HOME").as_deref());
    let mut out = std::io::stdout().lock();
    let mut problems = 0;

    writeln!(out, "this binary: {bin}")?;
    writeln!(out)?;
    let (_, config_problems) = tower_config::load_checked();
    let config = if config_problems.is_empty() {
        vec![doc_ok("config.json: every setting is understood".into())]
    } else {
        config_problems.into_iter().map(doc_bad).collect()
    };
    let title = match tower_config::path() {
        Some(p) => format!("configuration ({})", p.display()),
        None => "configuration".into(),
    };
    problems += doc::print_section(&mut out, &title, &config)?;
    writeln!(out)?;
    let regs = doc::registrations(&doc::registration_files(&home, &opencode_dir));
    problems += doc::print_section(
        &mut out,
        "registered paths",
        &doc::check_registrations(&regs, &this),
    )?;
    if let Some(f) = doc::check_opencode_plugin(&opencode_dir, &bin) {
        writeln!(out)?;
        problems += doc::print_section(&mut out, "opencode plugin", &[f])?;
    }

    writeln!(out)?;
    let now = OffsetDateTime::now_utc();
    let daemon = match tower_state::DaemonStore::new(tower_state::default_dir()).read() {
        Ok(Some(s)) if s.fresh(now) => doc_ok(format!(
            "running: pid={} version={} herdr={}",
            s.pid,
            s.version,
            if s.connected {
                "connected"
            } else {
                "disconnected"
            }
        )),
        Ok(Some(s)) => doc_bad(format!(
            "stale since {} — herdr-tower daemon start",
            s.updated_at
        )),
        Ok(None) => doc_bad("has never reported — herdr-tower daemon install".into()),
        Err(e) => doc_bad(format!("unreadable: {e}")),
    };
    problems += doc::print_section(&mut out, "resident supervisor", &[daemon])?;

    writeln!(out)?;
    problems += doc::print_section(&mut out, "remote MCP", &super::mcp::doctor(&home))?;

    // Informational in every state (`tower_cli::parallax::doctor`): asked
    // live, with the same read the supervisor makes, so the answer is about
    // Parallax now rather than about the last snapshot.
    writeln!(out)?;
    problems += doc::print_section(
        &mut out,
        "parallax",
        &[tower_cli::parallax::doctor(
            &tower_cli::parallax::data_root_here(&home),
            tower_config::load().parallax_enabled(),
        )],
    )?;

    writeln!(out)?;
    writeln!(out, "last hooks")?;
    let agents = store()?.list()?;
    let lines = doc::last_hooks(&agents, now);
    if lines.is_empty() {
        writeln!(out, "  (no live sessions)")?;
    }
    for l in lines {
        writeln!(out, "  {l}")?;
    }

    writeln!(out)?;
    if problems == 0 {
        writeln!(out, "no problems found")?;
        Ok(())
    } else {
        Err(
            format!("{problems} problem(s) — `herdr-tower init` rewrites every registration")
                .into(),
        )
    }
}

fn doc_ok(line: String) -> tower_cli::doctorcmd::Finding {
    tower_cli::doctorcmd::Finding { ok: true, line }
}

fn doc_bad(line: String) -> tower_cli::doctorcmd::Finding {
    tower_cli::doctorcmd::Finding { ok: false, line }
}

/// `herdr-tower report [--since 12h] [--json]` — see [`tower_cli::reportcmd`].
pub fn run_report(args: &[String]) -> Result<()> {
    let fs = tower_cli::helpcmd::FlagSet::new("report")
        .string("since", "12h", "how far back: 30m, 12h, 2d")
        .bool("json", false, "machine-readable output");
    let p = flags_only("report", &fs, args)?;
    let now = OffsetDateTime::now_utc();
    let since = tower_cli::reportcmd::window_start(now, p.str("since"))?;
    let store = store()?;
    let entries = tower_state::journal::read_since(store.dir(), since);
    let lines = tower_cli::reportcmd::summarize(&entries, since, now);
    let verdicts = tower_cli::reportcmd::verdicts(&tower_state::default_dir(), since);
    let remote = tower_cli::mcp::audit::read_since(store.dir(), since);
    let mut out = std::io::stdout().lock();
    if p.bool("json") {
        let body = tower_cli::reportcmd::to_json(&lines, &verdicts, &remote, since);
        writeln!(out, "{}", serde_json::to_string_pretty(&body)?)?;
        return Ok(());
    }
    let l = since.to_offset(tower_state::local_offset());
    let local = format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        l.year(),
        l.month() as u8,
        l.day(),
        l.hour(),
        l.minute()
    );
    tower_cli::reportcmd::render(&mut out, &lines, &verdicts, &remote, &local, p.str("since"))?;
    Ok(())
}

/// `herdr-tower tree`: who works under whom, and links made by hand. See
/// `tower_cli::tree`.
pub fn run_tree(args: &[String]) -> Result<()> {
    use tower_cli::tree;
    let state_dir = tower_state::default_dir();
    let store = store()?;
    let now = OffsetDateTime::now_utc();
    let backend = tower_cli::muxsel::default_backend_id();
    let links = tower_state::TreeStore::new(&state_dir);
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["show"] => {
            let agents = store.list()?;
            tower_cli::out!(
                "{}",
                tree::show_text(&tree::read_parents(&state_dir, &agents, now))
            )?;
            Ok(())
        }
        ["link", worker, rest @ ..] if rest.len() <= 1 => {
            let coordinator = match rest.first() {
                Some(c) => c.to_string(),
                None => herdrx::current_pane_id()
                    .ok_or("name the coordinator: tree link <worker> <coordinator>")?,
            };
            let agents = store.list()?;
            let parents = tree::read_parents(&state_dir, &agents, now);
            let plan = tree::plan_link(&agents, &parents, &backend, worker, &coordinator)?;
            let mut worker = plan.worker.clone();
            if let Some(workspace_id) = &plan.move_to {
                let h = tower_cli::muxsel::default_mux();
                let moved = h
                    .move_pane(
                        &worker,
                        &herdrx::PaneMoveDestination::NewTab {
                            workspace_id: workspace_id.clone(),
                        },
                    )
                    .map_err(|e| format!("could not move {worker} to {workspace_id}: {e}"))?;
                // herdr gives a moved pane a new id; the link names that one.
                tower_cli::outln!("moved {worker} to {}", moved.pane.pane_id)?;
                worker = moved.pane.pane_id;
            }
            links.link(tower_state::TreeLink {
                backend: backend.clone(),
                worker: worker.clone(),
                coordinator: plan.coordinator.clone(),
                at: now,
            })?;
            tower_cli::outln!("{worker} works under {}", plan.coordinator)?;
            Ok(())
        }
        ["unlink", worker] => {
            if links.unlink(&backend, worker)? {
                tower_cli::outln!("{worker} is drawn in its own tab again")?;
            } else {
                tower_cli::outln!(
                    "{worker} had no link (a board or a wt task may still place it)"
                )?;
            }
            Ok(())
        }
        _ => Err(tree::TREE_USAGE.into()),
    }
}

/// `herdr-tower board …` — see [`tower_cli::boardcmd`] and
/// [`tower_cli::boardwatch`].
pub fn run_board(args: &[String]) -> Result<()> {
    use tower_cli::boardcmd::{self, Cmd};
    let (given_repo, args) = boardcmd::take_repo(args)?;
    let cmd = boardcmd::parse(args)?;
    let state_dir = tower_state::default_dir();
    let store = store()?;
    let now = OffsetDateTime::now_utc();
    let backend = tower_cli::muxsel::default_backend_id();
    let boards = tower_state::BoardStore::new(&state_dir)?;
    let caller_pane = herdrx::current_pane_id();
    let cwd = std::env::current_dir()?;
    let repo = match &cmd {
        Cmd::Watch { repo: Some(r) } => r.clone(),
        _ => match &given_repo {
            Some(r) => boardcmd::board_repo(&state_dir, r)?,
            None => boardcmd::board_repo(&state_dir, &cwd.to_string_lossy())?,
        },
    };
    // The socket this process's herdr environment selects, and the default
    // one: the delivery derives the backend id from the pair itself.
    let home = home()?;
    let home = home.to_string_lossy();
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let socket = herdrx::socket_path(env, &home);
    let default_socket = herdrx::socket_path(|_| None, &home);
    let open_spec = |caller: &str| -> std::result::Result<tower_cli::boardwatch::Opened, String> {
        let exe = tower_cli::selfpath::current().map_err(|e| e.to_string())?;
        tower_cli::boardwatch::open(
            &tower_cli::muxsel::default_mux(),
            &boards,
            &tower_cli::boardwatch::OpenSpec {
                repo: &repo,
                backend: &backend,
                caller,
                exe: &exe,
                state_dir: &state_dir,
                socket: &socket,
                pid: std::process::id(),
            },
        )
    };
    match &cmd {
        Cmd::Watch { .. } => return watch_board(&boards, &store, &repo, &backend),
        Cmd::Open => {
            let caller =
                herdrx::current_pane_id().ok_or("board open must run inside a herdr pane")?;
            let opened = open_spec(&caller)?;
            tower_cli::outln!("{}", describe_opened(&opened))?;
            return Ok(());
        }
        _ => {}
    }
    // Fresh records: `send` finds its worker in them, `show` reads progress.
    // The backend whose records the very listing that refreshed them came
    // from; `None` when it did not answer and the records are as last seen.
    let observed = if matches!(
        cmd,
        Cmd::Send { .. }
            | Cmd::Next { .. }
            | Cmd::Show { .. }
            | Cmd::Assign { .. }
            | Cmd::Wake { on: true, .. }
    ) {
        let h = tower_cli::muxsel::default_mux();
        tower_scan::sync_named_backend_observed(&store, &h, &backend, now)?
            .then_some(backend.clone())
    } else {
        None
    };
    let ctx = boardcmd::Ctx {
        state_dir: &state_dir,
        repo: &repo,
        boards: &boards,
        store: &store,
        pid: std::process::id(),
        now,
        observed: observed.as_deref(),
        backend: &backend,
        settle: std::time::Duration::from_secs(15),
        caller: caller_pane.as_deref(),
    };
    let mut out = std::io::stdout().lock();
    boardcmd::run(&mut out, &ctx, &cmd, |to, text| match to {
        tower_state::SendTo::Task { task, .. } => tower_wt::send_board_item(
            &state_dir,
            task,
            &store,
            &socket,
            &default_socket,
            &repo,
            text,
        ),
        tower_state::SendTo::Pane(pin) => {
            tower_wt::send_board_item_to_pane(&store, &socket, &default_socket, pin, text)
        }
    })
    .map_err(|e| super::CliError::Message(e.to_string()))?;
    // After a delivery (never before it), the board pane — best effort, and
    // reported on its own line: a pane that will not open never fails a send.
    if matches!(cmd, Cmd::Send { .. } | Cmd::Next { .. }) {
        if let Some(caller) = herdrx::current_pane_id() {
            let line = match open_spec(&caller) {
                Ok(tower_cli::boardwatch::Opened::AlreadyOpen(_)) => None,
                Ok(o) => Some(describe_opened(&o)),
                Err(e) => Some(format!("board pane: {e}")),
            };
            if let Some(line) = line {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    Ok(())
}

fn describe_opened(o: &tower_cli::boardwatch::Opened) -> String {
    use tower_cli::boardwatch::Opened;
    match o {
        Opened::AlreadyOpen(p) => format!("board pane already open ({p})."),
        Opened::Unreadable(p) => {
            format!("board pane {p} is there but could not be read; left as it is.")
        }
        Opened::Started(p) => format!("board pane opened ({p})."),
        Opened::BeingOpened => "board pane is being opened by another command.".into(),
    }
}

/// `board --watch`: redraws the board every couple of seconds and keeps its
/// token on its pane. On a terminal, a double click on an item's lines moves
/// the view to the pane it is with.
///
/// It only draws. The agent records it reads are the ones the resident
/// supervisor keeps current, and whether they are current comes from the
/// supervisor's snapshot; the linked PRs' states are the ones the supervisor
/// last asked `gh` for. A pane per board used to repeat both — a herdr listing
/// every few seconds and a `gh` call per PR every minute — for what one
/// process already reads.
fn watch_board(
    boards: &tower_state::BoardStore,
    store: &tower_state::Store,
    repo: &str,
    backend: &str,
) -> Result<()> {
    use std::io::IsTerminal as _;
    use tower_cli::boardwatch;
    let h = tower_cli::muxsel::default_mux();
    let pane = herdrx::current_pane_id();
    let color = std::io::stdout().is_terminal();
    if color {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(boardwatch::CLEAR_ALL.as_bytes());
        let _ = out.flush();
    }
    let daemon = tower_state::DaemonStore::new(store.dir());
    // Clicks only on a terminal; the board still draws without one.
    let keys = std::io::stdin()
        .is_terminal()
        .then(|| super::usage::key_reader(std::path::Path::new(tower_cli::cbreak::CONTROLLING_TTY)))
        .flatten();
    let _mouse = keys
        .as_ref()
        .map(|_| super::usage::MouseReporting::clicks());
    // A click that could not go where it pointed, said on the last line for
    // a few seconds.
    let mut notice: Option<(String, std::time::Instant)> = None;
    let mut clicks = boardwatch::Clicks::default();
    let mut scroll = 0;
    let mut show_closed = false;
    loop {
        let now = OffsetDateTime::now_utc();
        let observed = boardwatch::observed(daemon.read().ok().flatten().as_ref(), backend, now);
        let prs = boards.pr_states(repo);
        // Rendered for the size the pane has when it is written: a pane that
        // shrank while the board was read gets a frame for its new size.
        let loaded = (boards.load(repo, now), store.list());
        let draw = |(cols, rows): (usize, usize)| match &loaded {
            (Ok(board), Ok(records)) => boardwatch::render_rows_scrolled(
                board,
                records,
                now,
                observed.as_deref(),
                cols,
                rows,
                color,
                &prs,
                scroll,
                show_closed,
            ),
            (Err(e), _) | (_, Err(e)) => {
                let mut l = vec![boardwatch::fit_line(
                    &format!("cannot read the board: {e}"),
                    cols,
                )];
                l.truncate(rows);
                boardwatch::Page {
                    rows: l.into_iter().map(|l| (l, None)).collect(),
                    offset: 0,
                    max_offset: 0,
                }
            }
        };
        let size = boardwatch::stdout_size();
        let mut page = draw(size);
        let mut now_size = boardwatch::stdout_size();
        if now_size != size {
            page = draw(now_size);
            now_size = boardwatch::stdout_size();
        }
        scroll = page.offset;
        let mut rows = page.rows;
        notice = notice.filter(|(_, at)| at.elapsed() < boardwatch::NOTICE_FOR);
        if let Some((text, _)) = &notice {
            boardwatch::put_notice(&mut rows, text, now_size);
        }
        let lines: Vec<String> = rows.iter().map(|(l, _)| l.clone()).collect();
        {
            let mut out = std::io::stdout().lock();
            // One write, in place and within the pane: it never flickers and
            // never scrolls a copy of itself into the scrollback.
            let _ = out.write_all(boardwatch::frame(&lines).as_bytes());
            let _ = out.flush();
        }
        if let Some(p) = &pane {
            boardwatch::claim_token(&h, p);
        }
        let Some((_mode, rx)) = &keys else {
            std::thread::sleep(boardwatch::REDRAW_EVERY);
            continue;
        };
        match rx.recv_timeout(boardwatch::REDRAW_EVERY) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // The terminal is gone: keep drawing at the usual pace.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(boardwatch::REDRAW_EVERY);
            }
            Ok(bytes) => {
                for key in tower_cli::usagecmd::parse_keys(&bytes) {
                    match key {
                        // -isig: Ctrl-C is a key, so the terminal is put back.
                        tower_cli::usagecmd::Key::Quit => return Ok(()),
                        tower_cli::usagecmd::Key::Up | tower_cli::usagecmd::Key::WheelUp => {
                            clicks.clear();
                            scroll = scroll.saturating_sub(
                                if matches!(key, tower_cli::usagecmd::Key::WheelUp) {
                                    3
                                } else {
                                    1
                                },
                            );
                        }
                        tower_cli::usagecmd::Key::Down | tower_cli::usagecmd::Key::WheelDown => {
                            clicks.clear();
                            scroll = scroll
                                .saturating_add(
                                    if matches!(key, tower_cli::usagecmd::Key::WheelDown) {
                                        3
                                    } else {
                                        1
                                    },
                                )
                                .min(page.max_offset);
                        }
                        tower_cli::usagecmd::Key::Toggle => {
                            clicks.clear();
                            show_closed = !show_closed;
                            scroll = 0;
                        }
                        tower_cli::usagecmd::Key::Mouse { row, .. } => {
                            let pointed = usize::from(row)
                                .checked_sub(1)
                                .and_then(|i| rows.get(i))
                                .and_then(|(_, p)| p.as_deref());
                            if let Some(target) = clicks.press(pointed, std::time::Instant::now()) {
                                if let Err(e) = h.focus(&target) {
                                    notice = Some((
                                        format!("could not go to {target}: {e}"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
