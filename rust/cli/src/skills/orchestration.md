---
name: orchestration
description: herdr-tower로 멀티 에이전트 오케스트레이션. worker N개(claude·codex·gemini)를 각각 git worktree+herdr 탭에 띄워 태스크를 dispatch하고, 완료를 기다렸다가 결과를 취합·비교한다. "worker 2개 만들어서 같은 태스크 시켜줘", "claude랑 codex한테 A/B로 시켜봐", "서로 다른 태스크 나눠서 병렬로", "이 작업 worktree에서 codex한테 시켜"(worker 1개도 같은 절차), "/orchestration" 등으로 트리거. 머지는 기본적으로 사용자 몫이다.
---

# herdr-tower Orchestration — instructions for the coordinator

You are the coordinator. Create the workers, dispatch to them, notice when they
finish, and gather the results, in the order below.

Assumed: you are running inside herdr, `herdr-tower` is installed with its
hooks registered (`herdr-tower init`), and the target folder is a git
repository. If any of that is not true, say so before starting.

## 0. Say the plan back

Take these out of the request, say them in one sentence, then begin:
- how many workers, and of which kind (claude | codex | gemini)
- the shape of the work: the same task A/B, or different tasks split up
- the commit and merge policy (by default: workers do not commit, nobody touches the base branch, and the person chooses what gets merged)

## 1. Make the workers — one worktree per task

```
herdr-tower wt new <task> --agent <kind> [--repo <path>] [--base <branch>] [--test '<command>']
```

- The task name becomes the branch (`agent/<name>`) and the herdr tab name. For an A/B comparison, put the kind on the end: `hero-dark-claude`, `hero-dark-codex`.
- The worktree appears at `<repo>/.herdr-tower/worktrees/<name>`, the tab opens in this herdr session, and the CLI starts itself.
- **Do not wait for the boot yourself.** `wt new` leaves the starting to herdr, and herdr answers only once it has seen its agent come up — if it did not, the command fails with `agent_not_ready`. If the command succeeded, you may dispatch. The old routine of scraping the screen for a prompt is answered by herdr now.
- The folder-trust prompt is handled by herdr's own startup path too. If an unexpected prompt is still on screen, read it the way §3 describes and report it — never approve one on your own.

**Getting the pane coordinate.** herdr addresses a window by pane ID (`w2:p4S`), not by name:
```
herdr-tower info <task>               # the "herdr  <session> <pane-id>" line
herdr-tower status --json             # .pane.pane_id on each record
```

## 2. Dispatch — through the board

Run these from your own pane, in the repository the workers were cut from:

```
herdr-tower board add "<item>" ["<item>" …]     # the plan, one line per item
herdr-tower board send <N> <worker>             # deliver item N to that worker
herdr-tower board send --new "<item>" <worker>  # add and deliver in one go
herdr-tower board assign <N> <worker>           # queue item N for a worker, not sent yet
herdr-tower board send <N>                      # send item N to the worker it is assigned to
```

A **worker** is a `wt` task (`i-3205`) or the **pane** of a long-lived agent (`w2:pBR`) — one that takes items in turn. `task:`, `pane:` and `label:` say which outright; `label:buildbox w5:p2` names a worker on another machine, shown on the row but never sent to or tracked. A pane-shaped name with no agent here is refused, not taken for a label. The board leads each row with the pane it is with — `[w2:pBR]` — so there is no need to write it in notes.

- `board send` is the dispatch. It types the item into the worker as `[board item N] <text>` and records the assignment in the same command, so the board follows what was sent; when the outcome is unclear the item says so (`sent?`) instead of guessing. The first successful send also tries to open the board beside your pane (a pane that will not open never fails the send; `board open` retries). Every worker's progress shows there on its own.
- It sends only to a worker that is settled right now, and it waits until herdr sees the worker **start**: an instruction herdr accepts can still be lost (a codex just started has swallowed one). Read its exit status:
  - `sent item N to <worker>.` — delivered and started.
  - `not sent: …` — nothing reached the worker (busy, waiting on an approval, not up yet). Fix the cause and send the **same number** again — `board send N <task>`; after a `--new` the item already exists (`item N stays on the board`), so do not `--new` it twice. Right after `wt new`, "no agent is running" means the worker is not recorded yet: wait a few seconds and send again.
  - `may or may not have reached …` — the item is `sent?`. Look at the worker's screen. If it never arrived: `board send N <task> --again`. If it did, do not resend: note it (`board note N "arrived"`), let the worker finish, and `board done N` once you have reviewed the result — a `sent?` item has no receipt, so the board cannot show its progress; the worker's screen and `herdr-tower status` can.
- One open item per worker: a worker holding an item is refused a second one until you mark the first `done` (or `drop` it), or send with `--replace`. A follow-up to an item in progress — "tests are failing, fix them" — goes to the live agent with `herdr agent prompt <pane-id> '<instruction>'`; review comments written into a `wt review` file go with `herdr-tower wt send <task>`.
- **Put the reporting contract at the end of the item text**, or once in the worker's first item:
  > When you are done, leave a REPORT.md at the root of the worktree. Sections: `## Summary` (two or three sentences) / `## Changed Files` (one line each) / `## How to Verify` (how to run and check it) / `## Notes` (anything the coordinator needs to know, such as wiring this into an existing file). Do not commit. Never touch the base branch.
