// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower wt <command>` — the command line over the worktree lifecycle.
//!
//! Nothing here decides anything. Every rule about worktrees lives in `tower_wt`
//! (record before you build, check before you delete, no automatic merge); this
//! file parses a subcommand, calls one function, and writes the result down.
//! Keeping it that thin is what lets the TUI drive the same operations without
//! reimplementing the guards.
//!
//! The usage block is frozen in `expected/wt-help.txt`: `herdr-tower wt` with no
//! arguments is how a person discovers the subcommands, and
//! `tests/helpcmd_golden.rs` holds it to the recorded bytes.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use herdrx::Herdr;
use time::OffsetDateTime;
use tower_state::Store;

use crate::helpcmd::flags::{FlagError, FlagSet, Parsed};
use crate::shorten_home;
use crate::waitcmd::{print_wait_results, wait_tasks, DEFAULT_WAIT_TIMEOUT};

/// What `herdr-tower wt` prints with no subcommand.
pub const WT_USAGE: &str = r##"Usage: herdr-tower wt <command>

  new <task>     a worktree, a branch, a window and an agent in it
                 [--agent claude|codex|gemini|opencode] [--repo path] [--base branch] [--test 'command']
  list           the tasks and where they stand
  diff <task>    what changed against base
  test <task>    run the tests and record the result  [--cmd 'command']
  review <task>  write a review file (a diff you annotate with "#> comment")
  send <task>    send the review comments to the agent as changes to make
  wait <task>…   wait for the workers to finish (several at once)  [--timeout 30m]
  drive <task>…  wait, test, hand failures back, until it is merge-ready or stuck
                 [--attempts 2] [--timeout 30m] [--cmd 'command']
  merge <task>   show the merge and run it once confirmed (never automatic)
  clean <task>   tidy up, keeping anything unsafe (refuses while uncommitted or unmerged)
"##;

/// How long the startup-question watcher keeps looking, and how often.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(40);
const PROMPT_INTERVAL: Duration = Duration::from_millis(500);

type Failure = Box<dyn std::error::Error>;

