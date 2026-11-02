// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Installs and controls the one resident supervisor for this user.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use time::OffsetDateTime;
use tower_state::DaemonStore;

const LABEL: &str = "dev.herdr.tower";

#[cfg(unix)]
fn current_uid() -> libc::uid_t {
    // SAFETY: getuid(2) has no arguments, no failure mode, and touches no process memory.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

pub fn install(
    out: &mut impl Write,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
    dry_run: bool,
) -> std::io::Result<()> {
    // 🚨 Here, not at the call sites. Every path that writes a definition goes
    // through this function — `init`, `daemon install`, `daemon start` with no
    // service yet — and a relative store serialized into a plist or unit names
    // a different, probably empty one from the service's own working
    // directory, leaving the caller's agents and leases unsupervised.
    let state_dir = &absolute_state_dir(state_dir)?;
    match std::env::consts::OS {
        "macos" => install_macos(out, home, exe, state_dir, config, dry_run),
        "linux" => install_linux(out, home, exe, state_dir, config, dry_run),
        "windows" => install_windows(out, home, exe, state_dir, config, dry_run),
        os => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("the resident supervisor is not supported on {os}"),
        )),
    }
}

pub fn start(
    out: &mut impl Write,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
) -> std::io::Result<()> {
    let service = service_path(home);
    // An existing definition is not necessarily the right one. `init` may have
    // installed the service before a config was chosen, and both launchd and
    // systemd would then go on starting the old definition — the supervisor
    // keeps reading the default config and a deliberate `dock_keep: false`
    // never takes effect. Reinstalling is idempotent, so compare and replace.
    //
    // 🚨 A caller with no `HERDR_TOWER_CONFIG` in its environment is not
    // asking for the installed one to be removed. `start` is run from
    // anywhere — a login shell, `make install`, a hook — and rewriting the
    // definition without the override would drop the chosen config and
    // restart the supervisor on defaults, re-enabling `dock_keep` behind the
    // user's back. Absent means "keep whatever is installed"; changing it is
    // `install`'s job, which is the command that takes the new value.
    let installed = installed_config(&service);
    let effective = config.or(installed.as_deref());
    // 🚨 The binary is preserved too. Every tower calls `start` with its own
    // executable when it opens, so a tower run from a development build or
    // another checkout used to rewrite the service to that binary — and the
    // supervisor, and the dock it keeps, then ran the test build until
    // someone noticed (2026-10-10). The installed binary stays while it is
    // there; choosing another is `install`'s job.
    let installed_exe = installed_exe(&service);
    let exe = kept_exe(installed_exe.as_deref(), exe);
    // 🚨 The state directory is preserved on the same terms as the config
    // path, and for a sharper reason: rewriting the definition with the
    // default store would point the supervisor at an empty one, while the
    // agents and leases it is meant to be supervising sit in the installed
    // directory, unattended and with nothing to say so. Only a caller that
    // set `HERDR_TOWER_STATE_DIR` itself is asking to change it.
    let installed_state = installed_state_dir(&service);
    let state_dir = match (state_dir_is_explicit(), installed_state.as_deref()) {
        (false, Some(installed)) => installed,
        _ => state_dir,
    };
    // Compared in the same shape it will be written in, so a relative spelling
    // does not read as a difference from the absolute one already installed.
    let absolute_state = absolute_state_dir(state_dir)?;
    let state_dir = absolute_state.as_path();
    // 🚨 Every tower runs this before it opens, so a service manager this
    // session cannot reach must not end here: a shell from `su`, a container
    // with no systemd, a Mac with nobody signed in at the screen (#14). What
    // the service manager adds is being restarted and started at login, and
    // the supervisor works without both.
    match start_service(out, home, &service, exe, state_dir, effective) {
        Ok(()) => Ok(()),
        Err(reason) => start_unmanaged(out, home, exe, state_dir, effective, reason),
    }
}

fn start_service(
    out: &mut impl Write,
    home: &Path,
    service: &Path,
    exe: &Path,
    state_dir: &Path,
    effective: Option<&Path>,
) -> std::io::Result<()> {
    if !service.is_file() || definition_differs(service, home, exe, state_dir, effective) {
        return install(out, home, exe, state_dir, effective, false);
    }
    // Running, not fresh: every tower, `loop start` and `schedule add` calls
    // this, and a supervisor that is only behind must not be restarted.
    let running = DaemonStore::new(state_dir)
        .read()
        .ok()
        .flatten()
        .is_some_and(|snapshot| snapshot.running(OffsetDateTime::now_utc(), tower_os::pid_alive));
    match std::env::consts::OS {
        "macos" => {
            let target = format!("gui/{}/{}", current_uid(), LABEL);
            let loaded = Command::new("launchctl")
                .args(["print", &target])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?
                .success();
            if running && loaded {
                return writeln!(out, "resident supervisor already running");
            }
            if !loaded {
                // `effective`, not `config`: bootstrapping an unloaded job
                // rewrites the plist, and writing it from the caller's empty
                // environment would drop the installed override and start the
                // supervisor on defaults — the same loss the comparison above
                // exists to prevent.
                return install(out, home, exe, state_dir, effective, false);
            }
            run(Command::new("launchctl").args(["kickstart", "-k", &target]))?;
        }
        "linux" => {
            let active = systemd_unit_active()?;
            if running && active {
                return writeln!(out, "resident supervisor already running");
            }
            let action = if active { "restart" } else { "start" };
            run_systemctl(&[action, "herdr-tower.service"])?;
        }
        "windows" => {
            if running {
                return writeln!(out, "resident supervisor already running");
            }
            windows::restart(state_dir, service)?;
        }
        _ => unreachable!("start follows service_path on a supported OS"),
    }
    writeln!(out, "resident supervisor started")
}

/// Starts the supervisor as a plain process of this user, for when the service
/// manager could not. `reason` is why, and is what comes back if this fails too.
///
/// Windows has no service manager to fall back from: what failed there is
/// already this.
fn start_unmanaged(
    out: &mut impl Write,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
    reason: std::io::Error,
) -> std::io::Result<()> {
    if std::env::consts::OS == "windows" {
        return Err(reason);
    }
    let unmanaged = || -> std::io::Result<bool> {
        fs::create_dir_all(state_dir)?;
        // The lock, not the snapshot: one started a moment ago holds it before
        // it has reported anything, and `daemon run` waits for the lock rather
        // than giving up, so another start would sit there for its whole life.
        if crate::loopcmd::supervisor_lock_held(state_dir)? {
            let hung = DaemonStore::new(state_dir)
                .read()
                .ok()
                .flatten()
                .filter(|snapshot| {
                    tower_os::pid_alive(snapshot.pid)
                        && !snapshot.running(OffsetDateTime::now_utc(), tower_os::pid_alive)
                });
            let Some(hung) = hung else {
                return Ok(false);
            };
            // The one that takes its place waits on the lock until it is gone.
            let _ = tower_os::kill(hung.pid);
        }
        let vars: Vec<_> = std::env::vars_os().map(|(name, _)| name).collect();
        spawn_detached(&mut unmanaged_command(home, exe, state_dir, config, &vars))?;
        Ok(true)
    };
    match unmanaged() {
        Ok(true) => {
            crate::supervisorlog::Log::open(state_dir)
                .line(&format!("started without a service manager: {reason}"));
            writeln!(
                out,
                "resident supervisor started without a service manager: {reason}"
            )
        }
        Ok(false) => writeln!(out, "resident supervisor already running"),
        Err(e) => Err(std::io::Error::other(format!(
            "{reason}; and it could not start without a service manager: {e}"
        ))),
    }
}

