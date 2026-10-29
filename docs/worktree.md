# Worktree parallel mode

[← README](../README.md)

Lets several agents — claude, codex, gemini and opencode in any mix — work in one
repository in parallel without treading on each other's files. A single command
gets a worktree, an `agent/<task>` branch, a herdr tab and a started agent, and
the backend does all four.

```bash
herdr-tower wt new auth-api --agent claude --test 'cargo test'
herdr-tower wt new login-ui --agent codex --test 'cargo test'   # same repo, no conflict
herdr-tower wt list                              # dirty, unmerged, test and drive state at a glance
herdr-tower wt diff auth-api                     # what changed against base
herdr-tower wt test auth-api                     # run the tests and record the result
herdr-tower wt review auth-api                   # write a review file, annotate it with "#> comment"
herdr-tower wt send auth-api                     # send the comments to the agent as changes to make
herdr-tower wt wait auth-api login-ui            # wait for several workers to finish
herdr-tower wt drive auth-api login-ui           # wait, test, hand failures back, until merge-ready or stuck
herdr-tower wt merge auth-api                    # summarise the checks, show the commands, merge after a y
herdr-tower wt clean auth-api                    # tidy up, keeping anything unsafe
```

- **No agent stops to ask** — every agent the tower starts (`wt new`, `open`
  and the remote MCP `open_agent`, `restore`, `resume`) runs without approval
  prompts: claude `--dangerously-skip-permissions`, codex
  `-s danger-full-access -a never`, opencode `--auto`, agy
  `--dangerously-skip-permissions`, stock gemini `--yolo`. Always — there is no
  setting to turn it off (`worker_auto_approve` was retired; an old value in
  `config.json` is ignored)
- **Nothing merges itself** — a merge is always explained and always confirmed
- **Cleaning keeps what is unsafe** — one uncommitted file, one untracked file or
  one unmerged commit and `clean` refuses
- The worktree goes in `<repo>/.herdr-tower/worktrees/<task>`, its metadata in
  the state directory
- **A worker may move to a branch of its own** (`agent/i-2919` →
  `agent/explain-2969-stage2`). Everything that shows a branch — the tower's
  `⎇`, herdr's sidebar, the Discord card, a preview window's title, `wt list`
  (`agent/explain-2969-stage2 (cut as agent/i-2919)`) — shows the one checked
  out now, read from the worktree's `HEAD`. `clean` counts unmerged commits on
  both branches, and `merge` names both and merges neither: which one lands is
  your call, and it prints the commands for the branch the work is on

### Driving a task to a verdict

`wt wait`, `wt test` and `wt send` can be run in that order by hand. What is
missing is the part nobody is awake for: when the tests fail at 3am, someone has
to read the failure, give it back to the worker and wait again. `wt drive` does
that, and stops.

```bash
herdr-tower wt drive --attempts 2 --timeout 2h auth-api login-ui
```

Every task named is driven **at the same time** — one worker's two-hour wait is
not a two-hour silence for the others — and each line of output says which task
it belongs to.

**Stopping is the design.** "Finished" here means *a branch that passed its tests
is standing in the merge queue*, not a branch that was merged. Merging stays a
person's decision, because that is where an unattended mistake becomes
everyone's; everything up to it can be done while asleep.

`wt list`'s DRIVE column is the one to read in the morning:

| | |
|---|---|
| `merge-ready` | tests passed, on the task branch, nothing uncommitted |
| `running` | a failure was handed back and the drive is still going |
| `stalled` | the same failure twice — a third copy of the message will not help |
| `exhausted` | out of attempts with the tests still failing |
| `uncommitted` | tests passed, but on a dirty tree or the wrong branch, so the branch does not carry what was tested |
| `unfinished` | the worker never settled, or is holding a question — nothing was tested |
| `superseded` | the verdict was reached on a commit the branch has since moved past |

What comes back to the worker is the **end** of the failing run, not the whole
log: the part it is missing is the last screen, and the rest would spend its
context on what it already knows. That same tail is the fingerprint for "the
same failure twice".

A drive verdict is not sticky. `wt send` and a later `wt test` both clear it —
a branch being revised, or one that just failed, is not standing in the merge
queue.

- **Orchestration you ask for in words**: tell a coordinator session "give this to
  claude and codex as an A/B" and the `orchestration` skill — installed by
  `herdr-tower init` — walks `wt new`, the dispatch, the wait and the gathering
  for you. You choose what gets merged.
