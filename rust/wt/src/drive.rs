// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Driving one task to a verdict: wait, test, hand the failure back, repeat.
//!
//! The pieces this composes already existed — `wt wait`, `wt test`, `wt send` —
//! and a person could run them in that order by hand. What was missing is the
//! part nobody is awake for: when the tests fail at 3am, someone has to read
//! the failure, give it back to the worker and wait again. This does that, and
//! stops.
//!
//! **Stopping is the design.** "Finished" here means *a branch that passed its
//! tests is standing in the merge queue*, not a branch that was merged. Merging
//! stays a person's decision ([`crate::merge_guide`] still refuses to act
//! without one), because that is the point where an unattended mistake becomes
//! everyone's. Everything up to it can be done while asleep; that cannot.
//!
//! The other half of stopping is not retrying forever. An automatic retry loop
//! is a machine for producing commits, and a worker that fails the same way
//! twice is not going to be argued out of it by a third copy of the same
//! message. Two things end a run early: a repeated failure, and a cap.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::meta::{load_meta, save_meta};
use crate::Result;

/// How many test runs one drive may spend before giving up.
///
/// Two, deliberately: the first failure is information the worker did not have,
/// the second says the information did not help. A third would mostly buy
/// commits.
pub const DEFAULT_ATTEMPTS: u32 = 2;

/// How much of a failing run is handed back.
///
/// The end of the output, because that is where the assertion and the summary
/// line are. Handing back the whole log would spend the worker's context on
/// what it already knows — the part it is missing is the last screen.
pub const SUMMARY_LINES: usize = 40;

/// Where a driven task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriveState {
    /// A drive is under way: the tests have failed at least once and the
    /// worker has been given the failure.
    ///
    /// Recorded rather than kept in memory so that a drive killed halfway —
    /// Ctrl-C, a closed laptop — leaves the truth behind instead of whatever
    /// the last finished attempt happened to be.
    Running,
    /// Tests passed. Waiting for a person to merge it.
    MergeReady,
    /// The same failure twice. A person should look.
    Stalled,
    /// Out of attempts with the tests still failing.
    Exhausted,
    /// The worker never settled inside the timeout, or settled on a question
    /// it needs answered, so nothing was concluded.
    Unfinished,
    /// The tests passed, but on a worktree with changes that are not committed.
    ///
    /// Not merge-ready: `wt merge` merges the branch, and those edits are not
    /// on it — the base would get the commits without the change that made the
    /// suite pass.
    Uncommitted,
}

/// How long a `Running` record is believed without anything refreshing it.
///
/// Long enough to cover a lap that waits out its timeout and then runs a slow
/// suite, and short enough that a drive killed overnight is not still reported
/// as running in the morning.
pub const RUNNING_LIFETIME: time::Duration = time::Duration::hours(12);

impl Drive {
    /// Whether a `Running` record has outlived any drive that could have
    /// written it.
    ///
    /// A drive waits up to its per-lap timeout for the worker and then runs the
    /// suite, so a live one can be quiet for a long time; the window is
    /// generous for that reason. A record older than this was left by a process
    /// that is gone.
    pub fn is_stale(&self, now: OffsetDateTime) -> bool {
        now - self.at > RUNNING_LIFETIME
    }

    /// Whether this verdict is still about the tree in `path`.
    ///
    /// An unknown head on either side answers `true`: the verdict was recorded
    /// without one, or the repository cannot be read now, and inventing a
    /// mismatch would hide a real result on a transient failure.
    pub fn describes(&self, path: &str) -> bool {
        let now = OffsetDateTime::now_utc();
        if self.state == DriveState::Running || self.head.is_empty() {
            return self.describes_at(now, None, None);
        }
        let head = crate::git::head(path).ok();
        // Only a merge-ready verdict asks about the working tree, so only that
        // one pays for the second git call.
        let dirty = if self.state == DriveState::MergeReady {
            crate::git::dirty(path).ok().map(|d| d.len())
        } else {
            None
        };
        self.describes_at(now, head.as_deref(), dirty)
    }

    /// The same question, answered from readings the caller already has.
    ///
    /// `wt list` reads the working tree for its own DIRTY column one line
    /// earlier; asking git again here doubled the subprocesses per row and let
    /// the two reads disagree, so DIRTY could print `0` beside a DRIVE of
    /// `superseded` taken from a later look at the same tree.
    ///
    /// `None` means "could not be read", and the two sides of that are not
    /// symmetric: an unreadable head cannot prove a mismatch, so it does not,
    /// while an unreadable tree cannot prove cleanliness, so a merge-ready
    /// verdict stops describing it.
    pub fn describes_at(
        &self,
        now: OffsetDateTime,
        head: Option<&str>,
        dirty: Option<usize>,
    ) -> bool {
        // 🚨 A drive still going is not superseded by the commit it is waiting
        // for. `Running` holds the head from *before* the fix, so the worker's
        // expected next commit would otherwise blank out the in-progress
        // marker at exactly the moment it means the most.
        //
        // But only while it can still be going. Nothing writes an ending when a
        // drive is interrupted — Ctrl-C, a closed laptop — so without a
        // deadline `wt list` showed `running` for a drive that died hours ago,
        // in the column the README calls the one to read in the morning. This
        // is what `at` was being recorded for.
        if self.state == DriveState::Running {
            return !self.is_stale(now);
        }
        if self.head.is_empty() {
            return true;
        }
        if head.is_some_and(|now| now != self.head) {
            return false;
        }
        // 🚨 A tree can change without a commit. A direct prompt or a manual
        // edit leaves the head alone, and those changes were never tested and
        // would not be merged — so a verdict that promises a mergeable branch
        // does not survive them.
        if self.state == DriveState::MergeReady {
            return dirty == Some(0);
        }
        true
    }
}

