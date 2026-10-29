// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower mcp serve|pair|grants|revoke|on|off` — the remote MCP server.
//! See `tower_cli::mcp`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use serde_json::Value;
use time::OffsetDateTime;
use tower_cli::mcp::{self, audit, grants, http, oauth, tools};
use tower_state::Agent;
use tower_state::AgentState;

use super::common::*;
use super::Result;

const USAGE: &str = "usage: herdr-tower mcp <serve|pair|grants|revoke|on|off>\n\
  serve            run the server in the foreground (the supervisor serves it already)\n\
  pair [id]        issue a pairing code for the approval page showing that id (5 minutes, one use)  [--discord]\n\
  grants           list connected clients\n\
  revoke <id>      disconnect one client  (--all: every one)\n\
  on | off         have the supervisor serve it (the default), or not";

pub fn run_mcp(args: &[String]) -> Result<()> {
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("serve") => no_args("serve", rest).and_then(|_| serve()),
        Some("pair") => pair(rest),
        Some("grants") => no_args("grants", rest).and_then(|_| list_grants()),
        Some("revoke") => revoke(rest),
        Some("on") => no_args("on", rest).and_then(|_| on()),
        Some("off") => no_args("off", rest).and_then(|_| off()),
        _ => Err(USAGE.into()),
    }
}

fn no_args(name: &str, rest: &[String]) -> Result<()> {
    match rest.first() {
        Some(word) => Err(format!("mcp {name} takes no arguments (got {word:?})").into()),
        None => Ok(()),
    }
}

/// The public URL: `mcp.public_url`, or else this machine's Tailscale name —
/// the origin `tailscale funnel` publishes. Or why there is none.
fn public_url(cfg: &tower_config::Config) -> Result<String> {
    if let Some(u) = cfg.mcp.public_url() {
        return Ok(u);
    }
    if cfg.mcp.public_url.is_some() {
        return Err(
            "mcp.public_url is not an https:// origin, like https://<mac>.<tailnet>.ts.net".into(),
        );
    }
    tailscale_url().ok_or_else(|| {
        "no mcp.public_url, and Tailscale did not name this machine — sign in to Tailscale, or set mcp.public_url"
            .into()
    })
}

/// The Tailscale CLI to ask, overriding the search: a path, or `none`.
/// A test's server must not find the machine's real Tailscale and bind the
/// real port (it did, 2026-09-25).
pub const TAILSCALE_ENV: &str = "HERDR_TOWER_TAILSCALE";

/// `https://<name>` for this machine, from `tailscale status --json`.
fn tailscale_url() -> Option<String> {
    let bin = match std::env::var_os(TAILSCALE_ENV) {
        Some(v) if v == "none" || v.is_empty() => return None,
        Some(v) => std::path::PathBuf::from(v),
        None => tower_usage::ToolSearch::from_env()
            .lookup("tailscale")
            .or_else(|| {
                let app = std::path::PathBuf::from(
                    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                );
                app.is_file().then_some(app)
            })?,
    };
    let out = Command::new(bin)
        .args(["status", "--json"])
        // The Mac app's binary is the CLI only when it can tell it is being
        // used as one. Started by launchd, with no terminal, it tries to open
        // its GUI instead and fails ("The Tailscale GUI failed to start") —
        // so the supervisor never found the name `doctor` found from a shell
        // (seen on v0.24.1). The plain `tailscale` CLI ignores the variable.
        .env("TAILSCALE_BE_CLI", "1")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    tailscale_url_from(&String::from_utf8_lossy(&out.stdout))
}

/// The URL in a `tailscale status --json`: `Self.DNSName` without its final
/// dot. `None` when there is no name — logged out, or never set up.
fn tailscale_url_from(status: &str) -> Option<String> {
    let v: Value = serde_json::from_str(status).ok()?;
    let name = v.pointer("/Self/DNSName")?.as_str()?.trim_end_matches('.');
    (!name.is_empty() && !name.contains(['/', ' '])).then(|| format!("https://{name}"))
}

/// The remote MCP server inside the supervisor: a thread of `daemon run`,
/// served unless `mcp.enabled` is false. Without a public URL, or with the
/// port taken, it says why once and this thread ends; the supervisor goes on.
pub fn serve_in_supervisor() {
    if !tower_config::load().mcp.enabled() {
        return;
    }
    if let Err(e) = serve() {
        eprintln!("herdr-tower mcp: not serving — {e}");
    }
}

