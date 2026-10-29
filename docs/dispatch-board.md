# The dispatch board

[← README](../README.md)

One agent — the advisor — takes a to-do list and hands the items to `wt`
workers. The board records what went to whom, and a pane beside the advisor
shows every item with its worker's live progress, struck through once the
advisor has reviewed the result. Since v0.37.0.

## Using it

From the advisor's pane, in the repository the workers were cut from
(`herdr-tower wt new <task>`):

```bash
herdr-tower board add "parser" "tests" "benchmark"   # the plan, one item each
herdr-tower board send 1 i-parser                     # deliver item 1 to that worker
herdr-tower board send --new "fix the flaky test" i-tests   # add and deliver at once
herdr-tower board show                                # the board, with each worker's progress
herdr-tower board done 1                              # strike it through, after you reviewed it
herdr-tower board drop 3 --reason "later"             # cancel one
herdr-tower board note 2 "3 tests failing, sent back" # a line under an item
herdr-tower board prune                               # remove the done, dropped and superseded items
herdr-tower board rm 4                                # remove one for good (--force if it is out with a worker)
herdr-tower board clear --all                         # empty the board
herdr-tower board open                                # reopen the pane if you closed it
```

Long-lived workers — an agent in a pane that takes items in turn — are named
by their pane:

```bash
herdr-tower board assign 3 w2:pBR           # queue item 3 for the agent in pane w2:pBR
herdr-tower board assign 4 w2:pBR           # several may wait for one worker
herdr-tower board send 3                    # send it to the worker it is assigned to
herdr-tower board next w2:pBR               # or: send the first item queued for that worker (highest priority first)
herdr-tower board send 3 --file brief.md    # the worker gets the title, then this instruction (or --message "…")
herdr-tower board prio 3 1                  # P1 (highest) to P3, or - to clear
herdr-tower board move 7 --to 1             # reorder; numbers do not change
herdr-tower board link 3 #3231              # show that PR's state on the row; announced when it merges
herdr-tower board wake on                   # from the advisor's pane: wake it when an item is ready for review
herdr-tower board wake on w2:pBQ            # or name the advisor's pane, from anywhere
herdr-tower board --repo ~/src/app show     # another repository's board, from any folder
herdr-tower board assign 5 w2:pBS --sent    # you already sent it yourself: track it from now
herdr-tower board assign 6 "label:buildbox w5:p2"   # another machine: shown, not sent or tracked
```

A worker argument is a `wt` task, a pane id, or a label; `task:`, `pane:` and
`label:` say which outright. Each row starts with the pane the item is with —
`[w2:pBR]` — named, or the one live agent in a task's worktree.

The worker receives `[board item 1] parser`. The first successful send opens
the board to the right of your pane:

```
Board · dbstore_go_v2  1/4
──────────────────────────────
 1 parser                          (struck through)
   i-parser  – done
 2 tests
   i-tests  ● working  12m
 3 benchmark
   i-bench  ✓ review  3m
 4 docs
   ○ planned
```

- **`send` only goes to a settled worker** and waits until herdr sees it start.
  It prints `sent item N to <task>.`, or `not sent: …` (busy, waiting on an
  approval, not up yet — right after `wt new`, wait a few seconds and send the
  same number again), or `may or may not have reached …`: the item is `sent?`;
  look at the worker, then `board send N <task> --again` if it never arrived.
- **One open item per worker** — per agent, however it is named: a task and
  the pane of the same agent count as one. A second one is refused until the
  first is `done` or dropped, or you send with `--replace`. Assigning (not
  sending) queues freely; an item that is out cannot be reassigned. A follow-up to an item in
  progress goes to the worker with `herdr agent prompt <pane> '…'`.
- **The column:** `sent  no start seen`, `working`, `review`, `wait`,
  `error`, `worker replaced`/`worker unclear — check`, `gone`, `sent?`.
  `(uncertain)` means weaker evidence; `(stale)` means herdr did not answer
  the last refresh. One glyph per group leads the words, for scanning:
  `!` needs you (`wait`, `error`), `✓` review, `?` check (`sent?`, replaced,
  unclear, several agents, gone, no record), `●` working, `◌` sent but not
  started, `○` planned, `–` closed (`done`, `dropped`, `superseded`).
- **Nothing is marked done for you.** `review` means the worker's turn
  on the item ended; `board done` is yours to run.
