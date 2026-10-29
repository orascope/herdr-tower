#!/usr/bin/env bash
# One command for both halves: the unit suite, then the live scenarios that
# unit tests cannot see.
#
# The live half exists because every regression it guards against shipped with
# a green `cargo test`. They were all in the seam between herdr-tower and a
# running herdr server — a stale token believed over a process listing, a
# version hidden on one surface, a dock that came back as two tabs instead of
# one. No amount of unit testing reaches those, because the thing that was
# wrong was the conversation between two live processes.
#
# So after `make test`, this runs the installed binary against a real herdr
# server. By default it
# starts its own — headless, on its own socket, named `tower-smoke` — so the
# session you are working in is never touched. `--live` points it at your real
# session instead; read what each scenario does before you do that.
#
# It runs `make test` first, so one command answers both questions: is the code
# right (the unit suite, on this working tree), and does the product built from
# it behave (the scenarios, on the installed binary).
#
#   ./TEST00.SH                  make test, then every scenario
#   ./TEST00.SH --list           what it would run
#   ./TEST00.SH --only unit-tests            just make test, no herdr needed
#   ./TEST00.SH --no-unit        the live scenarios only (`make smoke` ran the
#                                suite itself, before installing)
#   ./TEST00.SH --only dock-tower-kill-recovers
#   ./TEST00.SH --keep           leave the server up afterwards to poke at
#   ./TEST00.SH --live --yes     against your own session (it closes your dock
#                                tab and kills your tower; both come back)
#
# Exit status is 0 only if every scenario passed. A skip is not a pass and says
# why it was skipped.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SESSION_NAME="tower-smoke"
SESSION_DIR="$HOME/.config/herdr/sessions/$SESSION_NAME"
TIMEOUT="${SMOKE_TIMEOUT:-60}"
LIVE=0
NO_UNIT=0
KEEP=0
YES=0
ONLY=""
STARTED_SERVER=0
UNIT_LOG=""

SCENARIOS=(
  unit-tests
  version-stamp
  dock-appears
  dock-header-version
  dock-tab-label-pinned
  dock-tab-close-recovers
  dock-tower-kill-recovers
  popup-header-version
  loop-pause-lifts
)

# The header comment, and only the comment: it stops at the first line that is
# not one, so the range cannot drift into code as the header grows or shrinks.
usage() {
  sed -n '2,/^[^#]/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --live) LIVE=1 ;;
    --no-unit) NO_UNIT=1 ;;
    --keep) KEEP=1 ;;
    --yes|-y) YES=1 ;;
    # 🚨 A flag that takes a value is refused without one. `--timeout` alone
    # set TIMEOUT to "", every `$((SECONDS + ))` then failed as a syntax error,
    # and the recovery scenarios reported failures without ever waiting; a bare
    # `--only` silently meant "run everything".
    --only)
      [ -n "${2:-}" ] || { echo "--only needs a scenario name (see --list)" >&2; exit 2; }
      ONLY="$2"; shift ;;
    --timeout)
      case "${2:-}" in
        ''|*[!0-9]*) echo "--timeout needs a number of seconds" >&2; exit 2 ;;
      esac
      TIMEOUT="$2"; shift ;;
    --list) printf '%s\n' "${SCENARIOS[@]}"; exit 0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

# ── output ────────────────────────────────────────────────────────────────────

if [ -t 1 ]; then
  R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; D=$'\033[2m'; Z=$'\033[0m'
else
  R=""; G=""; Y=""; D=""; Z=""
fi

PASSED=0; FAILED=0; SKIPPED=0; UNMET=0
declare -a FAILURES=()
declare -a SKIPS=()

# Scenarios that may legitimately find nothing to test. Everything else that
# skips leaves the run unproven, and the exit status says so.
#
# `unit-tests` is here for one case only: a machine with no Rust toolchain.
# This script's main job is the installed product, and a machine that has the
# binary but not `cargo` should be able to check it and exit 0. Asked for by
# name (`--only unit-tests`) a skip still fails, because then it is the run.
OPTIONAL_SCENARIOS="loop-pause-lifts unit-tests"