/// The URL the server names itself by: the public one, or — with no
/// Tailscale and no `mcp.public_url` — its own loopback address, serving this
/// machine alone (`true`). A `mcp.public_url` that is set but wrong is an
/// error, never a quiet fallback.
fn base_url(cfg: &tower_config::Config) -> Result<(String, bool)> {
    match public_url(cfg) {
        Ok(u) => Ok((u, false)),
        Err(e) if cfg.mcp.public_url.is_some() => Err(e),
        Err(_) => Ok((format!("http://{}", cfg.mcp.listen()?), true)),
    }
}

fn serve() -> Result<()> {
    let cfg = tower_config::load();
    let (base, local) = base_url(&cfg)?;
    if local {
        eprintln!(
            "herdr-tower mcp: no public URL (no Tailscale name, no mcp.public_url) — serving this machine only on {base}/mcp"
        );
    }
    let listen = cfg.mcp.listen()?;
    let state_dir = tower_state::default_dir();
    let settings = http::Settings {
        loopback_callbacks: local,
        base,
        redirect_uris: cfg.mcp.redirect_uris(),
        state_dir: state_dir.clone(),
        act: cfg.mcp.act_enabled(),
        limiter: tower_cli::mcp::limit::Limiter::default(),
        read_deadline: http::READ_DEADLINE,
    };
    let src = Arc::new(Tower {
        home: home()?,
        state_dir,
    });
    http::serve(listen, settings, src)?;
    Ok(())
}

const PAIR_USAGE: &str = "usage: herdr-tower mcp pair [request id] [--discord]";

/// `mcp pair`'s arguments: the request id, if given, and whether to post the
/// code to the notification channel.
///
/// Posting is opt-in (`--discord`). It used to be on whenever Discord was, so
/// anyone who could read the channel held every code as it was issued, and
/// could race the person to the page — or, with a page of their own the only
/// one open, be handed an act grant (#25). `--no-discord`, the old way to
/// skip it, is still accepted and does what it always did: nothing is posted.
fn pair_args(rest: &[String]) -> std::result::Result<(Option<&str>, bool), String> {
    let mut discord = false;
    let mut request: Option<&str> = None;
    for a in rest {
        match a.as_str() {
            "--discord" => discord = true,
            "--no-discord" => {}
            id if !id.starts_with('-') && request.is_none() => request = Some(id),
            _ => return Err(PAIR_USAGE.into()),
        }
    }
    Ok((request, discord))
}

/// The page a code goes to when `mcp pair` was given no id.
///
/// Never picked silently (#25). A stranger's page calls itself whatever it
/// likes, "Claude" included, and when the person's own page has not loaded
/// yet — or has expired — the only page open is the stranger's. So the one
/// open page is described (id, the name it gave itself, where it returns
/// to, how long it has been open) and used only on a `y` typed at a
/// terminal. Without a terminal there is nobody to ask, and the id is
/// required.
fn choose_page(
    open: &[grants::Pending],
    now: OffsetDateTime,
    tty: bool,
    ask: &mut dyn FnMut(&str) -> bool,
) -> std::result::Result<String, String> {
    match open {
        [] => Err("no approval page is open — add the connector in claude.ai (or reload its approval page) first, then run this again".into()),
        [only] => {
            let desc = oauth::describe_page(only, now);
            if !tty {
                return Err(format!(
                    "one approval page is open:\n  {desc}\nrun `herdr-tower mcp pair {}` if it is yours — with no terminal to confirm on, no page is picked for you",
                    only.id
                ));
            }
            let question = format!(
                "one approval page is open:\n  {desc}\nIs it yours — does your page show request {}? [y/N] ",
                only.id
            );
            if ask(&question) {
                Ok(only.id.clone())
            } else {
                Err(format!(
                    "no code issued. If your page shows another request, run `herdr-tower mcp pair <id>` with it; if it shows {}, run `herdr-tower mcp pair {}`",
                    only.id, only.id
                ))
            }
        }
        several => {
            let list: Vec<String> = several
                .iter()
                .map(|p| format!("  {}", oauth::describe_page(p, now)))
                .collect();
            Err(format!(
                "{} approval pages are open — run `herdr-tower mcp pair <id>` with the id shown on yours:\n{}",
                several.len(),
                list.join("\n")
            ))
        }
    }
}

