// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `wake-all`, `close-all` and `broadcast` — one message to every session.
//!
//! The three share a body because they differ in exactly two things: the text
//! sent, and whether the folder has to observe the session-handoff discipline
//! to receive it. Everything else — who counts as a target, the confirmation,
//! the optional wait for each turn to end — is the same instruction going to
//! several places at once, and that is the part worth getting right in one
//! place.
//!
//! Who counts as a target, and what a folder needs to have to receive a wake,
//! are [`tower_ui::targets`] and [`tower_ui::has_session_handoff`]. Those two landed in
//! `tower_ui` first because the TUI's `W`/`C`/`B` keys do the same thing and `tower_ui`
//! cannot depend on `tower_cli`; a second copy here would be two answers to one
//! question. What this module adds on top is the `--except` list, which only
//! exists on the command line.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::Duration;

use herdrx::Herdr;
use time::OffsetDateTime;
use tower_state::{Agent, AgentState, Store};

use crate::helpcmd::flags::{FlagError, FlagSet};
use crate::{pad_right, shorten_home, since};

/// The wake and close texts, worded to match the user's own session-handoff
/// prompt fragments — an agent that has been taught the wake phrase as a
/// ritual phrase reacts to it, and merely discusses a paraphrase.
pub use tower_ui::{has_session_handoff, shown, CLOSE_MESSAGE, WAKE_MESSAGE};

/// How often the completion watch re-reads the store.
const WATCH_POLL: Duration = Duration::from_secs(5);

/// Who receives a batch instruction, minus the sessions named in `--except`.
///
/// `self_pane` is passed in rather than read from the environment here:
/// reading one backend's pane variable directly ties the check to that
/// backend's naming, and a batch instruction issued from anywhere else is then
/// delivered to the session issuing it — the tower telling itself to wrap up.
/// An empty string means "no self to exclude".
///
/// Exclusions are trimmed because they arrive as a comma-separated list, and
/// `--except a, b` is what a person types.
pub fn targets<'a>(agents: &'a [Agent], self_pane: &str, except: &[String]) -> Vec<&'a Agent> {
    let skip: HashSet<&str> = except
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .collect();
    tower_ui::targets(agents, Some(self_pane).filter(|p| !p.is_empty()))
        .into_iter()
        .filter(|a| !skip.contains(a.pane.session.as_str()))
        .collect()
}

/// What the three commands accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllOptions {
    pub yes: bool,
    pub except: Vec<String>,
    pub watch: bool,
    pub timeout: Duration,
}

/// `default_watch` differs per command, which is why it is a parameter and not
/// a constant: waking a fleet is worth watching to completion, broadcasting an
/// arbitrary message usually is not.
pub fn parse_all_flags(
    name: &str,
    args: &[String],
    default_watch: bool,
) -> Result<(AllOptions, Vec<String>), FlagError> {
    let fs = all_flag_set(name, default_watch);
    let p = fs.parse(args)?;
    let except = p.str("except");
    Ok((
        AllOptions {
            yes: p.bool("yes"),
            except: if except.is_empty() {
                Vec::new()
            } else {
                except.split(',').map(str::to_string).collect()
            },
            watch: p.bool("watch"),
            timeout: p.duration("timeout"),
        },
        p.args().to_vec(),
    ))
}

/// Kept separate so a caller can print the usage block for a refusal.
pub fn all_flag_set(name: &str, default_watch: bool) -> FlagSet {
    FlagSet::new(name)
        .bool("yes", false, "send without asking (for automation)")
        .string("except", "", "session names to leave out (comma separated)")
        .bool("watch", default_watch, "watch for completion after sending")
        .duration("timeout", Duration::from_secs(600), "how long to watch")
}

