# Parallax Browser handoffs

[← README](../README.md)

Parallax Browser is a local
browser with agents of its own. When one of them stops and needs a person — an
approval, a question, a blocked step — Parallax records a **handoff**. The
tower's job is to be the one place that shows who is waiting on you, so those
handoffs are shown beside the agents in herdr.

It is **read-only**. The tower does not answer, resolve or drive anything in
Parallax, does not proxy it, and does not register Parallax's MCP anywhere. You
answer a handoff in Parallax.

## What is shown

- **The tab bar** (`herdr-tower status --line`): `N parallax` right after wait —
  `🗼 2 wait · 1 parallax · 3 working`. It shows even with no agent running
- **The tower**: `parallax N` in the header, in wait's colours, and one line
  under it naming the most urgent: `◆ parallax  Research · needs you · Pick a
  plan  +1 more`
- **`herdr-tower status`**: a short section after the table, one row per
  handoff (workspace, status, title) — only when there are any
- **A notification** the first time a handoff asks for you: a desktop notice,
  and a line in the notification channel if `notify_discord` is on (mentions
  switched off, as for every notice). Once per handoff — the IDs already
  announced are kept in the state directory, so restarting the supervisor does
  not repeat them

Only handoffs that ask for you count: Parallax's `action` and `urgent` levels
(`needs_human`, `approval_expired`, `blocked`, `waiting_approval`). Finished work
to look at (`ready_to_resume`, `completed_review`) is `review`, recorded but not
counted as waiting, and resolved ones are not recorded at all.

A handoff Parallax *redacts* — one whose content the reader is not entitled
to — has no title here; it is shown by its status alone.

## How it reads

The resident supervisor asks Parallax's local API about every ten seconds, on a
thread of its own so a slow Parallax cannot hold up the idle loops:

- `GET /workspaces`, then `GET /handoffs?openOnly=1&workspaceId=<id>` for each
  workspace, with the bearer token in `api-token`. Never an unnamed
  `GET /handoffs` (Parallax refuses it by design) and never Parallax's own
  `handoffs.json` — both would step around Parallax's entitlement model
- `127.0.0.1` only, the port from `api-port` (8765 when missing), a 1.5 s
  timeout per request, no redirects, no proxy. The token is read fresh every
  poll and is never logged or written anywhere
- The data root is `$PARALLAX_DATA_ROOT`, else `~/.parallax`. Not installed,
  not running, no token: nothing to show, silently
- What it read goes to `parallax.json` in the state directory (0600, written
  whole). Every surface above reads that file and never the API, so the tab
  bar stays cheap. A snapshot older than 60 seconds is not shown
- A change between answering and not answering goes into the supervisor log
  once, not on every poll

## The setting

```bash
herdr-tower config set parallax false   # stop reading Parallax (default: true)
herdr-tower config unset parallax       # back to the default
```

Taken on the supervisor's next poll; turning it off also removes
`parallax.json`, so nothing lingers on screen.

`herdr-tower doctor` has a `parallax` section: off, not installed, installed
but not answering, or answering with the number of workspaces read. None of
these is a problem for the tower — Parallax is closed as often as it is open.

## Agents and Parallax

An agent that needs a browser uses Parallax Browser: connect it to the agent
in Parallax Settings → Connected Agents. `herdr-tower init` does not set that
up.