/// Asks a yes/no question on the terminal; only `y` or `yes` is a yes.
fn ask_on_terminal(question: &str) -> bool {
    use std::io::{BufRead as _, Write as _};
    let mut out = std::io::stdout().lock();
    if write!(out, "{question}").and_then(|_| out.flush()).is_err() {
        return false;
    }
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn pair(rest: &[String]) -> Result<()> {
    use std::io::IsTerminal as _;
    let (request, discord) = pair_args(rest)?;
    let now = OffsetDateTime::now_utc();
    let dir = tower_state::default_dir();
    let id = match request {
        Some(id) => id.to_string(),
        None => {
            let open = oauth::open_pages(&grants::load(&dir)?, now);
            let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            choose_page(&open, now, tty, &mut ask_on_terminal)?
        }
    };
    let (code, req) = grants::update(&dir, now, |g| {
        oauth::issue_pairing(g, &id, now, &mut oauth::OsRandom)
    })??;
    let client = oauth::shown_name(&req.client_name);
    tower_cli::outln!("pairing code: {code}")?;
    tower_cli::outln!(
        "{}",
        request_line(
            &req.id,
            client,
            oauth::redirect_host(&req.redirect_uri),
            req.created_at,
            now
        )
    )?;
    tower_cli::outln!(
        "valid for 5 minutes, for that page only. Check the page shows request {}, then enter the code there.",
        req.id
    )?;
    if discord {
        let content = format!(
            "🔑 MCP pairing code for request {} ({}): **{code}** (5 minutes, that page only)",
            req.id,
            tower_cli::mcp::audit::discord_escape(client)
        );
        match post_to_channel(&content) {
            Some(Ok(())) => tower_cli::outln!("also posted to the notification channel.")?,
            Some(Err(e)) => eprintln!("could not post it to Discord: {e}"),
            None => eprintln!(
                "not posted: Discord notifications are off (notify_discord) or have no webhook"
            ),
        }
    }
    Ok(())
}

/// The notification channel's webhook and a message for it, when Discord
/// notifications are on and a webhook is set.
fn channel_message(content: &str) -> Option<(String, Vec<u8>)> {
    let cfg = outbound_config();
    if !cfg.notify_discord {
        return None;
    }
    let url = cfg.notify_url()?.to_string();
    let body = serde_json::json!({
        "username": "herdr-tower",
        "content": content,
        // Somebody's text rides along; an `@everyone` in it must not ping.
        "allowed_mentions": { "parse": [] },
    });
    Some((url, serde_json::to_vec(&body).ok()?))
}

/// Posts to the notification channel and says how it went; `None` when
/// there is nowhere to post.
fn post_to_channel(content: &str) -> Option<std::result::Result<(), String>> {
    let (url, body) = channel_message(content)?;
    let post = notify_sender().post_json?;
    Some(post(&url, &body).map_err(|e| e.to_string()))
}

/// [`post_to_channel`] off the calling thread: a request must not wait on a
/// slow webhook.
fn post_to_channel_later(content: String) {
    std::thread::spawn(move || {
        let _ = post_to_channel(&content);
    });
}

/// The line naming the approval page a pairing code is for. The name is the
/// client's own claim; the host it returns to is not.
fn request_line(
    id: &str,
    client: &str,
    host: &str,
    opened: OffsetDateTime,
    now: OffsetDateTime,
) -> String {
    format!(
        "for request {id} — \"{client}\" (self-declared), returns to {host}, opened {}",
        tower_text::age(opened, now)
    )
}

/// A `mcp grants` time cell: when it happened, or never.
fn when_used(t: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    t.map_or("never".to_string(), |t| tower_text::age(t, now))
}

fn list_grants() -> Result<()> {
    let g = grants::load(&tower_state::default_dir())?;
    if g.grants.is_empty() {
        tower_cli::outln!("no connected clients")?;
        return Ok(());
    }
    let now = OffsetDateTime::now_utc();
    let when = |t: Option<OffsetDateTime>| when_used(t, now);
    tower_cli::outln!("ID        CLIENT                SCOPES      CONNECTED     LAST USED")?;
    for x in &g.grants {
        let name = if x.client_name.is_empty() {
            "(unnamed)"
        } else {
            &x.client_name
        };
        tower_cli::outln!(
            "{:<9} {} {:<11} {:<13} {}",
            x.id,
            tower_text::pad_right(&tower_text::truncate_cell(name, 20), 21),
            x.scopes.join(","),
            when(Some(x.created_at)),
            when(x.last_used)
        )?;
    }
    Ok(())
}

fn revoke(rest: &[String]) -> Result<()> {
    let [which] = rest else {
        return Err("usage: herdr-tower mcp revoke <id> | --all".into());
    };
    let now = OffsetDateTime::now_utc();
    let gone = grants::update(&tower_state::default_dir(), now, |g| {
        oauth::revoke(g, which)
    })?;
    match gone {
        0 => Err(format!("no grant {which:?} — see herdr-tower mcp grants").into()),
        n => {
            tower_cli::outln!(
                "revoked {n} grant(s); the client has to be approved again with a new pairing code"
            )?;
            Ok(())
        }
    }
}

#[cfg(unix)]
fn uid() -> libc::uid_t {
    // SAFETY: getuid(2) takes nothing and cannot fail.
    unsafe { libc::getuid() }
}

/// Only the LaunchAgent paths ask, and they run on macOS alone.
#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

fn launchctl(args: &[&str]) -> Result<()> {
    let out = Command::new("launchctl").args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into())
    }
}