say()  { printf '%s\n' "$*"; }
note() { printf '%s    %s%s\n' "$D" "$*" "$Z"; }
pass() { PASSED=$((PASSED + 1)); printf '  %sPASS%s  %s\n' "$G" "$Z" "$1"; }
skip() {
  SKIPPED=$((SKIPPED + 1))
  # Optional unless it was the one scenario asked for: `--only X` that skips
  # has tested nothing at all.
  case " $OPTIONAL_SCENARIOS " in
    *" $1 "*) [ "$ONLY" = "$1" ] && { UNMET=$((UNMET + 1)); SKIPS+=("$1 — $2"); } ;;
    *) UNMET=$((UNMET + 1)); SKIPS+=("$1 — $2") ;;
  esac
  printf '  %sSKIP%s  %s — %s\n' "$Y" "$Z" "$1" "$2"
}
fail() {
  FAILED=$((FAILED + 1))
  FAILURES+=("$1 — $2")
  printf '  %sFAIL%s  %s\n        %s\n' "$R" "$Z" "$1" "$2"
}

# ── herdr helpers ─────────────────────────────────────────────────────────────

# The socket is the only thing that decides which server we talk to, and it
# outranks HERDR_SESSION. A pane's own identity is inherited from whatever
# terminal launched this script and would point commands at the wrong place.
unset HERDR_PANE_ID HERDR_WORKSPACE_ID HERDR_TAB_ID
# 🚨 `HERDR_SESSION` is kept for `--live`. It names the session the caller is
# working in, and unsetting it sent every call to the *default* session — so
# `--live` from `herdr --session work` closed the dock and killed the tower of a
# session nobody named. The disposable mode sets `HERDR_SOCKET_PATH`, which
# outranks it, so it needs no unsetting there either.

DOCK_LABEL='🗼 HERDR-TOWER 🗼'

# Reads one value out of a herdr JSON reply with python, which is always here.
# `jq` is not: this has to run on a machine that has just cloned the repo.
jpy() { python3 -c "$1" 2>/dev/null; }

dock_workspace() {
  herdr workspace list 2>/dev/null | jpy '
import json,sys
d=json.load(sys.stdin).get("result",{})
for w in d.get("workspaces",[]):
    if w.get("label")=="'"$DOCK_LABEL"'":
        print(w["workspace_id"]); break
'
}

# Tab ids and labels of one workspace, one per line: "<tab_id>\t<label>".
workspace_tabs() {
  herdr tab list --workspace "$1" 2>/dev/null | jpy '
import json,sys
for t in json.load(sys.stdin).get("result",{}).get("tabs",[]):
    print("%s\t%s" % (t["tab_id"], t.get("label") or ""))
'
}

workspace_panes() {
  herdr pane list --workspace "$1" 2>/dev/null | jpy '
import json,sys
for p in json.load(sys.stdin).get("result",{}).get("panes",[]):
    print(p["pane_id"])
'
}

pane_token() {
  herdr pane list --workspace "$1" 2>/dev/null | jpy '
import json,sys
for p in json.load(sys.stdin).get("result",{}).get("panes",[]):
    if p["pane_id"]=="'"$2"'":
        print((p.get("tokens") or {}).get("'"$3"'","")); break
'
}

# The pid of a tower running in this pane, or nothing. One argv entry and the
# herdr-tower binary: the same test the dock itself makes.
tower_pid() {
  herdr pane process-info --pane "$1" 2>/dev/null | jpy '
import json,sys,os
info=json.load(sys.stdin).get("result",{}).get("process_info",{})
for p in info.get("foreground_processes",[]) or []:
    argv=p.get("argv") or []
    if len(argv)==1 and os.path.basename(argv[0])=="herdr-tower":
        print(p["pid"]); break
'
}

# The dock pane that currently holds a running tower, or nothing.
dock_tower_pane() {
  local ws; ws="$(dock_workspace)"
  [ -n "$ws" ] || return 0
  local pane
  while read -r pane; do
    [ -n "$pane" ] || continue
    if [ -n "$(tower_pid "$pane")" ]; then printf '%s\n' "$pane"; return 0; fi
  done < <(workspace_panes "$ws")
}

# The tower's top line, which is the header.
pane_header() {
  herdr pane read "$1" --lines 60 2>/dev/null | head -1
}