/// The body of `wake-all` / `close-all` / `broadcast`.
///
/// `confirm` is injected rather than read from stdin here, matching
/// `tower_wt::merge_guide`: the prompt is the caller's business, and a function
/// that reads the terminal cannot be run twice in one test process.
///
/// The listing is printed *before* the confirmation on purpose. The user is
/// about to interrupt several working agents at once and has to see which.
///
/// The kind and session columns are padded to a **display** width, not a
/// character count. A character-count pad shears on any CJK session name — a
/// four-character one occupies eight columns — so the folder column would step
/// right by however many CJK characters the session name held. `status`
/// already pads by column for exactly this reason.
#[allow(clippy::too_many_arguments)]
pub fn run_all<W: Write>(
    w: &mut W,
    store: &Store,
    h: &Herdr,
    self_pane: &str,
    message: &str,
    o: &AllOptions,
    handoff_only: bool,
    backend: &str,
    confirm: impl FnOnce() -> bool,
) -> std::io::Result<()> {
    let agents = store.list()?;
    let mut targets = targets(&agents, self_pane, &o.except);
    targets.retain(|agent| agent.pane.backend == backend);

    if handoff_only {
        let before = targets.len();
        targets.retain(|a| has_session_handoff(&a.cwd));
        if targets.len() < before {
            writeln!(
                w,
                "({} folders left out: no SESSION.md and no handoff discipline)",
                before - targets.len()
            )?;
        }
    }
    if targets.is_empty() {
        return writeln!(w, "there is nobody to send to.");
    }

    writeln!(
        w,
        "{} recipients — message: {}",
        targets.len(),
        shown(message)
    )?;
    for a in &targets {
        // A WORKING agent will not see this until its current turn ends. Say
        // so, or the silence reads as a failed delivery.
        let note = if a.state == AgentState::Working {
            "  ⚠ working — the instruction is handled after the current turn"
        } else {
            ""
        };
        writeln!(
            w,
            "  {} {} {}{note}",
            pad_right(&a.kind, 7),
            pad_right(&a.pane.session, 20),
            shorten_home(&a.cwd)
        )?;
    }

    if !o.yes && !confirm() {
        return writeln!(w, "cancelled.");
    }

    // Closing a fleet and leaving an autonomous lease behind would reopen the
    // very work the close instruction is ending. This happens only after the
    // confirmation, so cancelling a close changes nothing.
    //
    // Removed under the lease lock in one step (#127). Reading the list and
    // writing back what was left raced the supervisor, which plans under that
    // lock and keeps each lease's send reservation in the same file: a pass
    // between the two was overwritten, and another server's lease lost its
    // reservation and could be sent the same idle generation twice.
    if message == CLOSE_MESSAGE {
        let loops = tower_state::LoopStore::new(store.dir())?;
        for lease in loops.disarm_backend(backend)? {
            crate::loopcmd::clear_display(h, &lease.pane_id);
        }
    }

    // The send boundary, read *after* the confirmation. `now` was captured before
    // the store was synchronized and before the operator read the target list, so
    // an agent that finished while they were deciding had a completion stamped
    // later than it — and `finished_since` counted that old completion as the
    // answer to an instruction that had not been sent yet, reporting the session
    // complete immediately.
    let sent_at = OffsetDateTime::now_utc();

    let mut sent: Vec<&Agent> = Vec::new();
    let mut failed = 0usize;
    // Turn counts read at the send boundary, not from the list the operator was
    // shown. Between the listing and their answer an agent can finish a turn and
    // start another, or go from idle to working — and a counter sampled before
    // that was the wrong one, which put the watch back where it started: the
    // intervening turn's completion counted as the answer to this instruction.
    let mut queued: HashMap<&str, Dispatch> = HashMap::new();
    for a in &targets {
        let before = store.load(&a.id).ok();
        match dispatch(before.as_ref(), || {
            tower_mux::send_text(h, &a.pane.pane_id, message)
        }) {
            Ok(d) => {
                queued.insert(a.id.as_str(), d);
                // Dispatching is new work, so a completion the user already read
                // is no longer the current one. Without this a turn answered
                // faster than the poll interval reads as the acknowledged
                // completion and the row stays IDLE — see `Agent::done_acked`.
                store.note_dispatch(&a.id, sent_at);
                sent.push(a);
            }
            Err(e) => {
                failed += 1;
                writeln!(w, "  ✖ {} could not be reached: {e}", a.pane.session)?;
            }
        }
    }
    writeln!(w, "sent to {} sessions.", sent.len())?;

    if o.watch && !sent.is_empty() {
        watch_done(
            w,
            store,
            h,
            CompletionWatch {
                backend,
                sent: &sent,
                queued: &queued,
                sent_at,
                timeout: o.timeout,
            },
        )?;
    }
    if failed > 0 {
        // Every target was attempted and each failure was named above; this is
        // the exit code. `--yes` is the automation mode, and a supervisor that
        // read success here would record a `wake-all` that some agents never
        // got.
        return Err(std::io::Error::other(format!(
            "could not reach {failed} sessions"
        )));
    }
    Ok(())
}