/// `mcp on`: the supervisor serves it — which is the default, so this is for
/// undoing `mcp off`. Also removes the LaunchAgent that served it before.
fn on() -> Result<()> {
    switch(true)
}

/// `mcp off`: the supervisor stops serving it. Grants and the tunnel stay.
fn off() -> Result<()> {
    switch(false)?;
    tower_cli::outln!(
        "connected clients keep their grants — `herdr-tower mcp revoke --all` drops them"
    )?;
    tower_cli::outln!("the tunnel is separate — `tailscale funnel reset` closes it")?;
    Ok(())
}

fn switch(on: bool) -> Result<()> {
    let path = tower_config::path().ok_or("no config path — is HOME set?")?;
    let mut sink = Vec::new();
    tower_cli::configcmd::run(
        &mut sink,
        &path,
        &["set".into(), "mcp.enabled".into(), on.to_string()],
    )?;
    retire_old_agent()?;
    restart_supervisor()?;
    let cfg = tower_config::load();
    let listen = cfg.mcp.listen()?;
    if !on {
        tower_cli::outln!("the supervisor no longer serves the remote MCP server")?;
        return Ok(());
    }
    tower_cli::outln!("the supervisor serves the remote MCP server on http://{listen}")?;
    match public_url(&cfg) {
        Ok(base) => {
            tower_cli::outln!("public URL: {base}/mcp")?;
            tower_cli::outln!("publish it with: tailscale funnel --bg {}", listen.port())?;
        }
        Err(why) => {
            tower_cli::outln!("this machine only (http://{listen}/mcp) — no public URL: {why}")?
        }
    }
    tower_cli::outln!(
        "log: {}",
        tower_state::default_dir().join("daemon.log").display()
    )?;
    Ok(())
}

/// Stops and removes the LaunchAgent `mcp on` used to install, which would
/// hold the port the supervisor now serves on.
fn retire_old_agent() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let home = home()?;
    let path = mcp::service_path(&home);
    if !path.is_file() {
        return Ok(());
    }
    let _ = launchctl(&["bootout", &format!("gui/{}/{}", uid(), mcp::LABEL)]);
    std::fs::remove_file(&path)?;
    tower_cli::outln!("removed {} — the supervisor serves it now", mcp::LABEL)?;
    Ok(())
}

/// Restarts the supervisor so it reads the switch. Where it is not a
/// LaunchAgent, says how.
fn restart_supervisor() -> Result<()> {
    let home = home()?;
    if cfg!(target_os = "macos") && tower_cli::daemoncmd::service_path(&home).is_file() {
        launchctl(&["kickstart", "-k", &format!("gui/{}/dev.herdr.tower", uid())])?;
        tower_cli::outln!("restarted the supervisor")?;
    } else {
        tower_cli::outln!("restart the supervisor to apply it: herdr-tower daemon install")?;
    }
    Ok(())
}

// ------------------------------------------------------------------ the source

/// What the tools read, from the real store, the real sockets and the real
/// agent files.
///
/// Read-only: nothing here writes the store. The resident supervisor keeps it
/// current, and a server reachable from the internet has no business being a
/// second writer of it.
struct Tower {
    home: PathBuf,
    state_dir: PathBuf,
}