# Runs a command under a time limit, with or without coreutils.
#
# 🚨 `timeout` is not a macOS base tool — where it exists on a Mac it came from
# Homebrew coreutils. On a clean machine `timeout 120 herdr-tower dock` is a
# "command not found", the dock is never asked for at all, and the recovery
# scenarios then fail for a reason that has nothing to do with the dock.
BOUND=""
bounded() {
  local secs="$1"; shift
  if [ -n "$BOUND" ]; then "$BOUND" "$secs" "$@"; return $?; fi
  "$@" &
  local pid=$! waited=0
  while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt "$secs" ]; do
    sleep 1
    waited=$((waited + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    return 124
  fi
  wait "$pid"
}

# Polls a predicate until it succeeds or the deadline passes.
# `wait_for <seconds> <command...>`
wait_for() {
  local limit="$1"; shift
  local deadline=$((SECONDS + limit))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  return 1
}

have_tower() { [ -n "$(dock_tower_pane)" ]; }

# Who is expected to bring the dock back, which is not the same in both modes.
#
# In your own session the supervisor is the answer: it keeps a dock open, and
# waiting for it unattended is the behaviour worth testing. It watches only the
# default session's socket, though (`loopcmd.rs`, where `dock_workspaces` is
# filled in for the default backend alone), so on the disposable server nothing
# is watching and waiting would only ever time out. There recovery is driven by
# `herdr-tower dock` — which is no loss: the wedge that started this file was
# in that command, where a dead tower's token made it answer "already running"
# and do nothing.
recover() {
  if [ "$LIVE" = 1 ]; then
    wait_for "$TIMEOUT" have_tower
  else
    bounded 120 herdr-tower dock >/dev/null 2>&1
    wait_for 30 have_tower
  fi
}

recovery_driver() {
  if [ "$LIVE" = 1 ]; then printf 'the supervisor'; else printf 'herdr-tower dock'; fi
}

# ── setup and teardown ────────────────────────────────────────────────────────

cleanup() {
  local status=$?
  # A unit run cut short leaves a log of half a suite; nothing will read it.
  if [ -n "$UNIT_LOG" ]; then
    rm -f "$UNIT_LOG"
  fi
  if [ "$STARTED_SERVER" = 1 ] && [ "$KEEP" = 0 ]; then
    note "stopping the disposable server"
    HERDR_SOCKET_PATH="$SESSION_DIR/herdr.sock" herdr server stop >/dev/null 2>&1
    # The supervisor keeps a dock in every server it can see; give it a moment
    # to notice this one is gone before the directory disappears underneath it.
    sleep 1
    rm -rf "$SESSION_DIR"
  elif [ "$STARTED_SERVER" = 1 ]; then
    note "left running: HERDR_SOCKET_PATH=$SESSION_DIR/herdr.sock"
    note "stop it with: HERDR_SOCKET_PATH=$SESSION_DIR/herdr.sock herdr server stop"
  fi
  exit $status
}

preflight() {
  # What the herdr scenarios need. The unit suite has run by now and needs none
  # of it — `run_unit_tests` checks for `make` and `cargo` itself — so a fresh
  # clone with no herdr still gets its code checked before this can stop it.
  local missing=0
  for bin in herdr herdr-tower python3 git; do
    command -v "$bin" >/dev/null 2>&1 || { echo "missing: $bin" >&2; missing=1; }
  done
  [ "$missing" = 0 ] || exit 2

  INSTALLED_VERSION="$(herdr-tower version 2>/dev/null | head -1 | awk '{print $2}')"
  if [ -z "$INSTALLED_VERSION" ]; then
    echo "herdr-tower reports no version at all" >&2
    exit 2
  fi
  say "installed: $(herdr-tower version 2>/dev/null | head -1)"

  # 🚨 The installed binary is what is under test, not the working tree. A run
  # against a stale install passes or fails for the wrong build, which is worse
  # than not running at all — so say it out loud rather than guess.
  local head_desc
  head_desc="$(git -C "$REPO" describe --tags --always 2>/dev/null)"
  # Both sides stripped of the display `v`: `git describe` does not write one
  # unless a tag carries it, and comparing one stripped side against one
  # unstripped side made this fire on every run, including matching ones.
  local installed="${INSTALLED_VERSION#v}" head_bare="${head_desc#v}"
  if [ -n "$head_desc" ] && [ "$installed" != "$head_bare" ] &&
     [ "$installed" != "${head_bare}+dirty" ]; then
    say "${Y}note${Z}    the installed binary is not this working tree ($head_desc)."
    say "        run 'make install' first if you meant to test your changes."
  fi

  BOUND="$(command -v timeout || command -v gtimeout || true)"

  # 🚨 Only `--live` needs a server already running. The default mode promises
  # to start its own, and demanding one here turned a clean machine — exactly
  # the machine this is most worth running on — into an exit 2 before it ever
  # tried. A disposable server that fails to start is reported by `start_server`.
  if [ "$LIVE" = 1 ] && ! herdr status server >/dev/null 2>&1; then
    echo "--live needs a running herdr session, and none is reachable" >&2
    exit 2
  fi

  DAEMON_STATUS="$(herdr-tower daemon status 2>&1 | head -1)"
  case "$DAEMON_STATUS" in
    running*) ;;
    *) say "${Y}note${Z}    the supervisor is not running: $DAEMON_STATUS"
       say "        the recovery scenarios need it — 'herdr-tower daemon install'" ;;
  esac
}

