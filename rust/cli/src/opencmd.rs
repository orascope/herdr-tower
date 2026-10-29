// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower open <folder>` and the remote MCP server's `open_agent`: a new
//! herdr tab in a folder, with an agent started in it and, when asked, a first
//! message typed into it.
//!
//! What `wt new` does minus the worktree. `wt new` is for a task that gets its
//! own branch; this is for "start claude over there", where a branch and a
//! checkout nobody asked for would be debris to clean up afterwards.
//!
//! The launch goes through herdr's `agent.start`, as `wt new`'s does, so herdr
//! waits for the agent to come up and answers once it can take input — which is
//! what makes typing the first message right after it safe.

use std::path::{Path, PathBuf};
use std::time::Duration;

use herdrx::{Herdr, PromptOptions, StartOptions};

/// What to open.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenOptions<'a> {
    /// The folder the agent starts in. `~/` is expanded; anything else has to
    /// be absolute, because a relative path would resolve against wherever the
    /// caller happens to run — the supervisor's folder, for the MCP server.
    pub dir: &'a str,
    /// Empty means `claude`.
    pub agent: &'a str,
    /// The workspace to open the tab in, by label. Created in the background
    /// when there is none by that name. Empty means the caller's own workspace
    /// inside herdr, or the focused one outside it.
    pub workspace: &'a str,
    /// Typed into the agent once it is up. Empty sends nothing.
    pub prompt: &'a str,
    /// With a prompt: how long to wait for the agent to settle after it.
    pub wait: Option<Duration>,
}

/// What was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The record ID the tower will list it under (`claude-w2:p9B`).
    pub id: String,
    /// The store's name for the herdr server it is on; empty for the default.
    pub backend: String,
    pub kind: String,
    /// The unique name herdr knows the agent by.
    pub name: String,
    pub pane_id: String,
    /// The workspace's label, when herdr could name it.
    pub workspace: String,
    pub dir: String,
    pub prompt: Prompted,
}

/// What happened to the first message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompted {
    /// None was asked for.
    Nothing,
    Delivered,
    /// Delivered, and the agent settled within the wait.
    Settled,
    /// Delivered, and the agent was still working when the wait ran out.
    StillGoing,
    /// The agent is up but the message did not go in.
    Failed(String),
}

/// Resolves the folder: `~/` expanded, absolute, an existing directory, with
/// its symlinks resolved so the tower's records name it the way hooks will.
pub fn resolve_dir(dir: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Err("a folder is required".into());
    }
    let expanded = match (dir, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (d, Some(h)) if d.starts_with("~/") => h.join(&d[2..]),
        (d, None) if d == "~" || d.starts_with("~/") => {
            return Err("the home directory is unknown ($HOME)".into())
        }
        (d, _) => PathBuf::from(d),
    };
    if !expanded.is_absolute() {
        return Err(format!(
            "{dir:?} is not an absolute path — give the whole path, or one starting with ~/"
        ));
    }
    let canon =
        std::fs::canonicalize(&expanded).map_err(|e| format!("{}: {e}", expanded.display()))?;
    if !canon.is_dir() {
        return Err(format!("{} is not a folder", canon.display()));
    }
    Ok(canon)
}

/// The agent kind, defaulted and checked.
pub fn resolve_agent(agent: &str) -> Result<&str, String> {
    let agent = if agent.is_empty() { "claude" } else { agent };
    if !tower_wt::AGENTS.contains(&agent) {
        return Err(format!(
            "unknown agent {agent:?} — one of {}",
            tower_wt::AGENTS.join(", ")
        ));
    }
    Ok(agent)
}

/// A herdr agent name for this launch: the kind and the folder, readable in
/// herdr's own list, made unique by `nonce`.
///
/// herdr refuses a name already in use, and "claude in this folder" is exactly
/// what somebody opens twice, so the folder alone cannot be the name. The
/// nonce goes into [`tower_wt::agent_name`]'s digest rather than onto the end,
/// which keeps the result inside herdr's 32 characters.
pub fn launch_name(kind: &str, dir: &Path, nonce: u64) -> String {
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    tower_wt::agent_name(&format!("{kind}-{base}-{nonce:x}"))
}

