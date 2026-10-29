// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Everything the screen needs from outside its own process.
//!
//! # Why this is a trait when `tower_mux` deliberately has none
//!
//! `tower_mux`'s own doc explains that it dropped its four role interfaces because
//! herdr is the only backend and "one implementation behind an interface is
//! indirection with nothing on the other side of it". That reasoning holds
//! there and does not hold here, because the second implementation is not a
//! second backend — it is *no* backend.
//!
//! Every method below either talks to a socket, spawns a process, or sweeps the
//! filesystem. A test that renders the tower, presses `enter`, and checks which
//! pane was focused cannot have any of that happening for real: it would need a
//! running herdr server, and it would move the view of whoever is working in
//! that terminal. A trait is the seam for that: rather than a function field
//! per effect injected one by one, the arguments are named and the whole set is
//! substitutable at once.
//!
//! The trait is `Send + Sync` because the background threads in [`crate::feed`]
//! hold a clone: the pane preview runs on its own clock, off the redraw path.
//!
//! # Errors are strings
//!
//! Every failure here ends up on the tower's bottom line as one sentence, and
//! nothing branches on which kind it was. An enum whose every variant is
//! `Display`ed and discarded would be ceremony; the message is the interface, so
//! the message is the type. This is the opposite of the choice `tower_mux::Error`
//! makes, and for the opposite reason — a caller of `tower_mux` decides what to do
//! about `NoWorkspace`, whereas the tower only ever tells the user.

use std::collections::HashMap;

/// The outside world, as the control tower needs it.
pub trait Tower: Send + Sync {
    /// Whether the tower is running inside the backend it is supervising.
    ///
    /// Decides what `enter` means. Inside, it moves the user's view, and the
    /// tower exits unless it has a pane of its own ([`Tower::self_pane`]).
    /// Outside — an ssh session, a plain terminal — it attaches this terminal to
    /// the session instead of assuming there is a view here to move.
    ///
    /// **It does not stop the other client's screen moving, and this comment used
    /// to claim it did.** Landing on the agent's pane means focusing it, focus is
    /// a property of the *session* rather than of a client, and herdr exposes no
    /// client-scoped selection — checked against `herdr --help` and the socket
    /// schema (`herdr api schema`), which carry no such parameter: `pane.focus`
    /// takes a pane id alone, and `herdr session attach` a session name alone.
    /// So pressing enter over ssh still moves the pane on the desk. Attaching
    /// without focusing would leave the person who pressed it wherever the
    /// session happened to be, which is not what they asked for either.
    ///
    /// What the outside path does avoid is moving that desk for nothing: the
    /// selection is sent once the attach is running, so a command that never
    /// started leaves every screen where it was. See [`crate::Action::Run`].
    fn inside(&self) -> bool;

    /// The pane the tower itself occupies, so a broadcast skips it.
    fn self_pane(&self) -> Option<String>;

    /// Moves the user's view to a pane.
    fn focus(&self, pane_id: &str) -> Result<(), String>;

    /// The command that attaches *this* terminal to the session.
    ///
    /// Only the command. The pane is selected afterwards, through [`Self::focus`]
    /// and only once that command is running.
    fn attach_argv(&self, target: &str) -> Result<Vec<String>, String>;

    /// The last `lines` rows of a pane, for the preview. `None` on any failure:
    /// a preview that cannot be read is a blank panel, never an error banner.
    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String>;

    /// Writes one instruction into a pane.
    fn send(&self, pane_id: &str, text: &str) -> Result<(), String>;

    /// Clears a previous agent-side stop signal before a new loop lease starts.
    fn prepare_loop(&self, pane_id: &str);

    /// Removes this tool's sidebar badge when a lease is switched off.
    fn clear_loop_display(&self, pane_id: &str);

    /// Ensures the resident supervisor service is installed and running.
    fn start_loop_supervisor(&self, exe: &str) -> Result<(), String>;

    /// Whether `dir` is inside a git repository.
    ///
    /// Not backend work, but a process spawn, which is why it is here: `lazygit`
    /// exits instantly in a non-repository, which on a suspended TUI reads as a
    /// screen flicker and nothing else. Asking first turns that into a sentence.
    /// A `.git` existence check is not a substitute — a worktree's `.git` is a
    /// file, and a subdirectory of a repository has neither.
    fn is_git_repo(&self, dir: &str) -> bool;