start_server() {
  if [ "$LIVE" = 1 ]; then
    say ""
    say "${Y}Running against your real herdr session.${Z}"
    say "It will close your dock tab and kill your tower. Both are designed to"
    say "come back by themselves, and that is exactly what is being tested."
    if [ "$YES" = 0 ]; then
      printf 'continue? [y/N] '
      read -r answer
      case "$answer" in y|Y|yes) ;; *) echo "stopped."; exit 1 ;; esac
    fi
    say ""
    return
  fi

  if [ -S "$SESSION_DIR/herdr.sock" ] &&
     HERDR_SOCKET_PATH="$SESSION_DIR/herdr.sock" herdr status server >/dev/null 2>&1; then
    # 🚨 Not ours, so not ours to stop. An earlier `--keep` run left this for
    # somebody to look at; claiming it here meant the next ordinary run tore it
    # down and deleted its session directory.
    note "reusing the '$SESSION_NAME' server already running — it will be left alone"
  else
    note "starting a disposable herdr server ('$SESSION_NAME')"
    rm -rf "$SESSION_DIR"
    (herdr --session "$SESSION_NAME" server >/dev/null 2>&1 &)
    STARTED_SERVER=1
    local deadline=$((SECONDS + 30))
    while [ "$SECONDS" -lt "$deadline" ]; do
      [ -S "$SESSION_DIR/herdr.sock" ] && break
      sleep 1
    done
    if [ ! -S "$SESSION_DIR/herdr.sock" ]; then
      echo "the disposable server never came up; see $SESSION_DIR/herdr-server.log" >&2
      exit 2
    fi
  fi
  export HERDR_SOCKET_PATH="$SESSION_DIR/herdr.sock"

  # Nobody is asked to dock here. A fresh herdr server runs the
  # `herdr-tower.dock-autostart` plugin at startup, so the dock appearing at
  # all is itself the first thing under test — that plugin being installed and
  # working is what puts the tower behind the sidebar button on a new machine.
  note "waiting for the autostart plugin to open the dock (up to ${TIMEOUT}s)"
  if ! wait_for "$TIMEOUT" have_tower; then
    note "it did not; asking for a dock directly so the rest can still run"
    bounded 120 herdr-tower dock >/dev/null 2>&1
    wait_for 30 have_tower
  fi
}

# ── scenarios ─────────────────────────────────────────────────────────────────