- `herdr-tower board show` prints the board (and each worker's progress) for you to read back; `--json` carries the evidence behind every column.
- **Strike an item through yourself, after you have reviewed the result**: `herdr-tower board done N`. The board shows `✓ review` when a worker's turn on it ended; it never marks anything done on its own. `board drop N --reason "…"` cancels one; `board note N "…"` puts a line under it.
- **Queue items on long-lived workers with `assign`**: several items may wait for one pane; `board send N` sends one to whoever it is assigned to, and `board next <pane>` sends the first one queued for that worker. A worker still holding an item is refused the next one until you `board done` it. An item that is out cannot be reassigned.
- To put a pane under a coordinator in the tower's session list without handing it work, run `herdr-tower tree link <worker-pane> [<coordinator-pane>]` (the coordinator defaults to your own pane). A worker in another workspace is moved into a new tab of the coordinator's workspace; the command prints its new pane id. `board send`/`assign` and `wt new` place workers on their own.
- The supervisor tells the person when a worker's turn on an item ends ("ready for review"); review it and `board done N`, then `board next <pane>`. Run `herdr-tower board wake on` once from your own pane and it tells you too: a `[herdr-tower board] Ready for review:` message arrives when you are idle — act on it, don't wait for the idle loop.
- **Keep the title short and put the instruction in `--message "…"` or `--file <path>`** on `send` or `next`: the worker gets `[board item N] <title>` and then the instruction. Do not send it yourself with `herdr agent prompt` — the board's send checks the worker, waits for it to start and tracks it exactly.
- `board prio N 1|2|3` instead of emoji in titles (`next` takes the highest first); `board move N --to K` to reorder; `board link N #PR` shows the PR's state on the row and the merge is announced — then `board done N`.
- **When you sent an item yourself** (`herdr agent prompt`), `board assign N <pane> --sent` takes it over as sent: its progress is tracked from then on, marked `(uncertain)` because the turn may have begun before the board knew. A pane assigned earlier that now holds another agent is refused on send — assign it again.
- For an agent the board cannot reach, `herdr agent prompt <pane-id> '<instruction>'` still works — never `herdr pane run`, which types into a shell.

## 3. Waiting — no screen scraping; the hooks are the truth

```
herdr-tower wt wait <task> [<task> ...]
```

- Blocks until the workers finish. Several tasks at once, up to thirty minutes by default (`--timeout 1h`).
- A result line saying `done` means that worker has finished. `needs you` means it cannot go on until somebody answers — handle that as below.
- For a board item, the board's column is the evidence that it finished: `✓ review` on that item. `wt wait` only says the agent in that worktree settled — it looks at whichever live record it finds there, on any Herdr server, and after a send that printed "completion boundary was not recorded" a completion left over from before can answer it at once. Check `herdr-tower board show` before gathering.
- Do not poll. herdr knows the moment its agent settles and the server answers once; there is no reason to call `status` in a loop.
- To glance at the state you can still use `herdr-tower status | grep -F 'worktrees/<task>'`.
- If a worker sits at `[working]` for a long time, look at the screen with `herdr pane read <pane-id> --source recent-unwrapped --lines 20` — if it is waiting for approval, report what it says and wait for instructions. Never approve on your own.
- A worker that turns `[wait]` gets the same look: answer the question if you can, otherwise put it to the person.

## 4. Gathering

In each worktree:
```
cat <worktree>/REPORT.md
git -C <worktree> diff --stat        # and the diff itself where it matters
```

- **For an A/B comparison**: a table of the size of the change, the approach, and how they are built differently, then a paragraph on how to choose (which criterion favours which). Do not merge — the person chooses.
- **For split work**: summarise each report, and do the wiring the workers left in their Notes (adding an import to a shared file, say) **yourself, inside each branch's worktree**, and commit it there. Never on base.
- If a preview is wanted, run a dev server per worktree on different ports. Start each **with that worktree as its cwd** (`cd <worktree> && python3 -m http.server <port> --bind 127.0.0.1 &`) and tell the person which port is which branch. A codex worker is sandboxed and cannot leave a background server running, so the coordinator starts those.
- Have a worker that starts a dev server print `http://localhost:<port>` on a line of its own, so the person can open it.

## 5. Merging and cleaning up — nothing merges itself

- Once a branch is chosen: commit the worker's changes (leaving REPORT.md out of the commit), then `herdr-tower wt merge <name>` (it summarises the checks and merges only after a y).
- If the person **named the worker in words** — "merge the claude one" — that is the confirmation: `herdr-tower wt merge <name> --yes` (Bash has no stdin, so a y cannot be given). If they have not chosen, show the table and ask.
- Cleaning up is `herdr-tower wt clean <name>` — it refuses while anything is uncommitted or unmerged, which is the point. If it refuses, show the reason and wait.
- Even a branch being thrown away is not deleted before the person says so.

## Do not

- **Named Herdr sessions are supported** — the resident supervisor discovers `~/.config/herdr/sessions/*/herdr.sock`, namespaces their pane IDs, and routes actions back to the owning socket. Keep workers in a standard named session when a second Herdr server is needed so it can be discovered automatically.
- Do not judge completion by parsing the screen — completion is what `herdr-tower status` says it is.
- Do not merge automatically, do not edit the base branch, and do not answer a worker's approval prompt on your own.
