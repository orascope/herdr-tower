# Driving it from a phone (remote MCP)

[← README](../README.md)

`herdr-tower mcp` runs an MCP server that a phone's Claude app (a claude.ai custom
connector) can reach, so you can ask *"what is waiting, and on what?"* from
anywhere, and tell an agent what to do next.

- **Read** (every connection): agent states, a pane's screen, `report`, schedules
  and loops, and herdr's workspaces — the empty ones too. `wait_agent` waits for
  an agent's state and returns its screen.
- **Act** (only when you tick the box on the approval page):
  - `send_prompt` types an instruction into an agent. herdr refuses to type into an
    agent at an approval dialog, and the server never writes to a pane without an agent.
  - `open_agent` opens a new herdr tab in a folder and starts an agent in it (claude
    unless you ask for another), optionally typing a first message. No worktree or
    branch — that is `wt new`.
  - `machine_add` runs `herdr machine add` to prepare and save an SSH machine.
    It cannot answer interactive SSH authentication; a timed-out call may have
    registered the machine, so check `herdr machine list` before retrying.
  - `interrupt_agent` presses Escape in a **working** agent to stop its turn; it
    presses nothing in an agent that is idle or at a dialog.
  - `close_agent` closes an agent's pane. It refuses an agent that is working or
    waiting on you unless `force` is set; the conversation stays on disk for `resume`.
  - `add_schedule` / `remove_schedule` set up and take back timed messages, and
    `start_loop` / `stop_loop` idle loops — the same as `schedule add`, `schedule rm`,
    `loop start` and `loop stop`, with each option a named argument.
  - `list_boards` and `board_show` read the [dispatch boards](dispatch-board.md);
    `board_add`, `board_send`, `board_assign`, `board_next`, `board_done`,
    `board_drop`, `board_prio`, `board_move`, `board_link`, `board_note`,
    `board_wake`, `board_remove`, `board_prune` and `board_clear` change them, as
    `herdr-tower board …`.
    A board is named by its repository's path, and only a repository that already
    keeps a board is accepted.
  - `answer_prompt` presses one key (1–9, yes, no, enter, escape) in an agent waiting
    on a dialog. It does so only if herdr says the agent is blocked and the screen is
    still the one `read_agent` returned (its `screen` token); otherwise it presses
    nothing and shows the screen as it is now.
  - `tower` runs **any herdr-tower command line**: `["loop","start","api","--idle-for","5m"]`,
    `["schedule","add","19:00","--workspace","api","wrap up"]`, `["wt","new","fix-login"]`,
    anything `herdr-tower help` lists, with every flag, so the same connection can
    run shell commands (`wt test --cmd`). What administers the server
    itself is refused and left to you on the Mac: every `mcp …` command,
    `config set` / `config unset` of `mcp.*`, `notify_*` and the webhook settings,
    `config --reveal`, `init`, and `daemon` other than `daemon status`. It runs without
    a shell and with nothing on stdin, so confirmations need `--yes`. It is killed after
    55 seconds, which means the interactive commands (the TUI, `dock`) cannot be used.

It is not tied to Claude. Any MCP client that speaks Streamable HTTP and OAuth
can connect once its callback URL is added to `mcp.redirect_uris`.

```json
{
  "mcp": {
    "public_url": "https://<mac>.<tailnet>.ts.net",
    "listen": "127.0.0.1:8787",
    "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
    "act": true,
    "notify_acts": true
  }
}
```

Nothing is required. `public_url` defaults to this machine's Tailscale name
(`tailscale status`); set it when the Funnel name is another. `enabled: false`
(`herdr-tower mcp off`) keeps the supervisor from serving it. `listen` has to be
a loopback address; the server refuses to start on anything else. `redirect_uris` defaults to claude.ai's
callbacks. `act: false` takes the act tools away from every connection, whatever
it was granted. `notify_acts: false` stops posting each action to the
notification channel.

### Setting it up, in order