- **Ready for review is announced.** When a worker's turn on an item ends, the
  supervisor sends one notice — desktop and the notification channel, like the
  Parallax and usage-guard ones: `✓ dbstore_go_v2 · board item 3 ready for
  review`. Once per delivery; a resend is announced again.
- `board next <worker>` sends the first item queued for that worker, and
  refuses while it still holds one: `board done N`, then `next`.
- An idle loop armed on the advisor (`L` in the tower) adds a one-line board
  summary to its message.
- The orchestration skill (`herdr-tower init` installs it) teaches the
  coordinator all of this.

After upgrading, run `herdr-tower daemon install` and restart the dock tower
before using the board (see Upgrading below).

The rest of this page is the design and its contract.

## Decisions

- 2026-09-30 The boards over remote MCP. `list_boards` and `board_show`
  read; `board_add`, `board_send`, `board_assign`, `board_next`,
  `board_done`, `board_drop`, `board_prio`, `board_move`, `board_link`,
  `board_note`, `board_wake`, `board_remove`, `board_prune` and
  `board_clear` need the act scope and run the command line (`board --repo <path> …`), so every rule
  here holds and each call is audited. A tool takes only a repository that
  already keeps a board: the command line would start an empty one for any
  git repository. `wake on` takes a pane id, since the server runs in no pane.
  `open`, `--watch` and `--file` have no tool: the first two draw in a herdr
  pane, and a remote caller sends the instruction as `message` instead.
- 2026-09-30 `board wake on` wakes the advisor itself. The review notice
  reached the person; the advisor learned of a finished worker only when the
  idle loop came round. With `wake on` (run from the advisor's pane, which is
  pinned like a pane worker), the supervisor types one line listing the items
  ready for review into the advisor — only while it is settled and still that
  agent; a busy advisor gets it on a later pass. Once per delivery.
- 2026-09-30 `wt send` confirms the start too (herdr's prompt wait), as board
  sends do: a codex that updates itself on start exits with the prompt. A
  drive's own retry still returns on acceptance. A board send to a task whose
  worker `wt new` just started waits up to 15 s for its record.

- 2026-09-30 Instructions apart from titles, priorities, PR links. The advisor
  sent 9 of 16 items itself with `herdr agent prompt` and adopted them, very
  likely because an item's title is a one-line summary while the instruction
  is long: `send`/`next` now take `--message`/`--file`, delivered after the
  title. Priority (P1–P3, replacing emoji in titles) orders `next`; `move`
  reorders. A linked PR's state is read with `gh` (the board view once a
  minute, the supervisor once a minute for notices) and its merge is
  announced once, suggesting `board done N`.

- 2026-09-30 `board next <worker>` and "ready for review" notices. The advisor
  wrote "assign from the top whenever a worker frees up" as a note and did it
  by hand; `next` does the send, the notice tells the person the moment a turn
  on an item ends (the supervisor, every 10 s, once per delivery, marked on the
  item as `review_noticed`). Nothing is sent to a worker on its own.

- 2026-09-30 Items can be removed (`rm`, `prune`, `clear --all`), reversing
  "done and dropped items are the history": a long-running board filled up
  with finished rows. Numbers stay unique for the board's life (`last_item`).

- 2026-09-30 Workers may be panes (v0.38.0). In real use the advisor never ran
  `board send`: its workers are long-lived panes reused for many items, and it
  wrote `[w2:pBR]` in notes by hand. An item's owner is now one of a `wt` task,
  a pane (pinned to the agent it held — record generation and session — and
  refused on send if the pane now holds another), or a label for a worker the
  tower cannot see. `assign` queues without sending; `assign --sent` takes
  over an item the advisor sent itself, marked uncertain. The one-open-item
  rule is per agent.

- 2026-09-28 `board send` delivers the item to the worker itself; recording and
  sending are one command.
- 2026-09-28 The board pane opens automatically on the first `board send`.
- 2026-09-28 v1 supports `wt` workers only. An item's target is a `wt` task
  (its worktree path), never an agent id or a pane id — both are reused.
- 2026-09-28 The board belongs to the repository, not to the advisor session.
  Advisors are handed over to fresh sessions several times a day; a
  session-keyed board would be orphaned each time.
- 2026-09-28 One open item per worker, and no sending to a worker that is
  `working` or `wait` right now. No queue in v1.
- 2026-09-28 Nothing is ever resent automatically. An unknown delivery outcome
  is shown as such and needs an explicit decision.