/// `daemon run` with the environment a service definition would give it.
///
/// 🚨 The caller here is a tower in a pane, so what it would otherwise pass on
/// is that pane's identity: `HERDR_PANE_ID`, `HERDR_SOCKET_PATH`, the dock's
/// lease. A service never sees those, and a supervisor that did would take one
/// pane's server and dock for its own. `vars` is the names now set.
fn unmanaged_command(
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
    vars: &[std::ffi::OsString],
) -> Command {
    let mut command = Command::new(exe);
    command.args(["daemon", "run"]).current_dir(home);
    for name in vars {
        if name.to_string_lossy().starts_with("HERDR_") {
            command.env_remove(name);
        }
    }
    command
        .env("HOME", home)
        .env("HERDR_TOWER_STATE_DIR", state_dir);
    if let Some(config) = config {
        command.env("HERDR_TOWER_CONFIG", config);
    }
    command
}

/// Starts `command` in a session of its own, so the terminal it came from can
/// close, with its output in `daemon.log`.
///
/// 🚨 No descriptor of the caller's survives into it. Whoever ran `daemon
/// start` reads its stderr to the end, and a supervisor holding that pipe would
/// keep the tower waiting forever.
#[cfg(unix)]
fn spawn_detached(command: &mut Command) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let state_dir = command
        .get_envs()
        .find(|(name, _)| *name == "HERDR_TOWER_STATE_DIR")
        .and_then(|(_, value)| value)
        .map(PathBuf::from)
        .ok_or_else(|| std::io::Error::other("the state directory is not set"))?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("daemon.log"))?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid(2) is async-signal-safe and touches no process memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map(drop)
}

#[cfg(not(unix))]
fn spawn_detached(_command: &mut Command) -> std::io::Result<()> {
    unreachable!("Windows has no service manager to fall back from")
}

/// Whether the service manager is what keeps the supervisor alive here, for
/// the platforms that have one. A supervisor that is running while this says
/// no came from [`start_unmanaged`].
pub fn service_manager_active() -> Option<bool> {
    match std::env::consts::OS {
        "macos" => Some(
            Command::new("launchctl")
                .args(["print", &format!("gui/{}/{}", current_uid(), LABEL)])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
        ),
        "linux" => Some(systemd_unit_active().unwrap_or(false)),
        _ => None,
    }
}

/// What a person can do about a supervisor running without a service manager.
pub fn unmanaged_advice() -> &'static str {
    if std::env::consts::OS == "macos" {
        "sign in at the screen, then: herdr-tower daemon install"
    } else {
        "loginctl enable-linger \"$USER\", then: herdr-tower daemon install"
    }
}

/// `systemctl --user`, able to find this user's service manager from a shell
/// that was not given the way there (#14).
fn systemctl_user() -> Command {
    let mut command = Command::new("systemctl");
    command.arg("--user");
    if let Some(dir) = missing_runtime_dir(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        current_uid(),
        |dir| dir.is_dir(),
    ) {
        command.env("XDG_RUNTIME_DIR", dir);
    }
    command
}

/// The value `XDG_RUNTIME_DIR` should have had, when it is unset and the
/// standard directory exists. `su` and some remote sessions never set it, and
/// without it `systemctl --user` cannot reach a manager that is running.
fn missing_runtime_dir(
    current: Option<&std::ffi::OsStr>,
    uid: impl std::fmt::Display,
    exists: impl FnOnce(&Path) -> bool,
) -> Option<PathBuf> {
    if current.is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    exists(&dir).then_some(dir)
}

fn systemd_unit_active() -> std::io::Result<bool> {
    Ok(systemctl_user()
        .args(["is-active", "--quiet", "herdr-tower.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| std::io::Error::other(with_linger_hint(e)))?
        .success())
}

fn run_systemctl(args: &[&str]) -> std::io::Result<()> {
    run(systemctl_user().args(args)).map_err(|e| std::io::Error::other(with_linger_hint(e)))
}

/// `systemctl`'s complaint, and what to do about it when it is the one that
/// means there is no user manager to talk to.
fn with_linger_hint(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        return "systemctl was not found".into();
    }
    let said = error.to_string();
    if said.contains("to bus") || said.contains("user scope bus") {
        format!(
            "{said} (no user service manager in this session — loginctl enable-linger \"$USER\")"
        )
    } else {
        said
    }
}

pub fn service_path(home: &Path) -> PathBuf {
    match std::env::consts::OS {
        "macos" => home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")),
        "windows" => home
            .join(".config")
            .join("herdr-tower")
            .join("supervisor.cmd"),
        _ => home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("herdr-tower.service"),
    }
}

fn install_macos(
    out: &mut impl Write,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
    dry_run: bool,
) -> std::io::Result<()> {
    let path = service_path(home);
    let body = macos_plist(home, exe, state_dir, config);
    writeln!(out, "Resident supervisor: {}", path.display())?;
    if dry_run {
        writeln!(out, "  would install and start {LABEL}")?;
        return Ok(());
    }
    write_service(&path, body.as_bytes())?;
    let domain = format!("gui/{}", current_uid());
    let target = format!("{domain}/{LABEL}");
    reload(&Launchctl, &domain, &target, &path)?;
    run(Command::new("launchctl").args(["kickstart", "-k", &target]))?;
    writeln!(out, "  installed and started")?;
    for r in RETIRED {
        retire_agent(out, &Launchctl, home, &domain, r)?;
    }
    Ok(())
}

/// A LaunchAgent from the days before the supervisor did its job.
struct Retired {
    label: &'static str,
    /// The arguments after the `herdr-tower` binary, exactly.
    args: &'static [&'static str],
    why: &'static str,
}

/// What `daemon install` removes. Both at once with the supervisor would do
/// the work twice — the card published twice as often, or two servers for
/// one port.
const RETIRED: &[Retired] = &[
    Retired {
        label: "dev.herdr.tower.card",
        args: &["card"],
        why: "the supervisor refreshes the Discord card itself now",
    },
    Retired {
        label: "dev.herdr.tower.mcp",
        args: &["mcp", "serve"],
        why: "the supervisor serves the remote MCP server itself now",
    },
];

/// Removes one [`Retired`] agent when it runs exactly `herdr-tower <args>`. A
/// job with anything else in it is somebody's own and is left alone.
fn retire_agent(
    out: &mut impl Write,
    l: &impl Launchd,
    home: &Path,
    domain: &str,
    r: &Retired,
) -> std::io::Result<()> {
    let plist = home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", r.label));
    let Ok(body) = fs::read_to_string(&plist) else {
        return Ok(());
    };
    if !runs_exactly(&body, r.args) {
        return Ok(());
    }
    l.bootout(&format!("{domain}/{}", r.label));
    fs::remove_file(&plist)?;
    writeln!(out, "  removed {} — {}", r.label, r.why)
}

/// Whether a plist's `ProgramArguments` is exactly `<…/herdr-tower> <args>`.
fn runs_exactly(body: &str, args: &[&str]) -> bool {
    let Some(array) = body
        .split_once("<key>ProgramArguments</key>")
        .and_then(|(_, rest)| rest.split_once("<array>"))
        .and_then(|(_, rest)| rest.split_once("</array>"))
        .map(|(inner, _)| inner)
    else {
        return false;
    };
    let strings: Vec<String> = array
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split_once("</string>").map(|(v, _)| unxml(v)))
        .collect();
    let Some((bin, rest)) = strings.split_first() else {
        return false;
    };
    Path::new(bin)
        .file_name()
        .is_some_and(|n| n == "herdr-tower")
        && rest.iter().map(String::as_str).eq(args.iter().copied())
}