impl DriveState {
    /// The word `wt list` shows.
    pub fn label(self) -> &'static str {
        match self {
            DriveState::Running => "running",
            DriveState::MergeReady => "merge-ready",
            DriveState::Stalled => "stalled",
            DriveState::Exhausted => "exhausted",
            DriveState::Unfinished => "unfinished",
            DriveState::Uncommitted => "uncommitted",
        }
    }
}

/// What a drive left behind, stored on the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub state: DriveState,
    /// Test runs spent, including the one that produced `failure`.
    pub attempts: u32,
    /// The commit the verdict was reached on, when there is one.
    ///
    /// A verdict is about a tree. Without this, `merge-ready` keeps standing
    /// after the worker is prompted directly, commits again, or the branch is
    /// amended — nothing in `wt send` or `wt test` sees that, the tree is still
    /// clean, and the queue advertises a branch whose tests ran against a
    /// different commit.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head: String,
    /// The summary of the last failing run, empty when the last run passed.
    ///
    /// Kept because it is also the fingerprint: an identical summary on the
    /// next attempt is what "the same failure twice" means.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// What to do after a test run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Passed. Put it in the merge queue and stop.
    Ready,
    /// Failed, and there is reason to think another go would differ. Hand this
    /// summary to the worker and wait again.
    Retry(String),
    /// Failed the same way as last time.
    Stalled,
    /// Failed, and the attempts are spent.
    Exhausted,
}

/// Decides what a test run means, given what the last one meant.
///
/// `attempts` counts runs already spent *including this one*.
///
/// The order matters. A repeat is checked before the cap because it is the more
/// specific answer: both end the run, but "it failed the same way" tells a
/// person where to look and "it ran out of tries" does not.
pub fn decide(
    passed: bool,
    summary: &str,
    previous: Option<&Drive>,
    attempts: u32,
    cap: u32,
) -> Step {
    if passed {
        return Step::Ready;
    }
    // 🚨 An empty summary compares too. A suite that fails without printing
    // anything fails the same way every time, and `previous` being there at all
    // already says a run happened — it is only set once a failure was handed
    // back. Excluding empty summaries here let a silent failure retry until the
    // cap instead of stopping at the second identical one.
    //
    // 🚨 And compared by fingerprint, not by text. Every runner named in the
    // README prints how long it took — `finished in 0.03s`, `FAIL\tpkg\t0.123s`
    // — so two runs of one unchanged failure never match as strings and
    // `Stalled` was effectively unreachable: the drive burned to the cap and
    // reported `exhausted`, losing the one signal that says where to look.
    if previous.is_some_and(|d| fingerprint(&d.failure) == fingerprint(summary)) {
        return Step::Stalled;
    }
    if attempts >= cap.max(1) {
        return Step::Exhausted;
    }
    Step::Retry(summary.to_string())
}

/// The part of a failure that identifies it, with the clock taken out.
///
/// 🚨 Two runs of one unchanged failure are not the same string. Every runner
/// the README names prints its own elapsed time in the tail — cargo's
/// `finished in 0.03s`, a per-package `FAIL\tpkg\t0.123s` — so comparing the
/// text made "the same failure twice" impossible to observe. Durations are
/// replaced with a placeholder rather than dropped, so a failure that differs
/// *only* in a number that is not a duration still reads as different.
fn fingerprint(summary: &str) -> String {
    // Some runners print `(cached)` exactly where a run's elapsed time would
    // be, so the same failing package reads `ok pkg 0.31s` on one run and `ok
    // pkg (cached)` on the next.
    let summary = summary.replace("(cached)", "<elapsed>");
    let b = summary.as_bytes();
    let mut out = String::with_capacity(summary.len());
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            // The whole run up to the next digit, as text. Digits are ASCII, so
            // the position of the next one is always a character boundary.
            let next = b[i..]
                .iter()
                .position(u8::is_ascii_digit)
                .map_or(b.len(), |n| i + n);
            out.push_str(&summary[i..next]);
            i = next;
            continue;
        }
        match duration_end(&summary, i) {
            Some(end) => {
                out.push_str("<elapsed>");
                i = end;
            }
            None => {
                // Not a duration: the number is copied and the text after it is
                // left for the loop, exactly as it was.
                let end = number_end(b, i);
                out.push_str(&summary[i..end]);
                i = end;
            }
        }
    }
    out
}

/// Where a duration that starts at `i` ends, if one does.
///
/// A duration is one or more `<number><unit>` pieces run together — `2s`,
/// `40ms`, `1.5µs`, and compound ones such as `1m30s` and `1h2m3.5s` —
/// ending at a word boundary. A single piece may put one space before `s` or
/// `ms`, which is how Jest writes it (`Time: 1.234 s`).
///
/// 🚨 A compound is one duration, not several. Reading `1m30s` piece by piece
/// left `1m<elapsed>` — `1m` is followed by a digit, not a word boundary — so a
/// failure that took `59.9s` one run and `1m0.1s` the next never compared equal,
/// and a second pass over that output changed it again.
fn duration_end(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = start;
    let mut first = true;
    loop {
        if !b.get(i).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        let mut j = number_end(b, i);
        let spaced = first && b.get(j) == Some(&b' ');
        if spaced {
            j += 1;
        }
        let unit = time_unit(&s[j..])?;
        if spaced && !matches!(unit, "s" | "ms") {
            return None;
        }
        j += unit.len();
        match b.get(j) {
            // Another piece follows directly: `1m` then `30s`.
            Some(c) if c.is_ascii_digit() && !spaced => {
                i = j;
                first = false;
            }
            None => return Some(j),
            // A unit has to end the word, or `10submissions` would read as
            // ten seconds.
            Some(c) if !c.is_ascii_alphanumeric() => return Some(j),
            _ => return None,
        }
    }
}