- 2026-09-28 A board send waits for an observed start (herdr's prompt wait);
  no start seen is `sent?`. From a live test with a real codex worker.
- 2026-09-28 The tower never marks an item done. It shows `review`; the advisor
  decides.

## The problem

One advisor takes a to-do list and hands the items to workers. Over half a day
nobody can see, at a glance, what was handed to whom, whether it is still
running, and what is finished. A list the advisor keeps by hand goes stale the
moment it forgets to update it — the same failure the idle loop exists to catch.

A plain task-list pane (such as the third-party herdr-tasks plugin) does not
fit: it tracks one agent working through steps in order, allows one
in-progress item, and shows only what the agent remembered to write.

## The idea

The tower already knows each worker's live state from hooks. The advisor
records only the assignment, by sending through the board; the tower fills in
progress. A split pane beside the advisor shows the board, and the advisor
strikes an item through once it has checked the result.

| layer | identity | lifetime |
|---|---|---|
| board | the repository (base checkout root) | as long as the repo |
| item target | the `wt` task and its worktree path | as long as the worktree |
| one delivery | backend + native session id + `work_seq` + time, captured at send | that worker session |
| board pane | a token on a pane | disposable, recreated on demand |

## Commands

Run from the advisor's pane. The board is the one for the caller's repository
(see Ownership).

```
herdr-tower board add "<text>" ["<text>" …]   plan items (no worker yet)
herdr-tower board send <N> <task>             assign item N to wt task and deliver it
herdr-tower board send --new "<text>" <task>  add an item and deliver it in one go
herdr-tower board done <N>…                   strike through (after review)
herdr-tower board drop <N>… [--reason "…"]    cancel: struck through and dimmed
herdr-tower board note <N> "<text>"           one line under the item
herdr-tower board show [--json]               print the board (what the advisor reads back)
herdr-tower board open                        idempotent: open the pane, or return the open one
herdr-tower board --watch                     the pane's own view
```

Removing is explicit and never frees a number: `rm N…` (all or nothing; a
send in flight always refuses, an item out with a worker needs `--force`),
`prune` for every closed item, `clear --all` for everything. A late send
outcome for a removed item lands nowhere, rather than on a new item that took
its number.

## Sending

`board send` refuses when any of these holds. The repository check runs before
anything is written; the worker checks run inside the delivery, after the
reservation, and a refusal there takes the reservation back — the item ends as
it was (only its `revision` moved). With `send --new` the new item stays on the
board, planned, and the refusal says so:

- the task's `wt` meta names a different repository than this board
  (`wt` task names are global in `<state>/worktrees/`, so this is checked, not assumed);
- the worker already holds an open item on this board — unless `--replace`,
  which freezes the old item as `superseded` once the new delivery is confirmed;
- there is no live agent in the worktree on **this** backend, or more than one
  live candidate there;
- a fresh `pane.get` shows the agent `Working`, `Blocked` or unknown;
- the live pane's native session differs from the worker's record (a
  replacement the scanner has not caught up with yet).

"More than one live candidate" counts non-dead **records**, before any pane is
checked. A stale record the scanner has not retired yet therefore refuses a
send that a live check would have allowed — deliberately: refusing is safe,
guessing is not, and the scanner clears it within a pass.

`send_board_item` takes the socket and the default socket and derives the
backend id itself (`herdrx::backend_id`), so the record it picks and the
server it prompts cannot disagree. The receipt's session id is the live pane's.
herdr reports none for some agents at all (codex), so otherwise it is the
record's — pinned by the record generation the receipt also stores; if that
record turns out to be a gone agent's, the scanner's new record has another
`born_at` and the board shows `worker unclear`. A record from before `born_at`
has no such guard, and its session is not borrowed. A worker just started may
have no session anywhere yet; the generation still pins it.

Then, in order:

1. Under the board lock, reserve attempt `k` on the item (`sending`), then
   release the lock — the socket round trip never holds it.
2. Deliver through a sibling of `tower_wt::review::send_dispatch` that keeps its
   guards (the pane still holds an agent, and its cwd is inside the worktree)
   and uses `Herdr::prompt`, never `pane.run`. It returns a **receipt**: the
   backend, the native session id, `work_seq` and time captured **before** the
   prompt, and whether the completion boundary was lost.
3. Under the lock, apply the outcome only if `k` is still the item's latest
   attempt:

| outcome | item becomes |
|---|---|
| herdr refused (approval dialog, no agent, pane moved) | back to what it was; the reservation is dropped |
| prompt accepted | `sent`, with the receipt |
| accepted but the boundary was lost | `sent`, marked "completion tracking uncertain" |
| transport or decode error (the server may have received it) | `sent?` |

**An outcome lands only on its own reservation.** The ticket carries the
attempt number, and an outcome is applied only if the item is still `sending`
**under that attempt**. `done`, `drop` and recovery end the reservation, so a
send result that arrives late cannot put a finished item back to `sent`. A
`note` does not end it — it changes nothing about the delivery, and discarding
a confirmed delivery would record a certain send as uncertain. Every change
still bumps the item's `revision`. With `--replace`, the new item and the ones
it replaces change together under one lock when the delivery is confirmed; if
the delivery ends `sent?`, the replaced item stays open too (neither can be
called superseded), and the next `--replace` closes both.

**A refused board command changes nothing it was asked to change.** Recovery of
abandoned reservations runs first under the same lock and is saved even when the
request is then refused (it is not reported separately; `show` displays the
result as `sent?`).

**Exit status.** `send` exits non-zero whenever the board does not end with a
confirmed delivery: refused, unknown, recorded-late (`done`/`drop` won the
race), or delivered-but-not-recorded. `show` marks worker progress `(stale)`
when herdr did not answer the refresh just before it.

Pane claims carry an id, so a late `set` or `release` from an abandoned claim
cannot touch a newer one made by the same (or a reused) pid.

**A reservation left behind.** The reservation stores the sending process's
pid. On any load, a `sending` item whose pid is gone becomes `sent?` — the
process may have died before, during or after the prompt, so the worker may or
may not have it. It is never resent automatically.

**Delivered but not recorded.** If the prompt succeeded and writing the outcome
failed, the CLI says both, on separate lines, and exits non-zero; the item stays
`sending` and becomes `sent?` by the rule above.

**`sent?` and the flags:**

| situation | command | effect |
|---|---|---|
| retry the same `sent?` item | `send N <task> --again` | new attempt on the same row; says the worker may get it twice |
| send a different item to a worker whose open item is `sent?` | `send M <task> --replace` | on confirmation the old item becomes `superseded (uncertain)`, never silently dropped |
| any other send to that worker | — | refused: it holds an open item |

The sibling also keeps what `send_dispatch` does after a delivery: it clears
the task's stale `drive` verdict under the lock (review.rs:266-293), so
`wt list` does not keep calling a task merge-ready after new work went out. A
failure there is reported separately from the delivery, like the boundary.

**A board send waits for the worker to start.** The prompt goes out with
herdr's wait (`until: working, blocked`), so it counts as delivered only once
the worker is seen starting after it. A live test found why: the first prompt
to a codex worker just started by `wt new` was accepted by herdr and then lost
— the composer stayed empty. herdr answers `agent_prompt_stalled` when no start
comes within five seconds; that is classed as unknown (`sent?`), never as
refused. `wt send` is unchanged and still returns on acceptance.

**Identity residual.** herdr reports no session for codex panes, so a codex
receipt carries the record's session, pinned by the record generation. A pane
id reused by a new agent without the scanner ever seeing the old one dead keeps
the record and its generation; a hook from the new agent then writes its own
session and the board shows `worker replaced`, but a turn the scanner infers
before any hook lands would be counted for the old item. Closing that needs an
identity herdr does not give for codex.

The check-then-prompt gap is real: herdr has no "prompt only if idle", so
another prompt can slip in between. That case is shown as "sent, not started
yet" rather than prevented.

## What the board shows

```
Board · dbstore_go_v2                             3/7
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 1 parser tokenizer (struck)       i-2911  –       done
 2 float exponent #2863            i-e     ● working  48m
 3 HANDOVER summary                i-2919  ✓ review
   ↳ tests 3 failing, sent back
 4 grammar                         i-2911  ! wait
 5 benchmark                               ○       planned
```

The column is computed at render time from the item and the worker's current
record, as a pure function:

| observation | column |
|---|---|
| status `planned` | `planned` |
| status `sending` | `sending…` |
| status `sent?` | `sent?  check the worker` |
| same session, no turn start seen after the receipt's `work_seq` | `sent  no start seen  Nm` (a hookless worker can start and finish between polls unseen) |
| same session, `working` after the receipt | `working  Nm` |
| same session, a turn after the receipt has **ended** | `review` — time shown only while unread (reading restamps `state_since`) |
| `wait` | `wait` — shown whoever's turn it is; uncertain if no start was seen |
| `err` | `error` — likewise |
| a different native session id in the worktree | `worker replaced — check` (frozen; the new session's progress is not this item's) |
| the record was made afresh since the delivery — a different `born_at` than the receipt read, or (for a receipt older than `born_at`) a turn count below the receipt's | `worker unclear — check`; counts from two records are never compared |
| the record has no session id yet | its progress, marked uncertain |
| session id not known at delivery | progress shown, the agent pinned by the record generation; marked uncertain only when the receipt has no `record_born` either (a record from before `born_at`) |
| no live agent in the worktree, record present and dead | `gone` |
| the backend did not answer the listing that refreshes the records, or the item's receipt names another backend than the one refreshed | the column from the stored records, marked `(stale)` |
| the worker's record file is unreadable (`Store::list` skips it) | `sent  no record` — never `gone` |
| status `done` / `dropped` / `superseded` | struck through (`SGR 9`, ratatui `Modifier::CROSSED_OUT`), dimmed |

**Record generations.** Every agent record carries `born_at`, and a receipt
stores the one it read. Turn counts are compared only within one generation. A
record is born anew when the scanner reuses a pane id, and when a hook reaches a
dead record: from a different session it starts over; from the same session it
is revived as is; with no session evidence it is revived with nothing wiped but
a new `born_at`, so the board shows `worker unclear` rather than guess.

A missing record is never read as `gone`: `Store::list` skips files it cannot
parse. "A turn ended" is observed from the same session, not from the state
alone — `DONE_UNREAD → IDLE` happens on reading, and `work_seq` counts starts.

`show --json` carries the same column plus its evidence (session ids, the
receipt, observation times).

## Ownership

The board is keyed by the **base checkout root** — the same value `wt new`
stores as `repo` in the task meta (`git rev-parse --show-toplevel` of where it
was started). From the caller's cwd: take its git toplevel; if that is a
registered `wt` worktree, use that task's `repo` instead. `send` always checks
the task's `repo` against the board.

Storage: `<state>/boards/<hash of the canonical repo path>.json`, written
atomically under a lock like `loops.json`. The file also stores the repo path
itself, checked on every load. Items keep their numbers for life.

```json
{
  "repo": "/Users/alice/dbstore_go_v2",
  "items": [
    { "n": 2, "text": "float exponent #2863", "status": "sent", "revision": 4,
      "task": "i-e", "worktree": "/Users/alice/dbstore_go_v2/.herdr-tower/worktrees/i-e",
      "attempts": 1,
      "receipt": { "session_id": "…", "work_seq": 12, "record_born": "…",
                   "at": "…" },
      "created_at": "…" }
  ],
  "panes": [
    { "id": 1, "caller_pane": "w2:p9B", "pid": 4242, "at": "…",
      "pane_id": "w2:pC", "terminal_id": "term_…", "started": true }
  ],
  "last_claim": 1
}
```

Empty and default fields are left out (`backend` for the default server,
`note`, `reason`, `closed_at`, `reservation`, `uncertain`, `boundary_lost`).

Two advisors on one repo share the board data; each gets its own pane.

## The board pane

After a delivery (never before it), if the calling advisor has no live board
pane, `send` opens one:

1. **Reserve before splitting.** Under the board lock, record a pane
   reservation for `(backend, caller pane)` with this process's pid. A second
   concurrent `send` sees it and does not split. A reservation whose pid is gone
   is ignored.
2. Split the caller's pane to the right (`focus: false`), with the state
   directory and socket in the split's environment, and record the new pane's
   id and terminal id on the reservation.
3. Once the new shell is idle — the way the usage tab waits for one — type
   `env HERDR_TOWER_STATE_DIR=… HERDR_SOCKET_PATH=… herdr-tower board --watch
   --repo …`, so nothing is taken from the shell's cwd or environment.
4. The running view keeps its own expiring `tower-board` token (value `1`)
   fresh; ownership is proven by the claim's pane and terminal ids, not by the
   token.

If two panes were made anyway, the one not on the reservation closes itself
when it is still an idle shell holding our claim, and is left alone otherwise.

- A pane is reused only on proof: the claim holds its pane id **and** its
  terminal id (herdr reuses pane ids), no agent lives there, and it sits at an
  idle shell — then the view is typed (again), even if the token of a view that
  just ended is still there. A pane that is not provably ours is never typed
  into; its claim is released and a new pane split. Only herdr's
  `pane_not_found` releases a claim; a lost answer about a live pane does not.
- Typing the view is reserved on the claim (`typing`, 20 s), so two opens that
  both see the same idle shell do not both type into it. The command carries
  the state directory and the socket itself (`env HERDR_TOWER_STATE_DIR=…
  HERDR_SOCKET_PATH=…`), so a restart in an old shell reads the same store.
- The dock exemption is explicit, next to `is_usage_pane`, by the view's
  token. The token lives 10 s past the view's last redraw (every 2 s), so a
  pane that ran the view and then the tower is hidden from the dock for at
  most that long.
- Pane failure never fails the send. The output reports delivery and pane on
  separate lines.
- Residual: a split whose answer is lost after herdr made the pane leaves the
  claim pending; once the opener exits, recovery drops the claim and the next
  open splits again, so that pane is left behind as a stray shell.
- After a herdr restart the pane is not restored; the next `send` or `open`
  recreates it.
- The view redraws from the stored records every 2 s. It never asks herdr or
  `gh` itself: the resident supervisor keeps the records current and reads the
  linked PRs' states once a minute (kept beside the board as
  `<state>/boards/<repo>-<hash>.prs`), and the view draws what it finds. When
  the supervisor's latest pass did not refresh the board's herdr server — it
  is not running, or herdr did not answer — the view says
  `not refreshed by the supervisor — progress as last seen`.