    /// The open pull request each of `folders` has its branch in, by number.
    ///
    /// A folder with none — no GitHub remote, a branch nobody opened a PR for,
    /// one already merged or closed, no `gh` or no login — is simply absent.
    /// The number is a label beside the branch; nothing waits on it.
    fn open_prs(&self, folders: &[String]) -> HashMap<String, u64>;

    /// The branch checked out in each of `folders`. A folder that is no git
    /// checkout, or one on a detached HEAD, is absent.
    fn checked_out_branches(&self, folders: &[String]) -> HashMap<String, String>;

    /// Agents on the machines herdr has saved, read-only. Empty when there are
    /// none or herdr cannot be run.
    fn remote_machines(&self) -> Vec<crate::RemoteMachine> {
        Vec::new()
    }

    /// Does `action` to an agent on a saved machine, and says what happened.
    fn remote_act(
        &self,
        _machine: &crate::RemoteMachine,
        _agent: &crate::RemoteAgent,
        _action: &crate::RemoteAction,
    ) -> Result<String, String> {
        Err("remote machines are not available here".into())
    }

    /// Puts `text` on the system clipboard.
    fn copy(&self, text: &str) -> Result<(), String>;

    /// Keeps the dock the way a click expects to find it: first in the
    /// sidebar, its tab wearing the dock's name, and its right-click going to
    /// the tower instead of to herdr's menu.
    ///
    /// Only for a tower whose own pane is in the dock ([`crate::in_dock`]
    /// decides); a tower started in an ordinary workspace changes nothing
    /// there. Nothing is returned: none of this is worth a line on the tower's
    /// screen, and the next pass tries again.
    fn hold_dock(&self);
}

/// The `ssh` to use: the one found on the path, else the system's.
fn remote_ssh(tools: &tower_usage::ToolSearch) -> String {
    tools
        .lookup("ssh")
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/usr/bin/ssh".to_string())
}

/// The real thing: a herdr client plus the process spawns.
pub struct HerdrTower {
    herdr: herdrx::Herdr,
    home: std::path::PathBuf,
    /// The pane whose right-click this tower has already routed to itself.
    /// See [`Tower::hold_dock`]: herdr never reports the setting back, so
    /// there is nothing to compare against and this is what keeps it from
    /// being re-sent every second. Holding the pane ID rather than a flag
    /// covers a tower whose pane was moved, which herdr gives a new ID.
    sealed: std::sync::Mutex<Option<String>>,
    /// Captured once at construction rather than read per call, so every
    /// resolution in one run agrees. `tower_mux` takes the same argument for the
    /// same reason.
    env: std::collections::HashMap<String, String>,
}

impl HerdrTower {
    /// Selects the backend from the environment.
    pub fn from_env() -> Self {
        let env: std::collections::HashMap<String, String> = std::env::vars().collect();
        let home = env
            .get("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let herdr = herdrx::Herdr::at(herdrx::socket_path(|_| None, &home.to_string_lossy()));
        HerdrTower {
            herdr,
            home,
            sealed: std::sync::Mutex::new(None),
            env,
        }
    }

    fn env(&self, k: &str) -> Option<String> {
        self.env.get(k).cloned()
    }

    fn client_for(&self, backend: &str) -> Result<herdrx::Herdr, String> {
        if backend.is_empty() {
            return Ok(self.herdr.clone());
        }
        tower_mux::discover_backends(&self.home)
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|candidate| candidate.id == backend)
            .map(|candidate| candidate.client())
            .ok_or_else(|| format!("Herdr backend {backend} is unavailable"))
    }

    fn target(&self, target: &str) -> Result<(herdrx::Herdr, String), String> {
        let (backend, pane) = tower_mux::split_target(target);
        Ok((self.client_for(backend)?, pane.to_string()))
    }
}

impl Tower for HerdrTower {
    fn inside(&self) -> bool {
        herdrx::inside()
    }