/// Opens the tab, starts the agent, and types the first message.
///
/// `backend` is the store's name for the server `h` talks to (empty for the
/// default socket); it only goes into the record ID. `nonce` makes the herdr
/// name unique — the clock, for the real callers.
///
/// A failure after the tab exists says which pane it left, because the pane
/// is kept on purpose: somebody has to be able to look at what went wrong.
pub fn open(h: &Herdr, backend: &str, o: &OpenOptions, nonce: u64) -> Result<Opened, String> {
    let kind = resolve_agent(o.agent)?;
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let dir = resolve_dir(o.dir, home.as_deref())?;
    let dir_str = dir.to_string_lossy().into_owned();
    let tab = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir_str.clone());

    let pane_id = if o.workspace.is_empty() {
        tower_mux::spawn_in_focused(h, &tab, &dir_str, None)
    } else {
        match h.workspace_id_by_label(o.workspace) {
            Ok(Some(_)) => tower_mux::spawn_in(h, o.workspace, &tab, &dir_str, None),
            Ok(None) => tower_mux::new_session(h, o.workspace, &dir_str),
            Err(e) => return Err(format!("herdr could not list its workspaces: {e}")),
        }
    }
    .map_err(|e| format!("herdr could not open a tab: {e}"))?;

    let command = tower_wt::command_for(kind, &tower_wt::gemini_dir());
    let (bin, args) = tower_wt::split_command(&command);
    let name = launch_name(kind, &dir, nonce);
    let start = StartOptions {
        name: name.clone(),
        kind: tower_wt::agent_kind(kind, bin).to_string(),
        pane_id: pane_id.clone(),
        args: args.into_iter().map(str::to_string).collect(),
        timeout: None,
    };
    // A tab's shell comes up a moment after the tab: `agent.start` straight
    // after `workspace.create` was refused with "agent target pane w32:p1 is not
    // an available shell" (measured 2026-09-26).
    herdrx::retry_while("agent_pane_busy", || h.start_agent(&start)).map_err(|e| {
        format!("{kind} did not start in pane {pane_id} (the tab is left open): {e}")
    })?;

    let prompt = if o.prompt.trim().is_empty() {
        Prompted::Nothing
    } else {
        let opts = o.wait.map(|t| PromptOptions {
            until: Vec::new(),
            timeout: Some(t),
        });
        // `agent.start` answers once the agent is up, but herdr registers it
        // as a named agent a moment later: a prompt sent straight after was
        // refused with "agent w31:p1 is not an active named agent", and the
        // same prompt a second later went in (measured 2026-09-26).
        match herdrx::retry_while("agent_not_ready", || {
            h.prompt(&pane_id, o.prompt, opts.as_ref())
        }) {
            Ok(()) if o.wait.is_some() => Prompted::Settled,
            Ok(()) => Prompted::Delivered,
            Err(e) if e.is_code("timeout") && o.wait.is_some() => Prompted::StillGoing,
            Err(e) => Prompted::Failed(e.to_string()),
        }
    };

    let workspace = h
        .pane(&pane_id)
        .ok()
        .and_then(|p| h.workspace_labels().ok()?.remove(&p.workspace_id))
        .unwrap_or_else(|| o.workspace.to_string());
    Ok(Opened {
        id: tower_scan::id_for_backend_pane(kind, backend, &pane_id),
        backend: backend.to_string(),
        kind: kind.to_string(),
        name,
        pane_id,
        workspace,
        dir: dir_str,
        prompt,
    })
}

/// `herdr-tower open`'s command line, parsed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CliArgs {
    pub dir: String,
    pub agent: String,
    pub workspace: String,
    pub prompt: String,
    pub json: bool,
}

pub const USAGE: &str = "usage: herdr-tower open <folder> [--agent claude|codex|gemini|opencode] [--workspace label] [--prompt text] [--json]";

