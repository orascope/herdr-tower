# The resident supervisor, scheduled messages and idle loops

[← README](../README.md)

## Scheduled messages

A message can be sent at a set time — once, every day, or every weekday — to
one session, a workspace, or every folder that keeps a session handoff (the
ones `wake all` and `close all` talk to). The resident supervisor delivers it,
so it goes whether or not a tower is open; making a schedule starts the
supervisor if it is not running.

```bash
herdr-tower schedule close 19:00 --weekdays   # the close-all message to every handoff session, Mon–Fri evenings
herdr-tower schedule wake 09:00 --weekdays    # the wake-all message every weekday morning
herdr-tower schedule add +30m --session c376295e "run the tests"   # once, to one conversation
herdr-tower schedule add 08:30 --daily --workspace dbstore "start with yesterday's failing tests"
herdr-tower schedule                          # what is scheduled, and when each goes next
herdr-tower schedule rm t3f2a91 | all
```

A time is `HH:MM` (a one-off goes at the next one), `YYYY-MM-DDTHH:MM`, or
`+30m` / `+2h` (the Korean minute and hour units work too), up to a year. A
session is named by its whole session ID or the eight characters in the SID
column.

**It waits for the agent to be idle.** At the time, each target that is idle
(finished or never started, and not asking anything) gets the message; one
that is still working gets it the moment it comes to rest. After 30 minutes
(`--wait` changes it) the ones still busy are skipped for that day, and
`supervisor.log` says so. A time the supervisor was not running for — the
machine asleep — is noted there too, not sent hours late. `--until DATE` ends a
repeating schedule.

Each target gets an occurrence **at most once**. A pane counts as served even
if its conversation changes inside the window (`/clear`), and one pane gets
one message at a time: two schedules due together, or an idle loop about to
fire, wait their turn rather than landing on top of each other. A scheduled
close message stops that agent's idle loop, as `close all` does. If
`schedules.json` gets damaged, `schedule rm all` empties it.

In the tower, **`T`** schedules a message for the selected session: type
`19:00 wrap up for today` or `+30m run the tests`, `tab` switches between once,
daily and weekdays, `enter` keeps it. `T` on a session that already has one
cancels it — still from the same row after a `/clear`. Like `L`, it needs the
agent's session ID, so a pane that has not reported one yet is refused. A
scheduled row shows `⏰ 19:00`, and the header counts every schedule with the
next time one goes.

## The resident supervisor daemon

Idle continuation no longer runs inside a tower popup. There is **one resident
supervisor per OS user**, no matter how many tower popups, Herdr servers,
repositories or sessions are open. Closing every popup does not stop it, and
opening several towers does not create additional supervisors. The process is:

```text
default Herdr socket ─┐
named Herdr sockets ──┤
idle-loop leases ─────┼─> one resident supervisor ─> atomic state + daemon snapshot ─> tower popup(s)
scheduled messages ───┘                └────────────> delivery through each pane's owning socket
```

`herdr-tower init`, `make install`, and the release installer install and start
the service. Its user-visible name is **herdr-tower resident supervisor**; the
platform registration and process names are:

| platform | registered service | process command |
|---|---|---|
| macOS | `~/Library/LaunchAgents/dev.herdr.tower.plist` (`dev.herdr.tower`) | `herdr-tower daemon run` |
| Linux | `~/.config/systemd/user/herdr-tower.service` | `herdr-tower daemon run` |

Both service managers keep the same process alive and start it at login. A
manual `herdr-tower daemon start` is idempotent: it leaves a healthy supervisor
alone, but restarts a loaded service whose heartbeat is stale. Stale is not
dead: a supervisor whose process is alive is left alone for five minutes after
its last heartbeat, since one pass on a loaded machine can run long, and only
one silent for longer is taken for hung and restarted. Use
`herdr-tower daemon status` to see its PID, running herdr-tower version,
Herdr connection state, number of managed backends, and last snapshot time.
`daemon run` is the service entry point, not a command normally run by hand.

The snapshot is only the latest pass. What happened before it is in
`~/.local/state/herdr-tower/supervisor.log`, one timestamped line per event:
the supervisor starting and why it stopped, a Herdr server connecting, failing
or disappearing (once per change, not once per poll), each idle-loop message
sent or failed, a loop stopping and why, each scheduled message sent, missed or
skipped, and the dock being brought back. A
pass that cannot read or write what it needs is logged once and tried again
every two seconds rather than ending the supervisor, and a supervisor that
stopped without saying why — restarted, signalled or killed — is noted by the
next one. It rolls over to `supervisor.log.1` past 1 MiB. `daemon status`
prints its path.

An upgrade needs no second `init` for the plugin files the tower owns whole.
At start the supervisor rewrites the opencode plugin
(`~/.config/opencode/plugins/herdr-tower.js`) and the dock's startup plugin
when a copy is installed, names this binary, and came from an earlier build,
and says so in `supervisor.log`. Nothing is installed that was not there, a
copy naming another binary is left alone, and files that hold your own
settings as well — `~/.claude/settings.json`, `~/.codex/hooks.json` — are
still only changed by `init`; `doctor` says when they need it.

Nothing the tower writes grows without a bound. At start and then hourly the
supervisor sweeps what otherwise would: tower lock files nobody has held for a
week, and loop instructions remembered for conversations that are gone.
`daemon.log`, where launchd sends a crash's output, is cut to its last 64 KiB
once it passes 1 MiB. Each sweep that removes something says so in
`supervisor.log`.