/// The launchd calls [`reload`] makes, so the sequence can be tested without
/// touching the user's real service.
trait Launchd {
    fn bootout(&self, target: &str);
    fn loaded(&self, target: &str) -> bool;
    fn bootstrap(&self, domain: &str, plist: &Path) -> std::io::Result<()>;
    fn sleep(&self, d: std::time::Duration);
}

struct Launchctl;

impl Launchd for Launchctl {
    fn bootout(&self, target: &str) {
        let _ = Command::new("launchctl")
            .args(["bootout", target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    fn loaded(&self, target: &str) -> bool {
        Command::new("launchctl")
            .args(["print", target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    fn bootstrap(&self, domain: &str, plist: &Path) -> std::io::Result<()> {
        run(Command::new("launchctl").args([
            "bootstrap",
            domain,
            plist.as_os_str().to_string_lossy().as_ref(),
        ]))
    }
    fn sleep(&self, d: std::time::Duration) {
        std::thread::sleep(d);
    }
}

/// How often, and how many times, [`reload`] looks for a booted-out job to
/// have left: ten seconds at most.
const UNLOAD_POLL: std::time::Duration = std::time::Duration::from_millis(100);
const UNLOAD_POLLS: u32 = 100;

/// How many times a bootstrap is tried, and the pause between tries.
const BOOTSTRAP_TRIES: u32 = 4;
const BOOTSTRAP_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// Unloads the job and loads the new definition.
///
/// 🚨 `bootout` can return while the job is still on its way out, and a
/// `bootstrap` into that gap fails with `5: Input/output error` — the job is
/// still loaded. On 2026-09-24 that left the supervisor down after an
/// install over a long-running one, until a second `daemon install` brought it
/// back. So the job's departure is waited for — bounded, because a job that
/// never leaves must not hang an install — and a bootstrap that fails anyway
/// is tried again before its error is reported.
fn reload(l: &impl Launchd, domain: &str, target: &str, plist: &Path) -> std::io::Result<()> {
    l.bootout(target);
    for _ in 0..UNLOAD_POLLS {
        if !l.loaded(target) {
            break;
        }
        l.sleep(UNLOAD_POLL);
    }
    let mut last = Ok(());
    for attempt in 0..BOOTSTRAP_TRIES {
        if attempt > 0 {
            l.sleep(BOOTSTRAP_PAUSE);
        }
        last = l.bootstrap(domain, plist);
        if last.is_ok() {
            break;
        }
    }
    last
}

fn install_linux(
    out: &mut impl Write,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
    dry_run: bool,
) -> std::io::Result<()> {
    let path = service_path(home);
    let body = systemd_unit(home, exe, state_dir, config)?;
    writeln!(out, "Resident supervisor: {}", path.display())?;
    if dry_run {
        writeln!(out, "  would install and start herdr-tower.service")?;
        return Ok(());
    }
    write_service(&path, body.as_bytes())?;
    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "herdr-tower.service"])?;
    // `restart`, not `enable --now`. `--now` starts a unit that is inactive
    // and does nothing to one that is already running, so reinstalling over a
    // live supervisor would leave it on the old executable and the old
    // environment — including the stale config path this rewrite exists to
    // replace. `restart` covers both cases; macOS gets the same effect from
    // `kickstart -k`.
    run_systemctl(&["restart", "herdr-tower.service"])?;
    writeln!(out, "  installed and started")
}

fn install_windows(
    out: &mut impl Write,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
    dry_run: bool,
) -> std::io::Result<()> {
    let path = service_path(home);
    let body = windows::launcher(home, exe, state_dir, config);
    writeln!(out, "Resident supervisor: {}", path.display())?;
    if dry_run {
        writeln!(out, "  would install it to start at sign-in, and start it")?;
        return Ok(());
    }
    write_service(&path, body.as_bytes())?;
    windows::register(&path)?;
    // Restarted, as `systemctl restart` does: a supervisor already running
    // would otherwise keep the old executable and environment.
    windows::restart(state_dir, &path)?;
    writeln!(out, "  installed and started")
}

/// The resident supervisor on Windows.
///
/// There is no user service manager to hand it to without administrator
/// rights — an at-sign-in scheduled task needs them — so it is two plain
/// pieces: a launcher script holding the environment and the command (read
/// back like a plist or a unit), and a `Run` value under the user's own
/// registry hive that starts it at sign-in. `conhost --headless` gives it a
/// console with no window, which every console program it starts — `herdr`,
/// `git`, the dock tower — inherits instead of opening a window of its own.
///
/// Nothing restarts it if it dies, as `KeepAlive` and `Restart=always` do;
/// every tower runs `daemon start` when it opens, and that starts it again.
mod windows {
    use std::path::Path;
    use std::process::Command;

    use super::{run, LABEL};

    /// The `Run` value's name.
    const RUN_VALUE: &str = "herdr-tower";
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    /// `%` is the only character a quoted `set` or command word cannot hold
    /// as it is; a Windows path holds no `"` or line break.
    fn word(value: &Path) -> String {
        value.to_string_lossy().replace('%', "%%")
    }

    pub(super) fn launcher(
        home: &Path,
        exe: &Path,
        state_dir: &Path,
        config: Option<&Path>,
    ) -> String {
        let config_line = config.map_or_else(String::new, |path| {
            format!("set \"HERDR_TOWER_CONFIG={}\"\r\n", word(path))
        });
        format!(
            "@echo off\r\n\
             rem herdr-tower resident supervisor ({LABEL}), written by `herdr-tower daemon install`.\r\n\
             set \"HOME={}\"\r\n\
             set \"HERDR_TOWER_STATE_DIR={}\"\r\n\
             {config_line}\
             \"{}\" daemon run >> \"{}\" 2>&1\r\n",
            word(home),
            word(state_dir),
            word(exe),
            word(&state_dir.join("daemon.log")),
        )
    }

    fn system_dir() -> std::path::PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Path::new(&root).join("System32")
    }

    /// The command line that runs `launcher` with no window.
    fn command_line(launcher: &Path) -> String {
        let sys = system_dir();
        format!(
            "\"{}\" --headless \"{}\" /d /s /c \"\"{}\"\"",
            sys.join("conhost.exe").display(),
            sys.join("cmd.exe").display(),
            launcher.display()
        )
    }

    /// Starts `launcher` at every sign-in.
    pub(super) fn register(launcher: &Path) -> std::io::Result<()> {
        run(Command::new("reg").args([
            "add",
            RUN_KEY,
            "/v",
            RUN_VALUE,
            "/t",
            "REG_SZ",
            "/d",
            &command_line(launcher),
            "/f",
        ]))
    }

    /// Ends the running supervisor, if any, and starts `launcher` now.
    pub(super) fn restart(state_dir: &Path, launcher: &Path) -> std::io::Result<()> {
        if let Ok(Some(snapshot)) = tower_state::DaemonStore::new(state_dir).read() {
            if tower_os::pid_alive(snapshot.pid) && snapshot.pid != std::process::id() {
                let _ = tower_os::kill(snapshot.pid);
            }
        }
        spawn(launcher)
    }

    #[cfg(windows)]
    fn spawn(launcher: &Path) -> std::io::Result<()> {
        // From the home folder, not the caller's: see `spawn_detached`.
        let home = tower_os::home().unwrap_or_else(std::env::temp_dir);
        tower_os::spawn_detached(
            &format!(
                "\"{}\" /d /s /c \"\"{}\"\"",
                system_dir().join("cmd.exe").display(),
                launcher.display()
            ),
            &home,
        )
        .map(drop)
    }

    #[cfg(not(windows))]
    fn spawn(_launcher: &Path) -> std::io::Result<()> {
        unreachable!("the Windows supervisor is started on Windows")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_launcher_reads_back() {
            let body = launcher(
                Path::new(r"C:\Users\u"),
                Path::new(r"C:\Users\u\.local\bin\herdr-tower.exe"),
                Path::new(r"C:\Users\u\.local\state\100% herdr-tower"),
                Some(Path::new(r"C:\cfg\tower.toml")),
            );
            assert!(body.contains(
                "set \"HERDR_TOWER_STATE_DIR=C:\\Users\\u\\.local\\state\\100%% herdr-tower\"\r\n"
            ));
            assert!(body.contains("\"C:\\Users\\u\\.local\\bin\\herdr-tower.exe\" daemon run >> "));
            assert!(body.contains("set \"HERDR_TOWER_CONFIG=C:\\cfg\\tower.toml\""));
        }

        #[test]
        fn the_run_value_has_no_window_and_quotes_the_path() {
            let line = command_line(Path::new(
                r"C:\Users\a b\.config\herdr-tower\supervisor.cmd",
            ));
            assert!(line.contains("conhost.exe\" --headless "), "{line}");
            assert!(
                line.ends_with(r#"/d /s /c ""C:\Users\a b\.config\herdr-tower\supervisor.cmd"""#),
                "{line}"
            );
        }
    }
}

/// The service reads the same configuration the tower does, so a
/// `HERDR_TOWER_CONFIG` that selected a non-default file has to travel with
/// it. Without that, a supervisor-enforced setting written in the chosen file
/// — `dock_keep`, say — is read from the default path instead and silently
/// takes its default. `dockcmd::launch_command` carries the variable into the
/// tower for the same reason.
/// The executable an installed service definition runs, if it can be read.
fn installed_exe(service: &Path) -> Option<PathBuf> {
    let body = fs::read_to_string(service).ok()?;
    installed_exe_raw(&body)
}

fn installed_exe_raw(body: &str) -> Option<PathBuf> {
    match std::env::consts::OS {
        "macos" => {
            let rest = body.split_once("<key>ProgramArguments</key>")?.1;
            let raw = rest.split_once("<string>")?.1.split_once("</string>")?.0;
            Some(PathBuf::from(unxml(raw)))
        }
        "linux" => body.lines().find_map(|line| {
            let raw = line
                .strip_prefix("ExecStart=")?
                .strip_suffix(" daemon run")?;
            Some(PathBuf::from(unsystemd_word(raw.trim())))
        }),
        "windows" => body.lines().find_map(|line| {
            let raw = line.strip_prefix('"')?.split_once("\" daemon run")?.0;
            Some(PathBuf::from(raw.replace("%%", "%")))
        }),
        _ => None,
    }
}

/// The binary `start` keeps the service on: the installed one while it still
/// exists, the caller's own otherwise (none installed, or it was deleted).
fn kept_exe<'a>(installed: Option<&'a Path>, caller: &'a Path) -> &'a Path {
    match installed {
        Some(path) if path.is_file() => path,
        _ => caller,
    }
}

/// The config override recorded in an installed service definition, if any.
///
/// Read back rather than remembered: the value belongs to the definition on
/// disk, and a `start` that cannot see the environment it was installed with
/// still has to leave it alone.
fn installed_config(service: &Path) -> Option<PathBuf> {
    let body = fs::read_to_string(service).ok()?;
    installed_config_raw(&body)
}

/// The state directory as it can be handed to another process.
///
/// A service starts from a different working directory, so a relative store —
/// `HERDR_TOWER_STATE_DIR=state`, say — names a different one there.
/// `config_override` normalizes the config path for the same reason; this is
/// the same handoff.
fn absolute_state_dir(state_dir: &Path) -> std::io::Result<PathBuf> {
    if state_dir.is_absolute() {
        return Ok(state_dir.to_path_buf());
    }
    std::path::absolute(state_dir).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "the state directory {} could not be made absolute ({error}); \
                 the resident supervisor starts elsewhere and would use a different store",
                state_dir.display()
            ),
        )
    })
}

