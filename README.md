# 🗼 herdr-tower 🗼 — the multi-agent control tower (herdr)

### Four agents, one conversation. Who is working and who is waiting, on one screen.

[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.89%2B-orange)](rust/README.md)
[![tests](https://img.shields.io/badge/tests-2235-brightgreen)](rust/README.md)
[![ci](https://github.com/orascope/herdr-tower/actions/workflows/ci.yml/badge.svg)](https://github.com/orascope/herdr-tower/actions/workflows/ci.yml)

A terminal tool that shows, on one screen, which of the Claude Code / Codex /
Gemini / opencode agents running inside [herdr](https://herdr.dev) is
**working, which is waiting for you, and which has finished something you have
not looked at yet**.

> **Requires [herdr](https://herdr.dev) 0.9.0 or newer.** The tower reads the
> agents from herdr's server; without herdr its list is empty.
> One line installs it: `herdr plugin install orascope/herdr-tower`.

> **On tmux instead of herdr?** Use [agentlayer](https://github.com/netwaif/agentlayer)
> (Korean docs), the tmux + iTerm2 control tower herdr-tower started from.

**Session list — v0.60.1**

![herdr-tower v0.60.1 session list: agents grouped by project and machine,
with working, idle and done states, task titles and usage information](docs/images/tower-sidebar.png)

*The machine label in this capture has been shortened; the rest of the screen is unchanged.*

Rows follow herdr's sidebar order — machine, workspace, tab, pane. Each machine
heads its rows with a band — `▼ @desk (6)  cpu … mem … · Ubuntu 26.04 amd64
(herdr-tower v…)` — that folds them away on a click.

**A click on `🗼 HERDR-TOWER 🗼` in herdr's sidebar opens the tower** — a
workspace of its own that the resident supervisor keeps (see
[Opening the tower with the mouse](docs/herdr.md#opening-the-tower-with-the-mouse)).
It is also a key away as a popup (`prefix+a`).

**Selected session preview — v0.60.1**

Select a session to read its recent output below the list, without switching
into that pane first. A click on the preview's `▼` heading folds it to one
line, as a machine's band folds; a click on the folded `▲` band opens it again,
and a drag sets its height.

![herdr-tower v0.60.1 with a selected session: its recent output is visible
in the preview panel below the session list](docs/images/tower-preview.png)

`u` in the tower puts the usage board where the preview was — each AI's plan
windows, when they reset, and the usage guard's sliders:

[View the usage board (`u`)](docs/images/tower-usage.png)

It reproduces what Orca ADE's control surface did without a GUI. One small
resident supervisor owns the Herdr socket connections and idle loops; the popup
is only a client. What herdr already knows — the session it keeps alive, the
kind of agent, its lifecycle state, its native session ID — is taken as given;
only what herdr does not know is filled in: context usage, the pinned default
model, the Discord wiring (⌁), worktree workers, the remote card.

> **herdr only, written in Rust.** herdr knows what lives inside each pane —
> the kind of agent, its lifecycle, its session — so the tower never guesses at
> process names or reads the screen to find out.
>
> The state store is `~/.local/state/herdr-tower/`, the configuration
> `~/.config/herdr-tower/config.json`, the environment variables
> `HERDR_TOWER_STATE_DIR` and `HERDR_TOWER_CONFIG`, and the worktree folder
> inside a repository `.herdr-tower/worktrees/`. A config file that does not
> parse leaves every setting at its default; the supervisor's log and
> `herdr-tower doctor` say so, and name any key they do not know.

## Installing

```bash
# the latest release — macOS on Apple silicon, Linux amd64 and arm64
curl -fsSL https://raw.githubusercontent.com/orascope/herdr-tower/main/install.sh | bash
# or build from source (Rust 1.89+ — it uses the standard library's file locks)
git clone https://github.com/orascope/herdr-tower.git && cd herdr-tower
make install   # ~/.local/bin/herdr-tower
```

`install.sh` puts the binary in `~/.local/bin` (`HERDR_TOWER_BIN_DIR` to
change it), refuses an archive that does not match the release's
`checksums.txt`, and installs the resident supervisor (`daemon install`).
`HERDR_TOWER_VERSION=<x.y.z>` pins a release. There is no Homebrew tap.

It is also listed on [herdr's plugin marketplace](https://herdr.dev/plugins/):

```bash
herdr plugin install orascope/herdr-tower
```

The plugin's build step runs the same `install.sh` (or `install.ps1` on
Windows), and its startup step brings the dock back whenever a herdr server
starts. `herdr-tower init` is still yours to run.

The Linux binaries need glibc 2.28 or newer, so they run on RHEL / Rocky / Alma
8 (8.10 included) and 9, Ubuntu 20.04 and later, Debian 10 and later, and
Fedora; they are built in `manylinux_2_28` containers
(`scripts/build-linux.sh`). On Linux the tower, the hooks, worktrees and
Discord were verified on Ubuntu 24.04 in a VM. What differs on Linux: desktop
notifications only where `notify-send` exists.

### Windows

herdr runs natively on Windows, and so does the tower: the release has a
`windows_amd64.zip` (x86_64, Windows 10 or newer — it needs no DLL Windows
does not ship).

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/orascope/herdr-tower/main/install.ps1 | iex"
& $env:USERPROFILE\.local\bin\herdr-tower.exe init
```

`install.ps1` checks the zip against `checksums.txt`, puts the binary in
`~\.local\bin\herdr-tower.exe` and installs the resident supervisor.
What differs on Windows:

- **Files** live where they do elsewhere, under `%USERPROFILE%`: the store in
  `~\.local\state\herdr-tower`, the config in `~\.config\herdr-tower` — the
  same `~` Claude Code and Codex use for `~\.claude` and `~\.codex`. herdr's own
  socket is a named pipe under `%APPDATA%\herdr`.
- **The resident supervisor** starts at sign-in from the user's `Run` registry
  key (no administrator rights), with no window: `daemon install` writes
  `~\.config\herdr-tower\supervisor.cmd` and registers it. Nothing restarts it
  if it dies, as launchd and systemd do; the next tower to open starts it again.
- **The pane's shell is PowerShell**, so the dock and the usage tab are typed in
  PowerShell's syntax, and the hooks are written as `C:/Users/…/herdr-tower.exe`
  — forward slashes, no quotes — which Git Bash, `cmd` and PowerShell all run.
- **`wt test`** runs a task's test command through `cmd /c` (`&&` chains as
  usual) rather than `sh -c`.
- Not on Windows: desktop notifications and the Discord bot wiring (it reads
  launchd and systemd files).

[herdr](https://herdr.dev) 0.9.0 or newer is required — the running *server*,
not just the CLI (`herdr plugin link` names the version it is talking to). Older
servers are not supported. The tower runs inside a herdr pane and reads the
agents over herdr's socket API. Without herdr the list looks empty.

Set it up once (this also installs the one per-user resident supervisor):

```bash
herdr-tower init            # hooks for claude, codex and gemini, the opencode plugin, the `/orchestration` skill (existing settings kept, a backup made)
herdr-tower init --dry-run  # see what it would change first
```

Then add the [recommended herdr settings](#recommended-herdr-settings) —
herdr-tower never edits herdr's config file itself — and install the
[recommended tools](#recommended-tools), lazygit above all.


## Quick start

```bash
herdr-tower init             # hooks for claude, codex, gemini and opencode, and the `/orchestration` skill
herdr-tower daemon install   # the one resident supervisor
```

Then click `🗼 HERDR-TOWER 🗼` in herdr's sidebar. `j`/`k` move, `enter` jumps
to the agent, `/` filters the list, `u` shows the usage board, and the key
guide at the bottom lists the rest. `herdr-tower doctor` checks that
everything `init` wired up still works.

## Recommended herdr settings

All of it goes in `~/.config/herdr/config.toml`, and `herdr server
reload-config` applies it without a restart. Use the path `which herdr-tower`
prints.

```toml
[ui]
# Strongly recommended: a lone pane keeps its frame, and so its name.
pane_borders = "always"
# Who is waiting, from any workspace: 🗼 2 wait · 1 done · 3 working.
tab_bar_right = [
  { type = "command", command = "~/.local/bin/herdr-tower status --line", interval_seconds = 5, timeout_seconds = 2 },
]

[experimental]
# Off by default. While the prefix (Ctrl+B) is held, herdr briefly switches the
# macOS input source to ASCII, so prefix commands work while typing Korean,
# Japanese or Chinese.
switch_ascii_input_source_in_prefix = true

# Return to the previously focused pane, even across tabs and workspaces:
# Ctrl+B, then ` (press again to switch back).
[keys]
last_pane = "prefix+backtick"

# The tower as a popup on prefix+a, or kept open in a pane on prefix+shift+a.
[[keys.command]]
key = "prefix+a"
type = "popup"
command = "herdr-tower"
width = "90%"
height = "85%"

[[keys.command]]
key = "prefix+shift+a"
type = "pane"
command = "herdr-tower"

# Context use, the model, Discord wiring, branch and idle loop under each claude
# agent in herdr's sidebar.
[ui.sidebar.agents.rows_by_agent]
claude = [
  ["state_icon", "workspace", "tab"],
  ["agent"],
  [{ token = "$ctx" }, { token = "$model" }],
  [{ token = "$wired" }, { token = "$branch" }, { token = "$loop" }],
]

# The current workspace stands out in herdr's sidebar. The default theme's row
# colour is almost the terminal background.
[theme.custom]
active_row_bg = "#45475a"   # the workspace you are on
selection_bg = "#585b70"    # the cursor row while choosing with the keyboard (Navigate mode)
```

Why each one, and the dock's colour in the sidebar:
[Keeping pane names visible](docs/herdr.md#keeping-pane-names-visible-strongly-recommended) ·
[Returning to the previous pane](docs/herdr.md#returning-to-the-previous-pane) ·
[Who is waiting, in herdr's tab bar](docs/herdr.md#who-is-waiting-in-herdrs-tab-bar) ·
[Opening the tower from herdr](docs/herdr.md#opening-the-tower-from-herdr) ·
[Putting ctx and the model in herdr's sidebar](docs/herdr.md#putting-ctx-and-the-model-in-herdrs-sidebar).

## Recommended tools

```bash
# Strongly recommended: `g` on an agent opens lazygit in its folder — every git
# gesture (diff, stage, commit, branch, push) without leaving the tower.
brew install lazygit        # Linux: https://github.com/jesseduffield/lazygit#installation
# Recommended: the open PR of each agent's branch, as #52 beside ⎇ on its row.
brew install gh && gh auth login
```

Without lazygit, `g` says so and does nothing else. Without `gh`, or logged
out, rows carry no PR number — but a branch named after its issue
(`fix/2955-bundle-setting`) still shows `#2955`, read from the name alone.

## What it does

- **One screen for every agent** — who is working, who waits on you, who has
  finished something you have not read, with context use, the model and the
  task. → [Using the tower](docs/using.md)
- **A resident supervisor** — one process that tracks the agents, sends
  scheduled messages, keeps idle loops going and serves everything below.
  → [The supervisor](docs/supervisor.md)
- **Plan usage and a guard** — claude's and codex's five-hour and seven-day
  windows, and a floor below which the tower stops spending them.
  → [Plan usage](docs/usage.md)
- **Notifications and Discord** — desktop notices and a status card.
  → [Notifications](docs/notifications.md)
- **Driving it from a phone** — a remote MCP server for the Claude app.
  → [Remote MCP](docs/remote-mcp.md)
- **Worktree workers** — parallel agents in git worktrees, driven to a
  verdict; the `/orchestration` skill teaches a coordinator to run them.
  → [Worktree parallel mode](docs/worktree.md)
- **A dispatch board** — an advisor hands worktree workers their items with
  `herdr-tower board send`; a pane beside it shows each worker's progress,
  struck through once reviewed. → [The dispatch board](docs/dispatch-board.md)
- **Parallax Browser handoffs** — its agents asking for you, shown beside
  yours, read-only. → [Parallax Browser handoffs](docs/parallax.md)
- **On top of herdr** — the dock, the popup, the sidebar, several servers.
  → [On top of herdr](docs/herdr.md)

Building and testing it: [Development](docs/development.md). What each release
changed: [history.md](history.md).

## Safety rules

- Never kill an existing workspace, tab or pane
- Never change an existing prefix or key binding — `~/.config/herdr/config.toml`
  is not touched, only explained (`prefix+a` is opt-in)
- Back up `settings.json` before editing it (`settings.json.herdr-tower.bak`)
- Never write a credential into the state files (`~/.local/state/herdr-tower/`)
- Agent processes do not survive a Mac sleeping or rebooting —
  `herdr-tower restore` reassembles the arrangement (herdr restores the
  workspaces and tab layout itself, so only the tabs are added)

## Credits

herdr-tower started from [agentlayer](https://github.com/netwaif/agentlayer)
by netwaif (MIT): the hook-driven state model, the Discord card and worktree
handling came from there. herdr-tower has since been rewritten in Rust for
herdr and grown well past that start — the remote MCP server, the plan usage
guard, scheduled messages and more. agentlayer's notice is
kept in [`NOTICE`](NOTICE).

## License

Apache License 2.0 — [`LICENSE`](LICENSE). Copyright 2026 ORASCOPE Co.,Ltd.;
redistributions must carry the [`NOTICE`](NOTICE) file.