- In a short pane, the header stays visible while the list scrolls. Use the
  mouse wheel, `↑`/`↓`, or `k`/`j`; the header shows the visible line range.
  Closed items are folded first; the title line shows their count and the
  `x: show` or `x: hide` action. `x` shows or hides them. Notes remain
  reachable. Redraws overwrite the same screen instead of filling terminal
  scrollback. The two header lines have a slate background and contrasting
  text, separating them from the item list.
- **A double click goes to the worker.** Double-clicking any line of an
  item's block — its title, status or `↳` note — moves the view to the pane
  the item is with (the `[w2:pBR]` at its head), across workspaces and tabs.
  A single click does nothing, so clicking into the board pane to read it
  never leaves it. An item with no pane (planned, or a label) does nothing; a
  pane that cannot be reached is said on the last line for a few seconds. The
  view turns on mouse reporting for clicks only, so selecting text in the
  board pane takes Shift (or your terminal's override). Ctrl-C ends the view
  and puts the terminal back.

## The idle loop

When an armed idle loop fires at an agent whose repository has a board, and
that agent is the advisor — not a worker in a `wt` worktree — the supervisor
appends one line just before it sends, e.g.

```
Dispatch board (herdr-tower board show): needs you 4 · review 3? · working 2 · planned 5. Strike an item through with `herdr-tower board done N` once you have reviewed it — that is separate from `loop done`.
```

Groups in that order, closed items left out, `?` on an item whose column is
uncertain or read from a backend whose panes were not listed on this pass. No board, or nothing open on it: the message is unchanged. The line
is built at send time (one git call per fired loop); the loop's planning stays
pure.

## Upgrading

Install the new binary and restart the resident supervisor and the dock tower
(the usual `herdr-tower daemon install` and dock restart) **before** using the
board. Every agent record gains `born_at`; an old binary still running — a hook
from an older install, the old supervisor — rewrites records without it, and
the board then shows those workers as `worker unclear` (a generation it cannot
match). Records and receipts from before `born_at` are read as before, their
progress marked uncertain.

## Out of scope for v1

- Workers that are not `wt` tasks.
- Queuing to a busy worker. A top-up instruction to an item in progress goes
  to the worker with `herdr agent prompt`.
- Multiple backends on one board beyond "only this backend's workers".
- The tower marking anything done; the board in the tower TUI or Discord card.

## Build order

1. `BoardStore` in `tower_state`: repo key and check, stable numbering,
   revisions with compare-on-apply, pid-stamped reservations and their
   recovery to `sent?`, the `--again`/`--replace` table, the pane reservation,
   atomic writes. Tests for every transition, for done/drop racing a send, and
   for a reservation whose process died.
2. The receipt-returning delivery sibling in `tower_wt`, sharing
   `send_dispatch`'s guards, restricted to the caller's backend.
3. The column as a pure function, tested on every row of its table — the stale
   `done`, the replaced session and the unreadable record included.
4. `board add|send|done|drop|note|show` in `tower_cli`.
5. `board --watch`, `board open`, the auto-open after `send`, the dock exemption.
6. The orchestration skill: dispatch with `board send`.
7. Phase 2: the idle-loop line.