/// The body of `herdr-tower wt ...`.
///
/// `now` is threaded in rather than read here, as in every other crate in this
/// tree: the clock is read once at the entry point so nothing below has a
/// hidden dependency on the wall clock.
///
/// `W: Send` because `tower_wt::run_test` streams a child's stdout and stderr from
/// two threads into one writer; nothing else here needs it.
pub fn run_wt<W: Write + Send>(
    w: &mut W,
    state_dir: &Path,
    store: &Store,
    h: &Herdr,
    args: &[String],
    now: OffsetDateTime,
) -> Result<(), Failure> {
    let Some((cmd, rest)) = args.split_first() else {
        write!(w, "{WT_USAGE}")?;
        return Ok(());
    };

    match cmd.as_str() {
        "new" => {
            let fs = FlagSet::new("wt new")
                .string("agent", "claude", "which agent")
                .string("repo", ".", "the repository")
                .string("base", "", "base branch (default: the current one)")
                .string("test", "", "test command");
            let (task, p) = task_and_flags(&fs, rest)?;
            let m = tower_wt::new_task(
                state_dir,
                &tower_wt::NewOptions {
                    task: &task,
                    repo: p.str("repo"),
                    base: p.str("base"),
                    agent: p.str("agent"),
                    test_cmd: p.str("test"),
                    window: Some(h),
                    // The pane asking is the coordinator the tower draws the
                    // worker under.
                    parent: herdrx::current_pane_id().map(|pane_id| tower_wt::Parent {
                        backend: crate::muxsel::default_backend_id(),
                        pane_id,
                    }),
                },
                now,
            )?;
            write!(
                w,
                "task {} started\n  worktree: {}\n  branch:   {} (base {})\n  agent:    {} (window {:?})\n",
                m.task,
                shorten_home(&m.path),
                m.branch,
                m.base,
                m.agent,
                m.task
            )?;
            Ok(())
        }

        "list" => {
            let metas = tower_wt::list_metas(state_dir)?;
            if metas.is_empty() {
                writeln!(
                    w,
                    "no worktree tasks — start one with herdr-tower wt new <task>."
                )?;
                return Ok(());
            }
            let mut rows = vec![[
                "TASK", "AGENT", "BRANCH", "DIRTY", "UNMERGED", "TEST", "DRIVE", "PATH",
            ]
            .map(String::from)
            .to_vec()];
            for m in &metas {
                // A git failure here is a column, not a crash: `wt list` is what
                // a person runs to find out what state things are in, and a
                // repository that cannot answer is part of that state.
                //
                // `?` rather than `0`, though. Folding the failure into the same
                // value a clean tree produces said "nothing uncommitted, nothing
                // unmerged" about a worktree that was moved, or whose base ref is
                // gone — which reads as ready to clean or merge. The two cases
                // have to look different.
                let dirty_paths = tower_wt::dirty(&m.path);
                let dirty = dirty_paths
                    .as_ref()
                    .map(|d| d.len().to_string())
                    .unwrap_or_else(|_| "?".into());
                // The branch the worktree is on now: a worker that moved to a
                // branch of its own has its commits there, and counting the
                // one `wt new` cut would say `0` about work that is not merged.
                let live = m.live_branch();
                let unmerged = tower_wt::unmerged(&m.repo, &m.base, &live)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|_| "?".into());
                let branch = if live == m.branch {
                    live
                } else {
                    format!("{live} (cut as {})", m.branch)
                };
                let test = match m.test_pass {
                    Some(true) => "✔",
                    Some(false) => "✖",
                    None => "—",
                };
                // The column a drive writes to, and the one to read when
                // coming back to a machine that worked overnight: `merge-ready`
                // is the queue this command exists to fill.
                // 🚨 And still about this tree. A verdict recorded against a
                // commit that has since been replaced — the worker prompted
                // directly, an amend, a manual commit — says nothing about what
                // would merge now, and `merge-ready` is the worst of those to
                // keep advertising.
                // 🚨 Answered from the readings above, not from two more git
                // calls. The tree was just read for the DIRTY column; reading
                // it again doubled the subprocesses per row and let one row
                // print `0` dirty beside a `superseded` taken from a later
                // look at the same tree.
                let drive = m
                    .drive
                    .as_ref()
                    .map(|d| {
                        let head = tower_wt::head(&m.path).ok();
                        let clean = dirty_paths.as_ref().ok().map(|p| p.len());
                        if d.describes_at(OffsetDateTime::now_utc(), head.as_deref(), clean) {
                            d.state.label()
                        } else {
                            "superseded"
                        }
                    })
                    .unwrap_or("—");
                rows.push(vec![
                    m.task.clone(),
                    m.agent.clone(),
                    branch,
                    dirty,
                    unmerged,
                    test.to_string(),
                    drive.to_string(),
                    shorten_home(&m.path),
                ]);
            }
            write!(w, "{}", columns(&rows))?;
            Ok(())
        }

        "diff" => {
            let task = one_task(rest)?;
            let m = tower_wt::load_meta(state_dir, task)?;
            writeln!(w, "{}", tower_wt::diff(&m.path, &m.base)?)?;
            Ok(())
        }

        "test" => {
            let fs = FlagSet::new("wt test").string("cmd", "", "test command (recorded)");
            let (task, p) = task_and_flags(&fs, rest)?;
            let pass = tower_wt::run_test(w, state_dir, &task, p.str("cmd"), now)?;
            writeln!(
                w,
                "{}",
                if pass {
                    "✔ tests passed"
                } else {
                    "✖ tests failed"
                }
            )?;
            if !pass {
                // The result and the output are recorded either way; this is the
                // exit code. A CI or orchestration script reading zero here would
                // treat a failed suite as a passing one, whatever the metadata
                // said.
                return Err(format!("{task} tests failed").into());
            }
            Ok(())
        }

        "review" => {
            let task = one_task(rest)?;
            let path = tower_wt::write_review_file(state_dir, task)?;
            write!(
                w,
                "review file: {}\nOpen it, write \"#> your comment\" lines, then send them with herdr-tower wt send {task}.\n",
                shorten_home(&path.to_string_lossy())
            )?;
            Ok(())
        }

        "send" => {
            let task = one_task(rest)?;
            let n = match tower_wt::send_comments(state_dir, task, store, h) {
                Ok(n) => n,
                // Taken, but no start seen: say what that means rather than
                // herdr's code.
                Err(tower_wt::Error::Herdr(e))
                    if e.is_code("agent_prompt_stalled") || e.is_code("timeout") =>
                {
                    return Err(format!(
                        "the worker was not seen starting on the comments ({e}) — look at its screen (it may have exited, to update itself say) and send again"
                    )
                    .into());
                }
                Err(e) => return Err(e.into()),
            };
            writeln!(w, "sent {n} comments to the agent.")?;
            Ok(())
        }

        "drive" => {
            let fs = FlagSet::new("wt drive")
                .duration(
                    "timeout",
                    DEFAULT_WAIT_TIMEOUT,
                    "how long to wait each time",
                )
                .string("cmd", "", "test command (recorded)")
                .string(
                    "attempts",
                    &tower_wt::DEFAULT_ATTEMPTS.to_string(),
                    "test runs before giving up",
                );
            let p = parse(w, &fs, rest)?;
            let tasks = p.args();
            if tasks.is_empty() {
                return Err("Usage: herdr-tower wt drive [--attempts 2] [--timeout 30m] [--cmd 'command'] <task> [<task> ...]".into());
            }
            let attempts: u32 = p
                .str("attempts")
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| "--attempts must be a positive integer".to_string())?;
            // 🚨 `FlagSet::parse` stops at the first positional argument, which
            // every other subcommand here also relies on. Said plainly rather
            // than left to fail later as "no such task: --attempts".
            // 🚨 One thread per task, and two threads on one task would share
            // its worktree, its test command and its record — including
            // `save_meta`'s temporary file, whose name is derived from the task
            // and this process's id, so one thread's rename can delete the file
            // the other is still writing.
            let mut seen = std::collections::HashSet::new();
            if let Some(dup) = tasks.iter().find(|t| !seen.insert((*t).clone())) {
                return Err(format!("{dup} is named twice — drive each task once").into());
            }
            if let Some(flag) = tasks.iter().find(|t| t.starts_with('-')) {
                return Err(format!(
                    "{flag} looks like a flag but follows a task name — put every flag before the tasks"
                )
                .into());
            }
            // 🚨 Together, not one after another. Driving them in turn means
            // the first task's two-hour wait is two hours in which a later
            // worker that already failed hears nothing, and an overnight run
            // costs the *sum* of every task's delays — which is the opposite of
            // what driving several tasks is for.
            //
            // The writer is shared under a lock and every line is prefixed with
            // its task, so interleaved output stays readable. A lock held only
            // for the duration of one write is not contention worth avoiding
            // here: these are terminal writes between minutes of waiting.
            let shared = std::sync::Mutex::new(w);
            let cmd = p.str("cmd").to_string();
            let timeout = p.duration("timeout");
            // The refusal crosses a thread boundary, and `Failure` is a boxed
            // `dyn Error` which is not `Send`. Worded here, where the task it
            // belongs to is still in hand.
            let outcomes: Vec<(String, Result<tower_wt::Drive, String>)> =
                std::thread::scope(|scope| {
                    let handles: Vec<_> = tasks
                        .iter()
                        .map(|task| {
                            let (shared, cmd) = (&shared, cmd.as_str());
                            scope.spawn(move || {
                                let mut out = Prefixed::new(shared, task);
                                let got = drive_one(
                                    &mut out, state_dir, store, h, task, cmd, attempts, timeout,
                                )
                                .map_err(|e| e.to_string());
                                (task.clone(), got)
                            })
                        })
                        .collect();
                    // 🚨 A panicking thread is that task's failure, not the
                    // command's. Joined with `expect`, the first panic unwound
                    // `thread::scope` and threw away every other task's
                    // verdict — after an overnight run of four, one bad unwrap
                    // meant the operator saw a Rust panic instead of the three
                    // results that had been computed.
                    handles
                        .into_iter()
                        .map(|hnd| match hnd.join() {
                            Ok(got) => got,
                            Err(_) => (String::new(), Err("the drive thread panicked".to_string())),
                        })
                        .collect()
                });

            let mut failed = Vec::new();
            for (task, got) in outcomes {
                match got {
                    Ok(drive) if drive.state == tower_wt::DriveState::MergeReady => {}
                    // A refusal is reported with the task it belongs to and
                    // does not cancel the others: they have been running for
                    // hours too, and their verdicts are already recorded.
                    Ok(_) => failed.push(task),
                    Err(e) => failed.push(format!("{task} ({e})")),
                }
            }
            if !failed.is_empty() {
                return Err(format!("not merge-ready: {}", failed.join(", ")).into());
            }
            Ok(())
        }

        // Internal, and no longer spawned by anything. herdr starts the agent
        // itself and reports a startup question such as "do you trust this
        // folder?" as *blocked*, so nothing has to answer it by reading the
        // screen. The subcommand survives because the function does: somebody staring at a worker stuck on a trust dialog can still
        // run it by hand.
        "accept-prompts" => {
            let [pane_id] = rest else {
                return Err("Usage: herdr-tower wt accept-prompts <pane-id>".into());
            };
            let n = tower_wt::accept_startup_prompts(
                |pane, lines| h.capture_pane(pane, lines).ok(),
                |pane| h.send_keys(pane, &["Enter"]).is_ok(),
                pane_id,
                PROMPT_TIMEOUT,
                PROMPT_INTERVAL,
                std::thread::sleep,
            );
            if n > 0 {
                writeln!(w, "answered {n} startup prompts ({pane_id})")?;
            }
            Ok(())
        }

        "wait" => {
            let fs = FlagSet::new("wt wait").duration(
                "timeout",
                DEFAULT_WAIT_TIMEOUT,
                "how long to wait",
            );
            // The task names may come before the flags as well as after them:
            // `wt wait hero --timeout 1h` is how people (and the orchestration
            // skill) write it, and the flag parser alone stops at the first
            // word that is not a flag — so that spelling read `--timeout` as a
            // task and failed at once with "--timeout did not finish".
            let lead = rest.iter().take_while(|a| !a.starts_with('-')).count();
            let p = parse(w, &fs, &rest[lead..])?;
            let tasks: Vec<String> = rest[..lead].iter().chain(p.args()).cloned().collect();
            if tasks.is_empty() {
                return Err(
                    "Usage: herdr-tower wt wait [--timeout 30m] <task> [<task> ...]".into(),
                );
            }
            let results = wait_tasks(
                store,
                h,
                state_dir,
                &tasks,
                p.duration("timeout"),
                &OffsetDateTime::now_utc,
            );
            print_wait_results(w, &results)?;
            // The results are printed first and *then* the failure is raised, so
            // a script's operator can see which of several tasks went wrong.
            if let Some(r) = results.iter().find(|r| r.error.is_some()) {
                return Err(format!("{} did not finish", r.task).into());
            }
            Ok(())
        }

        "merge" => {
            let fs = FlagSet::new("wt merge").bool("yes", false, "do not ask");
            let (task, p) = task_and_flags(&fs, rest)?;
            let yes = p.bool("yes");
            // Echoing the `y` under `--yes` keeps the transcript readable: the
            // guide prints a question, and a log with no answer in it reads as a
            // merge that happened unasked.
            //
            // The guide writes the question and the confirmation writes the
            // answer, into one stream. Rust cannot hand the same `&mut W` to
            // both, so the borrow is passed back and forth through a cell — see
            // [`Shared`].
            let cell = std::cell::RefCell::new(w);
            let confirm = || {
                if yes {
                    let mut echo = Shared(&cell);
                    let _ = writeln!(echo, "y (--yes)");
                    return true;
                }
                yes_from_stdin()
            };
            let mut sink = Shared(&cell);
            tower_wt::merge_guide(&mut sink, state_dir, &task, confirm)?;
            Ok(())
        }

        "clean" => {
            let task = one_task(rest)?;
            tower_wt::clean(state_dir, task)?;
            writeln!(
                w,
                "task {task} cleaned up (worktree, branch and metadata removed)"
            )?;
            Ok(())
        }

        other => {
            write!(w, "{WT_USAGE}")?;
            Err(format!("unknown wt command: {other}").into())
        }
    }
}