Every two seconds the supervisor discovers and polls the default
`~/.config/herdr/herdr.sock` plus every standard named server at
`~/.config/herdr/sessions/*/herdr.sock`. It owns reconciliation, loop scheduling,
scheduled messages and delivery for all of them. Each additional backend receives a stable
namespace, so identical pane IDs on different servers cannot collide; focus,
preview, attach, broadcast, metadata and loop messages are always routed to the
socket that owns the pane. The historical IDs of the default server do not
change.

The tower is now a client of the supervisor. It reads the atomic
`~/.local/state/herdr-tower/daemon.json` snapshot and the shared state instead
of starting its own independent watcher. Its header shows the daemon's actual
herdr-tower version and, when more than one server is managed, the backend
count. A disconnected backend is reported in the snapshot without preventing
the other backends from being supervised. Its loop leases remain durable, but
the outage neither sends a message nor consumes a transmission attempt; expiry
and an explicit transmission limit still apply.

## Idle continuation loops

An idle loop is an explicit, durable opt-in for one agent session; it is never
enabled globally. In the tower, select a session and press `L`. The first press
opens a one-line editor at the bottom, like the broadcast composer. It starts
with this safe default:

```text
Proceed with the next approved task and its tests. When complete, run herdr-tower loop done; if blocked, run herdr-tower loop blocked.
```

Edit the instruction, use `Tab` / `Shift+Tab` to choose `5m`, `10m`, `30m`, or
`60m`, and press `Enter` to arm it (`Esc` cancels). The defaults are a `10m`
idle interval, a `168h` maximum lifetime, and unlimited transmissions. Pressing
`L` on an active loop turns that loop off immediately. The last instruction is
remembered by backend and native agent session ID even after it is switched off,
so pressing `L` again restores the previous text instead of the default. The
durable leases and remembered messages live in `loops.json` and
`loop-messages.json` under the state directory.

The selected session's configured instruction is also drawn inside its pane
preview, whose light-grey background separates it from the session list. Loops
have a colour of their own — rose, dimmed while paused — rather than borrowing
one that already means a state: the badge on a row used to wear the accent that
also means `done`, and the preview line the yellow that means `wait`, so an
agent with a loop attached read as an agent in a state it was not in. Press `B` to open the same kind of bottom-line editor for a one-time
broadcast; its message is routed to every eligible session across all managed
backends.

The interval means **continuous settled time**, not a periodic timer that sends
regardless of activity:

| observed session state | supervisor action |
|---|---|
| `working` | send nothing; clear the idle clock and wait for the next settled transition |
| `idle` or `done` | start/restart the full selected interval, then reserve and send one instruction |
| still idle after that send | allow one retry after the same interval, then pause after two attempts |
| `wait`, `err`, or missing | pause rather than writing into a session that needs a person or recovery |
| dead | pause, and keep it paused — see below |
| native session identity changed | stop the old lease so it cannot target a replacement session |
| owning Herdr backend disconnected | retain the lease without sending or consuming an attempt |
| a turn begun by a background-task notification (claude's `<task-notification>`) | leave the lease exactly as it was — clock, attempts, pause — and send nothing into it; when it settles, the clock keeps running from before it |

**Background notifications do not reset the clock.** An agent watching its
workers with background commands is woken by each one finishing, and those
turns used to count as activity: a loop set to 30 minutes never fired for hours
while the agent only reacted to its watchers and never advanced its plan. Now
only a turn from a person, the tower or an unknown origin restarts the clock.
`loop done` and `loop blocked` still land during a background turn, a
background turn does not lift a pause, and an ordinary turn hidden behind one
(both between two looks) is still seen, by the record's `ordinary_seq`.

**A pause lifts itself when the agent moves again.** What counts as movement
depends on what the pause was: one that merely describes a state (`wait`, `err`,
a missing record) is answered by seeing the agent in any other state, while one
that is not an observation — the attempt cap, a failed delivery — waits for a
turn actually to begin. The distinction matters because opening a finished row
marks it read, which looks like a transition but is not work; the supervisor
counts turns rather than timestamps so that a glance cannot resume a loop.

An explicit `herdr-tower loop blocked` is the agent's own statement that a
person is needed; only a later turn or `herdr-tower loop start` lifts it. A dead agent pauses the lease
too, and herdr recycles pane coordinates, so what comes back decides the rest.
The same session resumed in the same pane (`claude --resume <id>`) is the same
agent and the loop goes on. A different session — or any arrival on a lease
that was armed before its session was known — is not provably that agent, and
the lease is removed (`agent session changed` / `agent replaced after it died`).

A new send cycle becomes eligible only after actual `working` is observed and the
session settles again. Before each delivery the supervisor durably reserves the
attempt, so a crash may conservatively miss a send but cannot duplicate it after
restart. A delivery whose answer from herdr is lost after the text went out —
on a loaded machine, a reply that misses its deadline — may have landed, so it
counts as sent, for loops and scheduled messages alike: the loop's own retry
covers one that did not, while sending it again would type it twice. Only a
delivery herdr refused, or one that never reached it, pauses the loop or is
retried. The agent can end its own lease with `herdr-tower loop done`, or pause
it for human input with `herdr-tower loop blocked`. The lifetime and an optional
`--max-turns` limit stop it as well. Thus “unlimited” means no lifetime send-count
cap; it does not mean repeatedly flooding one unchanged idle prompt.

Override the defaults when arming a lease:

```bash
herdr-tower loop start my-session --idle-for 20m --ttl 24h --max-turns 8
herdr-tower loop start my-session --instruction "Stay within issue #123."
```