/// The digits of a number at `i`, with a fraction if one follows.
fn number_end(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    i
}

/// The time unit `rest` starts with. Longest first, so `ms` is not read as
/// minutes followed by an `s`. Both micro signs appear in the wild: Rust and
/// most tools print U+00B5, and some the Greek U+03BC.
fn time_unit(rest: &str) -> Option<&'static str> {
    ["ms", "µs", "μs", "us", "ns", "h", "m", "s"]
        .into_iter()
        .find(|u| rest.starts_with(u))
}

/// The tail of a failing run, as it is handed back.
///
/// Trailing blank lines go first: a suite that ends with a newline would
/// otherwise spend part of the budget on nothing, and — since this doubles as
/// the repeat fingerprint — two identical failures whose logs differ only in
/// trailing whitespace have to compare equal.
pub fn summarize_failure(output: &str, lines: usize) -> String {
    let trimmed = output.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }
    let all: Vec<&str> = trimmed.lines().collect();
    let start = all.len().saturating_sub(lines.max(1));
    all[start..].join("\n")
}

/// The message a worker gets when its tests failed.
///
/// It says what to do and then shows the failure, in that order, because the
/// worker reads it as an instruction and the log is evidence for it. The test
/// command is named so the worker can run it itself rather than guessing how it
/// was invoked.
pub fn build_retry_instruction(task: &str, test_cmd: &str, log: Option<&Path>) -> String {
    let mut b = format!(
        "The tests for {task} are failing. Fix the cause and make them pass; \
         run `{test_cmd}` yourself to check."
    );
    if let Some(path) = log {
        b.push_str(&format!(
            " The end of the failing run is in {}.",
            path.display()
        ));
    }
    b
}

/// Where the tail of a failing run is left for the worker to read.
pub fn failure_log_path(state_dir: &Path, task: &str) -> PathBuf {
    state_dir
        .join("worktrees")
        .join(format!("{}.failure.log", crate::meta::encode_task(task)))
}

/// Writes the tail where the worker can read it, and returns the path.
///
/// 🚨 The tail does not go into the instruction. An agent instruction is one
/// line on purpose — `build_instruction` says why: the input is a TUI that
/// treats Enter as "go", so a multi-line send is several submissions, the first
/// dispatched as a complete instruction and the rest arriving as interruptions.
/// Forty lines of suite output would be forty-one of them.
///
/// And the tail is *untrusted*: it is whatever the tests printed. Submitted on
/// its own a line beginning `/` or `!` is a slash command or bash mode in the
/// agent's own input. A path is a path.
pub fn write_failure_log(state_dir: &Path, task: &str, summary: &str) -> Result<PathBuf> {
    let path = failure_log_path(state_dir, task);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{}\n", summary.trim_end()))?;
    Ok(path)
}

/// Records where a drive ended.
/// The checkout a test ran against: which branch, at which commit.
///
/// A verdict is only worth recording about the tree the test actually saw. The
/// worker can be prompted by hand, or another command can dispatch to it, while
/// a long suite is running; if it commits in that window, the tree that passed
/// and the tree `wt merge` would take are different, and recording the new HEAD
/// under a green verdict advertises code nobody tested. Reading this before and
/// after the run and requiring the two to match is what ties the verdict to
/// the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub branch: String,
    pub head: String,
}

impl Checkout {
    /// 🚨 An error, not a default. A checkout that cannot be read cannot be
    /// shown to have held still, and an empty value on both sides would compare
    /// equal and pass for exactly that.
    pub fn read(path: &str) -> Result<Checkout> {
        Ok(Checkout {
            branch: crate::git::current_branch(path)?.trim().to_string(),
            head: crate::git::head(path)?,
        })
    }
}

pub fn record(
    state_dir: &Path,
    task: &str,
    state: DriveState,
    attempts: u32,
    failure: &str,
    now: OffsetDateTime,
) -> Result<Drive> {
    let m = load_meta(state_dir, task)?;
    // Best effort: a repository that cannot answer still gets a verdict, and
    // an empty head simply means the check below has nothing to compare.
    let head = crate::git::head(&m.path).unwrap_or_default();
    record_for(state_dir, task, state, attempts, failure, &head, now)
}

/// What the worker was when the test began.
///
/// 🚨 [`Checkout`] catches the worker having *changed* the tree. This catches
/// it having been *told to*, which git cannot see: `wt send` can deliver new
/// work in the gap between a passing run and the verdict being written, and at
/// that moment the tree is still exactly as tested. Its own invalidation finds
/// no verdict to clear — `run_test` cleared it before the run — so the terminal
/// verdict written afterwards stands over an agent that is starting untested
/// edits, and `wt merge` offers it.
///
/// `settled` is whether the completion this drive is grading is still the
/// current one. A dispatch clears `done_acked`, which is what makes it
/// observable here; a person merely marking the row read sets it, so opening
/// the tower while a suite runs does not read as new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Worker {
    pub work_seq: u64,
    pub settled: bool,
}

impl Worker {
    pub fn of(agent: &tower_state::Agent) -> Worker {
        Worker {
            work_seq: agent.work_seq,
            settled: agent.state == tower_state::AgentState::DoneUnread || agent.done_acked,
        }
    }
}