/// A writer that the merge guide and its confirmation can both hold.
///
/// The two never write at the same moment — the guide finishes printing its
/// question before it asks — so the runtime borrow can never conflict. It is a
/// `RefCell` rather than a plain split because the guide's writer and the
/// callback are separate parameters of `tower_wt::merge_guide`, and there is exactly
/// one `&mut W` to go around.
struct Shared<'a, 'w, W: Write>(&'a std::cell::RefCell<&'w mut W>);

impl<W: Write> Write for Shared<'_, '_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// Reads one `y` off stdin.
///
/// Anything else is a no, including end of input: a merge is the one operation
/// here that rewrites the user's own branch, so silence must not read as
/// consent.
fn yes_from_stdin() -> bool {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok();
    line.trim().eq_ignore_ascii_case("y")
}

/// Records that nothing was concluded, under the same lock as every other
/// verdict.
///
/// 🚨 These three exits used `record` directly, which neither takes the record
/// lock nor re-reads the worker — so a `wt send` that had just cleared the
/// stored verdict got an `unfinished` written back over it, and `wt list`
/// showed `unfinished` for a task whose worker was at that moment working. It
/// is the same stale-verdict advertisement `record_for_if` exists to stop,
/// left open on the exits nobody was looking at.
///
/// The head is read fresh here on purpose: unlike a verdict reached by testing,
/// this one says only "no conclusion about the tree as it stands".
fn unfinished_verdict(
    store: &Store,
    state_dir: &Path,
    task: &str,
    attempts: u32,
    failure: &str,
    now: OffsetDateTime,
) -> Result<tower_wt::Drive, Failure> {
    let m = tower_wt::load_meta(state_dir, task)?;
    let head = tower_wt::head(&m.path).unwrap_or_default();
    let expected = crate::waitcmd::agent_for_task(store, state_dir, task)
        .ok()
        .map(|a| tower_wt::Worker::of(&a));
    let recorded = match expected {
        Some(worker) => tower_wt::record_for_if(
            state_dir,
            task,
            tower_wt::DriveState::Unfinished,
            attempts,
            failure,
            &head,
            worker,
            || {
                crate::waitcmd::agent_for_task(store, state_dir, task)
                    .ok()
                    .map(|a| tower_wt::Worker::of(&a))
            },
            now,
        )
        .map_err(Failure::from)?,
        // No worker to compare against — there is nothing a concurrent
        // dispatch could have moved, and refusing to record would lose the
        // only account of why this drive stopped.
        None => Some(
            tower_wt::record_for(
                state_dir,
                task,
                tower_wt::DriveState::Unfinished,
                attempts,
                failure,
                &head,
                now,
            )
            .map_err(Failure::from)?,
        ),
    };
    Ok(recorded.unwrap_or_else(|| superseded(attempts, &head, now)))
}

/// Publishes a verdict only while the worker is still the one that was tested.
///
/// `Ok(None)` means new work reached it first, so nothing was written: the
/// dispatch owns the record now, and a verdict about the turn before it would
/// be advertising work that has already been superseded.
#[allow(clippy::too_many_arguments)]
fn publish_verdict(
    store: &Store,
    state_dir: &Path,
    task: &str,
    state: tower_wt::DriveState,
    attempts: u32,
    failure: &str,
    head: &str,
    expected: tower_wt::Worker,
) -> Result<Option<tower_wt::Drive>, Failure> {
    tower_wt::record_for_if(
        state_dir,
        task,
        state,
        attempts,
        failure,
        head,
        expected,
        || {
            crate::waitcmd::agent_for_task(store, state_dir, task)
                .ok()
                .map(|a| tower_wt::Worker::of(&a))
        },
        OffsetDateTime::now_utc(),
    )
    .map_err(Failure::from)
}

/// What the drive reports when new work took the record out from under it.
///
/// Nothing is written — the dispatch has cleared the verdict and owns what
/// happens next — so this value is only what this call returns to its caller.
fn superseded(attempts: u32, head: &str, now: OffsetDateTime) -> tower_wt::Drive {
    tower_wt::Drive {
        state: tower_wt::DriveState::Unfinished,
        attempts,
        failure: String::new(),
        head: head.to_string(),
        at: now,
    }
}

