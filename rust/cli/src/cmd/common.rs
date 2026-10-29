// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Shared wiring functions used by multiple commands.
//!
//! Everything here reaches the real filesystem, the real process environment,
//! the real herdr socket and the real network, so that every crate under it can
//! be tested without any of them.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use time::OffsetDateTime;
use tower_state::Store;

use super::Result;

// ---------------------------------------------------------------- the outside world

/// Where records live. Every command shares one store.
pub fn store() -> Result<Store> {
    Ok(Store::new(tower_state::default_dir())?)
}

/// Reconciles the store with what the backend reports, and hands back the
/// client so the caller does not open a second connection.
///
/// An unreachable backend is *not* an error, and `sync_backend` has already
/// turned that case into `Ok`: a reboot before the herdr server is up looks
/// exactly like a missing socket, and the right answer is to show the stored
/// records, which will have fallen to DEAD and are what `restore` needs to see.
///
/// So anything that comes back here is a *local* failure — a state directory
/// that is full or read-only. Swallowing it let `status` print stale records and
/// exit zero while nothing was being written.
///
pub fn sync_panes(store: &Store, now: OffsetDateTime) -> Result<herdrx::Herdr> {
    let h = tower_cli::muxsel::default_mux();
    tower_scan::sync_named_backend(store, &h, &tower_cli::muxsel::default_backend_id(), now)?;
    Ok(h)
}

pub fn home() -> Result<PathBuf> {
    tower_usage::home_dir().ok_or_else(|| "the home directory is unknown ($HOME)".into())
}

/// The path this binary was invoked as.
///
/// Written into every hook registration, so it has to be the build the user is
/// actually running — a registration naming a different `herdr-tower` sends
/// events to the wrong one, or to nothing.
pub fn self_path() -> String {
    // The invoked spelling, not `current_exe`: see `tower_cli::selfpath` for
    // the upgrade that deletes the other one.
    tower_cli::selfpath::current()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// How notifications actually reach a person on this machine.
///
/// `None` for a channel this platform has no way to use, which
/// `tower_notify::notify` reads as "skip it" rather than as a failure.
pub fn notify_sender() -> tower_notify::Sender {
    tower_notify::Sender {
        notify: desktop_notifier(),
        post_json: Some(Box::new(|url, body| {
            // Five seconds, because this is called from a hook that is holding
            // up an agent's turn. A slow webhook must not become the agent's
            // problem.
            ureq::post(url)
                .config()
                .timeout_global(Some(std::time::Duration::from_secs(5)))
                .build()
                .header("content-type", "application/json")
                .send(body)
                .map(|_| ())
                .map_err(std::io::Error::other)
        })),
    }
}

pub fn desktop_notifier() -> Option<tower_notify::NotifyFn> {
    if cfg!(target_os = "macos") {
        return Some(Box::new(|title, body| {
            let script = format!(
                "display notification {} with title {}",
                applescript_string(body),
                applescript_string(title)
            );
            Command::new("osascript").arg("-e").arg(script).status()?;
            Ok(())
        }));
    }
    if cfg!(target_os = "linux") {
        // No `notify-send` means no desktop session worth notifying — a headless
        // box or a container. Checked once here rather than per notification, so
        // the hook path never pays for it.
        which("notify-send")?;
        return Some(Box::new(|title, body| {
            Command::new("notify-send")
                .args(notify_send_args(title, body))
                .status()?;
            Ok(())
        }));
    }
    None
}

/// `notify-send`'s arguments for one notification.
///
/// The `--` is not decoration. The body is an agent's question or task, which a
/// repository or a web page can choose; without the separator a body that
/// starts with `-` is parsed as options — `-w` blocks the hook until the
/// notification is dismissed, `-i /path` and `-h string:sound-file:/path` make
/// the desktop open files the agent named. After it, both are plain text.
fn notify_send_args<'a>(title: &'a str, body: &'a str) -> [&'a str; 4] {
    ["--app-name=herdr-tower", "--", title, body]
}

/// Quotes a string for AppleScript.
///
/// Only `\"` and `\\` are escaped, which is all an AppleScript string literal
/// needs; Korean text passes through literally.
pub fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            // A raw newline ends the statement, and AppleScript has no escape
            // for one inside a literal.
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn which(bin: &str) -> Option<()> {
    tower_os::on_path(bin).then_some(())
}

