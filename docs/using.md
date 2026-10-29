# Using the tower

[← README](../README.md)

```bash
herdr-tower            # the TUI tower (L edits/starts or stops the selected session's idle loop, T schedules a message for it)
herdr-tower status     # a plain table — for SSH and scripts
herdr-tower status --json
herdr-tower report     # what each session did in the last 12h: turns, work, time waiting on you (and on what), errors, wt drive verdicts  [--since 30m|12h|2d] [--json]
herdr-tower config     # read or change config.json: get [key] [--reveal] · set <key> <value> · unset <key> · keys · path (webhooks stay hidden unless --reveal)
herdr-tower card       # upsert the Discord status card now (the supervisor does it every 5 minutes) / --out prints the JSON only
herdr-tower open ~/proj            # a new tab in that folder with claude started in it — no worktree, no branch  [--agent codex] [--workspace label (created when missing)] [--prompt "hi"] [--json]
herdr-tower close <agent>          # close its pane (asks first) — refuses one that is working or waiting on you unless --force  [--yes]
herdr-tower interrupt <agent>      # stop a working agent's turn: presses Escape, and only in a working one
herdr-tower workspaces             # herdr's workspaces with tab / pane / agent counts — the empty ones too  [--json]
herdr-tower resume     # list dead claude conversations / resume <id> to rescue one (a record id, or a session id — whole, or the SID column's eight characters)
herdr-tower restore    # bring dead sessions back after a reboot — pick from a checklist and press enter (space toggles, a all, q cancels)
herdr-tower restore --resume       # revive the conversations too (same checklist) / --yes takes the lot / --dry-run plans only
herdr-tower restore <id> ...       # restore by ID. Bots started at login (LaunchAgent, or a systemd unit on Linux) and places that already hold a pane are left out — naming a bot's ID forces it
herdr-tower loop start <session|id> # opt in: continue after 10m idle, for at most 168h; transmissions are unlimited
herdr-tower loop list               # active and paused leases, send count, and remaining lifetime
herdr-tower loop stop <session|id>  # stop one lease (`loop stop --all` stops every lease)
herdr-tower loop done               # current session reports completion and ends its lease
herdr-tower loop blocked            # current session pauses its lease for user input
herdr-tower board add "parser" "tests"   # plan items on this repository's dispatch board (see docs/dispatch-board.md)
herdr-tower board send 1 <wt-task>       # deliver item 1 to that wt worker (or a pane id: w2:pBR); after a successful send the board pane is opened beside yours (best effort)  [--again] [--replace]
herdr-tower board send --new "bench" <wt-task>   # add an item and deliver it in one go
herdr-tower board assign 3 w2:pBR       # queue item 3 for the agent in a pane (a long-lived worker); `board send 3` sends it  [--sent: you already sent it yourself]
herdr-tower board show                   # each item with its worker's live progress: working, review, wait, …  [--json]
herdr-tower board done 1                 # strike it through once you have reviewed the result
herdr-tower board drop 3 --reason "later"   # cancel one · board note 2 "…" · board open (reopen the pane)
herdr-tower board prune                  # remove finished items · board rm 4 [--force] · board clear --all
herdr-tower board next w2:pBR           # send the first item queued for that worker (board assign), highest priority first; refuses while it still holds one
herdr-tower board send 3 --file brief.md   # title plus a long instruction (--message "…" for a short one); next takes them too
herdr-tower board prio 3 1              # P1–P3 or - · board move 7 --to 1 · board link 3 #3231 (the row shows the PR's state)
herdr-tower board wake on               # from the advisor's pane: the supervisor tells it when an item is ready for review (off to stop)
herdr-tower board --repo <path> show    # another repository's board; the remote MCP server has board tools too
herdr-tower schedule                # what is scheduled, and when each goes next (below)
herdr-tower schedule close 19:00    # the close-all message to every handoff session each evening (--weekdays: Mon–Fri)
herdr-tower schedule add <time> <who> <message>  # once or --daily/--weekdays, to --session <id>, --workspace <name> or --handoff
herdr-tower schedule rm <id>|all    # cancel one, or every schedule
herdr-tower daemon status           # supervisor PID, version, Herdr connectivity and backend count
herdr-tower daemon start            # start it, or restart it when its heartbeat is stale
herdr-tower daemon install          # install and start the per-user launchd/systemd service
herdr-tower doctor                  # check config.json is understood, and every hook, the opencode plugin and the service still point at a real binary; exits non-zero on a problem
herdr-tower tree                   # who works under whom in the session list; tree link <worker> [<coordinator>] / tree unlink <worker>
herdr-tower wake-all   # tell every claude and codex session to carry on
herdr-tower close-all  # tell them to wrap up, then watch until all are done, then summarise
herdr-tower broadcast "<message>"  # send any message to all of them (--except leaves some out, --yes skips the prompt)
herdr-tower info <session>         # the wiring in detail: folder, engine, Discord channel, what started it, how to resume (a workspace, a record id, or a session id)
herdr-tower dock [--focus]         # run the tower in its own herdr workspace, so a click in the sidebar opens it
herdr-tower wt ...     # worktree parallel mode (below) — including `wt drive`, which carries tasks to a verdict
```