/// Drives one task: wait for the worker to settle, run its tests, hand a
/// failure back, and go round again until there is a verdict.
///
/// Reports as it goes rather than at the end. A drive can take hours, and the
/// person who comes back to the terminal wants to see which attempt it is on,
/// not a single line saying it failed.
///
/// The loop counts *test runs*, not laps: waiting is not an attempt, so a
/// worker that settles twice without the tests ever running cannot burn the
/// budget.
#[allow(clippy::too_many_arguments)]
fn drive_one<W: Write + Send>(
    w: &mut W,
    state_dir: &Path,
    store: &Store,
    h: &Herdr,
    task: &str,
    cmd_override: &str,
    cap: u32,
    timeout: Duration,
) -> Result<tower_wt::Drive, Failure> {
    // 🚨 A drive's terminal is a courtesy; its record is the result. Every status
    // line below was written with `?`, so a terminal that went away mid-run —
    // a closed pane, a pipe whose reader exited — ended the drive at the next
    // line it printed: after dispatching a retry nothing would ever test, or
    // after recording `merge-ready` with an exit that called it a failure.
    // Swallowing one write, as an earlier fix did, only moved the abort to the
    // write after it. From here on nothing written can fail.
    let w = &mut Resilient::new(w);
    // 🚨 This drive's own failures only. A record left by a run days ago is not
    // evidence about this one: the person starting it again has usually changed
    // something, and inheriting that fingerprint would call the first failure a
    // repeat and stop before the worker had been told anything at all.
    let mut previous: Option<tower_wt::Drive> = None;
    let mut attempts = 0;
    let only = [task.to_string()];
    loop {
        // 🚨 A worker that has already finished and been *read* is `Idle`, not
        // `DoneUnread` — `Store::mark_read` moves it there when a person opens
        // the row. `wait_tasks` does not count `Idle` as an outcome (it cannot:
        // that is also what a worktree looks like before its first turn), so
        // waiting here would sit out the whole timeout for a transition that
        // already happened and then report `unfinished`. The turn counter tells
        // the two apart, and on the first lap a settled worker that has taken a
        // turn is tested as it stands.
        //
        // Only on the first lap. After a retry has been dispatched the whole
        // point is to wait for the turn it asked for.
        let settled_already = attempts == 0
            && crate::waitcmd::agent_for_task(store, state_dir, task).is_ok_and(|a| {
                a.work_seq > 0
                    && match a.state {
                        // Finished and nobody has looked: unambiguous.
                        tower_state::AgentState::DoneUnread => true,
                        // 🚨 `Idle` is two situations. A completion somebody
                        // read is one; a turn dispatched moments ago whose
                        // start has not been observed yet is the other, and
                        // `note_dispatch` marks exactly that by clearing
                        // `done_acked`. Testing the second would grade the tree
                        // as it was before the edits were asked for.
                        tower_state::AgentState::Idle => a.done_acked,
                        _ => false,
                    }
            });
        let results = if settled_already {
            Vec::new()
        } else {
            wait_tasks(
                store,
                h,
                state_dir,
                &only,
                timeout,
                &OffsetDateTime::now_utc,
            )
        };
        // 🚨 After the wait, not before it. That wait can consume the whole
        // per-lap timeout — hours — and a stamp taken in front of it would put
        // `test_at`, which the merge guide shows, that far before the run it
        // describes. Each terminal verdict below reads the clock again, because
        // the test takes time too.
        let now = OffsetDateTime::now_utc();
        print_wait_results(w, &results)?;
        if let Some(r) = results.iter().find(|r| r.error.is_some()) {
            // Nothing was concluded about the work itself, and saying so is the
            // point: a drive that reports failure here would read as "the tests
            // failed", which nobody checked.
            let reason = r.error.clone().unwrap_or_default();
            writeln!(w, "{task}: {reason} — nothing was tested.")?;
            let drive = unfinished_verdict(
                store,
                state_dir,
                task,
                attempts,
                &previous.map(|d| d.failure).unwrap_or_default(),
                now,
            )?;
            return Ok(drive);
        }

        // 🚨 `wait_tasks` counts `Waiting` as an outcome — it is a state a
        // person can act on — but for a drive it means the turn is unfinished:
        // the worker is holding a question. Testing now would grade a
        // half-applied tree, and a failure could not even be handed back
        // (`agent.prompt` refuses to type into an approval dialog).
        //
        // 🚨 `Waiting` is what this rejects — not "anything but `DoneUnread`".
        // A wait that returns without an error has seen the turn end, and the
        // record can have settled to `Idle` by then: the backend reconciles a
        // completion herdr reports as `idle`, and `Store::mark_read` moves the
        // row there the moment somebody opens it in the tower. From the second
        // lap on — `settled_already` is gated on `attempts == 0` and so cannot
        // cover it — a worker that had finished the fix was announced as
        // "waiting on something", and the drive abandoned the retry it had
        // just dispatched without ever testing it.
        if results
            .iter()
            .any(|r| r.state == Some(tower_state::AgentState::Waiting))
        {
            writeln!(
                w,
                "{task}: the worker is waiting on something — nothing was tested."
            )?;
            let drive = unfinished_verdict(
                store,
                state_dir,
                task,
                attempts,
                &previous.map(|d| d.failure).unwrap_or_default(),
                now,
            )?;
            return Ok(drive);
        }

        attempts += 1;
        let path = tower_wt::load_meta(state_dir, task)?.path;
        let before = tower_wt::Checkout::read(&path)?;
        // 🚨 And who the worker was. A worker that cannot be inspected cannot
        // be shown to have held still, so this is an error rather than a
        // default — the same reason `Checkout::read` is.
        let worker_before = tower_wt::Worker::of(
            &crate::waitcmd::agent_for_task(store, state_dir, task).map_err(Failure::from)?,
        );
        writeln!(w, "{task}: running tests (attempt {attempts}/{cap})")?;
        let mut log = Tee::new(w);
        let passed = tower_wt::run_test(&mut log, state_dir, task, cmd_override, now)?;
        let captured = log.into_captured();
        let summary = tower_wt::summarize_failure(&captured, tower_wt::SUMMARY_LINES);

        // 🚨 The verdict is about the tree the suite saw, so that tree has to
        // be the one still standing. The worker can be prompted by hand — or
        // dispatched to by another command — during a long run, and a commit in
        // that window means the result describes a checkout that no longer
        // exists. Passing would then record the new HEAD as merge-ready, and
        // failing would hand back a failure the worker may already have fixed.
        // Neither is a verdict; stop and say what happened.
        //
        // 🚨 And the tree standing still is not enough by itself. `wt send` can
        // deliver new work in the gap between the last test byte and the
        // verdict: the worker has not touched anything yet, so every check
        // above passes, while its own invalidation finds no verdict to clear —
        // `run_test` cleared it before the run. The terminal verdict written
        // next would then stand over an agent starting untested edits, and
        // `wt merge` would offer it. So ask who the worker is now, too.
        let moved = tower_wt::Checkout::read(&path)? != before;
        let redirected = crate::waitcmd::agent_for_task(store, state_dir, task)
            .map(|a| tower_wt::Worker::of(&a) != worker_before)
            // A worker that has gone missing is not one this run can conclude
            // anything about either.
            .unwrap_or(true);
        if moved || redirected {
            let drive = unfinished_verdict(
                store,
                state_dir,
                task,
                attempts,
                &previous.map(|d| d.failure).unwrap_or_default(),
                OffsetDateTime::now_utc(),
            )?;
            let what = if moved {
                "the worktree moved while the tests ran — the result describes a checkout that is gone"
            } else {
                "new work reached the worker while the tests ran — the result is not about what it is doing now"
            };
            writeln!(w, "{task}: {what}, so nothing was concluded.")?;
            return Ok(drive);
        }

        match tower_wt::decide(passed, &summary, previous.as_ref(), attempts, cap) {
            tower_wt::Step::Ready => {
                // 🚨 Passing on a tree with uncommitted changes says nothing
                // about the branch. `wt merge` merges commits and leaves those
                // edits behind, so the base would get everything except the
                // change that made the suite pass.
                let m = tower_wt::load_meta(state_dir, task)?;
                // 🚨 A tree that cannot be inspected is not a clean tree. The
                // question being asked is "are the tested changes committed",
                // and `?` rather than a default is the only answer that does
                // not invent a yes — `wt list` makes the same distinction for
                // the same reason.
                // 🚨 And on the branch the merge will use. A worktree moved to
                // another branch — or left on a detached HEAD — still tests and
                // still comes back clean, but `merge_guide` merges `m.branch`,
                // so what was verified and what would land are different trees.
                //
                // Taken from `before`, not read again: the comparison above has
                // already proven the checkout — branch included — is the one
                // the suite ran on. Asking git a third time answered a question
                // held in hand and reopened the window it was closing.
                let on = before.branch.as_str();
                if on != m.branch {
                    let Some(drive) = publish_verdict(
                        store,
                        state_dir,
                        task,
                        tower_wt::DriveState::Uncommitted,
                        attempts,
                        "",
                        &before.head,
                        worker_before,
                    )?
                    else {
                        writeln!(w, "{task}: new work reached the worker before the verdict was written — it supersedes this run, so nothing was recorded.")?;
                        return Ok(superseded(
                            attempts,
                            &before.head,
                            OffsetDateTime::now_utc(),
                        ));
                    };
                    let where_ = if on.is_empty() {
                        "a detached HEAD".to_string()
                    } else {
                        format!("branch {on}")
                    };
                    writeln!(
                        w,
                        "{task}: tests pass, but the worktree is on {where_}, not {} — that is not what a merge would take.",
                        m.branch
                    )?;
                    return Ok(drive);
                }
                let dirty = tower_wt::dirty(&m.path)?;
                if !dirty.is_empty() {
                    let Some(drive) = publish_verdict(
                        store,
                        state_dir,
                        task,
                        tower_wt::DriveState::Uncommitted,
                        attempts,
                        "",
                        &before.head,
                        worker_before,
                    )?
                    else {
                        writeln!(w, "{task}: new work reached the worker before the verdict was written — it supersedes this run, so nothing was recorded.")?;
                        return Ok(superseded(
                            attempts,
                            &before.head,
                            OffsetDateTime::now_utc(),
                        ));
                    };
                    writeln!(
                        w,
                        "{task}: tests pass, but {} path(s) are uncommitted — the branch does not carry what was tested.",
                        dirty.len()
                    )?;
                    return Ok(drive);
                }
                let Some(drive) = publish_verdict(
                    store,
                    state_dir,
                    task,
                    tower_wt::DriveState::MergeReady,
                    attempts,
                    "",
                    &before.head,
                    worker_before,
                )?
                else {
                    writeln!(w, "{task}: new work reached the worker before the verdict was written — it supersedes this run, so nothing was recorded.")?;
                    return Ok(superseded(
                        attempts,
                        &before.head,
                        OffsetDateTime::now_utc(),
                    ));
                };
                writeln!(
                    w,
                    "{task}: tests pass — merge-ready. Review and merge it with herdr-tower wt merge {task}."
                )?;
                return Ok(drive);
            }
            tower_wt::Step::Stalled => {
                let Some(drive) = publish_verdict(
                    store,
                    state_dir,
                    task,
                    tower_wt::DriveState::Stalled,
                    attempts,
                    &summary,
                    &before.head,
                    worker_before,
                )?
                else {
                    writeln!(w, "{task}: new work reached the worker before the verdict was written — it supersedes this run, so nothing was recorded.")?;
                    return Ok(superseded(
                        attempts,
                        &before.head,
                        OffsetDateTime::now_utc(),
                    ));
                };
                writeln!(
                    w,
                    "{task}: the same failure twice — stopping so a person can look."
                )?;
                return Ok(drive);
            }
            tower_wt::Step::Exhausted => {
                let Some(drive) = publish_verdict(
                    store,
                    state_dir,
                    task,
                    tower_wt::DriveState::Exhausted,
                    attempts,
                    &summary,
                    &before.head,
                    worker_before,
                )?
                else {
                    writeln!(w, "{task}: new work reached the worker before the verdict was written — it supersedes this run, so nothing was recorded.")?;
                    return Ok(superseded(
                        attempts,
                        &before.head,
                        OffsetDateTime::now_utc(),
                    ));
                };
                writeln!(w, "{task}: still failing after {attempts} attempts.")?;
                return Ok(drive);
            }
            tower_wt::Step::Retry(summary) => {
                let m = tower_wt::load_meta(state_dir, task)?;
                // The tail goes to a file and the instruction names it. An
                // agent instruction is one line — `build_instruction` says why
                // — and the tail is untrusted suite output besides.
                let log = tower_wt::write_failure_log(state_dir, task, &summary).ok();
                let instruction =
                    tower_wt::build_retry_instruction(task, &m.test_cmd, log.as_deref());
                // 🚨 Written *before* the dispatch, which is the irreversible
                // half. Interrupted between the two, the worker would be
                // applying a fix that nothing recorded — `run_test` has already
                // cleared the previous verdict — and `wt list` would show no
                // sign that a retry was handed off, which is the durability
                // this state exists for. Recorded as what it is, too: an
                // earlier draft wrote a terminal state here for want of a
                // value, which put "exhausted" against a drive still running.
                let Some(running) = publish_verdict(
                    store,
                    state_dir,
                    task,
                    tower_wt::DriveState::Running,
                    attempts,
                    &summary,
                    &before.head,
                    worker_before,
                )?
                else {
                    writeln!(w, "{task}: new work reached the worker before the verdict was written — it supersedes this run, so nothing was recorded.")?;
                    return Ok(superseded(
                        attempts,
                        &before.head,
                        OffsetDateTime::now_utc(),
                    ));
                };
                // 🚨 `Invalidate::Nothing`: the ordinary dispatch path clears
                // the stored verdict, which would wipe the `Running` just
                // written for this very window.
                match tower_wt::send_dispatch(
                    state_dir,
                    task,
                    store,
                    h,
                    &instruction,
                    tower_wt::Invalidate::Nothing,
                ) {
                    Ok(delivered) => {
                        writeln!(w, "{task}: handed the failure back to the agent.")?;
                        if delivered.boundary_lost {
                            // 🚨 Delivered, but the record of it was not kept,
                            // so the previous turn's `DoneUnread` may still be
                            // standing. The next lap's wait would accept that
                            // at once and test the tree the worker is in the
                            // middle of changing — and a pass there, or a
                            // repeat, is a terminal verdict about nobody's
                            // code. Saying so and carrying on was not enough.
                            // Stop, and leave `Running` in place: it is true,
                            // and it is what tells `wt list` a fix is under way.
                            writeln!(
                                w,
                                "{task}: the dispatch was delivered but could not be recorded, so the next result could not be told apart from the last one — stopping here. Run the drive again once the worker has finished."
                            )?;
                            return Ok(running);
                        }
                        previous = Some(running);
                    }
                    // 🚨 Only a refusal rolls back. Everything after the
                    // instruction actually arrives is bookkeeping, and
                    // overwriting `Running` there would erase the evidence that
                    // a worker is at this moment applying a fix — which is the
                    // single thing that record exists to preserve.
                    Err(e) => {
                        tower_wt::record_for(
                            state_dir,
                            task,
                            tower_wt::DriveState::Unfinished,
                            attempts,
                            &summary,
                            &before.head,
                            OffsetDateTime::now_utc(),
                        )?;
                        return Err(e.into());
                    }
                }
            }
        }
    }
}