/// Whether this process was told which state directory to use, as opposed to
/// having fallen back to the default.
fn state_dir_is_explicit() -> bool {
    std::env::var_os("HERDR_TOWER_STATE_DIR").is_some_and(|value| !value.is_empty())
}

/// The state directory recorded in an installed service definition.
fn installed_state_dir(service: &Path) -> Option<PathBuf> {
    let body = fs::read_to_string(service).ok()?;
    let found = installed_env(&body, "HERDR_TOWER_STATE_DIR")?;
    found.is_absolute().then_some(found)
}

fn installed_config_raw(body: &str) -> Option<PathBuf> {
    installed_env(body, "HERDR_TOWER_CONFIG")
}

fn installed_env(body: &str, name: &str) -> Option<PathBuf> {
    match std::env::consts::OS {
        "macos" => {
            let key = format!("<key>{name}</key><string>");
            let rest = body.split_once(&key)?.1;
            let raw = rest.split_once("</string>")?.0;
            Some(PathBuf::from(unxml(raw)))
        }
        "linux" => {
            let key = format!("Environment={name}=");
            body.lines()
                .find_map(|line| line.strip_prefix(&key))
                .map(|raw| PathBuf::from(unsystemd_word(raw.trim())))
        }
        "windows" => {
            let key = format!("set \"{name}=");
            body.lines()
                .find_map(|line| line.strip_prefix(&key)?.strip_suffix('"'))
                .map(|raw| PathBuf::from(raw.replace("%%", "%")))
        }
        _ => None,
    }
}

/// Whether the installed service file says something other than what this
/// invocation would write.
///
/// Unreadable counts as different: rewriting a file we cannot inspect is the
/// safe way round, and `install` is idempotent.
fn definition_differs(
    service: &Path,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
) -> bool {
    let wanted = match std::env::consts::OS {
        "macos" => macos_plist(home, exe, state_dir, config),
        // A path the unit cannot hold differs from every file; the install
        // that follows is what refuses it, with the reason.
        "linux" => match systemd_unit(home, exe, state_dir, config) {
            Ok(unit) => unit,
            Err(_) => return true,
        },
        "windows" => windows::launcher(home, exe, state_dir, config),
        _ => return false,
    };
    fs::read_to_string(service).map_or(true, |existing| existing != wanted)
}