1. **herdr-tower v0.21.0 or later** (v0.22.0 for the stateless revision and the fixes listed in [history.md](../history.md)). `herdr-tower mcp` is new in v0.21.0. Upgrade the
   way you installed (the `install.sh` line under
   [Installing](../README.md#installing) runs `herdr-tower daemon install` too, so the
   supervisor runs the new binary). `herdr-tower version` should say v0.21.0 or later.
2. **Tailscale with Funnel.** Install Tailscale on the Mac and log in. In the tailnet's
   admin console, turn on **MagicDNS** and **HTTPS certificates**, and allow Funnel for
   this machine (the `funnel` node attribute in the policy file). Plain Tailscale is not
   enough: connector calls come from Anthropic's cloud, not from the phone.
3. **The public URL.** The Mac's Funnel name is `https://<mac>.<tailnet>.ts.net`,
   and the supervisor reads it from `tailscale status` by itself. Set
   `mcp.public_url` only when it should be another (the block above).
4. **The server.** The resident supervisor serves it, on by default, on
   `127.0.0.1:8787` (log: `~/.local/state/herdr-tower/daemon.log`). With no public
   URL — no Tailscale, or Tailscale logged out — it serves this machine alone:
   `http://127.0.0.1:8787/mcp`, for a client on the Mac such as Claude Code (a
   callback on this machine is allowed then). A phone needs the public URL. `herdr-tower mcp off` turns it off and `mcp on` back on; both restart the
   supervisor. Until 2026-09 it was a LaunchAgent of its own (`dev.herdr.tower.mcp`),
   which `daemon install` and `mcp on` now remove.
5. **The tunnel.** `tailscale funnel --bg 8787` publishes that port at the public URL.
6. **Check.** `herdr-tower doctor` has a `remote MCP` section: the supervisor answers on
   the loopback listener, and the public URL answers through the tunnel.
7. **Connect a client.** In claude.ai, add a custom connector with the URL
   `https://<mac>.<tailnet>.ts.net/mcp`. It opens the approval page, which names its
   request, e.g. **Request 3F9A0C**.
8. **Approve with a pairing code.** With that page open, run `herdr-tower mcp pair 3F9A0C`
   on the Mac, with the id on your page, and type the six digits it prints on the page.
   The code is for that page only and lasts 5 minutes. The page names the client as it
   named itself — any client can call itself "Claude" — so it also shows what a stranger
   cannot choose: the host approving sends you back to (`claude.ai`) and how long ago the
   client registered. `mcp pair` with no id lists the open pages (id, the name each gave
   itself, where it returns to, how long it has been open) and never picks one by
   itself: with one page open it shows it and asks `[y/N]` at a terminal, and with no
   terminal it only prints the id to use. `--discord` also posts the code to the
   notification channel, to pair from the phone; it is off unless you ask, since anyone
   who can read the channel can read the code (`--no-discord`, the old switch, is still
   accepted and changes nothing).
   - **Leave the box unticked** for a read-only connection: states, screens, `report`,
     schedules, loops, boards and `wait_agent`.
   - **Tick "Also let it send instructions"** for act: `send_prompt`, `open_agent`,
     `interrupt_agent`, `close_agent`, `add_schedule`, `remove_schedule`, `start_loop`,
     `stop_loop`, `machine_add`, the `board_*` changes, `answer_prompt` and `tower` (any
     herdr-tower command line).
9. **Use it from the phone.** A connector added on claude.ai shows up in the Claude
   phone app too. Ask *"what is waiting, and on what?"*, then answer it.

A connection's rights are fixed when it is approved. To change them, revoke it
(`herdr-tower mcp revoke <id>`) and pair again. A connection cannot mint another:
`tower` refuses `mcp pair` and the rest of the server's own commands, so revoking one
connection is not undone by a grant it made for itself. Every new connection is also
announced in the notification channel when Discord is on (`🔗 new remote MCP connection:
Claude (act: yes)`), whatever `mcp.notify_acts` says.

The pairing code is the only way in: five minutes, one approval page, one connection.
Five wrong codes close that page. Nothing else is locked, and a stranger cannot stop you
from pairing: their guesses on their own page count for nothing (it never gets a code),
opening pages never makes the server refuse yours, and a page that holds a pairing code
is never pushed out, nor is a client that has a page open or a code to redeem. What a
sustained flood of pages can do is push out a page you opened but have not paired yet —
up to 100 are kept, the oldest without a code goes first — and then `mcp pair` says no
such page is open: reload the approval page and pair the new id.
Tokens are stored only as hashes
(`mcp-grants.json`, `0600`). `herdr-tower mcp grants` lists who is connected,
`herdr-tower mcp revoke <id>` (or `--all`) disconnects them, `herdr-tower mcp off`
stops serving it, and `tailscale funnel reset` closes the tunnel. Nothing reaches
the server from outside until the tunnel is opened: it listens on loopback only. `doctor` checks
all of it.

Every act is recorded: `mcp-audit.jsonl` in the state directory (who, which agent,
the first 200 characters, what came of it), a line in `report` under "remote
actions (MCP)", and, when Discord is on, a line in the notification channel
(`📱 send_prompt → api: \`go ahead\` — sent (Claude)`) so an action you did not
expect shows up where the WAITs arrive. A `tower config set` is kept with its value
redacted (`config set <key> <redacted>`), and `tower config …` is never posted to the
channel. A client's name is escaped before it goes to Discord, so it cannot carry
a link or formatting. A connection may make 30 act calls and
300 read calls per ten minutes.

What an agent's screen says is another program's output. The tool descriptions
tell the model to treat it as data and never to send anything because a screen
said to, but anything shown on a screen is readable by whoever holds a token, and
a connection with act can do anything herdr-tower can, which includes running
shell commands as you. Tick the box only for a client you trust. `mcp.act: false`
takes act away from every connection at once.

### Reference (v0.22.0)

**Commands** (`herdr-tower mcp …`)

| command | what it does |
|---|---|
| `on` / `off` | have the supervisor serve it (the default) / stop it; sets `mcp.enabled` and restarts the supervisor |
| `serve` | run the server in the foreground, for a machine without the supervisor |
| `pair [id]` | issue a pairing code for the approval page showing that id: six digits, 5 minutes, one use. With no id it lists the open pages, and uses the only one only after a `y` at a terminal. `--discord` also posts it to the notification channel (`--no-discord` is accepted and is the default) |
| `grants` | list connected clients: id, client name, scopes, when connected, when last used |
| `revoke <id>` / `revoke --all` | disconnect one client or every one. Calls already in flight are refused from then on |

**Tools**

| tool | needs | arguments | what it does |
|---|---|---|---|
| `list_agents` | read | `state?` (`waiting` `done` `working` `idle` `error` `dead`) | every agent across all herdr sessions, waiting ones first |
| `get_agent` | read | `agent` | one agent: folder, conversation id, model, context use, worktree branch, what it is asking, when its state changed |
| `read_agent` | read | `agent`, `lines?` (1–200, default 60) | the bottom of its screen, capped at 16 KiB, with a `screen` token for `answer_prompt` |
| `report` | read | `since?` (`30m`, `12h`, `2d`; default `12h`) | what each agent did in the window, as `herdr-tower report` |
| `list_schedules` | read | none | schedules and idle loops |
| `list_workspaces` | read | none | every herdr workspace, empty ones too, with tab, pane and live agent counts and which is in front |
| `list_panes` | read | `workspace_id?` | live pane IDs and labels, including ordinary shells, from the same herdr server as `list_workspaces` |
| `list_boards` | read | none | the repositories that keep a dispatch board, with item and open counts |
| `board_show` | read | `repo` | one board with each worker's progress, as `herdr-tower board show --json` |
| `send_prompt` | act | `agent`, `text` (≤ 4000 characters), `wait_seconds?` (0–50) | types and submits an instruction. Refused at a dialog or in a pane without an agent. With a wait, returns the state and screen once the agent settles |
| `open_agent` | act | `folder` (absolute or `~/…`), `agent?` (`claude` `codex` `gemini` `opencode`; default `claude`), `workspace?` (a label; created when missing), `prompt?` (≤ 4000 characters), `wait_seconds?` (0–20, needs a prompt) | opens a tab in the folder and starts the agent, as `herdr-tower open`. Returns its id; with a wait, also the state and screen once it settles |
| `machine_add` | act | `ssh_target`, `label?`, `remote_session?` | runs `herdr machine add` on the Mac without a shell or stdin; returns its output or error |
| `move_panes` | act | `pane_ids` (1–20 exact IDs), exactly one of `workspace_id` or `new_workspace`, `source_workspace_id?` | moves each existing terminal to a new tab in the chosen workspace without closing or restarting it. For `new_workspace`, creates a separate root shell pane to keep the destination open after moved panes exit. Verifies terminal ID and shell PID after each move; returns old and new pane IDs plus the root shell pane ID. Stops on an uncertain result, so a multi-pane call may partially succeed |
| `interrupt_agent` | act | `agent` | presses Escape, and only if herdr says the agent is working; returns the state and screen after |
| `close_agent` | act | `agent`, `force?` | closes the agent's pane, as `herdr-tower close`. Refused for a DEAD agent, and for a working or waiting one unless `force` |
| `add_schedule` | act | `at` (`19:00`, `2026-09-27T09:00`, `+30m`), `message`, one of `workspace` / `session` / `handoff: true`, `repeat?` (`once` `daily` `weekdays`), `wait?` (`10m`), `until?` (`2026-10-31`) | runs `herdr-tower schedule add`; answers with its line (the new id) or its error |
| `remove_schedule` | act | `ids` (1–50; not `all`) | runs `herdr-tower schedule rm <ids>`; nothing is removed when an id is unknown |
| `start_loop` | act | `agent`, `idle_for?` (default `10m`), `ttl?` (default `168h`), `max_turns?`, `instruction?` | runs `herdr-tower loop start` |
| `stop_loop` | act | `agent`, or `all: true` | runs `herdr-tower loop stop` |
| `board_add` | act | `repo`, `items` (1–50 titles, each ≤ 300 characters) | runs `herdr-tower board add` |
| `board_send` | act | `repo`, `item` or `new` (a title), `worker?` (needed with `new`), `message?` (≤ 4000 characters), `again?`, `replace?` | runs `herdr-tower board send`: delivers and waits up to about 15 seconds to see the worker start |
| `board_assign` | act | `repo`, `item`, `worker`, `sent?` | runs `herdr-tower board assign` |
| `board_next` | act | `repo`, `worker`, `message?` | runs `herdr-tower board next`: the worker's first queued item, highest priority first |
| `board_done` | act | `repo`, `items` (1–50 numbers) | runs `herdr-tower board done` |
| `board_drop` | act | `repo`, `items`, `reason?` | runs `herdr-tower board drop`; the worker is not told |
| `board_prio` | act | `repo`, `item`, `priority` (1–3, or 0 to clear) | runs `herdr-tower board prio` |
| `board_move` | act | `repo`, `item`, `to` (position, from 1) | runs `herdr-tower board move N --to K` |
| `board_link` | act | `repo`, `item`, `pr` (0 unlinks) | runs `herdr-tower board link` |
| `board_note` | act | `repo`, `item`, `text` (≤ 300 characters; empty removes it) | runs `herdr-tower board note` |
| `board_wake` | act | `repo`, `on`, `pane` (the advisor's pane id; needed with `on: true`) | runs `herdr-tower board wake on <pane>` or `wake off` |
| `board_remove` | act | `repo`, `items`, `force?` | runs `herdr-tower board rm`; an item out with a worker needs `force` |
| `board_prune` | act | `repo` | runs `herdr-tower board prune`: every done, dropped and superseded item goes |
| `board_clear` | act | `repo`, `all: true` | runs `herdr-tower board clear --all`: every item goes |
| `wait_agent` | read | `agent`, `until?` (`settled` `done` `waiting` `idle`), `seconds?` (1–50, default 30) | waits for a state, then returns the state and screen |
| `answer_prompt` | act | `agent`, `choice` (`1`–`9` `yes` `no` `enter` `escape`), `screen` | presses one key, and only if the agent is WAITING and the screen still matches the token |
| `tower` | act | `args` (≤ 64 items, each ≤ 8000 characters) | runs `herdr-tower <args>` with no shell and nothing on stdin. Output is capped at 16 KiB and the run is killed after 55 seconds. Refused: `mcp …`; `config set`/`unset` of `mcp.*`, `notify_*` and webhook settings; `config --reveal`; `init`; `daemon` but `daemon status` |

`move_panes` uses Herdr's `pane.move` operation and requires a terminal ID and observable shell PID before moving. For `new_workspace`, it first creates the workspace without changing focus, keeps its root shell pane, then moves requested panes into new tabs. The result's `anchor_pane_id` identifies that shell; closing the anchor itself can still close the workspace if no other panes remain. Moving the last pane out of a source tab or workspace may remove that now-empty source location. The move response gives each moved pane's new ID; callers must use it for later pane operations. A supervised agent keeps its Tower agent ID and conversation state while its pane coordinate changes. An error after workspace creation or a pane move can leave its outcome unknown, so the tool never retries automatically.

`agent` is an id from `list_agents` (like `claude-w2:p9B`), a herdr workspace name
when only one agent is in it, or the first eight characters of a conversation id.
`repo` is a path `list_boards` shows; any other is refused, so a call cannot open a
new board. A board's `worker` is a `wt` task, a pane id like `w2:pBR`, or (for
`board_assign`) a label.
Everything read from a screen, and everything `tower` prints, ends with a reminder
that it is another program's output.

**Instructions and prompts**

On connecting, a client gets the server instructions: the safety rules, then the
usual tasks in a line each, naming the tools in the order to call them (what needs
the user, answering a dialog, giving an agent work, starting one, moving panes,
handing out board work, keeping an agent going).

The prompts are the same tasks written out step by step, as ready-made requests a
client can offer its user (a slash command or a menu entry). They only lead to
tools the token can use, so a read-only connection does not see `dispatch`.

| Prompt | Scope | Arguments | Asks the model to |
|---|---|---|---|
| `triage` | read | — | find the WAITING agents and what each asks, without answering |
| `agent_status` | read | `agent` | say where one agent stands, from `get_agent` and `read_agent` |
| `review_board` | read | `repo?` | list the board items ready for review with what each worker reports |
| `dispatch` | act | `repo`, `work?` | propose a board plan, wait for agreement, then add and send the items |
| `daily_report` | read | `since?` (default `12h`) | summarize `report` per agent |

Arguments are strings of at most 4000 characters; an unknown, empty or missing
required one is a `-32602` error.

**Protocol**

- Streamable HTTP at `POST /mcp`. Each request gets a single JSON response or a `202`.
  There is no SSE, no `Mcp-Session-Id` and no batching. `GET` and `DELETE /mcp` get `405`.
- The **stateless revision 2026-07-28**:
  - Every request carries `_meta` (`io.modelcontextprotocol/protocolVersion` and
    `…/clientCapabilities`), and there is no handshake.
  - `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` must match the body.
    `Mcp-Name` may use the `=?base64?…?=` form.
  - Errors:
    - a missing `_meta` field: `400` with `-32602`
    - a header that does not match the body: `400` with `HeaderMismatch` (`-32020`)
    - an unknown revision: `400` with `UnsupportedProtocolVersion` (`-32022`), listing
      the supported revisions
    - a method outside the revision: `404` with `-32601`
  - `server/discover` returns the versions, capabilities and instructions.
  - Results carry `resultType: "complete"` and `serverInfo`. `tools/list` and
    `prompts/list` are cacheable for 5 minutes, `private` to their token.
- **Handshake revisions 2025-11-25, 2025-06-18 and 2025-03-26** work as they always
  did: `initialize`, then `tools/list` / `tools/call` / `prompts/list` / `prompts/get` / `ping`. Both eras are served
  on the same endpoint.
- Capabilities: `tools` and `prompts`. There are no resources, sampling or
  subscriptions.

**OAuth 2.1** (the server is its own authorization server, for one user)

| endpoint | spec | notes |
|---|---|---|
| `GET /.well-known/oauth-protected-resource` | RFC 9728 | names the authorization server |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 | `S256` only, `authorization_code` + `refresh_token`, `iss` in responses |
| `POST /register` | RFC 7591 | dynamic registration. Only callbacks on `mcp.redirect_uris` are accepted |
| `GET /authorize` | OAuth 2.1 + PKCE | the approval page, naming its request (e.g. **Request 3F9A0C**) |
| `POST /authorize` | | checks the pairing code, then redirects with `code`, `state` and `iss` (RFC 9207) |
| `POST /token` | OAuth 2.1 | code → tokens. Refresh tokens rotate |

Lifetimes:
- an authorization code: 60 seconds
- an access token: 1 hour
- a refresh token: 30 days
- a pairing code: 5 minutes
- an unanswered approval page: 10 minutes

At most 100 approval pages are kept, 3 per client; a client's fourth page replaces its
own oldest, and past 100 the oldest page without a pairing code makes room — a new
page is never refused, and the page a code was issued for is never pushed out. At most
200 registrations that have not connected are kept; past that the oldest one that has
no page open and no code to redeem goes. Five wrong codes close a page. `POST /mcp` without a valid
token gets `401` and a `WWW-Authenticate` header that points at the metadata. A
request whose `Origin` is neither this server nor an allowed callback's origin gets
`403`.

**Limits**

| what | limit |
|---|---|
| calls per connection | 300 read and 30 act per 10 minutes |
| a request | the head ≤ 16 KiB (`431`), the body ≤ 64 KiB (`413`, refused before it is read), no chunked body (`411`) |
| arriving | a whole request within 10 seconds; at most 32 connections still sending (`503` + `Retry-After: 1` beyond that). The last 8 are kept for connected clients: a connection that gets one has 2 seconds for its head, and goes on to read a body only with a live bearer token — any other request with a body still to come gets `503` before it is read, so strangers trickling bodies cannot shut out `/mcp` |
| handling | 8 tool calls at a time, and 8 other requests (sign-in, metadata) in a pool of their own, so busy tools never block sign-in |
| listening | loopback only; the server refuses any other address |

**Files** (in `~/.local/state/herdr-tower/`)

| file | what |
|---|---|
| `mcp-grants.json` | clients, grants, open approval pages. Tokens and codes are kept only as hashes. `0600`, locked while written, and written only when something in it changed |
| `mcp-audit.jsonl` | every act: who, which agent, the first 200 characters, the outcome. `0600`, locked, trimmed as it grows |
| `daemon.log` | the supervisor's log, where the remote MCP server writes one line per request (method, path, status; never the query) |