/// Sends to everyone with no prompt and no listing.
///
/// The confirmation is the caller's: the CLI asks `[y/N]`, the TUI asks with a
/// keypress, and neither wants the other's version of the question.
pub fn send_all(
    store: &Store,
    h: &Herdr,
    self_pane: &str,
    message: &str,
    handoff_only: bool,
    now: OffsetDateTime,
) -> std::io::Result<(usize, usize)> {
    let agents = store.list()?;
    let mut targets = targets(&agents, self_pane, &[]);
    if handoff_only {
        targets.retain(|a| has_session_handoff(&a.cwd));
    }
    let mut sent = 0usize;
    for a in &targets {
        if tower_mux::send_text(h, &a.pane.pane_id, message).is_ok() {
            store.note_dispatch(&a.id, now);
            sent += 1;
        }
    }
    Ok((sent, targets.len()))
}

/// Whether the turn *this dispatch* asked for has finished.
///
/// `at_dispatch` is the agent's turn count when the instruction went out, and
/// only for a session that was already WORKING — for anything else the first
/// completion after the dispatch is plainly its own.
///
/// The count is what makes this answerable. A completion does not say which turn
/// produced it, so a session that was mid-turn had its *running* turn's `DONE`
/// counted as the answer and `close-all` reported it finished while the queued
/// instruction had not begun. Watching for `WORKING` to appear instead missed a
/// turn that began and ended between two five-second polls; the count survives
/// the turn, which is the whole reason it is on the record.
fn dispatched_turn_finished(at_dispatch: Baseline, now: &Agent, sent_at: OffsetDateTime) -> bool {
    let its_own = match at_dispatch {
        Baseline::Settled => true,
        Baseline::Working(seq) => now.work_seq > seq,
        // Nothing to compare against, so nothing can be confirmed. The session is
        // reported unconfirmed when the watch ends, which is a person looking —
        // the alternative is automation tearing down an agent on the strength of
        // a record nobody could read.
        Baseline::Unknown => false,
    };
    its_own && finished_since(now, sent_at)
}

/// Reads this recipient's boundary, then sends.
///
/// The order is the whole of it, which is why it is a function and not two lines
/// in a loop. Both halves of the boundary — what the record said, and when —
/// describe the moment *before* the instruction went in. Read after `send`
/// returns, the instant lands on the far side of a turn that was answered inside
/// the delivery call, and `finished_since` then rejects the very completion that
/// was asked for: the watch waits out its whole timeout on finished work.
///
/// Nothing is lost by reading early. What the later reading was guarding against
/// — an older completion counted as the answer — is the baseline's job: a turn
/// that was already running finishes with the count where it was.
fn dispatch<E>(
    before: Option<&Agent>,
    send: impl FnOnce() -> Result<(), E>,
) -> Result<Dispatch, E> {
    let d = Dispatch {
        baseline: Baseline::of(before),
        // This recipient's own boundary, not the loop's: with one shared instant,
        // a session sent to late could have a completion from before its
        // instruction counted as the answer to it.
        at: OffsetDateTime::now_utc(),
    };
    send()?;
    Ok(d)
}

/// One instruction, as it was actually delivered.
#[derive(Debug, Clone, Copy)]
struct Dispatch {
    baseline: Baseline,
    /// When *this* recipient's instruction went in.
    at: OffsetDateTime,
}

/// What was known about a session when its instruction was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Baseline {
    /// Not working: the first completion after the dispatch is plainly its own.
    Settled,
    /// Mid-turn, with the count of turns it had begun by then.
    Working(u64),
    /// The record could not be read. Spelled out rather than folded into
    /// `Settled`, which is what an `Option` did — and "unknown" behaving as
    /// "idle" accepted the running turn's completion the moment the store came
    /// back.
    Unknown,
}