/// A writer that labels every line with its task and shares one sink.
///
/// Parallel drives write to the same terminal, and a test log with no owner is
/// unreadable the moment two of them overlap.
///
/// 🚨 Whole lines only. Holding the lock per `write` call is not enough: a
/// line usually arrives across several calls, and another drive taking the
/// lock between them lands its output *inside* that line. So a partial line is
/// held here until its newline arrives, and the lock is taken to emit complete
/// lines — which is the unit a reader can follow.
struct Prefixed<'a, W: Write> {
    sink: &'a std::sync::Mutex<&'a mut W>,
    prefix: String,
    pending: Vec<u8>,
    /// The last thing emitted was a line this writer broke itself, because it
    /// ran past `LINE_BUDGET`. The newline that line really ends with has not
    /// arrived yet, and when it does it is already accounted for.
    broke: bool,
}

impl<'a, W: Write> Prefixed<'a, W> {
    fn new(sink: &'a std::sync::Mutex<&'a mut W>, task: &str) -> Self {
        Prefixed {
            sink,
            prefix: format!("[{task}] "),
            pending: Vec::new(),
            broke: false,
        }
    }

    /// Emits every complete line held, labelled.
    ///
    /// 🚨 And an over-long partial one. A suite that draws progress with
    /// carriage returns, or prints one enormous line, never gives this a
    /// newline to stop on — and a buffer that waited for one would grow with
    /// the whole log and end an unattended drive before it recorded a verdict.
    /// Past the budget the fragment is emitted as its own line; the log is
    /// broken where it would otherwise not be, which is the lesser of the two.
    fn drain_lines(&mut self) -> std::io::Result<()> {
        let (end, synthetic) = match self.pending.iter().rposition(|b| *b == b'\n') {
            Some(i) => (i + 1, false),
            None if self.pending.len() > LINE_BUDGET => {
                self.pending.push(b'\n');
                (self.pending.len(), true)
            }
            None => return Ok(()),
        };
        // A poisoned lock means another drive panicked. Its output is not worth
        // losing this one's, so the guard is taken either way.
        let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        for line in self.pending[..end].split_inclusive(|b| *b == b'\n') {
            sink.write_all(self.prefix.as_bytes())?;
            sink.write_all(line)?;
        }
        self.pending.drain(..end);
        self.broke = synthetic;
        Ok(())
    }
}

impl<W: Write> Write for Prefixed<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 🚨 A line broken for length has already been ended. Its real newline,
        // arriving next, would otherwise start and immediately end a second
        // line — a stray `[t] ` with nothing after it. The same happens when
        // `run_test` ends an unterminated last line that was broken this way.
        let mut take = buf;
        if self.broke && self.pending.is_empty() && take.first() == Some(&b'\n') {
            take = &take[1..];
        }
        // Only the one newline: after it the line really has ended, and the
        // next `\n` is a genuine blank line.
        if !buf.is_empty() {
            self.broke = false;
        }
        self.pending.extend_from_slice(take);
        self.drain_lines()?;
        // Everything was taken or an error was returned, so a short write
        // cannot be reported here — which is what `Tee` relies on.
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.drain_lines()?;
        let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        sink.flush()
    }
}

impl<W: Write> Drop for Prefixed<'_, W> {
    /// A run whose last line has no newline still has to be seen.
    fn drop(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.pending.push(b'\n');
        let _ = self.drain_lines();
    }
}

/// How long a line may get before it is emitted without its newline.
///
/// Generous enough that ordinary output — even a wide table or a long path — is
/// never cut, and small enough that a progress bar cannot exhaust the process.
const LINE_BUDGET: usize = 8 * 1024;

/// How much of a run's output is kept for the summary.
///
/// 🚨 Bounded, because `run_test` streams in constant memory and this must not
/// take that away: an unattended drive can run a chatty suite for hours, and a
/// buffer that grows with the log would end the process before it could record
/// a verdict. Comfortably more than [`tower_wt::SUMMARY_LINES`] of ordinary
/// output, and the excess is dropped from the front — the tail is the only part
/// anyone reads.
const TAIL_BYTES: usize = 64 * 1024;