/// Starts this binary again with other arguments and does not wait.
///
/// Used only from the hook path, which runs inside an agent's process: the work
/// is worth doing but never worth delaying a turn for. No stdio, so nothing it
/// prints can land in the agent's terminal, and the `Child` is dropped rather
/// than waited on.
pub fn spawn_detached(args: &[&str]) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Whether the Antigravity CLI has left a trace: its shared config folder, its
/// own data folder, or the binary on `PATH`.
///
/// A freshly installed `agy` has no `config/` yet (measured on WSL2,
/// 2026-09-07), so that folder's absence proves nothing and the other two
/// signals have to be consulted before deciding not to register its hooks.
pub fn agy_installed(home: &Path) -> bool {
    let gemini = home.join(".gemini");
    if gemini.join("config").is_dir() || gemini.join("antigravity-cli").is_dir() {
        return true;
    }
    which("agy").is_some()
}

/// Parses flags and refuses a stray positional word.
///
/// `FlagSet` stops at the first non-flag word — so a stray word does not merely
/// go unused, it leaves *every flag after it* at its default.
/// `card accidental --out` published to the configured Discord webhook
/// instead of printing, and `init accidental --dry-run` rewrote every detected
/// CLI's configuration. Both are the quiet opposite of what was asked for, which
/// is why this refuses rather than warns.
pub fn flags_only(
    name: &str,
    fs: &tower_cli::helpcmd::FlagSet,
    args: &[String],
) -> Result<tower_cli::helpcmd::Parsed> {
    let p = match fs.parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprint!("{}", fs.diagnostic(&e));
            return Err(e.into());
        }
    };
    if let Some(word) = p.args().first() {
        return Err(
            format!("unknown argument: {word:?} — {name} takes no positional arguments").into(),
        );
    }
    Ok(p)
}

/// Reads `--name value` out of an argument list.
///
/// Hand-rolled rather than parsed: this list is whatever an agent's CLI handed
/// us, it may carry a JSON payload as a positional argument, and a parser that
/// rejected an unknown flag would turn a hook into a failure.
pub fn flag_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

/// Reads one `y` from the terminal.
///
/// `false` when there is nothing to read, which is the safe answer: these
/// prompts guard broadcasts and merges, and a script that meant to skip the
/// question has `--yes` for it.
pub fn confirm_on_tty() -> bool {
    confirmed_by(&mut std::io::stdin().lock())
}

/// The reading of it, over anything a line can come from.
///
/// Split out so the rule can be tested: what counts as yes, what a blank line
/// or an unreadable stream answers, and that a second line is not read. 🚨 The
/// rule was only reachable through the real stdin before, so nothing checked
/// it — and it guards a broadcast to every session.
pub fn confirmed_by(from: &mut impl std::io::BufRead) -> bool {
    let mut buf = String::new();
    from.read_line(&mut buf).is_ok() && buf.trim().eq_ignore_ascii_case("y")
}

/// The `HERDR_TOWER_CONFIG` this process was started with, if any, made
/// absolute.
///
/// Installed into the service so the supervisor reads the same file the tower
/// does; `run_dock` passes the same value into the tower's launch command, and
/// resolves it the same way.
///
/// 🚨 Absolute, because this value is handed to a *different* process with a
/// different working directory. A relative spelling that resolves here names
/// nothing under launchd or systemd, and a config that cannot be read is
/// replaced by defaults — silently turning a deliberate `dock_keep: false`
/// back on.
///
/// A failure to resolve one is therefore an error, not something to fall back
/// from: `std::path::absolute` fails when the working directory itself cannot
/// be read (it was removed out from under this process, say), and installing
/// the relative spelling anyway would produce exactly the silent default this
/// function exists to prevent. Say so instead.
pub fn config_override() -> Result<Option<PathBuf>> {
    let Some(path) = std::env::var_os("HERDR_TOWER_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(None);
    };
    let absolute = std::path::absolute(&path).map_err(|error| {
        format!(
            "HERDR_TOWER_CONFIG={} could not be made absolute ({error}); \
             the resident supervisor starts elsewhere and would read the default config instead",
            path.display()
        )
    })?;
    Ok(Some(absolute))
}