fn flags() -> crate::helpcmd::FlagSet {
    crate::helpcmd::FlagSet::new("open")
        .string("agent", "claude", "the agent to start")
        .string(
            "workspace",
            "",
            "the herdr workspace, by label (created when missing; default: this one)",
        )
        .string("prompt", "", "a first message to type once the agent is up")
        .bool("json", false, "print the result as JSON")
}

/// Parses `open <folder> [flags]` — the folder first or after the flags, and
/// exactly one of it.
pub fn parse_args(args: &[String]) -> Result<CliArgs, String> {
    let fs = flags();
    let (dir, p) =
        crate::agentctl::one_positional(&fs, args, USAGE, "a folder", "open takes one folder")?;
    Ok(CliArgs {
        dir,
        agent: p.str("agent").to_string(),
        workspace: p.str("workspace").to_string(),
        prompt: p.str("prompt").to_string(),
        json: p.bool("json"),
    })
}

/// The result as `--json` and the MCP tool give it.
pub fn to_json(o: &Opened) -> serde_json::Value {
    let (prompt, error) = match &o.prompt {
        Prompted::Nothing => ("none", None),
        Prompted::Delivered => ("sent", None),
        Prompted::Settled => ("settled", None),
        Prompted::StillGoing => ("still_going", None),
        Prompted::Failed(e) => ("failed", Some(e.as_str())),
    };
    let mut v = serde_json::json!({
        "id": o.id,
        "kind": o.kind,
        "name": o.name,
        "pane_id": o.pane_id,
        "workspace": o.workspace,
        "dir": o.dir,
        "prompt": prompt,
    });
    if let Some(e) = error {
        v["prompt_error"] = e.into();
    }
    v
}