fn macos_plist(home: &Path, exe: &Path, state_dir: &Path, config: Option<&Path>) -> String {
    let home = xml(&home.to_string_lossy());
    let exe = xml(&exe.to_string_lossy());
    let state = xml(&state_dir.to_string_lossy());
    let config_entry = config.map_or_else(String::new, |path| {
        format!(
            "\n    <key>HERDR_TOWER_CONFIG</key><string>{}</string>",
            xml(&path.to_string_lossy())
        )
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>daemon</string><string>run</string></array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>HERDR_TOWER_STATE_DIR</key><string>{state}</string>{config_entry}
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{state}/daemon.log</string>
  <key>StandardErrorPath</key><string>{state}/daemon.log</string>
</dict>
</plist>
"#
    )
}

/// See [`macos_plist`] for why the config path travels with the service.
///
/// An error when a path cannot be written into a unit at all — see
/// [`systemd_word`].
fn systemd_unit(
    home: &Path,
    exe: &Path,
    state_dir: &Path,
    config: Option<&Path>,
) -> std::io::Result<String> {
    let config_line = match config {
        Some(path) => format!(
            "Environment=HERDR_TOWER_CONFIG={}\n",
            systemd_word(&path.to_string_lossy())?
        ),
        None => String::new(),
    };
    Ok(format!(
        "[Unit]\nDescription=herdr-tower resident supervisor\nAfter=default.target\n\n\
         [Service]\nType=simple\nExecStart={} daemon run\nRestart=always\nRestartSec=2\n\
         Environment=HOME={}\nEnvironment=HERDR_TOWER_STATE_DIR={}\n{}\n\
         [Install]\nWantedBy=default.target\n",
        systemd_exec_word(&exe.to_string_lossy())?,
        systemd_word(&home.to_string_lossy())?,
        systemd_word(&state_dir.to_string_lossy())?,
        config_line
    ))
}

/// Undoes [`xml`] for the entities it produces.
fn unxml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Last, so an escaped ampersand in the original does not re-expand the
        // entity that followed it.
        .replace("&amp;", "&")
}

/// Undoes [`systemd_word`]'s quoting.
fn unsystemd_word(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    inner
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
        .replace("%%", "%")
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `value` as one double-quoted word of a unit file, or an error when no
/// quoting can hold it.
///
/// Quotes and backslashes are escaped for systemd's own word splitting, and
/// `%` is doubled because systemd expands `%h`, `%u` and the other specifiers
/// inside quotes too: a home directory with `%h` in its name would otherwise
/// be rewritten into a different path. A newline cannot be escaped at all — it
/// ends the line, and whatever follows it in the path would be read as a
/// directive of its own (a second `ExecStart=`, say) — so it is refused rather
/// than written.
fn systemd_word(value: &str) -> std::io::Result<String> {
    if value.contains(['\n', '\r']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{value:?} holds a line break, which a systemd unit cannot carry"),
        ));
    }
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

/// [`systemd_word`] for the command line of `ExecStart=`, where systemd also
/// expands `$NAME` and `${NAME}` from the environment: `$` is doubled so the
/// path is the path. Not in `Environment=` values, which are not expanded and
/// would keep both dollars.
fn systemd_exec_word(value: &str) -> std::io::Result<String> {
    Ok(systemd_word(value)?.replace('$', "$$"))
}

fn write_service(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "service path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, body)?;
    fs::rename(tmp, path)
}

fn run(command: &mut Command) -> std::io::Result<()> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const CARD_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.herdr.tower.card</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/q/.local/bin/herdr-tower</string>
    <string>card</string>
  </array>
  <key>StartInterval</key><integer>300</integer>