/// A writer that passes everything through and keeps the tail.
///
/// `run_test` streams rather than captures, on purpose — a failing suite is
/// only useful if you can read it while it runs. A drive needs both: the person
/// watching gets the stream, and the worker gets the tail.
/// A writer whose writes cannot fail.
///
/// The first error marks the writer gone, and every later write — that one
/// included — reports success without being offered to it. For output that is
/// informational only, where losing the terminal must not cost the work.
struct Resilient<'a, W: Write> {
    inner: &'a mut W,
    gone: bool,
}

impl<'a, W: Write> Resilient<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Resilient { inner, gone: false }
    }
}

impl<W: Write> Write for Resilient<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.gone {
            match self.inner.write(buf) {
                Ok(n) => return Ok(n),
                // Retried by `write_all`; not a dead writer.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Err(e),
                Err(_) => self.gone = true,
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.gone && self.inner.flush().is_err() {
            self.gone = true;
        }
        Ok(())
    }
}

struct Tee<'a, W: Write> {
    inner: &'a mut W,
    captured: Vec<u8>,
    /// The terminal refused a write outright. From then on nothing goes to it.
    gone: bool,
}

impl<'a, W: Write> Tee<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Tee {
            inner,
            captured: Vec::new(),
            gone: false,
        }
    }

    fn into_captured(self) -> String {
        String::from_utf8_lossy(&self.captured).into_owned()
    }

    /// Drops whatever is over budget, from the front.
    ///
    /// A partial line can be left at the start; `summarize_failure` takes whole
    /// lines from the end, so that fragment is dropped there.
    fn trim(&mut self) {
        if self.captured.len() <= TAIL_BYTES {
            return;
        }
        let mut cut = self.captured.len() - TAIL_BYTES;
        // 🚨 Forward to a character boundary. An arbitrary byte offset can land
        // inside a multi-byte sequence — a path with an accent, a Korean
        // assertion message, box-drawing progress output — and
        // `from_utf8_lossy` then puts U+FFFD at the front of the tail. That
        // fragment is dropped when whole lines are taken from the end, but a
        // suite whose tail is one long line has no line to drop, so the
        // replacement character travelled into the retry instruction and into
        // the repeat fingerprint.
        while cut < self.captured.len() && self.captured[cut] & 0b1100_0000 == 0b1000_0000 {
            cut += 1;
        }
        self.captured.drain(..cut);
    }
}

impl<W: Write> Write for Tee<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 🚨 Only what the writer underneath accepted. A legal short write
        // returns `n < buf.len()`, and `write_all` then retries the rest — so
        // capturing the whole buffer here would record that suffix twice. This
        // tail is both the retry message and the repeated-failure fingerprint,
        // and a duplicated fragment corrupts each of them.
        //
        // 🚨 But the capture must outlive the terminal. If the pane or pipe goes
        // away mid-suite, every later write fails, and `run_test` deliberately
        // ignores that and keeps draining the child — so returning the error
        // here froze the tail at the moment the terminal died, before the
        // failure it exists to record. Once the terminal refuses, stop offering
        // it anything (a `Prefixed` underneath would otherwise pile each
        // refused line into its pending buffer) and keep capturing.
        if !self.gone {
            match self.inner.write(buf) {
                Ok(n) => {
                    self.captured.extend_from_slice(&buf[..n]);
                    self.trim();
                    return Ok(n);
                }
                // Retried by `write_all`; not a dead terminal.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Err(e),
                Err(_) => self.gone = true,
            }
        }
        self.captured.extend_from_slice(buf);
        self.trim();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.gone {
            return Ok(());
        }
        self.inner.flush()
    }
}

#[cfg(test)]
mod drive_tests {
    use super::*;

    /// Parallel drives share one terminal, so every line has to say whose it
    /// is — and the label must land at line starts only, or a suite's output
    /// gains one in the middle of a word.
    #[test]
    fn each_line_is_labelled_with_its_task() {
        let mut sink = Vec::new();
        {
            let shared = std::sync::Mutex::new(&mut sink);
            let mut a = Prefixed::new(&shared, "hero");
            let mut b = Prefixed::new(&shared, "sidekick");
            write!(a, "one\ntw").expect("write");
            writeln!(b, "alpha").expect("write");
            // `a` finishes the line it left open.
            write!(a, "o\nthree\n").expect("write");
            // And a last line with no newline still has to appear.
            write!(b, "omega").expect("write");
        }
        let out = String::from_utf8(sink).expect("utf8");
        // 🚨 `hero`'s "two" comes *after* `sidekick`'s "alpha": the half-line
        // "tw" was held until its newline arrived, which is exactly what keeps
        // one drive's output out of the middle of another's line. Ordering
        // between tasks is not preserved, and cannot be — readability is.
        assert_eq!(
            out,
            "[hero] one\n[sidekick] alpha\n[hero] two\n[hero] three\n[sidekick] omega\n"
        );
        for line in out.lines() {
            assert!(
                line.starts_with("[hero] ") || line.starts_with("[sidekick] "),
                "every line is labelled exactly once: {line:?}"
            );
        }
    }

    /// 🚨 A line that never ends must not grow without bound either: progress
    /// bars drawn with carriage returns produce exactly that, and the wrapper
    /// would otherwise hold the whole log even though `Tee` bounds its own copy.
    #[test]
    fn an_endless_line_is_emitted_rather_than_hoarded() {
        let mut sink = Vec::new();
        {
            let shared = std::sync::Mutex::new(&mut sink);
            let mut a = Prefixed::new(&shared, "hero");
            for _ in 0..40 {
                // 400 bytes at a time, no newline anywhere.
                a.write_all(&b"x".repeat(400)).expect("write");
            }
            assert!(
                a.pending.len() <= LINE_BUDGET + 400,
                "held {} bytes",
                a.pending.len()
            );
        }
        let out = String::from_utf8(sink).expect("utf8");
        assert!(out.starts_with("[hero] "), "and it is still labelled");
        assert_eq!(out.matches("[hero] ").count(), 2, "emitted in two pieces");
        assert_eq!(
            out.chars().filter(|c| *c == 'x').count(),
            16_000,
            "every byte arrives"
        );
    }

    /// 🚨 The capture must not grow with the log. `run_test` streams in
    /// constant memory and a drive runs unattended for hours; a buffer that
    /// kept everything would end the process before it recorded a verdict.
    #[test]
    fn the_captured_tail_is_bounded_and_keeps_the_end() {
        let mut sink = Vec::new();
        let mut tee = Tee::new(&mut sink);
        for i in 0..20_000 {
            writeln!(tee, "line {i}").expect("write");
        }
        let captured = tee.into_captured();
        assert!(
            captured.len() <= TAIL_BYTES + 64,
            "kept {} bytes",
            captured.len()
        );
        assert!(
            captured.ends_with("line 19999\n"),
            "the end is what survives"
        );
        assert!(!captured.contains("line 0\n"), "the front is dropped");

        // Everything still reached the writer underneath.
        let passed = String::from_utf8(sink).expect("utf8");
        assert!(passed.starts_with("line 0\n"));
        assert!(passed.ends_with("line 19999\n"));

        // And what the worker is handed is whole lines from that tail.
        let summary = tower_wt::summarize_failure(&captured, 3);
        assert_eq!(summary, "line 19997\nline 19998\nline 19999");
    }

    /// 🚨 The front is cut on a character boundary. A suite printing non-ASCII
    /// past the budget used to leave half a multi-byte sequence at the start,
    /// which `from_utf8_lossy` turned into U+FFFD — and with a one-line tail
    /// there is no earlier line to drop, so it rode into the retry and into the
    /// repeat fingerprint.
    #[test]
    fn the_captured_tail_never_starts_inside_a_character() {
        let mut sink = Vec::new();
        let mut tee = Tee::new(&mut sink);
        // One long line of three-byte characters, well past the budget, with
        // no newline to drop at.
        let wide = "한".repeat(TAIL_BYTES / 3 + 1000);
        tee.write_all(wide.as_bytes()).expect("write");
        let captured = tee.into_captured();
        assert!(
            !captured.contains('\u{FFFD}'),
            "no replacement character at the cut"
        );
        assert!(captured.chars().all(|c| c == '한'));
        assert!(captured.len() <= TAIL_BYTES);
    }