impl Baseline {
    /// What the record said in the moment before the instruction went in.
    ///
    /// Read as late as it can be — immediately before the send — and not again
    /// afterwards. A second reading was tried and taken out: it cannot tell an
    /// unrelated turn that began during the send from *this prompt's own* turn,
    /// whose start hook usually beats it there. Treating that as unrelated made
    /// the ordinary case wait out the whole timeout, which is a worse answer than
    /// the race it was closing — a turn beginning inside the milliseconds between
    /// this read and the write.
    fn of(before: Option<&Agent>) -> Baseline {
        match before {
            Some(a) if a.state == AgentState::Working => Baseline::Working(a.work_seq),
            Some(_) => Baseline::Settled,
            None => Baseline::Unknown,
        }
    }
}

/// Whether this record's turn ended *after* the message went out.
///
/// The timestamp comparison is the whole point: an agent already sitting at
/// DONE when the message arrived would otherwise be counted as having
/// answered it instantly.
fn finished_since(a: &Agent, sent_at: OffsetDateTime) -> bool {
    a.state == AgentState::DoneUnread && a.state_since > sent_at
}

struct CompletionWatch<'a> {
    backend: &'a str,
    sent: &'a [&'a Agent],
    queued: &'a HashMap<&'a str, Dispatch>,
    sent_at: OffsetDateTime,
    timeout: Duration,
}