/// The config, minus its Discord webhooks when this process runs against a
/// store of its own.
///
/// A store picked with `HERDR_TOWER_STATE_DIR` is a test or a sandbox, but the
/// config still comes from `~/.config/herdr-tower` unless `HERDR_TOWER_CONFIG`
/// says otherwise — so its webhooks are the user's real channels. The card's
/// message ID lives in the store, so a sandbox found none, posted a second
/// card beside the real one, and orphaned it when the sandbox was deleted;
/// its transitions reached the notify channel as if they were real. Naming a
/// config of its own is how a sandbox asks for Discord.
pub fn outbound_config() -> tower_config::Config {
    let mut cfg = tower_config::load();
    if store_is_isolated(
        std::env::var_os("HERDR_TOWER_STATE_DIR").as_deref(),
        &tower_state::default_dir(),
        &tower_state::user_dir(),
        std::env::var_os("HERDR_TOWER_CONFIG").as_deref(),
    ) {
        cfg.discord_webhook_url = None;
        cfg.notify_webhook_url = None;
    }
    cfg
}

/// Whether the store was pointed somewhere other than the user's own while the
/// config was left as the user's own.
///
/// Setting `HERDR_TOWER_STATE_DIR` to the default directory — which the
/// supervisor's service definition does — is not isolation, however it is
/// spelled.
pub fn store_is_isolated(
    state_env: Option<&std::ffi::OsStr>,
    state_dir: &Path,
    user_dir: &Path,
    config_env: Option<&std::ffi::OsStr>,
) -> bool {
    let set = |v: Option<&std::ffi::OsStr>| v.is_some_and(|v| !v.is_empty());
    if !set(state_env) || set(config_env) {
        return false;
    }
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    real(state_dir) != real(user_dir)
}

/// A state name as `state_word` writes it.
pub fn parse_state(s: &str) -> Option<tower_state::AgentState> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

pub fn dock_observation_pause() {
    std::thread::sleep(std::time::Duration::from_millis(100));
}

pub fn retry_transient_dock_observation<T>(
    read: impl FnMut() -> herdrx::Result<T>,
    pause: impl FnMut(),
) -> herdrx::Result<T> {
    retry_transient_dock_observation_impl(read, pause, None)
}

/// Pre-coordination observations cannot rely on the hard lease fence. Three
/// ordinary Herdr RPC deadlines keep a persistent outage bounded to roughly
/// nine seconds while still covering isolated lost or undecodable replies.
pub fn retry_transient_dock_observation_bounded<T>(
    read: impl FnMut() -> herdrx::Result<T>,
    pause: impl FnMut(),
) -> herdrx::Result<T> {
    retry_transient_dock_observation_impl(read, pause, Some(3))
}

pub fn retry_transient_dock_observation_impl<T>(
    mut read: impl FnMut() -> herdrx::Result<T>,
    mut pause: impl FnMut(),
    max_transient_failures: Option<usize>,
) -> herdrx::Result<T> {
    let mut transient_failures = 0;
    loop {
        match read() {
            Ok(value) => return Ok(value),
            Err(error @ (herdrx::Error::Io(_) | herdrx::Error::Decode(_))) => {
                transient_failures += 1;
                if max_transient_failures.is_some_and(|maximum| transient_failures >= maximum) {
                    return Err(error);
                }
                pause();
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn pane_for_terminal<'a>(
    panes: &'a [herdrx::Pane],
    terminal_id: &str,
) -> Option<&'a herdrx::Pane> {
    panes.iter().find(|pane| pane.terminal_id == terminal_id)
}

pub fn pane_with_token_for_terminal<'a>(
    panes: &'a [herdrx::Pane],
    terminal_id: &str,
    token: &str,
    expected: &str,
) -> Option<&'a herdrx::Pane> {
    pane_for_terminal(panes, terminal_id)
        .filter(|pane| pane.tokens.get(token).map(String::as_str) == Some(expected))
}