    fn self_pane(&self) -> Option<String> {
        let pane = herdrx::current_pane_id()?;
        let home = self.home.to_string_lossy();
        let backend = herdrx::backend_from_env(|key| self.env(key), &home);
        Some(tower_mux::target_key(&backend, &pane))
    }

    fn focus(&self, target: &str) -> Result<(), String> {
        let (herdr, pane) = self.target(target)?;
        herdr.focus(&pane).map_err(|e| e.to_string())
    }

    fn attach_argv(&self, target: &str) -> Result<Vec<String>, String> {
        let (bin, _) = tower_mux::attach_cmd(|k| self.env(k)).map_err(|e| e.to_string())?;
        let mut argv = vec![bin.to_string_lossy().into_owned()];
        let (backend, _) = tower_mux::split_target(target);
        if !backend.is_empty() {
            let descriptor = tower_mux::discover_backends(&self.home)
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|candidate| candidate.id == backend)
                .ok_or_else(|| format!("Herdr backend {backend} is unavailable"))?;
            let name = descriptor
                .socket
                .parent()
                .and_then(std::path::Path::file_name)
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| format!("Herdr backend {backend} has no session name"))?;
            argv.extend(["session".into(), "attach".into(), name.into()]);
        }
        Ok(argv)
    }

    fn capture_pane(&self, target: &str, lines: u16) -> Option<String> {
        // A pane on a saved machine is read through herdr there.
        if let Some((label, pane)) = crate::remote::parse_preview_key(target) {
            let tools = tower_usage::ToolSearch::from_env();
            let herdr = tools.lookup("herdr")?.to_string_lossy().to_string();
            let lines = lines.to_string();
            return crate::remote::run_herdr(
                &herdr,
                &["--machine", label, "pane", "read", pane, "--lines", &lines],
            )
            .ok();
        }
        let (herdr, pane) = self.target(target).ok()?;
        herdr.capture_pane(&pane, u32::from(lines)).ok()
    }

    fn send(&self, target: &str, text: &str) -> Result<(), String> {
        let (herdr, pane) = self.target(target)?;
        tower_mux::send_text(&herdr, &pane, text).map_err(|e| e.to_string())
    }

    fn prepare_loop(&self, target: &str) {
        let Ok((herdr, pane)) = self.target(target) else {
            return;
        };
        let _ = herdr.report_metadata(&herdrx::Metadata {
            pane_id: pane,
            source: Some("herdr-tower-loop-agent".into()),
            tokens: [("loop_control".into(), String::new())].into(),
            ..herdrx::Metadata::default()
        });
    }

    fn clear_loop_display(&self, target: &str) {
        let Ok((herdr, pane)) = self.target(target) else {
            return;
        };
        let _ = herdr.report_metadata(&herdrx::Metadata {
            pane_id: pane,
            source: Some("herdr-tower-loop".into()),
            tokens: [("loop".into(), String::new())].into(),
            ..herdrx::Metadata::default()
        });
    }

    fn start_loop_supervisor(&self, exe: &str) -> Result<(), String> {
        let status = std::process::Command::new(exe)
            .args(["daemon", "start"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| e.to_string())?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("resident supervisor start exited with {status}"))
    }

    fn is_git_repo(&self, dir: &str) -> bool {
        std::process::Command::new("git")
            .args(["-C", dir, "rev-parse", "--git-dir"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn remote_machines(&self) -> Vec<crate::RemoteMachine> {
        let tools = tower_usage::ToolSearch::from_env();
        let Some(herdr) = tools.lookup("herdr") else {
            return Vec::new();
        };
        let herdr = herdr.to_string_lossy().to_string();
        let ssh = remote_ssh(&tools);
        crate::remote::fetch(&crate::remote::Runner {
            herdr: &|args| crate::remote::run_herdr(&herdr, args),
            ssh: &|target, command| crate::remote::run_ssh(&ssh, target, command),
        })
    }

    fn remote_act(
        &self,
        machine: &crate::RemoteMachine,
        agent: &crate::RemoteAgent,
        action: &crate::RemoteAction,
    ) -> Result<String, String> {
        let tools = tower_usage::ToolSearch::from_env();
        let herdr = tools
            .lookup("herdr")
            .ok_or("herdr is not installed")?
            .to_string_lossy()
            .to_string();
        let ssh = remote_ssh(&tools);
        crate::remote::act(
            &crate::remote::Runner {
                herdr: &|args| crate::remote::run_herdr(&herdr, args),
                ssh: &|target, command| crate::remote::run_ssh(&ssh, target, command),
            },
            machine,
            agent,
            action,
        )
    }

    /// One `gh pr view` per folder: the PR of the branch checked out there,
    /// if it is still open.
    fn open_prs(&self, folders: &[String]) -> HashMap<String, u64> {
        let tools = tower_usage::ToolSearch::from_env();
        let Some(gh) = tools.lookup("gh") else {
            return HashMap::new();
        };
        folders
            .iter()
            .filter(|f| !f.is_empty())
            .filter_map(|folder| {
                let out = std::process::Command::new(&gh)
                    .args([
                        "pr",
                        "view",
                        "--json",
                        "number,state",
                        "--jq",
                        r#"select(.state == "OPEN") | .number"#,
                    ])
                    .current_dir(folder)
                    .env("PATH", tools.extended_path())
                    .env("GH_PROMPT_DISABLED", "1")
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                let number = parse_pr_number(&String::from_utf8_lossy(&out.stdout))?;
                Some((folder.clone(), number))
            })
            .collect()
    }

    fn checked_out_branches(&self, folders: &[String]) -> HashMap<String, String> {
        folders
            .iter()
            .filter(|f| !f.is_empty())
            .filter_map(|folder| {
                let out = std::process::Command::new("git")
                    .args(["-C", folder, "branch", "--show-current"])
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()?;
                let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
                (out.status.success() && !branch.is_empty()).then(|| (folder.clone(), branch))
            })
            .collect()
    }

    fn copy(&self, text: &str) -> Result<(), String> {
        copy_to_clipboard(text)
    }

    fn hold_dock(&self) {
        let Some(target) = self.self_pane() else {
            return;
        };
        // `self_pane` answers with a backend-qualified target, and a tower
        // started under `HERDR_SESSION` or `HERDR_SOCKET_PATH` names a backend
        // that is not the default socket `self.herdr` speaks to. Resolving the
        // target picks the right client and strips the prefix; without it
        // every call below is made against the wrong server and the first one
        // simply fails, silently, for exactly the docks that are not the
        // default one's.
        let Ok((herdr, pane)) = self.target(&target) else {
            return;
        };
        let Ok(p) = herdr.pane(&pane) else {
            return;
        };
        let Ok(list) = herdr.workspaces() else {
            return;
        };
        if !crate::dock::in_dock(&list, &p.workspace_id) {
            return;
        }
        if crate::dock::needs_pin(&list, &p.workspace_id) {
            let _ = herdr.move_workspace(&p.workspace_id, 0);
        }
        // herdr renames a tab from underneath us — a fresh tab is its number,
        // and a tab named after its command follows the command. So the label
        // is read every pass and written only when it has drifted.
        if let Ok(tabs) = herdr.tabs() {
            if tabs
                .iter()
                .any(|tab| tab.tab_id == p.tab_id && crate::dock::needs_tab_label(&tab.label))
            {
                let _ = herdr.rename_tab(&p.tab_id, crate::dock::DOCK_LABEL);
            }
        }
        // Written once per pane, after the dock has been confirmed, and again
        // by `dock` on every launch. Only a call that was acknowledged marks
        // the pane sealed, so a server that refused this one is asked again on
        // the next pass instead of the pane keeping herdr's menu for the rest
        // of the session.
        // A poisoned lock means this tower panicked elsewhere, not that the
        // pane is sealed — the call is idempotent, so retry rather than leave
        // herdr's menu on it.
        let needs_sealing = self
            .sealed
            .lock()
            .map_or(true, |sealed| sealed.as_deref() != Some(p.pane_id.as_str()));
        if needs_sealing
            && herdr
                .set_right_click(&p.pane_id, herdrx::RightClick::Pane)
                .is_ok()
        {
            if let Ok(mut sealed) = self.sealed.lock() {
                *sealed = Some(p.pane_id.clone());
            }
        }
    }
}

/// The clipboard tools to try, in order: `pbcopy` on a Mac, then the three
/// Linux ones. Each takes the text on stdin.
const CLIPBOARD_TOOLS: [&[&str]; 4] = [
    &["pbcopy"],
    &["wl-copy"],
    &["xclip", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--input"],
];

/// The first clipboard tool on this machine that takes `text` on stdin, and else
/// the terminal's own clipboard escape (OSC 52), which many terminals honour
/// over SSH too. A tool that is installed but fails (`xclip` with no display) is
/// passed over for the next.
/// What `gh pr view --jq` printed: one number, or nothing for a PR that is
/// not open.
fn parse_pr_number(out: &str) -> Option<u64> {
    out.trim().parse().ok().filter(|n| *n > 0)
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    copy_with(text, run_clipboard_tool, &mut std::io::stdout())
}

/// The chain itself, over a runner and a writer.
///
/// 🚨 Split out because the chain is the part that goes wrong and the part
/// nothing could test: which tool is tried, that a tool that *fails* is passed
/// over rather than taken as done, and that the escape is written only when every
/// tool has been tried. Reaching it meant having `pbcopy` or `xclip` on the
/// machine and no way to make either fail on purpose.
fn copy_with(
    text: &str,
    mut run: impl FnMut(&[&str], &str) -> bool,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    for argv in CLIPBOARD_TOOLS {
        if run(argv, text) {
            return Ok(());
        }
    }
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

/// Hands `text` to one tool on stdin, and says whether it took it.
///
/// `false` for a tool that is not installed *and* for one that ran and failed —
/// the chain treats them the same, because both mean the text is not on the
/// clipboard yet.
fn run_clipboard_tool(argv: &[&str], text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().is_ok_and(|status| status.success())
}

/// Standard base64 with padding, for the OSC 52 escape.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod clipboard_tests {
    use super::{base64, copy_with, CLIPBOARD_TOOLS};

    /// The chain stops at the first tool that takes the text, and a tool that
    /// ran and failed is passed over rather than taken as done.
    ///
    /// 🚨 The failing-tool case is the one that matters on a Linux box with
    /// `xclip` installed and no display: taking its exit code for success would
    /// leave the selection uncopied and say nothing.
    #[test]
    fn the_copy_chain_stops_at_the_first_tool_that_takes_the_text() {
        for (takes, want_tried, want_escape) in [
            ("pbcopy", vec!["pbcopy"], false),
            ("wl-copy", vec!["pbcopy", "wl-copy"], false),
            ("xclip", vec!["pbcopy", "wl-copy", "xclip"], false),
            ("xsel", vec!["pbcopy", "wl-copy", "xclip", "xsel"], false),
            // None of them: every one is tried, then the terminal's own escape.
            ("none", vec!["pbcopy", "wl-copy", "xclip", "xsel"], true),
        ] {
            let mut tried: Vec<String> = Vec::new();
            let mut out: Vec<u8> = Vec::new();
            let got = copy_with(
                "가 fix",
                |argv, text| {
                    assert_eq!(text, "가 fix", "the text reaches the tool unchanged");
                    tried.push(argv[0].to_string());
                    argv[0] == takes
                },
                &mut out,
            );
            assert!(got.is_ok(), "{takes}: {got:?}");
            assert_eq!(tried, want_tried, "{takes}");
            assert_eq!(!out.is_empty(), want_escape, "{takes}: the escape");
            if want_escape {
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    format!("\x1b]52;c;{}\x07", base64("가 fix".as_bytes())),
                    "OSC 52 with the base64 of the text"
                );
            }
        }
    }

    /// The flags each tool needs are the tool's own, and a typo in them is a
    /// clipboard that silently does nothing on that platform.
    #[test]
    fn each_clipboard_tool_is_asked_the_way_it_expects() {
        assert_eq!(
            CLIPBOARD_TOOLS.map(|argv| argv.join(" ")),
            [
                "pbcopy",
                "wl-copy",
                "xclip -selection clipboard",
                "xsel --clipboard --input",
            ]
        );
    }

    /// A writer that cannot take the escape is an error, not a silent success:
    /// the selection is not on the clipboard, and the tower says so.
    #[test]
    fn a_failed_escape_is_reported() {
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("no terminal"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err = copy_with("x", |_, _| false, &mut Broken).expect_err("a broken writer");
        assert!(err.contains("no terminal"), "{err:?}");
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
            ("세션", "7IS47IWY"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
    }

    const ABC: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// Bytes of every value, in runs of every length from nothing up to a few
    /// dozen, from a fixed seed.
    fn inputs() -> Vec<Vec<u8>> {
        let mut rng = tower_testrng::Rng::new(0xB64B_64B6);
        let mut out: Vec<Vec<u8>> = (0..=64)
            .map(|len| (0..len).map(|_| (rng.draw() >> 56) as u8).collect())
            .collect();
        out.push((0..=255).collect());
        out.push((0..=255).rev().collect());
        out
    }

    /// The same encoding worked the slow way — every byte written out as
    /// eight bits, read back six at a time — so the shifts and masks of the
    /// fast one have something independent to answer to, over every byte
    /// value and every padding length.
    #[test]
    fn base64_agrees_with_the_encoding_worked_bit_by_bit() {
        for bytes in inputs() {
            let mut bits: Vec<u8> = bytes
                .iter()
                .flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1))
                .collect();
            while !bits.len().is_multiple_of(6) {
                bits.push(0);
            }
            let mut want: String = bits
                .chunks(6)
                .map(|six| {
                    let n = six.iter().fold(0usize, |n, bit| n << 1 | usize::from(*bit));
                    ABC.as_bytes()[n] as char
                })
                .collect();
            while !want.len().is_multiple_of(4) {
                want.push('=');
            }
            assert_eq!(base64(&bytes), want, "{bytes:?}");
        }
    }

    /// What the terminal decodes from the OSC 52 escape is exactly what was
    /// copied: the encoding reads back to the same bytes, at the length the
    /// standard says, whatever the bytes.
    #[test]
    fn base64_reads_back_to_the_bytes_it_was_given() {
        for bytes in inputs() {
            let encoded = base64(&bytes);
            assert_eq!(encoded.len(), bytes.len().div_ceil(3) * 4, "{bytes:?}");
            let body = encoded.trim_end_matches('=');
            assert_eq!(encoded.len() - body.len(), (3 - bytes.len() % 3) % 3);
            let mut acc: u32 = 0;
            let mut held = 0;
            let mut back = Vec::new();
            for c in body.chars() {
                let v = ABC
                    .find(c)
                    .unwrap_or_else(|| panic!("{c:?} in {encoded:?}"));
                acc = acc << 6 | v as u32;
                held += 6;
                if held >= 8 {
                    held -= 8;
                    back.push((acc >> held) as u8);
                    acc &= (1 << held) - 1;
                }
            }
            assert_eq!(acc, 0, "the padding bits are zero: {encoded:?}");
            assert_eq!(back, bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_pr_number;

    /// `--jq` prints the number of an open PR and nothing for a merged or
    /// closed one; anything else is not a number to show.
    #[test]
    fn only_a_printed_number_is_a_pr() {
        assert_eq!(parse_pr_number("52\n"), Some(52));
        assert_eq!(parse_pr_number(""), None);
        assert_eq!(parse_pr_number("\n"), None);
        assert_eq!(parse_pr_number("0\n"), None);
        assert_eq!(parse_pr_number("no pull requests found"), None);
        assert_eq!(parse_pr_number("  61  "), Some(61));
        assert_eq!(parse_pr_number("-1"), None);
        assert_eq!(parse_pr_number("52\n53\n"), None, "one number, not a list");
        assert_eq!(parse_pr_number("18446744073709551616"), None, "past u64");
    }

    /// Round trip: every positive number `gh` could print reads back as itself.
    #[test]
    fn a_printed_pr_number_reads_back_as_itself() {
        for n in [1u64, 9, 10, 54, 3034, 99_999, u64::MAX] {
            assert_eq!(parse_pr_number(&format!("{n}\n")), Some(n));
        }
    }
}