impl Tower {
    /// The herdr server that owns this record's pane.
    fn client_for(&self, a: &Agent) -> std::result::Result<herdrx::Herdr, String> {
        tower_mux::discover_backends(&self.home)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|b| b.id == a.pane.backend)
            .map(|b| b.client())
            .ok_or_else(|| "the herdr server that owns this pane is not running".to_string())
    }
}

impl tools::Source for Tower {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn agents(&self) -> std::result::Result<Vec<Agent>, String> {
        tower_state::Store::new(&self.state_dir)
            .and_then(|s| s.list())
            .map_err(|e| format!("the store could not be read: {e}"))
    }

    fn ctx(&self, agents: &[Agent]) -> HashMap<String, tower_usage::CtxInfo> {
        tower_usage::agent_ctx(
            agents,
            &tower_usage::codex_sessions_root(&self.home),
            &tower_usage::gemini_dir(&self.home),
            &tower_usage::claude_dir(&self.home),
        )
    }

    fn branch(&self, cwd: &str) -> Option<String> {
        tower_wt::list_metas(&self.state_dir)
            .ok()?
            .into_iter()
            .find(|m| m.path == cwd)
            .map(|m| m.live_branch())
    }

    fn read_pane(&self, a: &Agent, lines: u32) -> std::result::Result<String, String> {
        self.client_for(a)?
            .read(a.pane_id(), herdrx::ReadSource::Recent, lines)
            .map_err(|e| format!("the pane could not be read: {e}"))
    }

    fn prompt(
        &self,
        a: &Agent,
        text: &str,
        wait: Option<std::time::Duration>,
    ) -> std::result::Result<tools::Sent, tools::SendError> {
        use tools::{SendError, Sent};
        let h = self.client_for(a).map_err(SendError::Failed)?;
        let pane_id = a.pane_id();
        // Asked first, the way `tower_mux::send_text` does, but with no shell
        // door behind it: a pane without an agent is refused, never typed into.
        let pane = h
            .pane(pane_id)
            .map_err(|e| SendError::Failed(format!("the pane could not be read: {e}")))?;
        if !pane.has_agent() {
            return Err(SendError::NoAgent);
        }
        // An empty `until` is herdr's own "settled": idle, done or blocked,
        // observed after the prompt started a turn.
        let opts = wait.map(|t| herdrx::PromptOptions {
            until: Vec::new(),
            timeout: Some(t),
        });
        match h.prompt(pane_id, text, opts.as_ref()) {
            Ok(()) if wait.is_some() => Ok(Sent::Settled),
            Ok(()) => Ok(Sent::Delivered),
            Err(e) if e.is_code("agent_blocked") => Err(SendError::Blocked),
            Err(e) if e.is_code("agent_prompt_stalled") => Err(SendError::Stalled),
            Err(e) if e.is_code("timeout") && wait.is_some() => Ok(Sent::StillGoing),
            Err(e) => Err(SendError::Failed(e.to_string())),
        }
    }