    /// Why that takes more than a flush: `Prefixed::flush` drains *complete*
    /// lines only. It has to — splitting a partial line on flush would put a
    /// prefix in the middle of one drive's line, which is the interleaving it
    /// exists to prevent.
    #[test]
    fn a_flush_does_not_break_a_partial_line() {
        let mut sink = Vec::new();
        {
            let shared = std::sync::Mutex::new(&mut sink);
            let mut p = Prefixed::new(&shared, "t");
            write!(p, "half a li").expect("write");
            p.flush().expect("flush");
            writeln!(p, "ne").expect("write");
        }
        assert_eq!(
            String::from_utf8(sink).expect("utf8"),
            "[t] half a line\n",
            "one line, one prefix"
        );
    }

    /// 🚨 A line broken for length is not ended twice. `Prefixed` breaks a line
    /// that runs past `LINE_BUDGET` with a newline of its own, and the line's
    /// real newline — or the one `run_test` adds to an unterminated suite —
    /// then arrived as a second, empty line: a stray `[t] ` with nothing after.
    #[test]
    fn a_line_broken_for_length_is_not_ended_twice() {
        let mut sink = Vec::new();
        {
            let shared = std::sync::Mutex::new(&mut sink);
            let mut p = Prefixed::new(&shared, "t");
            p.write_all(&b"x".repeat(LINE_BUDGET + 1)).expect("write");
            p.write_all(b"\n").expect("its real newline");
            // After that one, a newline is a genuine blank line again.
            p.write_all(b"\n").expect("a blank line");
            p.write_all(b"next\n").expect("write");
        }
        let out = String::from_utf8(sink).expect("utf8");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out:?}");
        assert!(lines[0].starts_with("[t] xxx"));
        assert_eq!(lines[1], "[t] ", "the genuine blank line survives");
        assert_eq!(lines[2], "[t] next");
    }

    /// 🚨 A dead terminal cannot end a drive. Every status line a drive prints
    /// used `?`, so the first line after the pane closed returned an error and
    /// the drive stopped — after dispatching a retry, or before reporting a
    /// verdict it had already recorded. Nothing written through here fails, and
    /// a dead terminal is offered one write, not one per line.
    #[test]
    fn a_dead_terminal_cannot_end_a_drive() {
        let mut term = DiesAfter {
            left: 1,
            offered_after_death: 0,
        };
        {
            let mut w = Resilient::new(&mut term);
            writeln!(w, "t: running tests (attempt 1/2)").expect("before death");
            for line in [
                "t: tests pass — merge-ready.",
                "t: handed the failure back to the agent.",
                "t: the same failure twice — stopping so a person can look.",
            ] {
                writeln!(w, "{line}").expect("a status line must not fail the drive");
            }
            w.flush().expect("nor may a flush");
        }
        assert_eq!(term.offered_after_death, 1, "offered once, then left alone");
    }

    /// A terminal that accepts a few writes and then is gone for good — a pane
    /// closed, or a pipe whose reader exited, mid-suite.
    struct DiesAfter {
        left: usize,
        offered_after_death: usize,
    }

    impl Write for DiesAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.left == 0 {
                self.offered_after_death += 1;
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.left -= 1;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 🚨 The failure a drive hands back is at the *end* of the log, which is
    /// after the terminal died if it died at all. `run_test` ignores write
    /// errors so the child keeps draining; a capture that stopped with the
    /// terminal froze on whatever preceded the death instead.
    #[test]
    fn the_tail_is_captured_after_the_terminal_dies() {
        let mut term = DiesAfter {
            left: 2,
            offered_after_death: 0,
        };
        let mut tee = Tee::new(&mut term);
        writeln!(tee, "compiling").expect("write");
        writeln!(tee, "running 3 tests").expect("write");
        // The terminal is gone from here on. None of these may error out —
        // `run_test` would carry on regardless — and all must be kept.
        writeln!(tee, "test a ... ok").expect("write");
        writeln!(tee, "test b ... FAILED").expect("write");
        writeln!(tee, "assertion failed: left == right").expect("write");
        tee.flush().expect("flush");
        let captured = tee.into_captured();
        assert!(
            captured.ends_with("test b ... FAILED\nassertion failed: left == right\n"),
            "the failure itself survives: {captured:?}"
        );
        assert!(
            captured.starts_with("compiling\n"),
            "and nothing is lost before it"
        );
        // Offered once, refused, then left alone.
        assert_eq!(term.offered_after_death, 1);
    }
}

/// Exactly one task name, and nothing that looks like a flag.
fn one_task(args: &[String]) -> Result<&str, Failure> {
    match args {
        [one] if !one.starts_with('-') => Ok(one),
        _ => Err("a task name is required".into()),
    }
}

/// Accepts the task name on either side of the flags.
///
/// `wt test hero --cmd 'make test'` and `wt test --cmd 'make test' hero` both
/// work, because the flag parser stops at the first non-flag word and people
/// type it both ways. Lifting a leading task name out before parsing is
/// what makes the first form work at all.
fn task_and_flags<'a>(fs: &FlagSet, args: &'a [String]) -> Result<(String, Parsed), Failure> {
    let mut task = "";
    let mut rest: &'a [String] = args;
    if let Some((first, tail)) = args.split_first() {
        if !first.starts_with('-') {
            task = first;
            rest = tail;
        }
    }
    let p = fs.parse(rest).map_err(flag_failure)?;
    let lifted = !task.is_empty();
    let task = if lifted {
        task.to_string()
    } else {
        p.args().first().cloned().unwrap_or_default()
    };
    if task.is_empty() {
        return Err("a task name is required".into());
    }
    // Exactly one task, and nothing else. What is left over after the name was
    // taken used to be dropped on the floor, so `wt new hero typo` succeeded and
    // — worse — `wt merge hero --yes typo` went ahead with the merge. A word the
    // command does not understand is a typo in the word that matters more often
    // than it is noise.
    let extra: &[String] = if lifted { p.args() } else { &p.args()[1..] };
    if let Some(word) = extra.first() {
        return Err(format!("unknown argument: {word:?} (one task only)").into());
    }
    Ok((task, p))
}

/// Parses, writing the usage block to the caller's writer on a refusal.
fn parse<W: Write>(w: &mut W, fs: &FlagSet, args: &[String]) -> Result<Parsed, Failure> {
    match fs.parse(args) {
        Ok(p) => Ok(p),
        Err(e) => {
            write!(w, "{}", fs.diagnostic(&e))?;
            Err(e.into())
        }
    }
}

/// The same, for the paths that report the flag error without writing the
/// usage block.
fn flag_failure(e: FlagError) -> Failure {
    e.into()
}

