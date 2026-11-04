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
- **The tower**: `parallax N` in the header, in wait's colours
- **`herdr-tower status`**: `PARALLAX — N handoffs waiting on you` after the
  table, only when there are any
- **A notification** when more handoffs wait than at the last answer: a desktop
  notice, and a line in the notification channel if `notify_discord` is on
  (mentions switched off, as for every notice). Restarting the supervisor does
  not repeat it. One answered and a new one asked between two polls leave the
  number the same, so that one is not announced

The tower shows a number and nothing else: what a handoff says stays in Parallax.
Only handoffs that ask for you count: Parallax's `urgent` and `action` levels
(`blocked`, `waiting_approval`, `needs_human`, `approval_expired`). Finished work
to look at (`ready_to_resume`, `completed_review`) is `review`, recorded but not
counted as waiting.

## How it reads

Needs Parallax v0.26.2 or later. There is nothing to set up: the first read makes
one card, `herdr-tower`, in Parallax's Settings → Connected Agents, where you can
pause or remove it.

The resident supervisor asks Parallax's local API about every ten seconds, on a
thread of its own so a slow Parallax cannot hold up the idle loops:

- It connects the way Parallax documents for a client of the same OS user
  (`POST /pairing/local/challenge`, then `/pairing/local/connect`): each side
  proves it holds the owner-only key in `local-connect/key` without sending it,
  and Parallax's proof is checked first, so a listener without the key gets
  nothing. On macOS and Linux a key file or folder other accounts can reach is
  refused. On Windows the tower relies on the owner-only DACL Parallax sets and
  does not read it back
- `GET /handoffs/attention` with the token, which answers `{urgent,action,review}`
  and nothing about any handoff
- The token stays in memory and is dropped on any answer but a good one, so the
  next poll proves the listener again. It is never logged or written anywhere
- `127.0.0.1` only, the port from `api-port` (8765 when missing), a 1.5 s timeout
  per request, no redirects, no proxy
- The data root is `$PARALLAX_DATA_ROOT`, else `~/.parallax`, or on Windows
  `%APPDATA%\Parallax Browser`. Not installed or not running: nothing to show,
  silently
- What it read goes to `parallax.json` in the state directory (0600, written
  whole). Every surface above reads that file and never the API, so the tab bar
  stays cheap. A snapshot older than 60 seconds is not shown
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
but not readable (with the reason), or answering with the counts. None of these
is a problem for the tower — Parallax is closed as often as it is open.

## Agents and Parallax

An agent that needs a browser uses Parallax Browser: connect it to the agent
in Parallax Settings → Connected Agents. `herdr-tower init` does not set that
up.