/// Waits for each recipient's turn to end, by reconciling and re-reading.
///
/// Polling rather than a subscription because the signal wanted is the DONE
/// transition that lands in the record store; herdr's own settle notification
/// answers a different question (the agent stopped typing) and would report
/// completion for a turn that was never started.
///
/// The reconcile inside the loop is what makes it work without hooks. A hook
/// writes that transition directly, but a session without them has only the
/// backend, and nothing else runs `sync_backend` while this loop holds the
/// terminal.
fn watch_done<W: Write>(
    w: &mut W,
    store: &Store,
    h: &Herdr,
    watch: CompletionWatch<'_>,
) -> std::io::Result<()> {
    writeln!(
        w,
        "watching for completion (timeout {}) — Ctrl+C stops the watch, nothing else.",
        crate::helpcmd::format_duration(watch.timeout)
    )?;

    let mut pending: Vec<&Agent> = watch.sent.to_vec();
    // `queued` holds the turn count of each session whose instruction went into a
    // turn that was already running, read where the instruction was actually
    // written. Their *current* turn's completion also lands after `sent_at`, so
    // it satisfies `finished_since` while the queued instruction has not begun —
    // and a `close-all` that reported those as done let automation start tearing
    // down agents that were still working.
    let deadline = std::time::Instant::now() + watch.timeout;
    while !pending.is_empty() && std::time::Instant::now() < deadline {
        // Never past the deadline the caller set. A flat poll interval meant
        // `--timeout 1s` slept five seconds before its first look, so the ceiling
        // a script asked for was not the ceiling it got.
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(WATCH_POLL.min(left));
        // Reconcile before reading. Nothing else is writing these records while
        // we wait: a session whose hooks are not installed would never change on
        // disk, and the watch would run out its ten minutes with the turns long
        // finished. Hook precedence still
        // applies inside `apply_observations`, so a session that *does* have
        // hooks is unaffected by this.
        let _ = tower_scan::sync_named_backend(store, h, watch.backend, OffsetDateTime::now_utc());
        let mut still = Vec::new();
        for a in pending {
            // A record that cannot be read is not evidence of anything; keep
            // waiting for it rather than declaring it finished.
            let Ok(cur) = store.load(&a.id) else {
                still.push(a);
                continue;
            };
            let d = watch
                .queued
                .get(a.id.as_str())
                .copied()
                .unwrap_or(Dispatch {
                    baseline: Baseline::Settled,
                    at: watch.sent_at,
                });
            if dispatched_turn_finished(d.baseline, &cur, d.at) {
                writeln!(
                    w,
                    "  ✔ {} done ({})",
                    pad_right(&a.pane.session, 20),
                    since(watch.sent_at, OffsetDateTime::now_utc())
                )?;
                continue;
            }
            still.push(a);
        }
        pending = still;
    }

    if pending.is_empty() {
        return writeln!(w, "all {} sessions finished.", watch.sent.len());
    }
    let unconfirmed = pending.len();
    writeln!(w, "{unconfirmed} unconfirmed (timed out):")?;
    for a in pending {
        let reason = if a.kind == "codex" {
            "codex completions go unseen until notify is enabled — check it yourself"
        } else {
            "still working, or not answering"
        };
        writeln!(w, "  ? {} {reason}", pad_right(&a.pane.session, 20))?;
    }
    // The exit code, for the same reason a delivery failure has one. `close-all`
    // watches by default, and automation that read zero here would go on to tear
    // down or restart machines whose agents were still working.
    Err(std::io::Error::other(format!(
        "{unconfirmed} sessions unconfirmed"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn that began and ended between two polls still counts.
    ///
    /// Watching for `WORKING` to appear could not see it — the store held only
    /// the final `DONE` — so a `close-all` into a busy session reported it
    /// unconfirmed until the watch timed out. The count survives the turn.
    /// The baseline is what the record said just before the instruction went in.
    #[test]
    fn the_baseline_is_the_reading_before_the_send() {
        let idle = agent("claude", "s1", "%1", AgentState::Idle);
        assert_eq!(Baseline::of(Some(&idle)), Baseline::Settled);

        let mut busy = idle.clone();
        busy.state = AgentState::Working;
        busy.work_seq = 4;
        assert_eq!(Baseline::of(Some(&busy)), Baseline::Working(4));

        // A record nobody could read is not a record that said idle.
        assert_eq!(Baseline::of(None), Baseline::Unknown);
    }

    /// The boundary is read before the send, not after it.
    ///
    /// A short instruction can be answered inside the delivery call. Stamped
    /// afterwards, the boundary lands on the far side of that completion and
    /// `finished_since` rejects it — the watch then waits out its whole timeout
    /// on a turn that finished before it started looking.
    #[test]
    fn an_instruction_answered_inside_the_delivery_call_is_still_confirmed() {
        let idle = agent("claude", "s1", "%1", AgentState::Idle);

        // The agent answers while the send is still in progress.
        let mut answered_at = None;
        let d = dispatch::<std::io::Error>(Some(&idle), || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            answered_at = Some(OffsetDateTime::now_utc());
            Ok(())
        })
        .unwrap();

        let mut done = idle.clone();
        done.state = AgentState::DoneUnread;
        done.state_since = answered_at.unwrap();
        assert!(
            dispatched_turn_finished(d.baseline, &done, d.at),
            "the answer arrived before the send returned, and it is still the answer"
        );

        // A failed send is not a dispatch at all — nothing to watch for.
        let err = dispatch(Some(&idle), || Err(std::io::Error::other("no pane")));
        assert!(err.is_err());
    }

    #[test]
    fn a_queued_turn_is_recognised_by_the_count_not_by_catching_it_running() {
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let mut busy = agent("claude", "s1", "%1", AgentState::Working);
        busy.work_seq = 7;

        // The turn it was already running finishes: same count, so not ours.
        let mut old_turn_done = busy.clone();
        old_turn_done.state = AgentState::DoneUnread;
        old_turn_done.state_since = at + time::Duration::seconds(1);
        assert!(!dispatched_turn_finished(
            Baseline::Working(busy.work_seq),
            &old_turn_done,
            at
        ));

        // The queued one runs and finishes between two polls: the count moved.
        let mut ours_done = old_turn_done.clone();
        ours_done.work_seq = 8;
        assert!(dispatched_turn_finished(
            Baseline::Working(busy.work_seq),
            &ours_done,
            at
        ));

        // A session that was idle at dispatch needs no such proof.
        let idle = agent("claude", "s2", "%2", AgentState::Idle);
        let mut done = idle.clone();
        done.state = AgentState::DoneUnread;
        done.state_since = at + time::Duration::seconds(1);
        assert!(dispatched_turn_finished(Baseline::Settled, &done, at));

        // And a baseline nobody could read confirms nothing at all: an
        // unreadable record must not read as "it was idle".
        assert!(!dispatched_turn_finished(Baseline::Unknown, &done, at));
        assert!(!dispatched_turn_finished(Baseline::Unknown, &ours_done, at));
    }

    fn agent(kind: &str, session: &str, pane: &str, state: AgentState) -> Agent {
        let mut a = Agent::new(format!("{kind}-{pane}"), kind, OffsetDateTime::UNIX_EPOCH);
        a.state = state;
        a.pane.session = session.into();
        a.pane.pane_id = pane.into();
        a
    }

    fn sessions(got: &[&Agent]) -> Vec<String> {
        got.iter().map(|a| a.pane.session.clone()).collect()
    }

    #[test]
    fn the_issuing_session_dead_records_and_excluded_names_are_all_left_out() {
        let agents = vec![
            agent("claude", "collab-bot", "%1", AgentState::Idle),
            agent("codex", "codex-live", "%2", AgentState::Idle),
            agent("gemini", "gem", "%3", AgentState::Idle),
            agent("claude", "dead-bot", "%4", AgentState::Dead),
            agent("claude", "ai", "%5", AgentState::Working),
            agent("claude", "zzukumi-bot", "%6", AgentState::Idle),
        ];
        let got = targets(&agents, "%5", &["zzukumi-bot".to_string()]);
        assert_eq!(sessions(&got), ["collab-bot", "codex-live", "gem"]);
    }

    /// The 2026-08-26 rule that a control tower feature covers every
    /// supervised vendor. gemini was missing from the first version of this
    /// filter, and opencode from the one after it was supervised.
    #[test]
    fn every_supervised_kind_receives_batch_instructions() {
        for kind in tower_state::SUPERVISED_KINDS {
            let agents = vec![agent(kind, "s", "%1", AgentState::Idle)];
            assert_eq!(targets(&agents, "", &[]).len(), 1, "{kind}");
        }
        let agents = vec![agent("aider", "s", "%1", AgentState::Idle)];
        assert!(
            targets(&agents, "", &[]).is_empty(),
            "not a supervised kind"
        );
    }

    /// A DONE that was already there when the message went out is not an
    /// answer to it.
    #[test]
    fn only_a_turn_that_ended_after_the_message_counts_as_done() {
        let sent_at = OffsetDateTime::from_unix_timestamp(1_756_000_000).unwrap();
        let mut a = agent("claude", "s", "%1", AgentState::DoneUnread);

        a.state_since = sent_at - Duration::from_secs(60);
        assert!(!finished_since(&a, sent_at), "a turn that ended before");

        a.state_since = sent_at + Duration::from_secs(60);
        assert!(finished_since(&a, sent_at));

        a.state = AgentState::Working;
        assert!(!finished_since(&a, sent_at), "still working");
    }

    #[test]
    fn flags_and_the_message_after_them_both_survive_parsing() {
        let args: Vec<String> = ["--yes", "--except", "a,b", "hello there"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (o, rest) = parse_all_flags("broadcast", &args, false).unwrap();
        assert!(o.yes);
        assert_eq!(o.except, ["a", "b"]);
        assert!(!o.watch);
        assert_eq!(o.timeout, Duration::from_secs(600));
        assert_eq!(rest, ["hello there"]);
    }

    /// An empty `--except` is no exclusions, not one exclusion named "".
    #[test]
    fn an_empty_exclusion_list_excludes_nothing() {
        let (o, _) = parse_all_flags("wake-all", &["--except=".to_string()], true).unwrap();
        assert!(o.except.is_empty());
        assert!(o.watch, "wake-all watches by default");
    }

    /// A record with no coordinate has nowhere to receive anything, so a send
    /// to it can only fail; `tower_ui::targets` drops it first, and the
    /// composition here has to keep doing so.
    #[test]
    fn a_record_with_no_coordinate_is_not_a_target() {
        let mut a = agent("claude", "ghost", "", AgentState::Idle);
        a.pane.pane_id = String::new();
        assert!(targets(&[a], "", &[]).is_empty());
    }

    /// Exclusions arrive as a comma-separated list a person typed, so the
    /// spacing around the commas is theirs and not ours to insist on.
    #[test]
    fn an_exclusion_is_matched_after_trimming_the_spaces_around_it() {
        let agents = vec![
            agent("claude", "collab-bot", "%1", AgentState::Idle),
            agent("claude", "zzukumi-bot", "%2", AgentState::Idle),
        ];
        let except = vec![" zzukumi-bot ".to_string(), "  ".to_string()];
        assert_eq!(sessions(&targets(&agents, "", &except)), ["collab-bot"]);
    }
}