/// Lays rows out in columns for `wt list`.
///
/// A two-column minimum and two columns of padding, with widths measured in
/// display columns rather than characters. A Korean task name counts four
/// characters and occupies eight columns, so a character count would step
/// every following column to the right. `status` already measures columns for
/// exactly this reason.
///
/// The last cell of a row is never padded: it is not part of an aligned column,
/// because there is nothing after it to align.
fn columns(rows: &[Vec<String>]) -> String {
    const MIN_WIDTH: usize = 2;
    const PADDING: usize = 2;

    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![MIN_WIDTH; n];
    for r in rows {
        // `len() - 1` is the trailing cell, which no column contains.
        for (i, c) in r.iter().enumerate().take(r.len().saturating_sub(1)) {
            widths[i] = widths[i].max(tower_text::width(c) + PADDING);
        }
    }

    let mut out = String::new();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if i + 1 == r.len() {
                out.push_str(c);
            } else {
                out.push_str(&crate::pad_right(c, widths[i]));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One task, and nothing else. A leftover word used to be dropped — which
    /// made `wt new hero typo` succeed and `wt merge hero --yes typo` go ahead
    /// with the merge.
    #[test]
    fn a_leftover_word_is_refused_on_either_side_of_the_flags() {
        let words = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let fs = || FlagSet::new("wt test").string("cmd", "", "test command");

        // The name in front, the name behind: both forms work, and both refuse
        // the extra word.
        assert_eq!(
            task_and_flags(&fs(), &words(&["hero"]))
                .map(|(t, _)| t)
                .ok(),
            Some("hero".to_string())
        );
        assert_eq!(
            task_and_flags(&fs(), &words(&["--cmd", "make test", "hero"]))
                .map(|(t, _)| t)
                .ok(),
            Some("hero".to_string())
        );
        for bad in [
            vec!["hero", "typo"],
            vec!["hero", "--cmd", "make test", "typo"],
            vec!["--cmd", "make test", "hero", "typo"],
        ] {
            let e = task_and_flags(&fs(), &words(&bad)).err();
            assert!(e.is_some(), "{bad:?} should have been refused");
            assert!(
                format!("{}", e.unwrap()).contains("typo"),
                "the message has to name the word: {bad:?}"
            );
        }
    }

    fn store_in(dir: &Path) -> Store {
        Store::new(dir).unwrap()
    }

    /// A socket that cannot exist, so nothing in these tests reaches the
    /// developer's own herdr server.
    fn unreachable_herdr() -> Herdr {
        Herdr::at("/nonexistent/tower_cli-wtcmd/herdr.sock")
    }

    fn run(args: &[&str]) -> (String, Option<String>) {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut buf = Vec::new();
        let err = run_wt(
            &mut buf,
            d.path(),
            &store,
            &unreachable_herdr(),
            &args,
            OffsetDateTime::UNIX_EPOCH,
        )
        .err()
        .map(|e| e.to_string());
        (String::from_utf8(buf).unwrap(), err)
    }

    /// The bare command is how the subcommands are discovered, so it succeeds
    /// rather than failing with a usage error.
    #[test]
    fn the_bare_command_prints_the_subcommands_and_succeeds() {
        let (out, err) = run(&[]);
        assert_eq!(out, WT_USAGE);
        assert!(err.is_none(), "{err:?}");
    }

    /// An unknown subcommand shows the list *and* fails — the list because the
    /// user needs it, the failure because a script must not read a typo as a
    /// success.
    #[test]
    fn an_unknown_subcommand_shows_the_list_and_still_fails() {
        let (out, err) = run(&["frobnicate"]);
        assert_eq!(out, WT_USAGE);
        assert_eq!(err.as_deref(), Some("unknown wt command: frobnicate"));
    }

    #[test]
    fn a_subcommand_that_needs_a_task_says_so_rather_than_guessing() {
        for cmd in ["diff", "review", "send", "clean"] {
            let (_, err) = run(&[cmd]);
            assert_eq!(err.as_deref(), Some("a task name is required"), "{cmd}");
        }
        let (_, err) = run(&["wait"]);
        assert!(
            err.as_deref().is_some_and(|e| e.starts_with("Usage:")),
            "{err:?}"
        );
    }

    /// `wt wait` takes its task names on either side of `--timeout`, like the
    /// single-task commands do. Written the way the orchestration skill writes
    /// it — names first — the flag used to be read as a task name, and the wait
    /// failed at once with "--timeout did not finish" (2026-09-26).
    #[test]
    fn wt_wait_reads_task_names_before_or_after_the_timeout() {
        for args in [
            vec!["wait", "no-such-task", "--timeout", "1s"],
            vec!["wait", "--timeout", "1s", "no-such-task"],
            vec!["wait", "no-such-task", "-timeout", "1s"],
        ] {
            let (out, err) = run(&args);
            let err = err.unwrap_or_default();
            assert!(
                !err.contains("timeout") && !out.contains("timeout") && !out.contains("1s"),
                "{args:?}: a flag or its value was read as a task: out={out:?} err={err}"
            );
            assert!(
                err.contains("no-such-task") || out.contains("no-such-task"),
                "{args:?}: the task is the one reported: out={out:?} err={err:?}"
            );
        }
    }

    /// A task name is accepted on either side of the flags, because people type
    /// it both ways and the flag parser on its own supports only one of them.
    #[test]
    fn a_task_name_is_accepted_before_or_after_the_flags() {
        let fs = FlagSet::new("wt test").string("cmd", "", "test command");
        for args in [vec!["hero", "--cmd", "make"], vec!["--cmd", "make", "hero"]] {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let (task, p) = task_and_flags(&fs, &args).unwrap();
            assert_eq!(task, "hero");
            assert_eq!(p.str("cmd"), "make");
        }
    }

    /// An empty task list is a listing invitation, not an error — `wt list` on a
    /// fresh machine has to say how to start.
    #[test]
    fn an_empty_task_list_says_how_to_start_one() {
        let (out, err) = run(&["list"]);
        assert!(err.is_none(), "{err:?}");
        assert!(out.contains("herdr-tower wt new"), "{out}");
    }

    /// A worker that moved its worktree to a branch of its own is listed on
    /// that branch, with its commits counted there — not as `0` unmerged on
    /// the empty branch `wt new` cut.
    #[test]
    fn a_moved_worktree_is_listed_on_its_branch_with_its_commits() {
        // Without git's own environment: under a hook, `GIT_DIR` overrides
        // `-C` and these fixture commands would write into the real repository.
        let git = |dir: &std::path::Path, args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            for (k, _) in std::env::vars_os() {
                if k.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(k);
                }
            }
            let ok = cmd
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.name", "fixture"]);
        git(
            repo.path(),
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "init"]);

        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let m = tower_wt::new_task(
            d.path(),
            &tower_wt::NewOptions {
                task: "i-2919",
                repo: repo.path().to_str().unwrap(),
                ..Default::default()
            },
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        let wt = std::path::Path::new(&m.path);
        git(wt, &["switch", "-c", "agent/explain-2969-stage2"]);
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        git(wt, &["add", "."]);
        git(wt, &["commit", "-m", "work"]);

        let mut buf = Vec::new();
        run_wt(
            &mut buf,
            d.path(),
            &store,
            &unreachable_herdr(),
            &["list".to_string()],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let row = out.lines().find(|l| l.starts_with("i-2919")).unwrap();
        assert!(
            row.contains("agent/explain-2969-stage2 (cut as agent/i-2919)"),
            "{out}"
        );
        let cells: Vec<&str> = row.split_whitespace().collect();
        let unmerged = cells[cells.iter().position(|c| *c == "agent/i-2919)").unwrap() + 2];
        assert_eq!(unmerged, "1", "{out}");
    }

    /// Columns line up on display width, so one CJK cell does not shear every
    /// column after it.
    #[test]
    fn a_wide_cell_does_not_shear_the_columns_after_it() {
        let rows = vec![
            vec!["TASK".into(), "AGENT".into(), "PATH".into()],
            vec!["테스트".into(), "claude".into(), "~/a".into()],
            vec!["hero".into(), "codex".into(), "~/b".into()],
        ];
        let out = columns(&rows);
        // Where the second column begins, measured in display columns.
        let starts: Vec<usize> = out
            .lines()
            .map(|l| {
                let second = l.split_whitespace().nth(1).expect("a second cell");
                tower_text::width(&l[..l.find(second).expect("found above")])
            })
            .collect();
        assert!(
            starts.windows(2).all(|p| p[0] == p[1]),
            "the second column starts in different places: {starts:?}\n{out}"
        );
    }

    /// The last cell of a row is not padded: trailing spaces on every line of a
    /// table are invisible until something diffs it.
    #[test]
    fn no_row_ends_in_padding() {
        let rows = vec![
            vec!["TASK".into(), "PATH".into()],
            vec!["hero".into(), "~/a".into()],
        ];
        for line in columns(&rows).lines() {
            assert_eq!(line, line.trim_end(), "{line:?}");
        }
    }
}