</dict>
</plist>"#;

    #[test]
    fn a_missing_runtime_dir_is_filled_in_only_when_it_exists() {
        let any = |_: &Path| true;
        assert_eq!(
            missing_runtime_dir(None, 1002, any),
            Some(PathBuf::from("/run/user/1002"))
        );
        assert_eq!(
            missing_runtime_dir(Some(std::ffi::OsStr::new("")), 1002, any),
            Some(PathBuf::from("/run/user/1002")),
            "empty is unset to systemctl too"
        );
        assert_eq!(
            missing_runtime_dir(Some(std::ffi::OsStr::new("/x")), 1002, any),
            None,
            "one that is set is the session's own"
        );
        assert_eq!(missing_runtime_dir(None, 1002, |_| false), None);
    }

    #[test]
    fn the_bus_failure_says_what_to_do_and_others_are_left_alone() {
        let bus = "Failed to connect to user scope bus via local transport: not defined";
        assert!(with_linger_hint(std::io::Error::other(bus)).contains("enable-linger"));
        let old = "Failed to connect to bus: No such file or directory";
        assert!(with_linger_hint(std::io::Error::other(old)).contains("enable-linger"));
        assert_eq!(
            with_linger_hint(std::io::ErrorKind::NotFound.into()),
            "systemctl was not found"
        );
        let other = "Unit herdr-tower.service has a bad setting";
        assert_eq!(with_linger_hint(std::io::Error::other(other)), other);
    }

    #[test]
    fn the_unmanaged_supervisor_does_not_inherit_the_pane_it_was_started_from() {
        let vars = [
            "HERDR_PANE_ID",
            "HERDR_SOCKET_PATH",
            "HERDR_TOWER_DOCK_LEASE",
            "HERDR_TOWER_STATE_DIR",
            "PATH",
        ]
        .map(std::ffi::OsString::from);
        let command = unmanaged_command(
            Path::new("/home/me"),
            Path::new("/bin/herdr-tower"),
            Path::new("/state"),
            Some(Path::new("/cfg.toml")),
            &vars,
        );
        let envs: std::collections::HashMap<_, _> = command
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(PathBuf::from)))
            .collect();
        assert_eq!(envs["HERDR_PANE_ID"], None);
        assert_eq!(envs["HERDR_SOCKET_PATH"], None);
        assert_eq!(envs["HERDR_TOWER_DOCK_LEASE"], None);
        assert_eq!(envs["HERDR_TOWER_STATE_DIR"], Some(PathBuf::from("/state")));
        assert_eq!(envs["HERDR_TOWER_CONFIG"], Some(PathBuf::from("/cfg.toml")));
        assert_eq!(envs["HOME"], Some(PathBuf::from("/home/me")));
        assert!(!envs.contains_key("PATH"), "PATH is left as it is");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["daemon", "run"]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/home/me")));
    }

    /// Writes an executable script from a child process.
    ///
    /// 🚨 Not with `fs::write` here: a test thread forking at that moment keeps
    /// the write handle until it execs, and running the script then fails with
    /// `Text file busy`. A handle this process never opens cannot be inherited.
    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                "printf '%s' \"$0\" > \"$1\" && chmod 755 \"$1\"",
                body,
            ])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// A stand-in supervisor: says it ran, with what, and exits.
    #[cfg(unix)]
    fn stand_in(dir: &Path) -> PathBuf {
        let exe = dir.join("stand-in");
        write_script(
            &exe,
            "#!/bin/sh\nprintf '%s %s|%s\\n' \"$1\" \"$2\" \"$HERDR_PANE_ID\" >> \"$HERDR_TOWER_STATE_DIR/ran\"\n",
        );
        exe
    }

    #[cfg(unix)]
    fn wait_for(path: &Path) -> String {
        for _ in 0..500 {
            if let Ok(text) = fs::read_to_string(path) {
                if text.ends_with('\n') {
                    return text;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{} never appeared", path.display());
    }

    #[cfg(unix)]
    #[test]
    fn a_service_manager_that_cannot_be_reached_still_gets_a_supervisor() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state");
        let mut out = Vec::new();
        start_unmanaged(
            &mut out,
            d.path(),
            &stand_in(d.path()),
            &state,
            None,
            std::io::Error::other("no bus"),
        )
        .unwrap();
        assert_eq!(wait_for(&state.join("ran")), "daemon run|\n");
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("without a service manager: no bus"), "{said}");
        let log = fs::read_to_string(state.join(crate::supervisorlog::LOG_FILE)).unwrap();
        assert!(log.contains("no bus"), "{log}");
    }

    #[cfg(unix)]
    #[test]
    fn a_supervisor_holding_the_lock_is_not_given_a_second_one_to_wait_behind_it() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let lock = fs::File::create(state.join("loop-supervisor.lock")).unwrap();
        lock.lock().unwrap();
        let mut out = Vec::new();
        start_unmanaged(
            &mut out,
            d.path(),
            &stand_in(d.path()),
            &state,
            None,
            std::io::Error::other("no bus"),
        )
        .unwrap();
        assert!(String::from_utf8(out).unwrap().contains("already running"));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!state.join("ran").exists());
    }

    #[cfg(unix)]
    #[test]
    fn both_reasons_come_back_when_neither_way_starts_it() {
        let d = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let error = start_unmanaged(
            &mut out,
            d.path(),
            &d.path().join("no-such-binary"),
            &d.path().join("state"),
            None,
            std::io::Error::other("no bus"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("no bus; and it could not start"),
            "{error}"
        );
    }

    #[test]
    fn only_the_plain_card_job_is_the_supervisors_to_retire() {
        let mcp = crate::mcp::plist(
            Path::new("/u"),
            Path::new("/u/.local/bin/herdr-tower"),
            Path::new("/s"),
            None,
        );
        assert!(
            runs_exactly(&mcp, &["mcp", "serve"]),
            "the plist `mcp on` wrote"
        );
        assert!(!runs_exactly(&mcp, &["card"]));
        assert!(runs_exactly(CARD_PLIST, &["card"]));
        for other in [
            CARD_PLIST.replace(
                "<string>card</string>",
                "<string>card</string>\n    <string>--out</string>",
            ),
            CARD_PLIST.replace("/herdr-tower<", "/herdr-tower-dev<"),
            CARD_PLIST.replace("<string>card</string>", "<string>status</string>"),
            "<plist></plist>".to_string(),
        ] {
            assert!(!runs_exactly(&other, &["card"]), "{other}");
        }
    }

    #[test]
    fn the_card_agent_is_booted_out_and_removed_and_anything_else_stays() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("Library/LaunchAgents");
        fs::create_dir_all(&dir).unwrap();
        let plist = dir.join("dev.herdr.tower.card.plist");

        fs::write(&plist, CARD_PLIST).unwrap();
        let fake = Fake::new(0, 0);
        let mut out = Vec::new();
        retire_agent(&mut out, &fake, home.path(), "gui/501", &RETIRED[0]).unwrap();
        assert!(!plist.exists());
        assert_eq!(fake.count("bootout"), 1);
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("removed dev.herdr.tower.card"));

        let theirs = CARD_PLIST.replace(
            "<string>card</string>",
            "<string>card</string><string>--out</string>",
        );
        fs::write(&plist, &theirs).unwrap();
        let fake = Fake::new(0, 0);
        let mut out = Vec::new();
        retire_agent(&mut out, &fake, home.path(), "gui/501", &RETIRED[0]).unwrap();
        assert_eq!(fs::read_to_string(&plist).unwrap(), theirs);
        assert_eq!(fake.count("bootout"), 0);
        assert!(out.is_empty());

        // Nothing there: nothing to do.
        fs::remove_file(&plist).unwrap();
        retire_agent(
            &mut Vec::new(),
            &Fake::new(0, 0),
            home.path(),
            "gui/501",
            &RETIRED[0],
        )
        .unwrap();
    }

    /// A launchd whose job stays loaded for `lingers` looks after a bootout,
    /// and that refuses the first `refusals` bootstraps.
    struct Fake {
        lingers: Cell<u32>,
        refusals: Cell<u32>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(lingers: u32, refusals: u32) -> Self {
            Fake {
                lingers: Cell::new(lingers),
                refusals: Cell::new(refusals),
                calls: RefCell::default(),
            }
        }
        fn count(&self, what: &str) -> usize {
            self.calls.borrow().iter().filter(|c| **c == what).count()
        }
    }

    impl Launchd for Fake {
        fn bootout(&self, _: &str) {
            self.calls.borrow_mut().push("bootout");
        }
        fn loaded(&self, _: &str) -> bool {
            self.calls.borrow_mut().push("print");
            let n = self.lingers.get();
            self.lingers.set(n.saturating_sub(1));
            n > 0
        }
        fn bootstrap(&self, _: &str, _: &Path) -> std::io::Result<()> {
            self.calls.borrow_mut().push("bootstrap");
            let n = self.refusals.get();
            if n > 0 {
                self.refusals.set(n - 1);
                return Err(std::io::Error::other(
                    "Bootstrap failed: 5: Input/output error",
                ));
            }
            Ok(())
        }
        fn sleep(&self, _: std::time::Duration) {}
    }

    /// A job still on its way out is waited for, and the bootstrap goes in
    /// once it has left rather than into the gap.
    #[test]
    fn a_lingering_job_is_waited_out_before_the_bootstrap() {
        let l = Fake::new(3, 0);
        reload(&l, "gui/501", "gui/501/x", Path::new("/p.plist")).unwrap();
        assert_eq!(
            *l.calls.borrow(),
            ["bootout", "print", "print", "print", "print", "bootstrap"]
        );
    }

    /// A bootstrap that fails anyway is tried again.
    #[test]
    fn a_refused_bootstrap_is_tried_again() {
        let l = Fake::new(0, 2);
        reload(&l, "gui/501", "gui/501/x", Path::new("/p.plist")).unwrap();
        assert_eq!(l.count("bootstrap"), 3);
    }

    /// The wait and the tries are bounded, and the last error is reported.
    #[test]
    fn a_job_that_never_leaves_is_given_up_on_with_the_error() {
        let l = Fake::new(u32::MAX, u32::MAX);
        let err = reload(&l, "gui/501", "gui/501/x", Path::new("/p.plist")).unwrap_err();
        assert!(err.to_string().contains("Input/output error"));
        assert_eq!(l.count("print"), UNLOAD_POLLS as usize);
        assert_eq!(l.count("bootstrap"), BOOTSTRAP_TRIES as usize);
    }

    #[test]
    fn launchd_unit_is_one_keepalive_process_with_explicit_paths() {
        let body = macos_plist(
            Path::new("/Users/a&b"),
            Path::new("/opt/herdr tower"),
            Path::new("/Users/a&b/state"),
            None,
        );
        assert!(body.contains("<key>KeepAlive</key><true/>"));
        assert!(body.contains("<string>/opt/herdr tower</string>"));
        assert!(body.contains("/Users/a&amp;b/state"));
        assert_eq!(body.matches("daemon</string>").count(), 1);
    }

    /// `start` must not keep running a definition that no longer matches —
    /// `init` can have installed the service before a config was chosen, and
    /// the supervisor would go on reading the default file.
    #[test]
    fn an_existing_definition_is_replaced_when_it_no_longer_matches() {
        let dir = std::env::temp_dir().join(format!("tower-daemoncmd-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let service = dir.join("service");
        let (home, exe, state) = (
            Path::new("/home/me"),
            Path::new("/home/me/bin/herdr-tower"),
            Path::new("/home/me/state"),
        );

        let current = match std::env::consts::OS {
            "macos" => macos_plist(home, exe, state, None),
            "windows" => windows::launcher(home, exe, state, None),
            _ => systemd_unit(home, exe, state, None).unwrap(),
        };
        fs::write(&service, &current).expect("write service");

        assert!(
            !definition_differs(&service, home, exe, state, None),
            "the same inputs produce the same file"
        );
        assert!(
            definition_differs(
                &service,
                home,
                exe,
                state,
                Some(Path::new("/srv/conf.json"))
            ),
            "a newly chosen config path is a different definition"
        );
        assert!(
            definition_differs(&dir.join("missing"), home, exe, state, None),
            "a file that cannot be read is treated as different"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Every path that writes a definition normalizes the store, not just the
    /// one that compares against an existing one: `init` and `daemon install`
    /// write it directly, and a relative spelling there names a different
    /// directory from the service's own working directory.
    #[test]
    fn a_relative_state_directory_is_made_absolute_before_it_is_written() {
        let relative = Path::new("state");
        let absolute = absolute_state_dir(relative).expect("resolvable here");
        assert!(absolute.is_absolute());
        assert!(absolute.ends_with("state"));

        // Already absolute: used as given.
        let given = if cfg!(windows) {
            Path::new(r"C:\home\me\state")
        } else {
            Path::new("/home/me/state")
        };
        assert_eq!(absolute_state_dir(given).unwrap(), given);

        // And the definition carries the absolute form, whoever asked.
        let mut out = Vec::new();
        install(
            &mut out,
            Path::new("/home/me"),
            Path::new("/home/me/bin/herdr-tower"),
            relative,
            None,
            true,
        )
        .expect("dry run");
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("would install"), "{printed}");
    }

    /// A `start` with no `HERDR_TOWER_CONFIG` in its environment is not a
    /// request to remove the installed one — it is run from login shells,
    /// `make install` and hooks alike. Dropping the override there would
    /// restart the supervisor on defaults and re-enable `dock_keep`.
    #[test]
    fn an_installed_override_is_read_back_and_kept() {
        let dir = std::env::temp_dir().join(format!("tower-installed-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let service = dir.join("service");
        let (home, exe, state) = (
            Path::new("/home/me"),
            Path::new("/home/me/bin/herdr-tower"),
            Path::new("/home/me/state"),
        );
        // A path with both an ampersand and a space, to exercise the escaping
        // each format applies on the way in.
        let chosen = Path::new("/srv/a&b/my conf.json");

        let body = match std::env::consts::OS {
            "macos" => macos_plist(home, exe, state, Some(chosen)),
            "windows" => windows::launcher(home, exe, state, Some(chosen)),
            _ => systemd_unit(home, exe, state, Some(chosen)).unwrap(),
        };
        fs::write(&service, &body).expect("write service");

        assert_eq!(
            installed_config(&service).as_deref(),
            Some(chosen),
            "the override round-trips through the definition's escaping"
        );

        assert!(
            !definition_differs(&service, home, exe, state, Some(chosen)),
            "and matches, so a plain start rewrites nothing"
        );

        let bare = match std::env::consts::OS {
            "macos" => macos_plist(home, exe, state, None),
            "windows" => windows::launcher(home, exe, state, None),
            _ => systemd_unit(home, exe, state, None).unwrap(),
        };
        fs::write(&service, &bare).expect("write service");
        assert_eq!(
            installed_config(&service),
            None,
            "none installed, none read"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A tower opened from a development build calls `start` with that build.
    /// The installed binary has to stay, or the supervisor and the dock it
    /// keeps move to the test build (they did, 2026-10-10).
    #[test]
    fn start_keeps_the_installed_binary_while_it_exists() {
        let dir = std::env::temp_dir().join(format!("tower-exe-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let installed = dir.join("bin dir/herdr&tower");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, "").unwrap();
        let debug = Path::new("/w/target/debug/herdr-tower");
        let (home, state) = (Path::new("/home/me"), Path::new("/home/me/state"));

        let body = match std::env::consts::OS {
            "macos" => macos_plist(home, &installed, state, None),
            "windows" => windows::launcher(home, &installed, state, None),
            _ => systemd_unit(home, &installed, state, None).unwrap(),
        };
        let service = dir.join("service");
        fs::write(&service, &body).unwrap();
        assert_eq!(
            installed_exe(&service).as_deref(),
            Some(installed.as_path()),
            "read back through the format's escaping"
        );

        let read = installed_exe(&service);
        assert_eq!(kept_exe(read.as_deref(), debug), installed.as_path());
        assert!(
            !definition_differs(
                &service,
                home,
                kept_exe(read.as_deref(), debug),
                state,
                None
            ),
            "so a start from the debug build rewrites nothing"
        );

        fs::remove_file(&installed).unwrap();
        assert_eq!(
            kept_exe(read.as_deref(), debug),
            debug,
            "a deleted binary is replaced by the caller's"
        );
        assert_eq!(kept_exe(None, debug), debug, "nothing installed yet");
        fs::remove_dir_all(&dir).ok();
    }

    /// The state directory is read back on the same terms as the config path.
    /// Rewriting the definition with the default store would leave the agents
    /// and leases in the installed one unsupervised, with nothing to say so.
    #[test]
    fn an_installed_state_directory_is_read_back() {
        let dir = std::env::temp_dir().join(format!("tower-statedir-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let service = dir.join("service");
        let (home, exe) = (Path::new("/home/me"), Path::new("/home/me/bin/herdr-tower"));
        // Absolute where it is read back: only an absolute store is taken.
        let custom = if cfg!(windows) {
            Path::new(r"C:\srv\a&b\state store")
        } else {
            Path::new("/srv/a&b/state store")
        };

        let body = match std::env::consts::OS {
            "macos" => macos_plist(home, exe, custom, None),
            "windows" => windows::launcher(home, exe, custom, None),
            _ => systemd_unit(home, exe, custom, None).unwrap(),
        };
        fs::write(&service, &body).expect("write service");
        assert_eq!(
            installed_state_dir(&service).as_deref(),
            Some(custom),
            "it round-trips through each format's escaping"
        );

        // The two values are read independently of one another.
        let both = match std::env::consts::OS {
            "macos" => macos_plist(home, exe, custom, Some(Path::new("/srv/conf.json"))),
            "windows" => windows::launcher(home, exe, custom, Some(Path::new("/srv/conf.json"))),
            _ => systemd_unit(home, exe, custom, Some(Path::new("/srv/conf.json"))).unwrap(),
        };
        fs::write(&service, &both).expect("write service");
        assert_eq!(installed_state_dir(&service).as_deref(), Some(custom));
        assert_eq!(
            installed_config(&service).as_deref(),
            Some(Path::new("/srv/conf.json"))
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A config path chosen with `HERDR_TOWER_CONFIG` has to reach the
    /// service, or a setting the supervisor enforces is read from the default
    /// file and silently takes its default — `dock_keep: false` written in the
    /// chosen file would not hold.
    #[test]
    fn a_chosen_config_path_travels_into_the_service() {
        let plist = macos_plist(
            Path::new("/Users/a&b"),
            Path::new("/opt/herdr tower"),
            Path::new("/Users/a&b/state"),
            Some(Path::new("/srv/conf/mine&yours.json")),
        );
        assert!(plist.contains("<key>HERDR_TOWER_CONFIG</key>"));
        assert!(plist.contains("/srv/conf/mine&amp;yours.json"));

        let unit = systemd_unit(
            Path::new("/home/me"),
            Path::new("/home/me/bin/herdr-tower"),
            Path::new("/home/me/state"),
            Some(Path::new("/srv/conf/my conf.json")),
        )
        .unwrap();
        assert!(unit.contains("Environment=HERDR_TOWER_CONFIG=\"/srv/conf/my conf.json\""));
        assert!(unit.contains("[Install]"), "the unit is still well formed");

        // No override, no entry — the default path is what the binary already
        // resolves on its own.
        let bare = systemd_unit(
            Path::new("/home/me"),
            Path::new("/home/me/bin/herdr-tower"),
            Path::new("/home/me/state"),
            None,
        )
        .unwrap();
        assert!(!bare.contains("HERDR_TOWER_CONFIG"));
    }

    /// The characters systemd gives meaning to inside a quoted word: `%`
    /// specifiers everywhere, `$` expansion on the command line. Both come out
    /// literal, and an environment value reads back as the path it was.
    #[test]
    fn specifiers_and_dollars_in_a_path_stay_literal() {
        let unit = systemd_unit(
            Path::new("/home/100%h"),
            Path::new("/opt/$HOME/%u/herdr-tower"),
            Path::new("/home/100%h/state"),
            Some(Path::new("/srv/50%/conf.json")),
        )
        .unwrap();
        assert!(
            unit.contains("ExecStart=\"/opt/$$HOME/%%u/herdr-tower\" daemon run"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=HOME=\"/home/100%%h\"\n"),
            "{unit}"
        );
        assert!(unit.contains("Environment=HERDR_TOWER_CONFIG=\"/srv/50%%/conf.json\""));
        let state = unit
            .lines()
            .find_map(|l| l.strip_prefix("Environment=HERDR_TOWER_STATE_DIR="))
            .unwrap();
        assert_eq!(unsystemd_word(state), "/home/100%h/state");
    }

    /// A line break in any path would start a directive of its own; the unit
    /// is refused instead of written, whichever path holds it.
    #[test]
    fn a_path_with_a_line_break_is_refused() {
        let evil = Path::new("/tmp/x\nExecStartPre=/bin/sh -c evil");
        let ok = Path::new("/home/me");
        for (home, exe, state, config) in [
            (evil, ok, ok, None),
            (ok, evil, ok, None),
            (ok, ok, evil, None),
            (ok, ok, ok, Some(evil)),
            (ok, Path::new("/a\rb"), ok, None),
        ] {
            let err = systemd_unit(home, exe, state, config).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        let d = tempfile::tempdir().unwrap();
        let service = d.path().join("service");
        fs::write(&service, "anything").unwrap();
        if std::env::consts::OS == "linux" {
            assert!(definition_differs(&service, ok, evil, ok, None));
        }
    }

    #[test]
    fn systemd_unit_restarts_the_same_single_supervisor() {
        let body = systemd_unit(
            Path::new("/home/me"),
            Path::new("/home/me/bin/herdr tower"),
            Path::new("/home/me/state"),
            None,
        )
        .unwrap();
        assert!(body.contains("Restart=always"));
        assert!(body.contains("ExecStart=\"/home/me/bin/herdr tower\" daemon run"));
    }

    /// A generated path, heavy in the characters systemd reads inside a
    /// quoted word (`%`, `$`, quotes, backslashes) plus spaces and unicode.
    fn systemd_path(rng: &mut tower_testrng::Rng) -> String {
        const HOT: &[&str] = &[
            "%", "%%", "%h", "$", "$HOME", "${X}", "\"", "'", "\\", "\\\\", "\\\"", " ", "/",
            "세션", "😀", "é", "a", "b", ";", "#", "\t",
        ];
        (0..rng.below(12))
            .map(|_| *rng.pick(HOT).unwrap())
            .collect()
    }

    /// An independent decoder for a double-quoted systemd word: one pass,
    /// left to right, the way systemd reads it — a backslash takes the next
    /// character literally, `%%` is one `%`.
    fn systemd_decode(word: &str) -> Option<String> {
        let inner = word.strip_prefix('"')?.strip_suffix('"')?;
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?),
                '%' => {
                    if chars.next()? != '%' {
                        return None;
                    }
                    out.push('%');
                }
                '"' => return None,
                c => out.push(c),
            }
        }
        Some(out)
    }

    /// Round trip over generated strings (seeded): the quoted word decodes
    /// back to the input both through `unsystemd_word` and through the
    /// independent one-pass decoder; it is always one quoted word with no bare
    /// quote or lone `%` inside; and `unsystemd_word` leaves an unquoted
    /// (legacy) value as it is, so applying it twice changes nothing more.
    #[test]
    fn systemd_words_round_trip_for_generated_strings() {
        let mut rng = tower_testrng::Rng::new(0x0595_7E3D);
        for _ in 0..3000 {
            let s = systemd_path(&mut rng);
            let word = systemd_word(&s).unwrap();
            assert_eq!(unsystemd_word(&word), s, "{word}");
            assert_eq!(systemd_decode(&word).as_deref(), Some(s.as_str()), "{word}");
            let once = unsystemd_word(&word);
            if !(once.starts_with('"') && once.ends_with('"') && once.len() >= 2) {
                assert_eq!(unsystemd_word(&once), once, "unquoted is left alone");
            }
            let exec = systemd_exec_word(&s).unwrap();
            assert_eq!(
                exec.matches('$').count(),
                2 * s.matches('$').count(),
                "{exec}"
            );
            assert_eq!(exec.replace("$$", "$"), word);
        }
        for legacy in ["/home/me", "/a b/c", "", "\"", "no\"quote"] {
            assert_eq!(unsystemd_word(legacy), legacy);
        }
        assert_eq!(systemd_word("").unwrap(), "\"\"");
        assert_eq!(unsystemd_word("\"\""), "");
    }

    /// A line break anywhere — first, last, alone, CRLF, beside unicode — is
    /// refused by both the plain and the command-line word; any other control
    /// character is carried.
    #[test]
    fn every_line_break_is_refused_wherever_it_sits() {
        let mut rng = tower_testrng::Rng::new(0x0A0D);
        for _ in 0..500 {
            let mut s = systemd_path(&mut rng);
            let at = rng.below(s.chars().count() + 1);
            let pos = s.char_indices().nth(at).map_or(s.len(), |(i, _)| i);
            s.insert_str(pos, ["\n", "\r", "\r\n"][rng.below(3)]);
            assert!(systemd_word(&s).is_err(), "{s:?}");
            assert!(systemd_exec_word(&s).is_err(), "{s:?}");
        }
        for ok in ["\t", "\u{0}", "\u{1b}", "\u{2028}", "\u{85}"] {
            let w = systemd_word(ok).unwrap();
            assert_eq!(unsystemd_word(&w), ok, "{ok:?}");
        }
    }

    /// Round trip through a whole generated unit: every `Environment=` value
    /// reads back as the path it was written from.
    #[test]
    fn a_generated_unit_reads_back_its_paths() {
        let mut rng = tower_testrng::Rng::new(0x0417_0017);
        for _ in 0..300 {
            let home = format!("/home/{}", systemd_path(&mut rng));
            let state = format!("{home}/{}", systemd_path(&mut rng));
            let config = format!("/srv/{}.json", systemd_path(&mut rng));
            let unit = systemd_unit(
                Path::new(&home),
                Path::new("/bin/herdr-tower"),
                Path::new(&state),
                Some(Path::new(&config)),
            )
            .unwrap();
            let read = |name: &str| {
                let key = format!("Environment={name}=");
                unit.lines()
                    .find_map(|l| l.strip_prefix(key.as_str()))
                    .map(|raw| unsystemd_word(raw.trim()))
                    .unwrap()
            };
            assert_eq!(read("HOME"), home);
            assert_eq!(read("HERDR_TOWER_STATE_DIR"), state);
            assert_eq!(read("HERDR_TOWER_CONFIG"), config);
            assert_eq!(unit.matches("\nExecStart=").count(), 1);
        }
    }
}