pub fn ready_pane_for_launch<'a>(
    panes: &'a [herdrx::Pane],
    terminal_id: &str,
    expected: &str,
) -> Option<&'a herdrx::Pane> {
    pane_with_token_for_terminal(
        panes,
        terminal_id,
        tower_cli::dockcmd::ACTIVE_TOKEN,
        expected,
    )
    .filter(|pane| {
        pane.tokens
            .get(tower_cli::dockcmd::STARTING_TOKEN)
            .is_some_and(|value| !tower_cli::dockcmd::pane_starting_lease_is_held(value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a store of its own under the user's own config loses Discord.
    #[test]
    fn only_a_sandbox_store_under_the_users_config_is_isolated() {
        use std::ffi::OsStr;
        let home = tempfile::tempdir().unwrap();
        let user = home.path().join("state");
        let sandbox = home.path().join("sandbox");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&sandbox).unwrap();
        let env = |p: &Path| Some(p.as_os_str().to_owned());
        let cfg = Some(OsStr::new("/elsewhere/config.json"));

        // The incident: a test's store, the user's config.
        assert!(store_is_isolated(
            env(&sandbox).as_deref(),
            &sandbox,
            &user,
            None
        ));
        // A config of its own asks for whatever webhooks it names.
        assert!(!store_is_isolated(
            env(&sandbox).as_deref(),
            &sandbox,
            &user,
            cfg
        ));
        // No override at all is the ordinary run.
        assert!(!store_is_isolated(None, &user, &user, None));
        assert!(!store_is_isolated(Some(OsStr::new("")), &user, &user, None));
        // The supervisor's service definition spells out the default directory.
        assert!(!store_is_isolated(
            env(&user).as_deref(),
            &user,
            &user,
            None
        ));
        // Another spelling of the same directory is still the same directory.
        let dotted = sandbox.join("..").join("state");
        assert!(!store_is_isolated(
            env(&dotted).as_deref(),
            &dotted,
            &user,
            None
        ));
        // An empty config override is no override.
        assert!(store_is_isolated(
            env(&sandbox).as_deref(),
            &sandbox,
            &user,
            Some(OsStr::new(""))
        ));
    }

    /// What counts as yes, and what does not. 🚨 A prompt this guards sends a
    /// message to every session, so "anything but y" has to mean no — including
    /// an empty line, a word starting with y, and a stream that cannot be read.
    #[test]
    fn only_a_bare_y_confirms() {
        for (input, want) in [
            ("y\n", true),
            ("Y\n", true),
            ("  y  \n", true),
            ("y", true),
            ("yes\n", false),
            ("n\n", false),
            ("\n", false),
            ("", false),
            ("ynot\n", false),
            ("가\n", false),
        ] {
            let mut from = std::io::Cursor::new(input.as_bytes().to_vec());
            assert_eq!(confirmed_by(&mut from), want, "{input:?}");
        }
    }

    /// One line, not the stream. A second answer left behind is the *next*
    /// prompt's to read, and a confirmation that swallowed it would answer a
    /// question nobody had asked yet.
    #[test]
    fn one_line_is_read_and_no_more() {
        let mut from = std::io::Cursor::new(b"y\nn\n".to_vec());
        assert!(confirmed_by(&mut from));
        let mut rest = String::new();
        std::io::BufRead::read_line(&mut from, &mut rest).unwrap();
        assert_eq!(rest, "n\n", "the second line was consumed");
    }

    /// A body that looks like an option is still the body: the options end
    /// before the title, whatever the title and body say.
    #[test]
    fn notify_send_never_reads_the_text_as_options() {
        let args = notify_send_args("-w", "-h string:sound-file:/etc/passwd");
        let end = args.iter().position(|a| *a == "--").unwrap();
        assert_eq!(&args[end + 1..], ["-w", "-h string:sound-file:/etc/passwd"]);
        assert!(args[..end].iter().all(|a| a.starts_with("--app-name")));
    }

    /// A task line with a quote in it must not end the AppleScript statement.
    #[test]
    fn applescript_strings_survive_quotes_and_newlines() {
        assert_eq!(applescript_string("plain"), "\"plain\"");
        assert_eq!(applescript_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(applescript_string(r"back\slash"), r#""back\\slash""#);
        assert_eq!(applescript_string("two\nlines"), "\"two lines\"");
        assert_eq!(
            applescript_string("승인 대기"),
            "\"승인 대기\"",
            "Korean passes through literally"
        );
    }

    /// The hook takes its event from a flag that may sit anywhere, because the
    /// rest of the list belongs to the agent's CLI.
    #[test]
    fn the_event_flag_is_found_wherever_it_sits() {
        let args: Vec<String> = ["--event", "Stop", "{\"session_id\":\"x\"}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag_value(&args, "--event").as_deref(), Some("Stop"));
        assert_eq!(flag_value(&args, "--missing"), None);

        let trailing: Vec<String> = vec!["--event".into()];
        assert_eq!(
            flag_value(&trailing, "--event"),
            None,
            "a flag with nothing after it is not a value"
        );
    }
}