# The unit suite, through the same entry point a person types. Every other
# scenario tests the *installed* binary against a live server; this one tests
# the working tree with no herdr at all. The two are not substitutes — every
# regression the scenarios below exist for shipped with this suite green, and
# the suite catches what no scenario reaches — so the gate is both.
run_unit_tests() {
  local name=unit-tests
  local tool
  for tool in make cargo; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      skip "$name" "$tool is not installed, so the code itself was not checked"
      return
    fi
  done
  local log tmp="${TMPDIR:-/tmp}"
  # A log that could not be created is said so, not dressed up as a build
  # failure with an empty path after it.
  if ! log="$(mktemp "${tmp%/}/herdr-tower-unit.XXXXXX")"; then
    fail "$name" "could not create a log file in $tmp — make test was not run"
    return
  fi
  # Kept on a failure, for reading; removed on a pass — and on an interrupted
  # run, where it holds half a suite that nobody asked to see (see `cleanup`).
  UNIT_LOG="$log"
  note "running make test — the whole workspace, a minute or two"
  # 🚨 Without herdr's environment. From a herdr pane `HERDR_ENV=1` is
  # inherited, and the workspace's live tests (`herdrx/tests/live.rs`,
  # `cli/tests/e2e.rs`, …) run whenever it is set — against whatever socket is
  # in reach, which during a full run is the disposable server. A dock that
  # failed to start there then reported as a failing unit test: a product
  # problem blamed on the code. Stripped, they skip, and this is the same suite
  # anywhere. `CARGO_TERM_COLOR=never` so a failing test's line can be read.
  #
  # And the exit status of `make` itself, not of a pipe through `grep` or
  # `tee`: a summary pipeline would report its own success over a failure.
  #
  # Nor git's: a `GIT_DIR` inherited from a hook overrides `git -C`, and the
  # fixtures would build their scratch repositories inside the real one. It is
  # cleared in a subshell, so nothing after this scenario loses it.
  if (
       for var in $(env | sed -n 's/^\(GIT_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$var"; done
       env -u HERDR_ENV -u HERDR_SOCKET_PATH -u HERDR_SESSION \
         -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID \
         CARGO_TERM_COLOR=never make -C "$REPO" test
     ) >"$log" 2>&1; then
    local total
    total="$(grep -oE '^test result: ok\. [0-9]+' "$log" | awk '{s += $4} END {print s + 0}')"
    note "$total passed"
    rm -f "$log"
    UNIT_LOG=""
    pass "$name"
    return
  fi
  # A finished run's log is the evidence; it stays.
  UNIT_LOG=""
  # Say what kind of failure it was. A failing test names itself; a test binary
  # that crashed (a stack overflow, a panic during unwind) names none and says
  # it did not exit successfully; a build error has an `error:` line and no
  # test result at all.
  local failing crashed built
  failing="$(grep -E '^test .* \.\.\. FAILED$' "$log" | sed 's/^test //; s/ \.\.\. FAILED$//' | head -5 | tr '\n' ' ')"
  crashed="$(grep -m1 "process didn't exit successfully" "$log")"
  built="$(grep -m1 -E '^error(\[E[0-9]+\])?:' "$log")"
  if [ -n "$failing" ]; then
    fail "$name" "failing: ${failing}— full output in $log"
  elif [ -n "$crashed" ]; then
    fail "$name" "a test binary crashed without naming a test: ${crashed} — full output in $log"
  elif [ -n "$built" ] && ! grep -q '^test result:' "$log"; then
    fail "$name" "the workspace did not build: ${built} — full output in $log"
  else
    fail "$name" "make test failed — full output in $log"
  fi
}

# The build stamp itself. Cheap, needs no server, and catches a build.rs that
# stopped answering — which is how a release once installed under the previous
# release's name.
run_version_stamp() {
  local name=version-stamp
  local line; line="$(herdr-tower version 2>/dev/null | head -1)"
  # 🚨 The exact word, matched on the parsed field. `version_label` emits a bare
  # `dev` for a build that could not name itself — no `v`, nothing after it — so
  # a pattern hunting for it mid-line misses the one spelling that occurs, and
  # this scenario passes the very build it exists to reject.
  if [ -z "$INSTALLED_VERSION" ] || [ "$INSTALLED_VERSION" = "dev" ]; then
    fail "$name" "the binary could not name its version: $line"; return
  fi
  if ! printf '%s' "$line" | grep -qE 'commit [0-9a-f]{7,}'; then
    fail "$name" "no commit in the version line: $line"; return
  fi
  # A version and a commit that disagree about dirt means one of the two
  # rerun-if-changed sets went stale — in one direction only.
  #
  # 🚨 A clean version beside a dirty commit is legitimate: `build.rs` leaves an
  # *injected* version alone (a pipeline naming a tag must not have the state of
  # whatever tree built it appended) while still marking the commit it read
  # from git. Flagging that failed a correctly built release. The reverse — a
  # dirty version beside a clean commit — has no legitimate cause.
  local v_dirty=0 c_dirty=0
  printf '%s' "$line" | awk '{print $2}' | grep -q '+dirty' && v_dirty=1
  printf '%s' "$line" | grep -qE 'commit [0-9a-f]+\+dirty' && c_dirty=1
  if [ "$v_dirty" = 1 ] && [ "$c_dirty" = 0 ]; then
    fail "$name" "the version says modified but the commit says clean: $line"; return
  fi
  pass "$name"
}

run_dock_appears() {
  local name=dock-appears
  local ws; ws="$(dock_workspace)"
  if [ -z "$ws" ]; then
    fail "$name" "no workspace labelled $DOCK_LABEL"; return
  fi
  local pane; pane="$(dock_tower_pane)"
  if [ -z "$pane" ]; then
    fail "$name" "workspace $ws exists but no tower is running in it"; return
  fi
  # 🚨 Exactly one tab. A dock that could not reuse its own pane used to open a
  # second, empty one beside itself and call that success.
  local tabs count
  tabs="$(workspace_tabs "$ws")"
  count="$(printf '%s\n' "$tabs" | grep -c .)"
  if [ "$count" != 1 ]; then
    fail "$name" "expected one tab, found $count: $(printf '%s' "$tabs" | tr '\n' ' ')"; return
  fi
  local label; label="$(printf '%s' "$tabs" | cut -f2)"
  if [ "$label" != "$DOCK_LABEL" ]; then
    fail "$name" "tab is labelled '$label', not '$DOCK_LABEL'"; return
  fi
  pass "$name"
}

# The version on the surface people actually open. It was hidden here for one
# release because the dock took a different branch through the header code.
run_dock_header_version() {
  local name=dock-header-version
  local pane; pane="$(dock_tower_pane)"
  if [ -z "$pane" ]; then skip "$name" "no tower running"; return; fi
  # 🚨 Waited for, not read once. A tower the autostart plugin has just
  # launched is still showing the shell's echo of its launch command
  # (`env HERDR_TOWER_DOCK=1 …`) for a moment, and a single read then failed a
  # correctly started tower — seen on a real run, and passing on the retry.
  local header="" deadline=$((SECONDS + 30))
  while [ "$SECONDS" -lt "$deadline" ]; do
    header="$(pane_header "$pane")"
    printf '%s' "$header" | grep -qF "$INSTALLED_VERSION" && break
    sleep 2
  done
  if [ -z "$header" ]; then
    fail "$name" "the tower pane rendered nothing"; return
  fi
  if ! printf '%s' "$header" | grep -qF "$INSTALLED_VERSION"; then
    fail "$name" "header does not carry $INSTALLED_VERSION: $header"; return
  fi
  pass "$name"
}

run_dock_tab_label_pinned() {
  local name=dock-tab-label-pinned
  local ws; ws="$(dock_workspace)"
  if [ -z "$ws" ]; then skip "$name" "no dock workspace"; return; fi
  local tab; tab="$(workspace_tabs "$ws" | head -1 | cut -f1)"
  if [ -z "$tab" ]; then skip "$name" "no tab to rename"; return; fi

  herdr tab rename "$tab" "smoke-renamed" >/dev/null 2>&1
  local restored=1
  local deadline=$((SECONDS + TIMEOUT))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if workspace_tabs "$ws" | grep -qF "$DOCK_LABEL"; then restored=0; break; fi
    sleep 2
  done
  if [ "$restored" != 0 ]; then
    herdr tab rename "$tab" "$DOCK_LABEL" >/dev/null 2>&1
    fail "$name" "the tab stayed 'smoke-renamed' for ${TIMEOUT}s"; return
  fi
  pass "$name"
}

# Closing the dock tab is how a person makes the dock go away, and the button
# in the sidebar has to bring it back.
run_dock_tab_close_recovers() {
  local name=dock-tab-close-recovers
  local ws; ws="$(dock_workspace)"
  if [ -z "$ws" ]; then skip "$name" "no dock workspace"; return; fi
  local tab; tab="$(workspace_tabs "$ws" | head -1 | cut -f1)"
  if [ -z "$tab" ]; then skip "$name" "no tab to close"; return; fi

  herdr tab close "$tab" >/dev/null 2>&1
  if ! recover; then
    fail "$name" "no tower came back after closing $tab ($(recovery_driver))"; return
  fi
  # And as one tab, not two.
  local ws2 count
  ws2="$(dock_workspace)"
  count="$(workspace_tabs "$ws2" | grep -c .)"
  if [ "$count" != 1 ]; then
    fail "$name" "it came back as $count tabs"; return
  fi
  pass "$name"
}

# 🚨 The regression this file was written for. A killed tower leaves its
# `dock-active` token behind; nothing revokes a token for a process that is
# gone. Believing it deadlocked the dock: the supervisor saw no tower and
# called `dock`, `dock` saw the token and did nothing, and the sidebar button
# stopped working until somebody cleared the token by hand.
run_dock_tower_kill_recovers() {
  local name=dock-tower-kill-recovers
  local pane; pane="$(dock_tower_pane)"
  if [ -z "$pane" ]; then skip "$name" "no tower running"; return; fi
  local ws; ws="$(dock_workspace)"
  local pid; pid="$(tower_pid "$pane")"

  # 🚨 SIGKILL, so the tower gets no chance to tidy up. Under SIGTERM a tower
  # that cleared its own token would leave this scenario exercising ordinary
  # clean-shutdown recovery under the name of the stale-token regression — and
  # a dock that still wedges on a stale token could ship green.
  kill -9 "$pid" 2>/dev/null
  sleep 3

  local token; token="$(pane_token "$ws" "$pane" dock-active)"
  if [ -z "$token" ]; then
    # Not a pass. Whatever removed the token, the path this scenario is named
    # for was not taken, and saying so is the only honest outcome.
    skip "$name" "no stale dock-active token survived the kill — the wedge path was not exercised"
    return
  fi

  if ! recover; then
    fail "$name" "no tower came back after killing pid $pid via $(recovery_driver) (stale token: ${token:-none})"
    return
  fi
  pass "$name"
}

# The same header, opened the other way. The popup and the dock are one binary
# and should differ in nothing a person reads.
run_popup_header_version() {
  local name=popup-header-version
  local ws; ws="$(dock_workspace)"
  if [ -z "$ws" ]; then skip "$name" "no workspace to open a pane in"; return; fi
  local dock_pane; dock_pane="$(dock_tower_pane)"
  if [ -z "$dock_pane" ]; then skip "$name" "no dock pane to split"; return; fi

  local pane
  pane="$(herdr pane split "$dock_pane" --direction down --no-focus 2>/dev/null | jpy '
import json,sys
print(json.load(sys.stdin).get("result",{}).get("pane",{}).get("pane_id",""))
')"
  if [ -z "$pane" ]; then skip "$name" "could not open a pane"; return; fi

  herdr pane run "$pane" "herdr-tower" >/dev/null 2>&1
  # 🚨 Waiting for the version, not for the word "herdr-tower". A half-painted
  # header carries the name before it carries anything else, so breaking on the
  # name read a frame that was still being drawn and called the version missing.
  local header="" deadline=$((SECONDS + 30))
  while [ "$SECONDS" -lt "$deadline" ]; do
    header="$(pane_header "$pane")"
    printf '%s' "$header" | grep -qF "$INSTALLED_VERSION" && break
    sleep 2
  done
  herdr pane close "$pane" >/dev/null 2>&1

  if [ -z "$header" ]; then
    fail "$name" "the popup rendered nothing in 30s"; return
  fi
  if ! printf '%s' "$header" | grep -qF "$INSTALLED_VERSION"; then
    fail "$name" "popup header does not carry $INSTALLED_VERSION: $header"; return
  fi
  pass "$name"
}

# A pause that never lifts is the bug. A pause on an agent that is dead or
# waiting is correct and must not be reported. So: cross the paused loops
# against the agent states, and complain only about a pause whose reason the
# agent no longer matches.
run_loop_pause_lifts() {
  local name=loop-pause-lifts
  local loops; loops="$(herdr-tower loop list 2>/dev/null)"
  case "$loops" in
    ""|"no idle loops are configured."*) skip "$name" "no idle loops are configured"; return ;;
  esac
  local paused; paused="$(printf '%s\n' "$loops" | grep 'paused:')"
  if [ -z "$paused" ]; then pass "$name"; return; fi

  # 🚨 Judged by the pause's own reason, not by the agent's state alone.
  # `lift_pause` lifts a pause that merely *described a state* as soon as the
  # agent is demonstrably not in that state — but a pause from the delivery
  # cap, a failed delivery, or an explicit `loop blocked` is answered only by a
  # turn that comes after it, and this test cannot force one. Flagging those
  # would make a correctly paused loop fail the suite.
  # 🚨 Not through `jpy`, whose `2>/dev/null` hides a python error — and an
  # empty answer here reads as "nothing is stuck". A check that did not run is
  # a failure, never a pass: this is the one scenario whose skip is forgiven, so
  # between the two a broken pause-lift could never turn the suite red.
  local stuck rc
  stuck="$(printf '%s\n' "$paused" | python3 -c '
import json,subprocess,sys

# The reasons that describe a state, and the state each one claims. From
# `loopcmd.rs`: PAUSED_WAITING / PAUSED_ERROR / PAUSED_MISSING /
# PAUSED_SESSION_UNKNOWN are what `describes_a_state` covers.
CLAIMS = {
    "agent is waiting for input": {"WAITING"},
    "agent is in error": {"ERROR"},
    "agent is missing": {"MISSING", "DEAD"},
    "agent session identity is unavailable": {"MISSING", "DEAD"},
    # A dead agent keeps its pause: `lift_pause` refuses to clear while
    # `saw_dead` stands, whatever else moved.
    "agent is dead": None,
    # Answered only by a turn after it. Not judged here.
    "agent requested user input": None,
}

rows = [l for l in sys.stdin.read().splitlines() if l.strip()]
try:
    got = subprocess.run(["herdr-tower", "status", "--json"],
                         capture_output=True, text=True)
    if got.returncode != 0:
        raise RuntimeError("status --json exited %d" % got.returncode)
    agents = {a["id"]: (a.get("state") or "").upper()
              for a in json.loads(got.stdout)}
except Exception as e:
    print("could not read agent states: %s" % e, file=sys.stderr)
    sys.exit(3)

for row in rows:
    ident = row.split()[0]
    reason = row.split("paused:", 1)[1].strip() if "paused:" in row else ""
    if reason not in CLAIMS:
        continue          # attempts spent, delivery failed: waits for a turn
    claimed = CLAIMS[reason]
    if claimed is None:
        continue          # not answerable by state
    state = agents.get(ident)
    if state is None:
        continue          # the loop outlived the agent; the pause is right
    if state not in claimed:
        print("%s is paused for %r but its agent is %s" % (ident, reason, state))
')"
  rc=$?
  if [ "$rc" != 0 ]; then
    fail "$name" "the check itself did not run (exit $rc) — nothing was verified"; return
  fi
  if [ -n "$stuck" ]; then
    fail "$name" "$(printf '%s' "$stuck" | tr '\n' ';')"; return
  fi
  # `grep -c .`, not `wc -l`: command substitution stripped the trailing
  # newline, so `wc -l` reported N-1 — "0 loop(s)" for the one it examined.
  note "paused, and correctly so: $(printf '%s\n' "$paused" | grep -c .) loop(s)"
  pass "$name"
}

