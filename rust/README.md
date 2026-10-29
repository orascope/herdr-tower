# The Rust workspace

The crates are `tower_*`, after the product: `herdr-tower`. Each layer was
verified against a real herdr server before the one above it was built on it.

| crate | what it is |
|---|---|
| `herdrx` | the socket client, types, subscriptions, metadata, layout. Verified against a real server |
| `tower_state` | records, the store, idle-loop leases and scheduled messages |
| `tower_scan` | pane→record reconciliation, state reflection, hook precedence. Verified against a real server |
| `tower_config` | configuration. Missing or broken, it runs on the defaults |
| `tower_notify` | transition notifications. Failures are swallowed — a hook must not be disturbed |
| `tower_wiring` | the folder-bot, Discord and starting-unit wiring |
| `tower_usage` | ctx and the model. `UsedPct` separates measured from estimated in the type |
| `tower_mux` | driving and subscribing to the backend. **No trait** — there is one backend, so there is nothing to fill a second slot |
| `tower_hook` | a hook event → a record. A guard drops events fired outside herdr; events from other servers are kept, labelled with their server's namespace |
| `tower_text` | width, padding, truncation, elapsed time. **One width table** — see below |
| `tower_ui` | the supervising TUI (ratatui) |
| `tower_wt` | the worktree lifecycle, git, reviews, running tests, driving a task to a verdict |
| `tower_discord` | the status card, upserts, coalescing. Pinned byte for byte against a snapshot |
| `tower_cli` | the commands and the `herdr-tower` executable: `status`, `init`, `card`, `restore`, `wt`, `hook`, `schedule`, the supervisor |

## How it is verified

`tests/live.rs` connects to a real herdr server and skips itself when there is
none. A test that passes without the server proves nothing. Calls that move
somebody's view (`focus`) and calls that poke an agent (`prompt`) are excluded
from the tests.

```
cd rust && cargo test   # the unit tests plus the live ones (only inside herdr)
cargo run -p tower_cli -- status
```

Every rendered surface — `status` in both forms, `help`, `info`, the picker, the
Discord card and the webhook bodies — is pinned byte for byte against a recorded
snapshot.

## Recorded snapshots

The snapshots are rendered from a fixed input: the fixture store under
`expected/fixture/` and the pinned clocks in `expected/NOW`, `card.NOW` and
`info.NOW`. The fixture is not a sample — every row in it is a case that once
rendered wrong somewhere, such as an ordering inverted within one state or an
ellipsis added outside the width budget.

The **expected text** is in `rust/expected/`, and `UPDATE_EXPECTED=1 cargo test`
rewrites it. That is a deliberate act, to be read in the diff like any other
change, rather than a way to make a failure go away; `expected/README.md` draws
the line.

One thing to know before using that flag: a *writer* test returns without
comparing while updating, and a *comparison* test skips, because `include_str!`
embeds the old bytes at compile time. So every fixture a test pins has to be
written before that test returns — otherwise the documented command reports
success and the next ordinary run fails.

## There is only one width table

Padding a line with one width table and cutting it with another goes wrong
quietly. Ambiguous-width characters (`●`, `…`) are one column in some tables
and two in others, and a table that follows the locale disagrees with a fixed
one **only in a terminal on an East Asian locale** — there the padding believes
the state label is one column wider, so a full-width selection bar cannot reach
the right edge. Nothing dies and no test notices.

That is what `tower_text` is for. It enforces in structure, not in a comment,
that padding a string and cutting the same string cannot disagree.

## Types hold the contract

`ReadSource::RecentUnwrapped` cannot serialise as anything but the socket's
spelling, and mapping `AgentStatus::Unknown` to a completed state is stopped at
compile time.