/// Publishes a verdict, but only while the worker is still the one that was
/// tested.
///
/// 🚨 Comparing the worker and then writing the verdict are two steps, and
/// `wt send` fits between them. Its own invalidation finds no verdict to clear
/// — `run_test` cleared it before the run — so the write that follows
/// *re-establishes* one, over an agent that has just been handed other work.
///
/// Re-reading the worker inside the same lock the dispatch takes to clear the
/// verdict is what closes it. Either this call's read already sees the dispatch
/// and nothing is written, or the write lands first and the dispatch — taking
/// the lock afterwards — removes it. There is no ordering left in which a
/// superseded verdict survives.
///
/// `Ok(None)` means the worker moved and nothing was written.
#[allow(clippy::too_many_arguments)]
pub fn record_for_if(
    state_dir: &Path,
    task: &str,
    state: DriveState,
    attempts: u32,
    failure: &str,
    head: &str,
    expected: Worker,
    observe: impl FnOnce() -> Option<Worker>,
    now: OffsetDateTime,
) -> Result<Option<Drive>> {
    let _guard = crate::meta::claim_lock(&crate::meta::dir(state_dir));
    if observe() != Some(expected) {
        return Ok(None);
    }
    record_for(state_dir, task, state, attempts, failure, head, now).map(Some)
}