# ── run ───────────────────────────────────────────────────────────────────────

should_run() {
  # Not run is not skipped: `--no-unit` means the caller already ran the suite,
  # so it is neither a pass nor something missing from this run.
  if [ "$1" = unit-tests ] && [ "$NO_UNIT" = 1 ]; then
    return 1
  fi
  [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]
}

main() {
  say "herdr-tower smoke test"
  say ""
  # 🚨 One cleanup, on EXIT, and the signals only set the status it exits with.
  # With `cleanup` bound to INT and TERM as well, a signal arriving while a
  # foreground `sleep` runs is deferred until that child finishes normally —
  # `$?` is then 0, cleanup exits 0, and the EXIT trap fires a second time, so
  # an interrupted run reports success.
  trap 'exit 130' INT
  trap 'exit 143' TERM
  trap cleanup EXIT

  if [ "$NO_UNIT" = 1 ] && [ "$ONLY" = unit-tests ]; then
    echo "--no-unit and --only unit-tests ask for nothing" >&2
    exit 2
  fi
  if [ -n "$ONLY" ]; then
    local known=0
    for s in "${SCENARIOS[@]}"; do [ "$s" = "$ONLY" ] && known=1; done
    if [ "$known" = 0 ]; then
      echo "unknown scenario: $ONLY" >&2
      printf 'known: %s\n' "${SCENARIOS[*]}" >&2
      exit 2
    fi
  fi

  # 🚨 The unit suite first, before anything herdr is touched. It needs no
  # server and no installed binary, so running it after preflight and a server
  # spin-up made a broken tree wait a minute or more to be reported — and made
  # a machine without herdr exit in preflight without ever running it.
  if should_run unit-tests; then
    say "unit tests: this working tree ($REPO)"
    run_unit_tests
    say ""
  fi
  if [ "$ONLY" = "unit-tests" ]; then
    finish
    return
  fi

  preflight
  start_server
  if [ "$LIVE" = 1 ]; then
    say "target: your own session ($(herdr status server 2>/dev/null | grep socket || echo 'default socket'))"
  else
    say "target: disposable session '$SESSION_NAME'"
  fi
  say ""

  should_run version-stamp            && run_version_stamp
  should_run dock-appears             && run_dock_appears
  should_run dock-header-version      && run_dock_header_version
  should_run dock-tab-label-pinned    && run_dock_tab_label_pinned
  should_run dock-tab-close-recovers  && run_dock_tab_close_recovers
  should_run dock-tower-kill-recovers && run_dock_tower_kill_recovers
  should_run popup-header-version     && run_popup_header_version
  should_run loop-pause-lifts         && run_loop_pause_lifts
  say ""
  finish
}

# The verdict line and the exit status.
finish() {
  if [ "$FAILED" -gt 0 ]; then
    say "${R}$FAILED failed${Z}, $PASSED passed, $SKIPPED skipped"
    for f in "${FAILURES[@]}"; do say "  - $f"; done
    return 1
  fi
  # 🚨 A skip is not a pass, and the exit status has to say so. Only
  # `$OPTIONAL_SCENARIOS` may skip quietly — and not even those when one was
  # asked for by name, because then it is the whole run. Without this,
  # `./TEST00.SH --only loop-pause-lifts` on a machine with no loops configured
  # exercised nothing and exited 0, and `make smoke` read green.
  if [ "$SKIPPED" -gt 0 ] && [ "$UNMET" -gt 0 ]; then
    say "${R}$UNMET required scenario(s) skipped${Z}, $PASSED passed"
    for f in "${SKIPS[@]}"; do say "  - $f"; done
    return 1
  fi
  say "${G}$PASSED passed${Z}, $SKIPPED skipped"
  return 0
}

main