The tower and popup show only current agents. Dead records stay available for
24 hours through `resume` and `restore`, without filling the operational view.

Each row reads **STATE · AGENT · PANE · SID · TITLE · TASK · DIR·SINCE**. PANE
is the herdr pane coordinate (`w1:p2`). SID is eight characters of the CLI's
session ID — the head, or for codex's time-ordered IDs, whose head is a
timestamp shared by workers started together, the tail; a screen narrower than
140 columns drops it, and the detail card (`i`) shows the selected pane's.
A codex whose hooks never fire (codex runs only the hooks trusted through its
`/hooks`) still gets its SID within half a minute: the supervisor reads it from
the process's arguments (`codex resume <id>`) or else from the rollout codex is
writing for that folder, and leaves it empty when two such codex panes share a
folder and nothing tells them apart.
The preview heading shows the whole session ID — what `claude --resume` and
`codex resume` take — or the short one on a line too narrow for it;
its dark teal band starts right under the list and runs down to the key guide.
The preview folds like a machine's band: a click on its heading (`▼`) folds it
to that one line (`▲`, the way it opens) and captures nothing meanwhile, and a
click on the line opens it again at the height it had. A list too long to leave the preview any
rows shows it folded at the foot; a click opens it at half the screen, and a
drag pulls it up to any height. `]` and `[` make it taller and shorter — `[`
at its smallest folds it — and `=` goes back to the automatic split. The summary lines at the top (models, plan usage) sit on the same teal band, ending where the column names begin. TITLE reads `tab›title`: the herdr tab the
pane is in, then the name someone gave the pane, or else the pane's own
terminal title, which the CLI rewrites every turn — claude a summary of the
turn, codex the task and its branch; a `wt new` worker's generated name only
shows when there is neither. A long title is cut before the tab is.
The `[w2:p1] ` ID prefix herdr-tower puts on pane names (and any `[w2:t3] ` an
earlier version put on a tab) is left out here — PANE already says where the row is — and a pane named only
`[w2:p1]` counts as unnamed, so its live terminal title shows
([Tab and pane IDs in their names](herdr.md#pane-ids-in-their-names)).
TASK is what the pane was asked: the last prompt, or the question it is holding.
A cell reads `N/A` when there is nothing to show.

Workspaces follow Herdr's sidebar order; moving one updates the list on the next
supervisor refresh. Headings and previews include the sidebar's `[wX]` prefix,
and the tower dock keeps its plain name. Inside a workspace, the rows are
grouped by tab and ordered by pane (see [The order of the list](#the-order-of-the-list)).
A worker stays in its own tab, and `↳pEJ` at the head of its TITLE names
the coordinator it works under: the pane number for a coordinator in the
same workspace, the whole coordinate (`↳w2:pEJ`) for one in another. A link
never moves a row. The supervisor works the links out every few seconds from
three things, the last one winning:

1. the pane `wt new` was run from (kept on the task record);
2. a board's advisor over each worker it has an item with (`board send`/`assign`);
3. a link made by hand: `herdr-tower tree link <worker> [<coordinator>]`.

A branch never crosses a workspace. `tree link` to a coordinator in another
workspace moves the worker's pane there first, into a new tab, and links the
new pane id herdr gives it. `tree show` prints the tree as the supervisor sees
it; `tree unlink <worker>` takes a hand-made link away. herdr reuses pane ids,
so each relation holds only for the agents that were there when it was made.

`/` filters the list as you type: a row stays when its workspace, tab, title,
task, agent kind, folder or branch contains the text, ignoring case. `enter`
keeps the filter and closes the field, and the line above the key guide keeps
saying it (`filter / api  3 of 12`); `/` edits it again, and `esc` clears it —
in the field or on the list, before `esc` means leaving. The header's counts
stay those of every agent, and `W`, `C` and `B` still go to every session, not
only the rows the filter left.

## The order of the list

The list is ordered machine, then workspace, then pane. The machines are this
one first, then the saved machines in herdr's order. Within a machine,
workspaces follow herdr's sidebar and tabs their number. Within a tab, panes
follow herdr's own pane count (`w2:pJY`, `w2:pJZ`, `w2:pJ0`, `w2:pK2`, the
order herdr's `pane list` uses). This is the order of herdr's sidebar, so a
pane is in the same place in both. Neither an agent's state nor whom it works
under has a say: a row stays where it is when its agent starts or ends a turn
or gets linked. What is waiting on you shows in the header's counts and on its
`◆ wait` badge, and who works under whom in each worker's `↳` mark.

## Machines saved in herdr

Agents on the machines added with `herdr machine add` are part of the same
tree, under the local rows. Each machine is a root, `@local` first and then
`@gpu`, drawn as a blue band, and its workspaces hang off it. Each machine is
read with one `herdr --machine <label> api snapshot` every 10 seconds. A
machine that stops answering keeps its last read, dimmed, with `not reachable
· last read 2m ago` on its band. The `/` filter reaches remote rows too, and a
machine's name keeps all of its rows. A machine with no agents is still drawn
as its band, for its load and what it runs; with a filter on, a machine shows
only when something under it matches.

A pane whose title reads like an agent's (Claude Code's `✳`/`◑` glyph before
it, or a title that starts with `claude`, `codex` or `gemini`) but that herdr
there does not list as an agent gets a dim row under its workspace, after that
workspace's agents:

```
  └──── ? unseen           w3:p1            1›Claude Code                  not an agent to herdr — under sudo/su?
```

herdr tells an agent by what runs in front of the pane's own terminal, so an
agent started under `sudo su -` → `su - alice` is not one to it: `sudo` gives
it a terminal of its own, and `su -` clears the `HERDR_*` variables its hooks
report with. `under sudo/su?` is added when herdr could not read the folder of
what runs in front, which is what another user's process looks like. Start the
agent in the pane itself and it becomes an ordinary row within 10 seconds. An
unseen row is never selected, takes no keys and is not in the band's count.
Plain shells get no row.

Each machine's band says how many agents it has, how busy it is, with the
header's meters, and what it runs: the system and its version, the processor,
and the herdr-tower answering, `@local` included. The names and counts are
padded to the longest, so the meters line up down the screen:

```
 ▼ @local (21)    cpu ■■■■■  87%  mem ■■■■■  70% · macOS 27.0 arm64 (herdr-tower v0.57.4)
 ▶ @buildbox (3)  cpu ■■■■■   5%  mem ■■■■■  65% · macOS 26.1 arm64 (herdr-tower v0.57.4)
 ▼ @desk (6)      cpu ■■■■■  12%  mem ■■■■■  40% · Ubuntu 26.04 amd64 (herdr-tower v0.57.4)
```

With a `/` filter on, the count is of the agents the filter leaves. The system
is read from `/etc/os-release` on Linux, `SystemVersion.plist` on macOS and the
registry on Windows. A machine that stops answering keeps what it said last.

A saved machine's meters come
from that machine's own `herdr-tower host`, which runs in the same ssh call that
already reads its loops and schedules. That command prints counters, not a
share, so `cpu` is the busy share over the 10 seconds between two reads. It
first shows on the second read, about 10 seconds after the machine appears;
`mem` shows from the first. A machine whose herdr-tower predates `host`, or
that ssh cannot reach, shows no meters, and its loops come through as before;
one that predates the `sys` line `host` now prints shows its meters but not
what it runs. When the ssh call fails, the band says why, after the rest:
`herdr-tower not installed` when the machine has no herdr-tower, `ssh failed:
Permission denied (publickey)` and the like when ssh cannot get there, `no ssh
target` when herdr has none saved for it. Its agents are still listed, read
through herdr, but without load, loops or schedules.

A click on a machine's band folds its rows under it: the band stays, still
counting them, and another click opens them again. The mark
before the name says which a band is: `▼ @desk` is open, `▶ @desk` is folded,
and a band with no mark has nothing to fold. Folded
rows are skipped by `j`/`k` and the wheel. The click leaves the selection
where it is, and the band stays on the row you clicked it on, so neither
jumps; the next key or click lets the list follow the cursor again. Only a
selection that was on a row now folded moves, to the row after the band (the
row before, when nothing follows). `@local` folds too, but only while it has a
band, which it has only beside a saved machine. If that machine goes away, the
local rows open, and they fold again when it comes back. The header still
counts every agent. A band with nothing under it, such as a machine with no
agents or one that did not answer, does not fold. What is folded lasts as long as the tower does.

`j` runs on from the last local row into the machines, and `k` from the first
remote row comes back (as does `esc`). On a remote row the preview shows that
pane, read with `herdr --machine <label> pane read`. Only these keys act there;
the ones for a local agent (`enter`, `i`, `g`, …) just say so:

| key | what it does on a remote row |
|---|---|
| `L` | start the agent's idle loop, or stop it when `↻ 30m · …` is shown |
| `T` | schedule a message (`19:00 check the build`, `tab` for daily or weekdays), or cancel the row's schedules when `⏰ 19:00` is shown |
| `m` | a message for the agent, sent with `herdr --machine <label> agent prompt` |
| `x` | press Escape in a working agent, after a `[y/N]` |

A remote schedule is kept and sent by that machine, so the time is read in
**its** local time, and like a local one it needs the agent's session id (a row
showing `N/A` under SID cannot have one). The message reaches its shell as one
single-quoted word, so quotes, `$(…)` and backticks in it are sent as typed.

The loop, the schedule and the interrupt run **that machine's own herdr-tower** over ssh
(`herdr-tower loop start claude-w2:p6`), so the lease lives and is served by
its supervisor, not this one. That needs herdr-tower installed there and ssh
that works without a password (`BatchMode`); a machine without them still
lists its agents and shows the error when a key fails. Each action waits for
the command, up to 8 seconds, so the tower pauses that long on a machine that
does not answer. A loop started this way uses the defaults (10 minutes idle,
168 hours); the idle time and message are the machine's own to change.

The `L`, `T` and `/` fields open with a dim hint and the caret on its first
character: an example the first time (the default loop message,
`19:00 check the build and report back`, `codex`), and after that the last
value — the session's last loop message, the last line `T` armed, the last
filter used (kept in `tui-hints.json` in the state directory, so a restart
keeps them). The hint is not input: `→` or `End` takes it as text, typing
replaces it, and `enter` over the hint alone arms nothing (in `/`, it leaves
the list unfiltered). A filter already in force reopens as ordinary text.

The key guide at the bottom lists the keys used day to day. A few more work
without being listed there: `o` marks a finished session read without jumping
to it (`enter` does both).

The mouse works too: a click selects a row, a double click jumps like `enter`,
the wheel moves the selection, and a click on the key guide at the bottom
presses that key — the entry stays reversed while the button is held. In the
preview, a drag selects text (drawn reversed) and copies it to the clipboard
when the button comes up, and a click on the session ID in its heading copies
the whole ID — what `claude --resume` and `codex resume` take. On a
screen too narrow for the whole guide it wraps onto another line, so every key
stays on screen. A
confirmation (`[y/N]`) and a message being typed still take the keyboard, so a
stray click cannot answer or type — a click on the field itself only clears its
dim hint.


## The state model

```
● working  working (hook heartbeat); herdr's own word, as in its sidebar
? working  working, but no hook for a while — maybe stalled
◆ wait     waiting on you — input or approval
✔ done     finished and not looked at yet (stays until marked read)
✖ err      exited abnormally
· idle     idle
  dead     recovery record for a gone pane (hidden in the tower; cleaned up after 24h)
```

State is decided **without reading the screen**: only from the lifecycle herdr
reports and the agents' own official hooks. The `done → idle` transition happens
only through something a person does — the jump or read key — because not losing
track of "finished, unseen" is the reason this tool exists.


## What each agent signals

| agent | signal | states |
|---|---|---|
| Claude Code | hooks in `~/.claude/settings.json` (SessionStart, UserPromptSubmit, PostToolUse, Notification, Stop) | active, wait, done, idle |
| Codex | hooks in `~/.codex/hooks.json` (SessionStart, UserPromptSubmit, PostToolUse, PermissionRequest, Stop) plus the `notify` in `config.toml` | active, wait, done, idle |
| Gemini CLI | hooks in `~/.gemini/settings.json` (SessionStart, BeforeAgent, AfterTool, Notification, AfterAgent) | active, wait, done, idle |
| Gemini (agy) | hooks in `~/.gemini/config/hooks.json` (PreInvocation, PostToolUse, Stop) | active, done |
| opencode | the plugin `~/.config/opencode/plugins/herdr-tower.js` (chat.message, permission.asked, question.asked, their replies, session.idle, session.error except a user abort, and each finished step's token count); without it, herdr's own lifecycle status only — no task or question text | active, wait, done, err, idle |

One `herdr-tower init` sets all of it up at once — existing settings kept, backed
up, and idempotent: the resident supervisor, every hook above (the opencode plugin included), and the `orchestration`
skill. The herdr popup keys it only prints — that file is not its to edit.

**codex runs a hook only after you trust it.** The first time codex starts after
`init` — and again whenever the hooks change, an upgrade's `init` included — it
shows "hooks need review"; open `/hooks` in codex and press `t` to trust them.
Until then its hooks do not run, and codex rows carry herdr's lifecycle alone: no
task, no question. `herdr-tower doctor` says so when a codex session has never
heard a hook.

## Context and the model

Where an agent's CLI writes them down, its row carries the model, the context
share and the last activity: codex from its session rollouts, the stock Gemini
CLI from its session file, and the Antigravity CLI as an estimate from its
transcript (marked `~`). claude comes from its session transcript
(`~/.claude/projects/…/<session>.jsonl`): the model and prompt size of the last
turn. The transcript does not record the window, so the share is marked `~` —
200K assumed, 1M (`Opus 5.5 (1M)`) when the pinned default is a `[1m]` model of
the same family or the context has already passed 200K. opencode's plugin reports
the model, the prompt size and the model's window as each step finishes, so its
share is measured, not estimated.