    fn wait(
        &self,
        a: &Agent,
        until: &[AgentState],
        within: std::time::Duration,
    ) -> std::result::Result<bool, String> {
        let statuses: Vec<herdrx::AgentStatus> = until
            .iter()
            .copied()
            .filter_map(tower_mux::herdr_status)
            .collect();
        if statuses.is_empty() {
            // An empty list is herdr's "anything settled", not "nothing".
            return Err("nothing herdr can wait for".into());
        }
        match self
            .client_for(a)?
            .wait(a.pane_id(), &statuses, Some(within))
        {
            Ok(()) => Ok(true),
            Err(e) if e.is_code("timeout") => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }

    fn live_state(&self, a: &Agent) -> Option<AgentState> {
        tower_cli::agentctl::live_state(&self.client_for(a).ok()?, a)
    }

    fn keys(&self, a: &Agent, keys: &[&str]) -> std::result::Result<(), String> {
        self.client_for(a)?
            .send_keys(a.pane_id(), keys)
            .map_err(|e| format!("the keys could not be sent: {e}"))
    }

    fn run_tower(
        &self,
        args: &[String],
        timeout: std::time::Duration,
    ) -> std::result::Result<tools::Ran, String> {
        // This very binary, with the server's own environment: the same state
        // directory and config the command would see on the Mac.
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        cmd.args(args);
        tower_cli::mcp::run::run(cmd, timeout, tools::TOWER_OUTPUT_CAP)
            .map_err(|e| format!("herdr-tower could not be started: {e}"))
    }

    fn run_herdr(
        &self,
        args: &[String],
        timeout: std::time::Duration,
    ) -> std::result::Result<tools::Ran, String> {
        let local = self.home.join(".local/bin/herdr");
        let exe = if local.is_file() {
            local.into_os_string()
        } else {
            "herdr".into()
        };
        let mut cmd = Command::new(exe);
        cmd.args(args);
        tower_cli::mcp::run::run(cmd, timeout, tools::TOWER_OUTPUT_CAP)
            .map_err(|e| format!("herdr could not be started: {e}"))
    }

    fn open(
        &self,
        o: &tower_cli::opencmd::OpenOptions,
    ) -> std::result::Result<tower_cli::opencmd::Opened, String> {
        // The default server, the one `herdr-tower open` on the Mac reaches:
        // this process runs outside herdr, so there is no pane of its own to
        // say otherwise.
        let h = tower_cli::muxsel::default_mux();
        let nonce = OffsetDateTime::now_utc().unix_timestamp_nanos() as u64;
        tower_cli::opencmd::open(&h, &tower_cli::muxsel::default_backend_id(), o, nonce)
    }

    fn close(&self, a: &Agent) -> std::result::Result<(), String> {
        let h = self.client_for(a)?;
        tower_cli::agentctl::close(&h, a)
    }

    fn workspaces(&self) -> std::result::Result<Vec<herdrx::Workspace>, String> {
        tower_cli::muxsel::default_mux()
            .workspaces()
            .map_err(|e| format!("herdr could not list its workspaces: {e}"))
    }

    fn create_workspace(
        &self,
        label: &str,
        cwd: &str,
    ) -> std::result::Result<(herdrx::Workspace, herdrx::Tab, herdrx::Pane), String> {
        tower_cli::muxsel::default_mux()
            .new_workspace(label, cwd, false)
            .map_err(|e| format!("herdr could not create workspace {label:?}: {e}"))
    }

    fn panes(&self) -> std::result::Result<Vec<herdrx::Pane>, String> {
        tower_cli::muxsel::default_mux()
            .list_panes()
            .map_err(|e| format!("herdr could not list its panes: {e}"))
    }

    fn pane(&self, pane_id: &str) -> std::result::Result<herdrx::Pane, String> {
        tower_cli::muxsel::default_mux()
            .pane(pane_id)
            .map_err(|e| format!("herdr could not read pane {pane_id}: {e}"))
    }

    fn pane_process_info(&self, pane_id: &str) -> std::result::Result<herdrx::ProcessInfo, String> {
        tower_cli::muxsel::default_mux()
            .process_info(pane_id)
            .map_err(|e| format!("herdr could not inspect pane {pane_id}: {e}"))
    }

    fn move_pane(
        &self,
        pane_id: &str,
        destination: &herdrx::PaneMoveDestination,
    ) -> std::result::Result<herdrx::PaneMoveResult, String> {
        tower_cli::muxsel::default_mux()
            .move_pane(pane_id, destination)
            .map_err(|e| format!("herdr could not move pane {pane_id}: {e}"))
    }

    fn workspaces_backend(&self) -> String {
        tower_cli::muxsel::default_backend_id()
    }

    fn boards(&self) -> std::result::Result<Vec<tools::BoardSummary>, String> {
        let boards = tower_state::BoardStore::new(&self.state_dir)
            .map_err(|e| format!("the boards could not be read: {e}"))?;
        let now = OffsetDateTime::now_utc();
        let repos = boards
            .repos()
            .map_err(|e| format!("the boards could not be read: {e}"))?;
        Ok(repos
            .into_iter()
            .filter_map(|repo| {
                // A reader never takes the lock; a board that will not load
                // right now is left out rather than failing the rest.
                let b = boards.load(&repo, now).ok()?;
                Some(tools::BoardSummary::of(repo, &b))
            })
            .collect())
    }

    fn audit(&self, e: &audit::Entry) {
        if let Err(err) = audit::append(&self.state_dir, e) {
            eprintln!("herdr-tower mcp: the audit line could not be written: {err}");
        }
        if !(audit::announced(e) && outbound_config().mcp.notify_acts_enabled()) {
            return;
        }
        post_to_channel_later(audit::line(e));
    }

    /// Every new grant, in the log and in the channel when Discord is on —
    /// whatever `mcp.notify_acts` says: it is about who can act, not an act.
    fn grant_created(&self, client: &str, act: bool) {
        eprintln!(
            "herdr-tower mcp: new grant for {:?} (act: {})",
            client,
            if act { "yes" } else { "no" }
        );
        post_to_channel_later(audit::grant_line(client, act));
    }

    fn report(&self, since: OffsetDateTime) -> Value {
        let now = OffsetDateTime::now_utc();
        let entries = tower_state::journal::read_since(&self.state_dir, since);
        let lines = tower_cli::reportcmd::summarize(&entries, since, now);
        let verdicts = tower_cli::reportcmd::verdicts(&self.state_dir, since);
        let remote = audit::read_since(&self.state_dir, since);
        tower_cli::reportcmd::to_json(&lines, &verdicts, &remote, since)
    }

    fn schedules(&self) -> std::result::Result<Value, String> {
        let list = tower_state::ScheduleStore::new(&self.state_dir)
            .and_then(|s| s.list())
            .map_err(|e| e.to_string())?;
        serde_json::to_value(list).map_err(|e| e.to_string())
    }

    fn loops(&self) -> std::result::Result<Value, String> {
        let list = tower_state::LoopStore::new(&self.state_dir)
            .and_then(|s| s.list())
            .map_err(|e| e.to_string())?;
        serde_json::to_value(list).map_err(|e| e.to_string())
    }

    fn supervisor_fresh(&self) -> bool {
        tower_state::DaemonStore::new(&self.state_dir)
            .read()
            .ok()
            .flatten()
            .is_some_and(|s| s.fresh(OffsetDateTime::now_utc()))
    }
}

// ------------------------------------------------------------------ doctor

/// `doctor`'s remote MCP section: the service, the listener, the tunnel, and
/// who is connected. Nothing at all when it was never set up — an unused
/// feature is not a problem.
pub fn doctor(home: &std::path::Path) -> Vec<tower_cli::doctorcmd::Finding> {
    use tower_cli::doctorcmd::Finding;
    let ok = |line: String| Finding { ok: true, line };
    let bad = |line: String| Finding { ok: false, line };
    let cfg = tower_config::load();
    let mut out = Vec::new();
    let old = mcp::service_path(home);
    if old.is_file() {
        out.push(bad(format!(
            "{}: the LaunchAgent that served it before — `herdr-tower daemon install` removes it, and it holds the port the supervisor serves on",
            old.display()
        )));
    }
    if !cfg.mcp.enabled() {
        out.push(ok(
            "off — `herdr-tower mcp on` has the supervisor serve it".into()
        ));
        return out;
    }
    let probe = |url: &str| {
        ureq::get(url)
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build()
            .call()
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    let meta = "/.well-known/oauth-authorization-server";
    let base = public_url(&cfg);
    match (cfg.mcp.listen(), base_url(&cfg)) {
        (Err(e), _) => out.push(bad(e)),
        (Ok(_), Err(e)) => out.push(bad(e.to_string())),
        (Ok(listen), Ok(_)) => out.push(match probe(&format!("http://{listen}{meta}")) {
            Ok(()) => ok(format!("served by the supervisor on http://{listen}")),
            Err(e) => bad(format!(
                "nothing answers on http://{listen} ({e}) — is the supervisor running? herdr-tower daemon start"
            )),
        }),
    }
    // No public URL is a setup's state, not a fault: it serves this machine.
    if let (Err(why), Ok(listen)) = (&base, cfg.mcp.listen()) {
        if cfg.mcp.public_url.is_none() {
            out.push(ok(format!(
                "this machine only (http://{listen}/mcp) — no public URL: {why}"
            )));
        }
    }
    if let (Ok(base), Ok(listen)) = (&base, cfg.mcp.listen()) {
        // Unpublished is a choice, not a fault: nothing outside reaches the
        // server until the tunnel is opened.
        out.push(ok(match probe(&format!("{base}{meta}")) {
            Ok(()) => format!("public: {base}/mcp"),
            Err(_) => format!(
                "not published — `tailscale funnel --bg {}` publishes {base}/mcp",
                listen.port()
            ),
        }));
    }
    out.push(match grants::load(&tower_state::default_dir()) {
        Ok(g) => ok(format!("{} connected client(s)", g.grants.len())),
        Err(e) => bad(format!(
            "{}: {e}",
            grants::path(&tower_state::default_dir()).display()
        )),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_public_url_is_this_machines_tailscale_name() {
        assert_eq!(
            tailscale_url_from(
                r#"{"Self": {"DNSName": "mac.tail1234.ts.net.", "HostName": "mac"}}"#
            )
            .as_deref(),
            Some("https://mac.tail1234.ts.net")
        );
        // Logged out: no name. Not Tailscale at all: nothing.
        assert_eq!(tailscale_url_from(r#"{"Self": {"DNSName": ""}}"#), None);
        assert_eq!(tailscale_url_from("Tailscale is stopped."), None);
        assert_eq!(tailscale_url_from(r#"{"Self": {"DNSName": "a b."}}"#), None);
    }

    #[test]
    fn times_under_a_minute_read_just_now_not_just_now_ago() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let ago = |s: i64| now - time::Duration::seconds(s);
        assert_eq!(
            request_line("566D14", "Claude", "claude.ai", ago(3), now),
            "for request 566D14 — \"Claude\" (self-declared), returns to claude.ai, opened just now"
        );
        assert_eq!(
            request_line("566D14", "Claude", "claude.ai", ago(180), now),
            "for request 566D14 — \"Claude\" (self-declared), returns to claude.ai, opened 3m ago"
        );
        assert_eq!(when_used(Some(ago(0)), now), "just now");
        assert_eq!(when_used(Some(ago(7200)), now), "2h ago");
        assert_eq!(when_used(None, now), "never");
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// #25: the code goes to Discord only when asked; `--no-discord` is
    /// still accepted, and changes nothing.
    #[test]
    fn posting_the_code_is_opt_in() {
        let a = args(&[]);
        assert_eq!(pair_args(&a).unwrap(), (None, false));
        let a = args(&["3F9A0C"]);
        assert_eq!(pair_args(&a).unwrap(), (Some("3F9A0C"), false));
        let a = args(&["3F9A0C", "--discord"]);
        assert_eq!(pair_args(&a).unwrap(), (Some("3F9A0C"), true));
        let a = args(&["--no-discord", "3F9A0C"]);
        assert_eq!(pair_args(&a).unwrap(), (Some("3F9A0C"), false));
        assert!(pair_args(&args(&["--dicsord"])).is_err());
        assert!(pair_args(&args(&["A", "B"])).is_err());
    }

    fn open_page(id: &str, name: &str, opened: OffsetDateTime) -> grants::Pending {
        grants::Pending {
            id: id.into(),
            client_id: "c-1".into(),
            client_name: name.into(),
            redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
            challenge: String::new(),
            state: None,
            scopes: vec![],
            created_at: opened,
            expires_at: opened + time::Duration::minutes(10),
            failures: 0,
        }
    }

    /// #25: with no id, the one open page is never picked without a yes at
    /// a terminal — and with no terminal, not at all.
    #[test]
    fn the_only_open_page_is_never_picked_silently() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let only = [open_page(
            "3F9A0C",
            "Claude",
            now - time::Duration::minutes(2),
        )];
        let mut asked = Vec::new();
        let mut never = |q: &str| -> bool {
            asked.push(q.to_string());
            true
        };
        let e = choose_page(&only, now, false, &mut never).unwrap_err();
        assert!(e.contains("herdr-tower mcp pair 3F9A0C"), "{e}");
        assert!(
            e.contains("\"Claude\" (self-declared), returns to claude.ai, opened 2m ago"),
            "{e}"
        );
        assert!(asked.is_empty(), "no terminal, nobody asked");

        let mut questions = Vec::new();
        let mut no = |q: &str| {
            questions.push(q.to_string());
            false
        };
        assert!(choose_page(&only, now, true, &mut no).is_err());
        assert!(questions[0].contains("3F9A0C") && questions[0].ends_with("[y/N] "));
        let mut yes = |_: &str| true;
        assert_eq!(choose_page(&only, now, true, &mut yes).unwrap(), "3F9A0C");

        let two = [
            open_page("3F9A0C", "Claude", now),
            open_page("77AB12", "Claude", now),
        ];
        let e = choose_page(&two, now, true, &mut yes).unwrap_err();
        assert!(e.contains("3F9A0C") && e.contains("77AB12"), "{e}");
        assert!(choose_page(&[], now, true, &mut yes).is_err());
    }
}