/// Records a verdict against a named commit rather than against whatever HEAD
/// happens to be by the time this runs.
///
/// 🚨 A verdict reached by testing belongs to the commit that was tested. The
/// worker is still there while the checks between the last test byte and this
/// write are made, and a commit landing in that gap would otherwise be the one
/// [`record`] reads — so `describes` would match it and a merge-ready verdict
/// would advertise a commit no test ever saw. The caller knows which commit it
/// verified; it passes it here.
pub fn record_for(
    state_dir: &Path,
    task: &str,
    state: DriveState,
    attempts: u32,
    failure: &str,
    head: &str,
    now: OffsetDateTime,
) -> Result<Drive> {
    let mut m = load_meta(state_dir, task)?;
    let head = head.to_string();
    let drive = Drive {
        state,
        attempts,
        failure: failure.to_string(),
        head,
        at: now,
    };
    m.drive = Some(drive.clone());
    save_meta(state_dir, &m)?;
    Ok(drive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{load_meta, save_meta};
    use crate::Meta;

    fn drive_with(failure: &str) -> Drive {
        Drive {
            state: DriveState::Exhausted,
            attempts: 1,
            failure: failure.to_string(),
            head: String::new(),
            at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_pass_is_the_end_of_the_drive() {
        assert_eq!(decide(true, "", None, 1, 2), Step::Ready);
        // Even on the last attempt, and even after a failure before it.
        assert_eq!(
            decide(true, "", Some(&drive_with("boom")), 9, 2),
            Step::Ready
        );
    }

    /// 🚨 The same failure twice is not something a third message fixes. It is
    /// also reported *as* a repeat rather than as exhaustion, because that is
    /// the answer that tells a person where to look.
    #[test]
    fn the_same_failure_twice_stops_the_drive() {
        let previous = drive_with("assertion failed: left == right");
        assert_eq!(
            decide(
                false,
                "assertion failed: left == right",
                Some(&previous),
                1,
                5
            ),
            Step::Stalled
        );
        assert_eq!(
            decide(false, "a different failure", Some(&previous), 1, 5),
            Step::Retry("a different failure".into())
        );
    }

    /// No earlier run in this drive means nothing to repeat — however the
    /// first failure reads.
    #[test]
    fn an_absent_previous_failure_never_counts_as_a_repeat() {
        assert_eq!(
            decide(false, "boom", None, 1, 5),
            Step::Retry("boom".into())
        );
        assert_eq!(decide(false, "", None, 1, 5), Step::Retry(String::new()));
    }

    /// A suite that fails without printing anything fails identically every
    /// time. `previous` exists only once a failure has been handed back, so an
    /// empty summary there is a real earlier run, and a second silent failure
    /// is the same failure twice — not a reason to keep retrying to the cap.
    #[test]
    fn two_silent_failures_in_a_row_stop_the_drive() {
        assert_eq!(
            decide(false, "", Some(&drive_with("")), 2, 5),
            Step::Stalled
        );
        // And a silent failure after a loud one is a different failure.
        assert_eq!(
            decide(false, "", Some(&drive_with("boom")), 2, 5),
            Step::Retry(String::new())
        );
    }

    #[test]
    fn attempts_run_out() {
        assert_eq!(decide(false, "boom", None, 2, 2), Step::Exhausted);
        assert_eq!(decide(false, "boom", None, 3, 2), Step::Exhausted);
        // A cap of zero is read as one: a drive that never tests answers
        // nothing, which is not a state this can be asked for.
        assert_eq!(decide(false, "boom", None, 1, 0), Step::Exhausted);
    }

    #[test]
    fn a_summary_is_the_tail_without_trailing_blanks() {
        let output = "one\ntwo\nthree\nfour\n\n\n";
        assert_eq!(summarize_failure(output, 2), "three\nfour");
        assert_eq!(summarize_failure(output, 99), "one\ntwo\nthree\nfour");
        assert_eq!(summarize_failure("   \n\n", 5), "");
        assert_eq!(
            summarize_failure("only", 0),
            "only",
            "a zero budget is one line"
        );
    }

    /// 🚨 The summary doubles as the repeat fingerprint, so two runs that
    /// differ only in trailing whitespace have to compare equal — otherwise a
    /// suite whose last line ends with a newline on one run and not the next
    /// would never look like a repeat.
    #[test]
    fn trailing_whitespace_does_not_hide_a_repeat() {
        let first = summarize_failure("FAIL: t\n", 10);
        let second = summarize_failure("FAIL: t\n\n\n", 10);
        assert_eq!(first, second);
        assert_eq!(
            decide(false, &second, Some(&drive_with(&first)), 1, 5),
            Step::Stalled
        );
    }

    /// 🚨 One line, and no suite output in it. An agent's input treats Enter as
    /// "go", so a multi-line instruction is several submissions — and the tail
    /// is untrusted: a failing test that prints `/clear` or `!rm` on a line of
    /// its own would have had it submitted as a command. The tail goes to a
    /// file and the instruction names the file.
    #[test]
    fn the_retry_message_is_one_line_and_names_where_the_tail_is() {
        let state = tempfile::tempdir().expect("tempdir");
        let hostile = "FAIL: one\n/clear\n!rm -rf ~\nFAIL: two";
        let log = write_failure_log(state.path(), "hero", hostile).expect("write");
        let got = build_retry_instruction("hero", "make test", Some(&log));

        assert!(!got.contains('\n'), "one submission, not several: {got:?}");
        assert!(got.contains("hero"));
        assert!(got.contains("`make test`"), "{got}");
        assert!(got.contains(&log.display().to_string()), "{got}");
        for line in ["/clear", "!rm", "FAIL: one"] {
            assert!(
                !got.contains(line),
                "suite output must not ride along: {line}"
            );
        }
        // Nothing lost: the whole tail is where the instruction says it is.
        let kept = std::fs::read_to_string(&log).expect("read");
        assert!(kept.contains("/clear") && kept.ends_with("FAIL: two\n"));

        // A failure with nothing to show still says what to do.
        let bare = build_retry_instruction("hero", "make test", None);
        assert!(bare.contains("Fix the cause"));
        assert!(!bare.contains("The end of the failing run"));
        assert!(!bare.contains('\n'));
    }

    /// 🚨 The clock is not part of a failure. Every runner prints its elapsed
    /// time, so comparing text made "the same failure twice" unobservable and
    /// the drive burned to the cap instead of stopping to say where to look.
    #[test]
    fn a_repeat_is_recognized_through_a_different_elapsed_time() {
        let cases = [
            (
                "cargo",
                "test result: FAILED. 2 passed; 1 failed; finished in 0.03s",
                "test result: FAILED. 2 passed; 1 failed; finished in 0.05s",
            ),
            ("package line", "FAIL\tpkg/x\t0.123s", "FAIL\tpkg/x\t0.456s"),
            ("ms", "1 failing (12ms)", "1 failing (340ms)"),
            ("integer", "done in 2s", "done in 17s"),
            // A run that crossed a minute switches to a compound.
            ("compound", "panic after 59.9s", "panic after 1m0.1s"),
            ("hours", "timed out after 1h2m3.5s", "timed out after 58m"),
            // Jest puts a space before the unit.
            ("jest", "Time:        1.234 s", "Time:        12.005 s"),
            ("micro", "bench 1.5µs/op", "bench 240µs/op"),
            ("greek micro", "bench 1.5μs/op", "bench 3μs/op"),
            // `(cached)` stands in for the time of a package that was not rerun.
            ("cached", "ok  \tpkg/y\t0.31s", "ok  \tpkg/y\t(cached)"),
        ];
        for (name, first, second) in cases {
            assert_eq!(
                decide(false, second, Some(&drive_with(first)), 2, 5),
                Step::Stalled,
                "{name}: one failure, two clocks"
            );
        }
    }

    /// And only the clock. A count that changed is a different failure, and a
    /// number glued to a word is not a duration.
    #[test]
    fn the_fingerprint_keeps_every_number_that_is_not_a_duration() {
        assert_ne!(
            fingerprint("2 passed; 1 failed"),
            fingerprint("2 passed; 3 failed"),
            "a different count is a different failure"
        );
        assert_ne!(
            fingerprint("expected 10submissions"),
            fingerprint("expected 20submissions"),
            "`10submissions` is not ten seconds"
        );
        assert_ne!(
            fingerprint("line 3: boom"),
            fingerprint("line 7: boom"),
            "a line number is not a duration"
        );
        assert_eq!(fingerprint("in 0.03s"), fingerprint("in 9.99s"));
        // The space Jest uses is allowed only before `s` and `ms`, and only when
        // the unit ends the word.
        assert_ne!(fingerprint("1 skipped"), fingerprint("2 skipped"));
        assert_ne!(fingerprint("5 minutes"), fingerprint("6 minutes"));
        assert_ne!(fingerprint("5min"), fingerprint("6min"));
        // A compound only counts when it ends as a unit: `5s3x` is not a time.
        assert_ne!(fingerprint("5s3x"), fingerprint("6s3x"));
    }

    /// 🚨 A drive in progress has a state of its own. An earlier draft wrote a
    /// terminal one here for want of a value, which put "exhausted" in
    /// `wt list` for a task still being worked on — and left it there for good
    /// if the drive was interrupted.
    #[test]
    fn a_drive_in_progress_is_recorded_as_running() {
        let dir = std::env::temp_dir().join(format!("tower-drive-{}", std::process::id()));
        std::fs::create_dir_all(crate::meta::dir(&dir)).expect("state dir");
        let m = Meta {
            task: "hero".into(),
            repo: "/r".into(),
            base: "main".into(),
            branch: "agent/hero".into(),
            path: "/w".into(),
            agent: "claude".into(),
            test_cmd: "make test".into(),
            test_pass: None,
            test_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            drive: None,
            parent: None,
        };
        save_meta(&dir, &m).expect("save");

        let running = record(
            &dir,
            "hero",
            DriveState::Running,
            1,
            "FAIL: one",
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("record");
        assert_eq!(running.state, DriveState::Running);
        assert_eq!(running.failure, "FAIL: one");

        // And it round-trips, so an interrupted drive is readable afterwards.
        let back = load_meta(&dir, "hero").expect("load").drive.expect("drive");
        assert_eq!(back.state, DriveState::Running);
        assert_eq!(back.attempts, 1);
        assert_eq!(back.failure, "FAIL: one");

        let ready = record(
            &dir,
            "hero",
            DriveState::MergeReady,
            2,
            "",
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("record");
        assert_eq!(ready.state, DriveState::MergeReady);
        assert!(
            ready.failure.is_empty(),
            "a passing run leaves no fingerprint to trip over"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 🚨 A verdict is about a tree, and a tree is a commit. Without this,
    /// `merge-ready` keeps standing after the worker is prompted directly or
    /// the branch is amended — the tree is still clean, nothing in `wt send` or
    /// `wt test` sees it, and the queue advertises a branch whose tests ran
    /// against something else.
    #[test]
    fn a_verdict_stops_describing_a_tree_that_moved() {
        let mut d = drive_with("");
        d.head = "abc123".into();
        // A path git cannot answer for leaves the verdict standing: a
        // transient failure must not hide a real result.
        assert!(d.describes("/nonexistent/worktree"));

        // A verdict recorded without a head is likewise left alone.
        let mut old = drive_with("");
        old.head = String::new();
        assert!(old.describes("/nonexistent/worktree"));

        // And a head that does not match is superseded. (Checked against this
        // repository, whose HEAD is certainly not `abc123`.)
        let here = std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir");
        if crate::git::head(&here).is_ok() {
            assert!(
                !d.describes(&here),
                "a different commit is a different tree"
            );
            let mut same = drive_with("");
            same.head = crate::git::head(&here).expect("head");
            assert!(same.describes(&here));
        }

        // 🚨 A drive still going is not superseded by the commit it is waiting
        // for: `Running` carries the pre-fix head on purpose, and blanking the
        // in-progress marker there removes it at the moment it means most.
        let mut running = drive_with("boom");
        running.state = DriveState::Running;
        running.head = "abc123".into();
        running.at = OffsetDateTime::now_utc();
        assert!(running.describes(&here));

        // 🚨 But not forever. Nothing records an ending when a drive is
        // interrupted, so a `Running` left by a process that is gone would
        // otherwise read as a live drive indefinitely.
        running.at = OffsetDateTime::now_utc() - RUNNING_LIFETIME - time::Duration::minutes(1);
        assert!(
            !running.describes(&here),
            "a drive that died overnight is not still running in the morning"
        );
    }

    #[test]
    fn states_are_named_for_a_person_reading_a_list() {
        assert_eq!(DriveState::Running.label(), "running");
        assert_eq!(DriveState::MergeReady.label(), "merge-ready");
        assert_eq!(DriveState::Stalled.label(), "stalled");
        assert_eq!(DriveState::Exhausted.label(), "exhausted");
        assert_eq!(DriveState::Unfinished.label(), "unfinished");
        assert_eq!(DriveState::Uncommitted.label(), "uncommitted");
    }

    #[test]
    fn a_commit_during_the_run_is_a_different_checkout() {
        let repo = crate::git::tests::fixture_repo();
        let path = repo.path().to_str().expect("utf-8 path").to_string();
        let before = Checkout::read(&path).expect("read");
        assert_eq!(before.branch, "main");
        assert_eq!(
            Checkout::read(&path).expect("read"),
            before,
            "nothing happened, so nothing moved"
        );

        // The worker commits while the suite is running.
        std::fs::write(repo.path().join("a.txt"), "changed\n").expect("write");
        crate::git::tests::commit_all(&path, "mid-run");
        assert_ne!(
            Checkout::read(&path).expect("read"),
            before,
            "the suite saw the old commit; the branch now holds a new one"
        );
    }

    #[test]
    fn switching_branches_during_the_run_is_a_different_checkout() {
        // Same commit, different branch: `wt merge` takes the task's branch,
        // so a worktree that moved off it is not what was verified, even with
        // the commit unchanged.
        let repo = crate::git::tests::fixture_repo();
        let path = repo.path().to_str().expect("utf-8 path").to_string();
        let before = Checkout::read(&path).expect("read");
        let out = crate::git::git()
            .args(["-C", &path, "switch", "-c", "elsewhere"])
            .output()
            .expect("git");
        assert!(out.status.success());
        let after = Checkout::read(&path).expect("read");
        assert_eq!(after.head, before.head);
        assert_ne!(after, before);
    }

    #[test]
    fn an_unreadable_checkout_is_an_error_not_an_empty_match() {
        // Two empty snapshots would compare equal and pass for "held still".
        let d = tempfile::tempdir().expect("tempdir");
        assert!(Checkout::read(d.path().to_str().expect("utf-8")).is_err());
    }

    /// 🚨 A merge-ready verdict names the commit that was tested, and keeps
    /// naming it. The worker is still running while the branch and cleanliness
    /// checks are made, so a commit landing between the last test byte and the
    /// write would be the one a re-read of HEAD picked up — and `describes`
    /// would then match it, advertising as merge-ready a commit no test saw.
    #[test]
    fn a_verdict_keeps_the_commit_that_was_tested() {
        let repo = crate::git::tests::fixture_repo();
        let path = repo.path().to_str().expect("utf-8 path").to_string();
        let tested = crate::git::head(&path).expect("head");

        let dir = std::env::temp_dir().join(format!("tower-bound-{}", std::process::id()));
        std::fs::create_dir_all(crate::meta::dir(&dir)).expect("state dir");
        let m = Meta {
            task: "hero".into(),
            repo: path.clone(),
            base: "main".into(),
            branch: "main".into(),
            path: path.clone(),
            agent: "claude".into(),
            test_cmd: "make test".into(),
            test_pass: None,
            test_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            drive: None,
            parent: None,
        };
        save_meta(&dir, &m).expect("save");

        // The worker commits in the gap before the verdict is written.
        std::fs::write(repo.path().join("a.txt"), "late\n").expect("write");
        crate::git::tests::commit_all(&path, "after the test");
        let now = crate::git::head(&path).expect("head");
        assert_ne!(now, tested);

        let bound = record_for(
            &dir,
            "hero",
            DriveState::MergeReady,
            1,
            "",
            &tested,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("record_for");
        assert_eq!(bound.head, tested, "the tested commit, not the newest one");
        assert!(
            !bound.describes(&path),
            "and the branch having moved past it reads as superseded"
        );

        // `record` is the same call with HEAD read for you, for the verdicts
        // that are about the worktree as it stands rather than about a test.
        let read = record(
            &dir,
            "hero",
            DriveState::Unfinished,
            1,
            "",
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("record");
        assert_eq!(read.head, now);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 🚨 A dispatch has to be visible before the worker has touched anything.
    /// `wt send` can land between the last test byte and the verdict; at that
    /// moment the tree is exactly as tested, so nothing about git tells the
    /// drive that the result it is about to record is already out of date.
    #[test]
    fn new_work_reaching_the_worker_is_visible_before_it_edits_anything() {
        let store = tempfile::tempdir().expect("tempdir");
        let store = tower_state::Store::new(store.path()).expect("store");
        let at = OffsetDateTime::UNIX_EPOCH;
        let mut a = tower_state::Agent::new("hero", "claude", at);
        a.state = tower_state::AgentState::DoneUnread;
        a.work_seq = 4;
        store.save(&a).expect("save");
        let before = Worker::of(&store.load("hero").expect("load"));

        // `wt send` answers that completion. The worktree has not moved.
        assert!(store
            .acknowledge_dispatch("hero", 4, at)
            .expect("acknowledge"));
        let after = Worker::of(&store.load("hero").expect("load"));
        assert_ne!(after, before, "the drive must not conclude over this");
    }

    /// And opening the tower must not read as new work. Somebody scrolling the
    /// rows while a suite runs marks the completion read, which moves the same
    /// state field a dispatch moves — the difference is `done_acked`.
    #[test]
    fn a_person_reading_the_row_is_not_new_work() {
        let store = tempfile::tempdir().expect("tempdir");
        let store = tower_state::Store::new(store.path()).expect("store");
        let at = OffsetDateTime::UNIX_EPOCH;
        let mut a = tower_state::Agent::new("hero", "claude", at);
        a.state = tower_state::AgentState::DoneUnread;
        a.work_seq = 4;
        store.save(&a).expect("save");
        let before = Worker::of(&store.load("hero").expect("load"));

        store.mark_read("hero", at).expect("mark_read");
        let after = Worker::of(&store.load("hero").expect("load"));
        assert_eq!(
            after, before,
            "the same completion, still the one being graded"
        );
    }

    /// A turn that actually begins is caught by the count, whatever the flags
    /// say — that is what the count is for.
    #[test]
    fn a_turn_beginning_is_caught_by_the_count() {
        let at = OffsetDateTime::UNIX_EPOCH;
        let mut a = tower_state::Agent::new("hero", "claude", at);
        a.state = tower_state::AgentState::DoneUnread;
        a.work_seq = 4;
        let before = Worker::of(&a);
        a.work_seq = 5;
        assert_ne!(Worker::of(&a), before);
    }

    fn bound_meta(dir: &std::path::Path, path: &str) {
        let m = Meta {
            task: "hero".into(),
            repo: path.into(),
            base: "main".into(),
            branch: "main".into(),
            path: path.into(),
            agent: "claude".into(),
            test_cmd: "make test".into(),
            test_pass: None,
            test_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            drive: None,
            parent: None,
        };
        std::fs::create_dir_all(crate::meta::dir(dir)).expect("state dir");
        save_meta(dir, &m).expect("save");
    }

    /// 🚨 Comparing the worker and writing the verdict were two steps, and
    /// `wt send` fits between them: its invalidation finds no verdict to clear,
    /// so the write that follows re-establishes one over an agent already given
    /// other work. The re-read happens inside the lock the dispatch takes.
    #[test]
    fn a_verdict_is_not_published_over_newer_work() {
        let state = tempfile::tempdir().expect("tempdir");
        bound_meta(state.path(), "/w");
        let tested = Worker {
            work_seq: 4,
            settled: true,
        };

        // The dispatch lands in the window: the observation made inside the
        // lock is of a worker that has moved on.
        let refused = record_for_if(
            state.path(),
            "hero",
            DriveState::MergeReady,
            1,
            "",
            "abc123",
            tested,
            || {
                Some(Worker {
                    work_seq: 4,
                    settled: false,
                })
            },
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("record_for_if");
        assert!(refused.is_none(), "nothing may be written");
        assert!(
            load_meta(state.path(), "hero")
                .expect("load")
                .drive
                .is_none(),
            "and the record stays as the dispatch left it"
        );

        // Nothing moved: the verdict is published as usual.
        let published = record_for_if(
            state.path(),
            "hero",
            DriveState::MergeReady,
            1,
            "",
            "abc123",
            tested,
            || Some(tested),
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("record_for_if")
        .expect("published");
        assert_eq!(published.state, DriveState::MergeReady);
        assert_eq!(published.head, "abc123");
    }

    /// A worker that cannot be found at all is not one to publish over either.
    #[test]
    fn a_missing_worker_is_not_a_matching_one() {
        let state = tempfile::tempdir().expect("tempdir");
        bound_meta(state.path(), "/w");
        let refused = record_for_if(
            state.path(),
            "hero",
            DriveState::MergeReady,
            1,
            "",
            "abc123",
            Worker {
                work_seq: 4,
                settled: true,
            },
            || None,
            OffsetDateTime::UNIX_EPOCH,
        )
        .expect("record_for_if");
        assert!(refused.is_none());
    }

    /// `wt list` answers from readings it already took. An unreadable head
    /// cannot prove a mismatch; an unreadable tree cannot prove cleanliness.
    #[test]
    fn a_verdict_answers_from_readings_already_in_hand() {
        let now = OffsetDateTime::now_utc();
        let mut ready = drive_with("");
        ready.state = DriveState::MergeReady;
        ready.head = "abc123".into();

        assert!(ready.describes_at(now, Some("abc123"), Some(0)));
        assert!(
            !ready.describes_at(now, Some("def456"), Some(0)),
            "a different commit"
        );
        assert!(
            !ready.describes_at(now, Some("abc123"), Some(2)),
            "the same commit with uncommitted edits is not what would merge"
        );
        assert!(
            !ready.describes_at(now, Some("abc123"), None),
            "a tree that cannot be read is not a clean one"
        );
        assert!(
            ready.describes_at(now, None, Some(0)),
            "an unreadable head proves no mismatch"
        );

        // Only a merge-ready verdict asks about the working tree.
        let mut stalled = ready.clone();
        stalled.state = DriveState::Stalled;
        assert!(stalled.describes_at(now, Some("abc123"), None));
    }

    /// Non-ASCII passes through as itself. The first version copied each
    /// non-digit *byte* as a `char`, so `한` came out as three Latin-1
    /// characters — equality still held, but the fingerprint was no longer the
    /// text it claimed to be, and it was not idempotent.
    #[test]
    fn the_fingerprint_keeps_non_ascii_text_intact() {
        let cases = [
            ("korean", "테스트 실패 0.12s", "테스트 실패 <elapsed>"),
            ("accent", "café 3ms", "café <elapsed>"),
            (
                "box",
                "── 1 failing (40ms) ──",
                "── 1 failing (<elapsed>) ──",
            ),
            ("empty", "", ""),
            ("bare number", "42", "42"),
            ("unit only", "s", "s"),
        ];
        for (name, input, want) in cases {
            assert_eq!(fingerprint(input), want, "{name}");
        }
    }

    /// Property: applying it twice changes nothing.
    ///
    /// Not because a placeholder carries no digits — that alone did not hold
    /// it: `1m30s` became `1m<elapsed>`, and a second pass then read the `1m`
    /// as a duration of its own. It holds because every number is decided by
    /// what follows it, and what follows is either left as it was or becomes
    /// `<elapsed>`, which is never a unit. So a second pass makes the same
    /// decision at every number the first one did.
    #[test]
    fn the_fingerprint_is_idempotent() {
        let corpus = [
            "test result: FAILED. 2 passed; 1 failed; finished in 0.03s",
            "FAIL\tpkg/x\t0.123s",
            "테스트 12개 중 3개 실패 (1.5s)",
            "expected 10submissions, got 9",
            "line 3:7 in 2s and 40ms",
            "",
            "1m30s",
            "timed out after 1h2m3.5s",
            "Time:        1.234 s",
            "bench 1.5µs/op",
            "ok  \tpkg/y\t(cached)",
            "5s3x",
            "5 3s",
        ];
        for text in corpus {
            let once = fingerprint(text);
            assert_eq!(fingerprint(&once), once, "{text:?}");
        }
    }

    /// Property: summarizing a summary changes nothing. The tail doubles as the
    /// repeat fingerprint, so a second pass that trimmed further would make one
    /// failure compare unequal to itself.
    #[test]
    fn summarizing_is_idempotent() {
        let mut corpus: Vec<String> = vec![
            String::new(),
            "\n\n\n".into(),
            "one line".into(),
            "trailing\n\n".into(),
        ];
        // A deterministic spread of lengths either side of the budget.
        for n in [1, 39, 40, 41, 120] {
            corpus.push(
                (0..n)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        for text in &corpus {
            for budget in [1, 3, SUMMARY_LINES] {
                let once = summarize_failure(text, budget);
                assert_eq!(summarize_failure(&once, budget), once, "{budget}: {text:?}");
                assert!(once.lines().count() <= budget);
            }
        }
    }

    /// 🚨 A task name with a slash stays one file in `worktrees/`. `review_path`
    /// once joined the raw name, so `feat/login` wanted a `worktrees/feat/`
    /// directory that did not exist; the failure log is encoded the same way
    /// the record is so that mistake is not made twice.
    #[test]
    fn a_failure_log_for_a_slashed_task_is_one_file_beside_the_record() {
        let state = tempfile::tempdir().expect("tempdir");
        for task in ["feat/login", "a/b/c", "한글/작업", "plain"] {
            let path = write_failure_log(state.path(), task, "FAIL").expect(task);
            assert_eq!(
                path.parent(),
                Some(state.path().join("worktrees").as_path()),
                "{task}: no subdirectory"
            );
            assert_eq!(std::fs::read_to_string(&path).expect("read"), "FAIL\n");
        }
        assert_ne!(
            failure_log_path(state.path(), "feat/login"),
            failure_log_path(state.path(), "feat_login"),
            "encoding must not merge two tasks into one log"
        );
    }

    /// Edge: the lifetime is exclusive at its end, and staleness only grows
    /// with time.
    #[test]
    fn a_running_record_goes_stale_exactly_after_its_lifetime() {
        let mut d = drive_with("");
        d.state = DriveState::Running;
        let start = OffsetDateTime::UNIX_EPOCH;
        d.at = start;
        assert!(!d.is_stale(start));
        assert!(
            !d.is_stale(start + RUNNING_LIFETIME),
            "at the boundary: still live"
        );
        assert!(d.is_stale(start + RUNNING_LIFETIME + time::Duration::seconds(1)));

        let mut was_stale = false;
        for minutes in (0..=24 * 60).step_by(30) {
            let now = start + time::Duration::minutes(minutes);
            let stale = d.is_stale(now);
            assert!(
                !was_stale || stale,
                "{minutes}m: once stale, stale thereafter"
            );
            was_stale = stale;
        }
    }
}
