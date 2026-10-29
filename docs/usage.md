# Plan usage and the usage guard

[← README](../README.md)

![The usage board in the tower (`u`): a calendar of the resets, each AI's
five-hour and seven-day windows as meters with what is left, and the usage
guard's sliders above them](images/tower-usage.png)

`herdr-tower usage` shows how much of each plan window is used and when it
resets, for claude and codex. There is a five-hour window and a seven-day one,
each drawn as a meter of the share used, the way btop draws one: in colour,
every used cell is a `■` in the colour of its place on the bar — green at the
start, amber at 50%, red from 80% — so how far it reached shows in the colour
of its tip, and unused cells are grey. Beside it the share left, green below
50% used, orange (amber) from 50% and red from 80%. Printed to a pipe, without
colour, the bar is `█` used and `░` unused:

```
USAGE  updated 17:33

claude  Max 5x
 ◆5h  ███░░░░░░░░░░░░░░░░░  83% left  resets 21:39 (in 4h06m)
 ●7d  ███████░░░░░░░░░░░░░  67% left  resets Thu 02:59 (in 6d9h)

codex   Pro  as of Sep 21 00:07
 ◆5h  —
 ●7d  ███████████████████░   6% left  resets Sat 17:35 (in 2d0h)
```

At the top, a calendar of the next eight days shows when each AI's windows
reset. `●` marks the next seven-day reset and `◆` the next five-hour one, with
the time; the two share a cell on the same day. Today is bold, and the day a
held AI is released is yellow. Only the resets the AIs reported are shown;
later ones are not guessed.

```
        Thu     Fri     Sat     Sun     Mon     Tue     Wed     Thu
        09-24   09-25   09-26   09-27   09-28   09-29   09-30   10-01
claude  ◆21:39                                                  ●02:59
codex                   ●17:35
gemini  — no 5h or 7d resets
```

- **claude:**
  - The plan (`Max 5x`, `Pro`) is read from the credentials claude keeps: the
    `~/.claude/.credentials.json` file, or the macOS keychain item
    `Claude Code-credentials`.
  - The windows come from the endpoint claude's own `/usage` reads
    (`api.anthropic.com/api/oauth/usage`), using claude's current token. The
    token is sent only to that host and never written down.
  - The endpoint is not documented, so an answer in another shape reads as
    unavailable, never as 0%.
  - It is asked at most every five minutes; the answer is kept in
    `usage-claude.json` in the state directory. An expired login says so: run
    claude once to refresh it.
- **codex:**
  - The plan and windows come live from codex itself: `codex app-server`,
    asked `account/rateLimits/read`, which is what codex's `/status` shows. The
    server is stopped as soon as it answers.
  - It is asked at most every five minutes; the answer is kept in
    `usage-codex-live.json`.
  - A reached limit reads `LIMIT REACHED` in red, with how many free full
    resets the account holds (codex's *Reset usage*).
  - Without a live answer (codex not installed, or not answering), the last
    figures codex wrote into its session rollouts are shown instead, marked
    `as of`. codex writes them only while turns run, so once the limit is
    reached those figures stop at the last one before it. That is why the
    live answer comes first.
  - A Pro plan currently reports only the seven-day window.
- **gemini:** it has no five-hour or seven-day windows; its limits are per day.

The flags:
- `--json`: the same board as JSON, with each window's `level` (green, orange
  or red).
- `--watch`: redraws every 15 seconds.

**The usage guard.** The tower can stop spending a vendor's plan once only a
floor of a window is left. Each AI (claude, codex) has its own floors, and
`usage_guard_<ai>_<5h|7d>` overrides the shared `usage_guard_5h` /
`usage_guard_7d`, which default to 25%. gemini has no five-hour or seven-day
windows to read, so it is not guarded. Once claude's or codex's window has no more than
its floor left:

- The resident supervisor **holds what it sends** to that vendor's agents.
  - Idle-loop continuations wait, and scheduled messages wait as they do for a
    busy agent.
  - When the window resets (or the floor is lowered), they go on by
    themselves.
- **Each agent working at that moment is asked once** to finish the step it is
  on, write down where it stopped (in `SESSION.md` when the folder keeps one),
  and wait until the reset.
  - Nothing is interrupted: a turn in flight runs to its end.
  - A pane you type into yourself is never stopped.
- The board marks each floor on its bar (`┃`) and says `⏸ tower sends held —
  7d 0% left ≤ 25%, until Sat 17:35` under a held vendor.
- `off` disables that window's guard even at 0% left or `LIMIT REACHED`.
  The vendor's reached flag is informational: paid credits may allow
  continued use. Other windows with enabled floors still hold at their floors.
  To use credits without a guard, turn off both windows for that AI.
- When a hold ends, because the window reset or you lowered the floor, each
  agent that was asked to wait is told it may go on, and why.
- Holds, releases and floor changes also go to you, not the agents, as a
  desktop notification and a line in the notification channel:
  `⏸ usage guard: claude held — 5h 18% left ≤ 20%, until 09-24 Thu 21:40`,
  `▶ usage guard: claude released — the floor changed: 5h 20% → 10%`, and
  `🔋 usage guard: claude floors — 5h 25% → 20% · 7d 25% → 30%`.
  A change made by dragging counts once it has stayed put for four seconds,
  so the steps of a drag never hold or release anything.

Set the floors from 1% to 100%, with 0 automatically turning the guard off:
- In the tower, `u` or a click on the header's `u usage` puts the whole usage
  board where the preview was (`u` or esc again brings the preview back, and so
  does choosing a session — `j`/`k`,
  a click on its row or the wheel). Each AI's bar has its own slider right above it, drawn like
  an effort picker: a thin track with the handle `▼` at the floor, and the
  reserve beyond it in yellow. The track shares the bars' columns, one cell per
  5%. A scale at the top (`spend … keep`, `0 25 50 75 100% used`) reads for
  every slider. The same floor is marked between the bar's cells with a yellow
  `╎`, so the marker does not hide the used or unused share.

  ```
        spend           keep
        0    25   50   75 100% used
  claude  Max 5x
        ────────────────▼───  ▸ claude ◆5h · keep 20%
   ◆5h  ███████░░░░░░░░░╎░░░░  64% left  resets 09-24 Thu 21:40 (in 2h11m)
  ```

  - Click or drag on a slider to set that AI's floor for that window.
    Its right endpoint is 0 (off); moving left from it enables the guard.
  - Or use the keys: ↑↓ choose a slider, ←→ move its floor by 1% (→ keeps
    more in reserve). Going down from 1% turns it off; → from off sets 1%.
    `x` turns it off or back to 25%.
  - `r` asks claude and codex again now, without waiting out the five-minute
    pace (after switching accounts, say). It asks at most once every thirty
    seconds, so a held key cannot get the account throttled.
  - The list stays above the board, and `j`/`k` still move in it.
- Or from the command line:

```bash
herdr-tower config set usage_guard_5h 20          # every AI: hold at 20% of the 5h window left
herdr-tower config set usage_guard_codex_7d off   # codex's own 7-day floor: none
herdr-tower config set usage_guard_codex_7d 0     # also saved as off
herdr-tower config set usage_guard_codex_7d 3     # keep exactly 3% in reserve
herdr-tower config set usage_guard_claude_5h 40   # claude's own 5-hour floor
```

**In the tower.** A line under the tower's header shows every AI's windows as
short bars with what is left, the floor's `╎` on them, and `⏸ held → <reset>`
where the guard holds the tower's sends:

```
▌claude Max 20x ◆5h ■■■■╎ 94% ●7d ■■■■╎ 49%   ▌codex Pro ◆5h — ●7d ■■■■╎ 0% ⏸ held → 09-26 Sat 17:35   ▌gemini per-day   u usage
```

The whole board, with the sliders, is `u` away. The tower reads the figures
every 15 seconds from the board's own cache, which asks claude and codex at
most every five minutes.

The line above it, the header's first, ends with the machine the tower runs
on — `herdr-tower  new 8  working 1  idle 10  ·  cpu ■■■■■  28%  mem
■■■■■  73%`, in the place the pinned default models used to take. `cpu` is the
processor's busy share over the last two seconds, summed over every core, and
`mem` the share of memory in use — Activity Monitor's "Memory Used" on macOS
(app, wired and compressed), what is not `MemAvailable` on Linux, what is not
available on Windows. These two count what is used, not what is left, and
take the same colours: green, amber from 50%, red from 80%. The processor's
share needs two readings, so it appears two seconds after the tower opens.
Of the default models only one thing is left in the header: `⚠ ▌Claude Fable
5 default` in red, while Claude's default is pinned to Fable — every new
session would then run at the top tier. The Discord card still lists all
three defaults.

**The usage tab (off by default).** Until the tower showed the board, the
resident supervisor kept a second tab beside the dock's, `🔋 USAGE-GUARD 🔋`,
running `herdr-tower usage --watch` — a process of its own for as long as the
dock was up. It is off now (`usage_tab` defaults to false), and a supervisor
that finds the tab left from before closes it once — only a tab holding
nothing but the board or an idle shell. `herdr-tower config set usage_tab true`
brings it back, kept as before:
- It creates the tab when it is missing, always right of the dock's tab. herdr
  can neither move a tab nor create one anywhere but at the end, so a usage tab
  that ends up left of the tower's (the tower's tab was made again after it) is
  closed and made again at the end. Only a tab holding nothing but the board or
  an idle shell is ever closed.
- It starts the board again in the tab's idle shell after a herdr restart.
- It puts the name back when something renamed the tab.
- It never closes anything. It never types into the tab while something other
  than an idle shell runs there, or while the tab holds more than one pane.

With `usage_tab true`, close the tab and it comes back. `herdr-tower usage
--watch` runs the board in any terminal, too.