/// `herdr-tower open`'s report, one fact per line.
pub fn describe(o: &Opened) -> String {
    let mut out = format!(
        "opened {} ({}) in {}\n  pane:      {}\n  workspace: {}\n  name:      {}\n",
        o.id,
        o.kind,
        tower_text::shorten_home(&o.dir),
        o.pane_id,
        if o.workspace.is_empty() {
            "?"
        } else {
            &o.workspace
        },
        o.name
    );
    match &o.prompt {
        Prompted::Nothing => {}
        Prompted::Delivered => out.push_str("  prompt:    sent\n"),
        Prompted::Settled => out.push_str("  prompt:    sent; it has settled\n"),
        Prompted::StillGoing => out.push_str("  prompt:    sent; still going\n"),
        Prompted::Failed(e) => out.push_str(&format!("  prompt:    not sent — {e}\n")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};

    /// A counter, not the clock, names each test's scratch folder and socket:
    /// macOS's clock ticks in microseconds, so two tests starting together got
    /// the same name and one tore down the other's server mid-test.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn seq() -> u32 {
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("op{}-{}", std::process::id(), seq()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    #[test]
    fn a_folder_has_to_be_absolute_or_under_home_and_real() {
        let home = tmp();
        std::fs::create_dir_all(home.join("proj")).unwrap();
        assert_eq!(
            resolve_dir("~/proj", Some(&home)).unwrap(),
            home.join("proj")
        );
        assert_eq!(resolve_dir("~", Some(&home)).unwrap(), home);
        assert_eq!(
            resolve_dir(&home.join("proj").to_string_lossy(), None).unwrap(),
            home.join("proj")
        );
        assert!(resolve_dir("proj", Some(&home))
            .unwrap_err()
            .contains("not an absolute path"));
        assert!(resolve_dir("", Some(&home)).is_err());
        assert!(resolve_dir("~/proj", None).unwrap_err().contains("$HOME"));
        assert!(resolve_dir("~/nope", Some(&home)).is_err());
        std::fs::write(home.join("file"), "").unwrap();
        assert!(resolve_dir("~/file", Some(&home))
            .unwrap_err()
            .contains("not a folder"));
        let _ = std::fs::remove_dir_all(home);
    }

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_folder_can_come_before_or_after_the_flags_but_only_once() {
        let want = CliArgs {
            dir: "~/p".into(),
            agent: "codex".into(),
            prompt: "hi there".into(),
            ..CliArgs::default()
        };
        assert_eq!(
            parse_args(&words(&["~/p", "--agent", "codex", "--prompt", "hi there"])).unwrap(),
            want
        );
        assert_eq!(
            parse_args(&words(&["--agent", "codex", "--prompt", "hi there", "~/p"])).unwrap(),
            want
        );
        let d = parse_args(&words(&["~/p"])).unwrap();
        assert_eq!((d.agent.as_str(), d.json), ("claude", false));
        assert!(parse_args(&[])
            .unwrap_err()
            .contains("a folder is required"));
        assert!(parse_args(&words(&["~/p", "typo"]))
            .unwrap_err()
            .contains("\"typo\""));
        assert!(parse_args(&words(&["~/p", "--nope"])).is_err());
    }

    #[test]
    fn json_names_the_prompt_outcome_and_its_error() {
        let mut o = Opened {
            id: "claude-w2:pN".into(),
            backend: String::new(),
            kind: "claude".into(),
            name: "claude-p-1".into(),
            pane_id: "w2:pN".into(),
            workspace: "dev".into(),
            dir: "/p".into(),
            prompt: Prompted::Nothing,
        };
        assert_eq!(to_json(&o)["prompt"], "none");
        assert!(to_json(&o).get("prompt_error").is_none());
        o.prompt = Prompted::Failed("agent_blocked".into());
        assert_eq!(to_json(&o)["prompt"], "failed");
        assert_eq!(to_json(&o)["prompt_error"], "agent_blocked");
        assert!(describe(&o).contains("not sent — agent_blocked"));
    }

    #[test]
    fn the_agent_defaults_to_claude_and_must_be_supervised() {
        assert_eq!(resolve_agent("").unwrap(), "claude");
        assert_eq!(resolve_agent("codex").unwrap(), "codex");
        assert!(resolve_agent("bash").unwrap_err().contains("unknown agent"));
    }

    #[test]
    fn launch_names_are_legal_readable_and_distinct() {
        let dir = Path::new("/Users/me/Herdr Tower");
        let a = launch_name("claude", dir, 1);
        let b = launch_name("claude", dir, 2);
        assert_ne!(a, b, "opening the same folder twice needs two names");
        for n in [&a, &b] {
            assert!(n.starts_with("claude-herdr-tower-"), "{n}");
            assert!(n.len() <= 32, "{n}");
            assert!(n
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));
        }
    }

    /// A herdr server that answers from `resp` and records every call.
    #[cfg(unix)]
    struct Fake {
        dir: PathBuf,
        socket: PathBuf,
        calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }

    #[cfg(unix)]
    impl Fake {
        fn new(resp: fn(&str) -> &'static str) -> Fake {
            // Short, because a socket path is capped at 104 bytes on macOS.
            let dir = std::env::temp_dir().join(format!("oc{}-{}", std::process::id(), seq()));
            std::fs::create_dir_all(&dir).unwrap();
            let socket = dir.join("h.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let c = calls.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { return };
                    let mut line = String::new();
                    if BufReader::new(conn.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap_or(0)
                        == 0
                    {
                        continue;
                    }
                    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap_or_default().to_string();
                    c.lock()
                        .unwrap()
                        .push((method.clone(), req["params"].clone()));
                    let body = resp(&method).replace("$ID", req["id"].as_str().unwrap_or(""));
                    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                    let _ = conn.write_all(format!("{body}\n").as_bytes());
                }
            });
            Fake { dir, socket, calls }
        }
        fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }
        fn params(&self, method: &str) -> serde_json::Value {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .find(|(m, _)| m == method)
                .map(|(_, p)| p.clone())
                .unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    const OK: &str = r#"{"id":"$ID","result":{"type":"ok"}}"#;

    #[cfg(unix)]
    fn herdr(method: &str) -> &'static str {
        match method {
            "workspace.list" => {
                r#"{"id":"$ID","result":{"type":"workspace_list","workspaces":[
                {"workspace_id":"w2","label":"dev","focused":true,"pane_count":1,"tab_count":1}]}}"#
            }
            "tab.create" => {
                r#"{"id":"$ID","result":{"tab":{"tab_id":"w2:t9"},"root_pane":{"pane_id":"w2:pN"}}}"#
            }
            "pane.get" => {
                r#"{"id":"$ID","result":{"pane":{"pane_id":"w2:pN","workspace_id":"w2","tab_id":"w2:t9","cwd":"/"}}}"#
            }
            _ => OK,
        }
    }

    #[cfg(unix)]
    #[test]
    fn opens_a_tab_in_the_named_workspace_starts_the_agent_then_prompts() {
        let f = Fake::new(herdr);
        let dir = tmp();
        let o = OpenOptions {
            dir: &dir.to_string_lossy(),
            workspace: "dev",
            prompt: "hi",
            ..OpenOptions::default()
        };
        let got = open(&Herdr::at(&f.socket), "", &o, 7).unwrap();
        assert_eq!(got.id, "claude-w2:pN");
        assert_eq!(got.pane_id, "w2:pN");
        assert_eq!(got.workspace, "dev");
        assert_eq!(got.prompt, Prompted::Delivered);

        let m = f.methods();
        let pos = |name: &str| {
            m.iter()
                .position(|x| x == name)
                .unwrap_or_else(|| panic!("{name} in {m:?}"))
        };
        assert!(pos("tab.create") < pos("agent.start"));
        // The pane carries its ID before the agent starts, and the name
        // survives the start (checked against a live 0.9.1 server); the tab
        // keeps the plain name it was created with.
        assert!(!m.contains(&"tab.rename".to_string()), "{m:?}");
        assert!(pos("pane.rename") < pos("agent.start"));
        let name = f.params("tab.create")["label"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(f.params("pane.rename")["pane_id"], "w2:pN");
        assert_eq!(f.params("pane.rename")["label"], format!("[w2:pN] {name}"));
        assert!(
            pos("agent.start") < pos("agent.prompt"),
            "the prompt waits for the agent"
        );
        assert_eq!(f.params("tab.create")["workspace_id"], "w2");
        assert_eq!(
            f.params("tab.create")["cwd"],
            dir.to_string_lossy().as_ref()
        );
        assert_eq!(f.params("agent.start")["kind"], "claude");
        assert_eq!(f.params("agent.start")["pane_id"], "w2:pN");
        assert_eq!(f.params("agent.prompt")["target"], "w2:pN");
        assert_eq!(f.params("agent.prompt")["text"], "hi");
        assert!(
            !m.contains(&"pane.send_input".to_string()),
            "nothing is typed into a shell"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_that_does_not_exist_is_created_and_no_prompt_is_sent_unasked() {
        let f = Fake::new(|method| match method {
            "workspace.create" => {
                r#"{"id":"$ID","result":{"workspace":{"workspace_id":"w5","label":"new"},
                "tab":{"tab_id":"w5:t1"},"root_pane":{"pane_id":"w5:p1"}}}"#
            }
            m => herdr(m),
        });
        let dir = tmp();
        let o = OpenOptions {
            dir: &dir.to_string_lossy(),
            agent: "codex",
            workspace: "new",
            ..OpenOptions::default()
        };
        let got = open(&Herdr::at(&f.socket), "", &o, 7).unwrap();
        assert_eq!(got.id, "codex-w5:p1");
        assert_eq!(got.prompt, Prompted::Nothing);
        let m = f.methods();
        assert!(m.contains(&"workspace.create".to_string()), "{m:?}");
        assert!(!m.contains(&"tab.create".to_string()), "{m:?}");
        // The workspace keeps the name it is found by and its tab is not
        // renamed; the first pane gets its ID.
        assert_eq!(f.params("workspace.create")["label"], "new");
        assert!(!m.contains(&"workspace.rename".to_string()), "{m:?}");
        assert!(!m.contains(&"tab.rename".to_string()), "{m:?}");
        assert_eq!(f.params("pane.rename")["label"], "[w5:p1] new");
        assert!(!m.contains(&"agent.prompt".to_string()), "{m:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn nothing_reaches_herdr_for_a_bad_folder_or_agent() {
        let f = Fake::new(herdr);
        for o in [
            OpenOptions {
                dir: "relative",
                ..OpenOptions::default()
            },
            OpenOptions {
                dir: "/",
                agent: "bash",
                ..OpenOptions::default()
            },
        ] {
            assert!(open(&Herdr::at(&f.socket), "", &o, 1).is_err());
        }
        assert!(f.methods().is_empty(), "{:?}", f.methods());
    }

    #[cfg(unix)]
    #[test]
    fn a_first_prompt_waits_out_herdrs_registration_but_not_a_real_refusal() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NOT_READY: AtomicU32 = AtomicU32::new(0);
        let f = Fake::new(|method| match method {
            "agent.prompt" if NOT_READY.fetch_add(1, Ordering::SeqCst) < 2 => {
                r#"{"id":"$ID","error":{"code":"agent_not_ready","message":"agent w2:pN is not an active named agent"}}"#
            }
            m => herdr(m),
        });
        let dir = tmp();
        let o = OpenOptions {
            dir: &dir.to_string_lossy(),
            workspace: "dev",
            prompt: "hi",
            ..OpenOptions::default()
        };
        let got = open(&Herdr::at(&f.socket), "", &o, 1).unwrap();
        assert_eq!(got.prompt, Prompted::Delivered);
        let prompts = f.methods().iter().filter(|m| *m == "agent.prompt").count();
        assert_eq!(prompts, 3, "two refusals, then the one that went in");

        let f = Fake::new(|method| match method {
            "agent.prompt" => {
                r#"{"id":"$ID","error":{"code":"agent_blocked","message":"at a dialog"}}"#
            }
            m => herdr(m),
        });
        let got = open(&Herdr::at(&f.socket), "", &o, 2).unwrap();
        assert!(
            matches!(got.prompt, Prompted::Failed(_)),
            "{:?}",
            got.prompt
        );
        let prompts = f.methods().iter().filter(|m| *m == "agent.prompt").count();
        assert_eq!(prompts, 1, "a blocked agent is an answer, not a race");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_start_waits_for_the_new_tabs_shell() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static BUSY: AtomicU32 = AtomicU32::new(0);
        let f = Fake::new(|method| match method {
            "agent.start" if BUSY.fetch_add(1, Ordering::SeqCst) < 2 => {
                r#"{"id":"$ID","error":{"code":"agent_pane_busy","message":"agent target pane w2:pN is not an available shell"}}"#
            }
            m => herdr(m),
        });
        let dir = tmp();
        let o = OpenOptions {
            dir: &dir.to_string_lossy(),
            workspace: "dev",
            ..OpenOptions::default()
        };
        assert_eq!(
            open(&Herdr::at(&f.socket), "", &o, 1).unwrap().pane_id,
            "w2:pN"
        );
        let starts = f.methods().iter().filter(|m| *m == "agent.start").count();
        assert_eq!(starts, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_start_names_the_pane_it_left() {
        let f = Fake::new(|method| match method {
            "agent.start" => r#"{"id":"$ID","error":{"code":"agent_start_failed","message":"no"}}"#,
            m => herdr(m),
        });
        let dir = tmp();
        let o = OpenOptions {
            dir: &dir.to_string_lossy(),
            workspace: "dev",
            prompt: "hi",
            ..OpenOptions::default()
        };
        let e = open(&Herdr::at(&f.socket), "", &o, 1).unwrap_err();
        assert!(e.contains("w2:pN"), "{e}");
        assert!(!f.methods().contains(&"agent.prompt".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
