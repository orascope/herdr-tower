# On top of herdr

[← README](../README.md)

### Keeping pane names visible (strongly recommended)

**Always set this option.** herdr's default `pane_borders = "auto"` frames panes only
while a tab is split, and a pane name lives in that frame. Close the other pane in a
two-pane tab and the survivor maximizes with no frame, so its name vanishes from the
screen. The name itself is kept — `herdr pane get <id>` still returns it — but you can
no longer tell panes apart at a glance
([herdr#1939](https://github.com/herdrdev/herdr/issues/1939), closed without a fix).
`"always"` frames a lone pane too — `pane_borders = "always"`, which needs
`pane_outer_borders = true` (the default). The block to paste is in
[Recommended herdr settings](../README.md#recommended-herdr-settings).

Apply it with `herdr server reload-config`; no restart needed. If a name still changes
to something like `• claude` while an agent works, that is a different herdr bug
([herdr#3296](https://github.com/herdrdev/herdr/issues/3296)), not the frame.

### Pane IDs in their names

herdr-tower puts each pane's herdr ID in front of its name, so the coordinate
every `herdr` and `herdr-tower` command takes (`herdr pane run w2:p1 …`) is on
the pane's border: `[w2:p1] hero-dark-codex`, or just `[w2:p1]` for a pane with
no name. **Workspace names carry `[wX]`**, such as `[w2]project`, in
herdr's sidebar. The **`🗼 HERDR-TOWER 🗼` dock workspace and its tab are exceptions**:
they always keep their plain names, and any coordinate prefix is removed.
There is no separator space after `[wX]`; old spaced prefixes are migrated.
Otherwise, only the **first tab in each workspace** carries that same
workspace prefix; all later tabs show their plain names. First means tab order,
not ID order or the focused tab. After renaming, moving or closing tabs, the
resident supervisor restores this within about ten seconds. Repeated and stale
coordinate prefixes are replaced; ordinary brackets such as `[wip]` are kept.

This uses the existing ten-second reconcile, with one additional workspace-list
read per server. The planner visits each workspace and tab once and only renames
labels that differ. An unchanged pass sends no rename requests.

The panes the tower makes — a `wt new` worker, `open` and the MCP `open_agent`,
`resume`, `restore` — are named that way as soon as herdr has made them (herdr
only hands out the ID then, so it is a rename right after; if it fails, the
launch goes on under the plain name). Every other pane, the ones you made
included, is put right by the resident supervisor within about ten seconds: a
missing prefix is added, a stale one (a pane moved to another tab) is replaced,
a name you typed stays behind it — `[w2:p1] [wip] fix` — and one that is already
right is never written again.

- **The pane name is only drawn in the pane's border**, so `pane_borders =
  "always"` above is what makes it visible on a lone pane.
- **An unnamed pane is named `[w2:p1]`.** herdr draws a manual name first, so if
  you turn on `show_agent_labels_on_pane_borders` the ID stands where the agent's
  label would be. The tower's own list is not affected: a name that is only an ID
  counts as no name there, and the TITLE column still shows the live title.
- **The tower's TITLE column leaves the prefix out** — the PANE column already
  carries the coordinate.
- **Dock and usage panes stay plain.** Their workspace/first-tab display prefixes
  are removed when Tower compares semantic names, so restart recovery and
  creation requests continue to match. Sidebar color rules should use `contains`
  for a workspace name rather than `equals`, because the drawn name includes its ID.

To turn off **pane** prefixes:

```bash
herdr-tower config set id_labels false   # default: true
```

Existing pane prefixes remain until renamed. Workspace and first-tab prefixes
continue to be reconciled regardless of this setting.

### Returning to the previous pane

Herdr's built-in `last_pane` action switches to the pane you were viewing
before the current one, including when the two panes are in different tabs or
workspaces. Bound to `prefix+backtick` (Ctrl+B, then `; press again to switch
back) in [Recommended herdr settings](../README.md#recommended-herdr-settings).

Apply it with `herdr server reload-config`. This switches Herdr panes; a popup
remains a modal and is closed with its own exit key.

### Opening the tower from herdr

herdr offers popups as a key command out of the box — no plugin needed. The two
keys are in [Recommended herdr settings](../README.md#recommended-herdr-settings): `prefix+a` opens the
tower as a popup, which is session-modal and does not disturb the tab layout,
and `prefix+shift+a` opens it in a pane, for keeping it open.

Apply it with `herdr server reload-config`. Run `herdr-tower init` inside herdr
and it prints the `prefix+a` block for you, without touching the file.

### Opening the tower with the mouse

herdr runs custom commands — popups included — only from a key binding. A
workspace, though, is one click in the sidebar, so the tower can live in one:

```bash
herdr-tower dock          # --focus to jump there too
```

It creates a workspace labeled `🗼 HERDR-TOWER 🗼` at the top of the sidebar and
starts the tower in it. Running it again is safe: a tower that is already running
is left alone, and one that is gone — you quit it, or the herdr server restarted
— is started again in the shell it left behind.

To restore the dock automatically whenever the Herdr server starts, install and
link its startup plugin once. This works for release and source installs
alike:

```bash
herdr-tower dock --install-startup
```

The binary carries the matching plugin and installs it at
`~/.local/share/herdr-tower/plugins/dock-autostart` (or under
`$XDG_DATA_HOME`) and links that stable path into Herdr. Herdr runs the hook
after restoring the session and bringing up its API socket, so the idempotent
`dock` command can reuse an existing tower or recreate it. The hook keeps
retrying for up to five minutes while restored shells finish starting.
One case is left as is: a restored dock shell that takes longer than five
seconds to reach its prompt (a slow `.zshrc`) gets a second tab built beside
it — close the extra one by hand. Closing it automatically would mean the
supervisor closing tabs on its own, which is the riskier mistake.

The dock stays first, keeps its name, and cannot be closed by accident. herdr has
no pinned workspaces, so the tower running there moves its workspace back to the
top within a second of anything moving it, and puts `🗼 HERDR-TOWER 🗼` back on its
tab the same way — herdr would otherwise draw that tab as its number, or as
whatever command is running in it. Right-clicks inside the tower's pane go to the
tower rather than to herdr's context menu, so the *Close pane* entry is not
reachable there. `dock` sets both on every run; the tower keeps them while it is
up. That menu is only the pane's. herdr draws its own menus on the tab title and on
the sidebar row, each with a *Close*, and no client setting removes an entry
from them — so the dock is not defended there, it is reopened. The resident
supervisor notices within a couple of seconds that the dock workspace is gone,
or that nothing is running in it, and runs `dock` again; a tab closed by
accident comes back with its name, its position and its tower. Turn that off
with `dock_keep` if you would rather close it and have it stay closed. In
`~/.config/herdr-tower/config.json`:

```json
{ "dock_keep": false }
```

(That file is strict JSON — a `//` comment in it fails to parse, and a config
that fails to parse is replaced by the defaults, which turn this back on.)

`herdr pane input --pane <id> --right-click herdr` gives the pane's own menu
back.

To draw its name in a colour of its own, style the `workspace` token in herdr's
sidebar. Match the name with `contains` so the workspace ID prefix also matches:

```toml
# ~/.config/herdr/config.toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", { token = "workspace", rules = [{ contains = "🗼 HERDR-TOWER 🗼", fg = "#f04747", bold = true }] }],
  ["branch", "git_status"],
]
```

The text match is case-sensitive, so a rule written for an earlier spelling of the name
(`🗼 herdr-tower 🗼`, before it was capitalized) stops matching after an
upgrade: change it to the name above.

A tower in a pane of its own stays open when `enter` jumps to an agent; come
back by clicking the workspace. In a popup it still closes, as before.

The list answers the mouse too: a click selects a row (and its preview), a double
click jumps there as `enter` does, and the wheel moves the selection.

### Putting ctx and the model in herdr's sidebar

herdr knows the kind of agent and its state, but not the context usage, the model
or the Discord wiring (⌁). The tower computes those and hands them to herdr's
sidebar — automatically on every card refresh, with a fifteen-minute TTL so a
stopped tower does not leave stale values behind.

To see them, add rows to herdr's configuration — the `rows_by_agent` block for
`claude` in [Recommended herdr settings](../README.md#recommended-herdr-settings).

The ordinary tokens are written under `herdr-tower`'s own source name, so a
token planted by herdr or another tool (`$pr`, for instance) is left alone.
`$loop` uses the separate `herdr-tower-loop` source and reads like
`↻ 17 · 142h` (or `↻ paused`).

### Who is waiting, in herdr's tab bar

The sidebar shows each agent's state; `herdr-tower status --line` sums them up
in one line — `🗼 2 wait · 1 done · 3 working`, most urgent first, zero counts
left out — so a waiting agent is visible from any workspace, with the sidebar
collapsed. herdr runs it on a timer at the right edge of the tab row — the
`tab_bar_right` entry in [Recommended herdr settings](../README.md#recommended-herdr-settings).

Use the path `which herdr-tower` prints. The command only reads the records —
no herdr call — so a five-second interval costs next to nothing; the resident
supervisor keeps the records current. With every agent idle it prints
`🗼 idle`, and with none running it prints nothing, which herdr shows as no
entry at all.

### Watching several herdr servers at once

The resident supervisor manages the default Herdr socket and all standard named
session sockets in one process; it does not start one daemon per repository or
one daemon per Herdr server. See [The resident supervisor
daemon](supervisor.md#the-resident-supervisor-daemon) for discovery, namespacing, routing and
failure behavior.
