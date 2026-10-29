// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower schedule` — messages sent at a set time — and the supervisor's
//! plan for delivering them.
//!
//! ```text
//! herdr-tower schedule                          what is scheduled
//! herdr-tower schedule close 19:00 [--weekdays] the close instruction to every handoff session
//! herdr-tower schedule wake 09:00 [--weekdays]  the wake instruction to every handoff session
//! herdr-tower schedule add <time> [--daily|--weekdays] <who> [--wait 30m] [--until DATE] <message>
//! herdr-tower schedule rm <id>… | all
//! ```
//!
//! `<time>` is `HH:MM` (today, or tomorrow if it has passed), `YYYY-MM-DDTHH:MM`,
//! or `+30m` / `+2h`. `<who>` is `--session <id>` (whole, or the eight characters
//! the tower's SID column shows), `--workspace <name>` or `--handoff`.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use time::OffsetDateTime;
use tower_state::{
    clock_label, new_schedule_id, parse_date, parse_span, parse_when, Agent, AgentState, Repeat,
    Schedule, ScheduleStore, ScheduleTarget, When,
};

type Failure = Box<dyn std::error::Error>;

/// One message the supervisor should send now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleSend {
    pub schedule: String,
    /// What marks the target as served for this occurrence — recorded
    /// already, by [`plan`]; [`unmark`] takes them back if the send fails.
    pub keys: Vec<String>,
    pub agent_id: String,
    pub backend: String,
    pub pane_id: String,
    pub message: String,
}

/// What a pass decided: messages to send, and lines for the supervisor's log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SchedulePlan {
    pub sends: Vec<ScheduleSend>,
    pub notes: Vec<String>,
}

/// What marks `a` as served: its record, its pane, and its conversation
/// where it has one. All of them, so a pane whose conversation changes inside
/// the window — a `/clear`, or a first session ID reported — is not sent to a
/// second time, nor a conversation whose pane moves, nor a pane two records
/// claim.
pub fn delivery_keys(a: &Agent) -> Vec<String> {
    let mut keys = vec![
        format!("a:{}", a.id),
        format!(
            "p:{}",
            tower_mux::target_key(&a.pane.backend, &a.pane.pane_id)
        ),
    ];
    let sid = a.session_id.trim();
    if !sid.is_empty() {
        keys.push(format!("s:{sid}"));
    }
    keys
}

fn served(s: &Schedule, a: &Agent) -> bool {
    delivery_keys(a).iter().any(|k| s.delivered.contains(k))
}

/// At rest: finished or never started, and not asking anything.
fn at_rest(a: &Agent) -> bool {
    matches!(a.state, AgentState::Idle | AgentState::DoneUnread)
}

/// The agents a schedule could be for, as the records stand — the ones a
/// broadcast reaches, on a backend this pass reached — before the handoff
/// test, which is left to [`Folders`].
fn candidates<'a>(
    s: &Schedule,
    agents: &'a [Agent],
    connected: &HashSet<String>,
) -> Vec<&'a Agent> {
    tower_ui::targets(agents, None)
        .into_iter()
        .filter(|a| connected.contains(&a.pane.backend))
        .filter(|a| match &s.target {
            ScheduleTarget::Session { id, .. } => a.session_id.trim() == id,
            ScheduleTarget::Workspace { name } => &a.pane.session == name,
            ScheduleTarget::Handoff => true,
        })
        .collect()
}

/// The handoff test, asked last — of an agent otherwise ready for the
/// message — and once per folder a pass, because it reads the folder's files.
struct Folders<'a> {
    test: &'a dyn Fn(&str) -> bool,
    known: HashMap<String, bool>,
}

impl Folders<'_> {
    fn keep(&mut self, s: &Schedule, a: &Agent) -> bool {
        if !matches!(s.target, ScheduleTarget::Handoff) {
            return true;
        }
        if let Some(&known) = self.known.get(&a.cwd) {
            return known;
        }
        let found = (self.test)(&a.cwd);
        self.known.insert(a.cwd.clone(), found);
        found
    }
}

/// One pass over the schedules: which occurrences open or close, and who gets
/// a message now.
///
/// `now` carries the local offset, since that is what a time of day means.
/// `busy` names agents the supervisor wrote to a moment ago — an idle loop, or
/// a schedule on an earlier pass whose message the records do not show yet —
/// which are left for later rather than handed two messages at once. For the
/// same reason one pane, or one conversation, gets one message a pass however
/// many schedules are due for it.
///
/// `held` names agents the usage guard holds back. They are not sent to
/// either, but unlike `busy` they do not run the wait out: a hold lasts until
/// a window resets, hours or days, and a target held when the wait would end
/// would lose the message for good. While one the occurrence still owes is
/// held, the wait starts over each pass (`Schedule::held_at`), so the send
/// lands once the hold lifts — as the guard's own `resume_message` does.
///
/// Changes the schedules in place — the occurrence being delivered, schedules
/// that are over — and records every send it plans as served, before it is
/// sent: delivery is at most once, and the caller [`unmark`]s a send that
/// failed so it is tried again while the wait lasts.
pub fn plan(
    schedules: &mut Vec<Schedule>,
    agents: &[Agent],
    connected: &HashSet<String>,
    handoff: &dyn Fn(&str) -> bool,
    busy: &HashSet<String>,
    held: &HashSet<String>,
    now: OffsetDateTime,
) -> SchedulePlan {
    let mut out = SchedulePlan::default();
    let offset = now.offset();
    let mut folders = Folders {
        test: handoff,
        known: HashMap::new(),
    };
    // Delivery keys — panes among them — already handed a message this pass.
    let mut queued: HashSet<String> = HashSet::new();
    schedules.retain_mut(|s| {
        let label = s.when_label(offset);
        let once = matches!(s.when, When::Once { .. });

        if s.slot.is_none() {
            if s.until.is_some_and(|until| now > until) {
                out.notes
                    .push(format!("schedule {} ({label}): ended", s.id));
                return false;
            }
            if let Some(occurrence) = s.latest(now) {
                let fresh = s.last_slot.is_none_or(|last| occurrence > last);
                if fresh && occurrence < s.created_at {
                    // From before the schedule existed: the first one it
                    // owes is the next.
                    s.last_slot = Some(occurrence);
                } else if fresh && now - occurrence > s.wait() {
                    out.notes.push(format!(
                        "schedule {} ({label}): missed {} — the supervisor was not running then",
                        s.id,
                        clock_label(occurrence, now)
                    ));
                    s.last_slot = Some(occurrence);
                    if once {
                        return false;
                    }
                } else if fresh {
                    s.slot = Some(occurrence);
                    s.delivered.clear();
                    s.held_at = None;
                }
            }
        }

        // An occurrence the usage guard kept open past the next one is
        // replaced by it: a daily message held for days goes once when the
        // hold lifts, not once per day it was held, nor twice in a row.
        if let (Some(slot), Some(_)) = (s.slot, s.held_at) {
            if let Some(next) = s
                .latest(now)
                .filter(|&next| next > slot && s.until.is_none_or(|until| next <= until))
            {
                out.notes.push(format!(
                    "schedule {} ({label}): {} was held by the usage guard until {} came due, which replaces it",
                    s.id,
                    clock_label(slot, now),
                    clock_label(next, now)
                ));
                // `held_at` stays: the replacement may itself be older than
                // the wait, and still owes the targets the hold kept back.
                s.last_slot = Some(slot);
                s.slot = Some(next);
                s.delivered.clear();
            }
        }

        let Some(slot) = s.slot else {
            return true;
        };
        let candidates = candidates(s, agents, connected);
        let one_agent = matches!(s.target, ScheduleTarget::Session { .. });
        // 🚨 Held is not busy: a target the guard holds keeps the occurrence
        // open, or a hold longer than the wait (any of them) would drop the
        // message silently, with nothing to reopen it when the hold lifts.
        if candidates
            .iter()
            .any(|a| held.contains(&a.id) && !served(s, a) && folders.keep(s, a))
        {
            s.held_at = Some(now);
        }
        let since = s.held_at.map_or(slot, |at| at.max(slot));
        let closing = now - since > s.wait();
        if (one_agent && !s.delivered.is_empty()) || closing {
            if closing {
                let skipped: Vec<&str> = candidates
                    .iter()
                    .filter(|a| !served(s, a) && folders.keep(s, a))
                    .map(|a| a.id.as_str())
                    .collect();
                if !skipped.is_empty() {
                    out.notes.push(format!(
                        "schedule {} ({label}): still busy when the {}-minute wait ran out, skipped: {}",
                        s.id,
                        s.wait_minutes,
                        skipped.join(", ")
                    ));
                } else if s.delivered.is_empty() {
                    out.notes.push(format!(
                        "schedule {} ({label}): nobody to send to at {}",
                        s.id,
                        clock_label(slot, now)
                    ));
                }
            }
            s.last_slot = Some(slot);
            s.slot = None;
            s.delivered.clear();
            s.held_at = None;
            return !once;
        }

        for a in candidates {
            let keys = delivery_keys(a);
            if served(s, a)
                || !at_rest(a)
                || busy.contains(&a.id)
                || held.contains(&a.id)
                || keys.iter().any(|k| queued.contains(k))
                || !folders.keep(s, a)
            {
                continue;
            }
            queued.extend(keys.iter().cloned());
            s.delivered.extend(keys.iter().cloned());
            s.sent += 1;
            out.sends.push(ScheduleSend {
                schedule: s.id.clone(),
                keys,
                agent_id: a.id.clone(),
                backend: a.pane.backend.clone(),
                pane_id: a.pane.pane_id.clone(),
                message: s.message.clone(),
            });
        }
        true
    });
    out
}

/// Takes back a delivery [`plan`] recorded, when the message could not be
/// written after all.
pub fn unmark(schedules: &mut [Schedule], id: &str, keys: &[String]) {
    if let Some(s) = schedules.iter_mut().find(|s| s.id == id) {
        let before = s.delivered.len();
        s.delivered.retain(|k| !keys.contains(k));
        if s.delivered.len() < before {
            s.sent = s.sent.saturating_sub(1);
        }
    }
}

// ---------------------------------------------------------------- the command

/// What `add`, `close` and `wake` were asked for.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    when: When,
    target: Who,
    wait_minutes: Option<u32>,
    until: Option<OffsetDateTime>,
    message: String,
}

/// A target as typed, before a session ID is looked up.
#[derive(Debug, PartialEq, Eq)]
enum Who {
    Session(String),
    Workspace(String),
    Handoff,
}

fn parse_request(
    args: &[String],
    default_repeat: Option<Repeat>,
    preset: Option<(&str, Who)>,
    now: OffsetDateTime,
) -> Result<Request, String> {
    let mut time: Option<&str> = None;
    let mut repeat = default_repeat;
    let mut target: Option<Who> = None;
    let mut wait_minutes = None;
    let mut until = None;
    let mut words: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .map(String::as_str)
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--daily" => repeat = Some(Repeat::Daily),
            "--weekdays" => repeat = Some(Repeat::Weekdays),
            "--session" => target = Some(Who::Session(value("--session")?.to_string())),
            "--workspace" => target = Some(Who::Workspace(value("--workspace")?.to_string())),
            "--handoff" => target = Some(Who::Handoff),
            "--wait" => {
                let span = parse_span(value("--wait")?)?;
                wait_minutes =
                    Some(u32::try_from(span.whole_minutes()).map_err(|_| "--wait is too long")?);
            }
            "--until" => {
                let day = value("--until")?;
                let date = parse_date(day)
                    .ok_or_else(|| format!("--until {day:?}: a date, like 2026-10-31"))?;
                // The end of that day, locally.
                until = Some(
                    date.with_hms(23, 59, 59)
                        .expect("valid")
                        .assume_offset(now.offset()),
                );
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            word if time.is_none() => time = Some(word),
            word => words.push(word),
        }
    }
    let time = time.ok_or("give a time — 19:00, 2026-09-23T09:00 or +30m")?;
    let when = parse_when(time, repeat.unwrap_or(Repeat::Once), now)?;
    let (message, target) = match preset {
        Some((message, who)) => {
            if !words.is_empty() || target.is_some() {
                return Err("close and wake take a time and --weekdays, nothing else".into());
            }
            (message.to_string(), who)
        }
        None => (
            words.join(" "),
            target.ok_or("say who: --session <id>, --workspace <name> or --handoff")?,
        ),
    };
    if message.trim().is_empty() {
        return Err("give the message to send".into());
    }
    Ok(Request {
        when,
        target,
        wait_minutes,
        until,
        message,
    })
}

/// `herdr-tower schedule …`
///
/// `ensure_supervisor` starts the resident supervisor, which is what sends a
/// schedule: one it cannot start is taken back out rather than left to be
/// missed.
pub fn run_schedule(
    w: &mut dyn Write,
    schedules: &ScheduleStore,
    agents: &[Agent],
    args: &[String],
    now: OffsetDateTime,
    ensure_supervisor: &dyn Fn() -> std::io::Result<()>,
) -> Result<(), Failure> {
    let (sub, rest) = match args.split_first() {
        Some((sub, rest)) => (sub.as_str(), rest),
        None => ("list", &[][..]),
    };
    let request = match sub {
        "list" | "ls" => return list(w, schedules, now),
        "rm" | "remove" => return remove(w, schedules, rest),
        "add" => parse_request(rest, None, None, now)?,
        "close" => parse_request(
            rest,
            Some(Repeat::Daily),
            Some((tower_ui::CLOSE_MESSAGE, Who::Handoff)),
            now,
        )?,
        "wake" => parse_request(
            rest,
            Some(Repeat::Daily),
            Some((tower_ui::WAKE_MESSAGE, Who::Handoff)),
            now,
        )?,
        other => {
            return Err(format!(
                "unknown schedule command {other:?} — list, add, close, wake or rm"
            )
            .into())
        }
    };
    let target = match request.target {
        Who::Handoff => ScheduleTarget::Handoff,
        Who::Workspace(name) => ScheduleTarget::Workspace { name },
        Who::Session(key) => {
            let found = crate::infocmd::by_session(agents, &key);
            let conversations: HashSet<&str> = found.iter().map(|a| a.session_id.trim()).collect();
            match conversations.into_iter().collect::<Vec<_>>().as_slice() {
                [sid] => ScheduleTarget::Session {
                    id: sid.to_string(),
                    origin: String::new(),
                },
                [] => {
                    return Err(format!(
                        "no session {key:?} — the SID column in the tower shows them"
                    )
                    .into())
                }
                _ => {
                    return Err(
                        format!("{key:?} names more than one session — give the whole id").into(),
                    )
                }
            }
        }
    };
    let offset = now.offset();
    let created = schedules.add(|taken| {
        let mut s = Schedule::new(
            new_schedule_id(now, taken),
            request.message.clone(),
            target,
            request.when,
            now,
        );
        if let Some(wait) = request.wait_minutes {
            s.wait_minutes = wait;
        }
        s.until = request.until;
        s
    })?;
    if let Err(e) = ensure_supervisor() {
        let _ = schedules.remove_where(|s| s.id == created.id);
        return Err(format!(
            "could not start the resident supervisor, which sends scheduled messages — nothing was scheduled: {e}"
        )
        .into());
    }
    let next = created
        .next(now)
        .map_or_else(|| "never".into(), |at| clock_label(at, now));
    writeln!(
        w,
        "⏰ {} {} → {}: {:?} (next {next}; waits up to {}m for a busy agent)",
        created.id,
        created.when_label(offset),
        created.target,
        created.message,
        created.wait_minutes
    )?;
    Ok(())
}

fn list(w: &mut dyn Write, schedules: &ScheduleStore, now: OffsetDateTime) -> Result<(), Failure> {
    let all = schedules.list().map_err(|e| {
        format!(
            "the schedules cannot be read ({e}) — `herdr-tower schedule rm all` starts them afresh"
        )
    })?;
    if all.is_empty() {
        writeln!(w, "nothing scheduled — `herdr-tower schedule close 19:00` sends \"{}\" to every handoff session each evening", tower_ui::CLOSE_MESSAGE)?;
        return Ok(());
    }
    let offset = now.offset();
    writeln!(
        w,
        "{:<8} {:<17} {:<24} {:<12} {:>4}  MESSAGE",
        "ID", "WHEN", "TO", "NEXT", "SENT"
    )?;
    for s in &all {
        let next = if s.slot.is_some() {
            "sending".into()
        } else {
            s.next(now)
                .map_or_else(|| "—".into(), |at| clock_label(at, now))
        };
        writeln!(
            w,
            "{:<8} {:<17} {:<24} {:<12} {:>4}  {}",
            s.id,
            s.when_label(offset),
            s.target.to_string(),
            next,
            s.sent,
            s.message
        )?;
    }
    Ok(())
}

fn remove(w: &mut dyn Write, schedules: &ScheduleStore, ids: &[String]) -> Result<(), Failure> {
    if ids.is_empty() {
        return Err("rm takes schedule ids (see `herdr-tower schedule`), or all".into());
    }
    // Everything is checked before anything is removed: a call that fails
    // leaves the schedules as they were (#124).
    let gone = if ids.iter().any(|id| id == "all") {
        if ids.len() > 1 {
            return Err(
                "all removes every schedule and is given alone (rm all); nothing was removed"
                    .into(),
            );
        }
        // Whatever state the file is in: this is the way out of a bad one.
        match schedules.clear()? {
            Some(gone) => gone,
            None => {
                writeln!(w, "the schedules could not be read — emptied them")?;
                return Ok(());
            }
        }
    } else {
        // Under one lock: an id that is not there stops the whole removal.
        let outcome = schedules.update(|all| {
            let missing: Vec<String> = ids
                .iter()
                .filter(|id| !all.iter().any(|s| &s.id == *id))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Ok(Err(missing));
            }
            let (gone, kept): (Vec<_>, Vec<_>) = all.drain(..).partition(|s| ids.contains(&s.id));
            *all = kept;
            Ok(Ok(gone))
        })?;
        match outcome {
            Ok(gone) => gone,
            Err(missing) => {
                return Err(
                    format!("no schedule {}; nothing was removed", missing.join(", ")).into(),
                )
            }
        }
    };
    for s in &gone {
        writeln!(w, "removed {} ({:?})", s.id, s.message)?;
    }
    if gone.is_empty() {
        writeln!(w, "nothing was scheduled")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use time::{Duration, UtcOffset};
    use tower_state::ClockTime;

    /// September 2026 at `h:m`, KST. The 22nd is a Tuesday.
    fn kst(day: u8, h: u8, m: u8) -> OffsetDateTime {
        time::Date::from_calendar_date(2026, time::Month::September, day)
            .unwrap()
            .with_hms(h, m, 0)
            .unwrap()
            .assume_offset(UtcOffset::from_hms(9, 0, 0).unwrap())
    }

    fn agent(id: &str, sid: &str, state: AgentState, cwd: &str) -> Agent {
        let mut a = Agent::new(id, "claude", kst(1, 0, 0));
        a.session_id = sid.into();
        a.state = state;
        a.cwd = cwd.into();
        a.pane.pane_id = format!("p-{id}");
        a.pane.session = "work".into();
        a
    }

    fn daily_close(created: OffsetDateTime) -> Schedule {
        Schedule::new(
            "t000001".into(),
            tower_ui::CLOSE_MESSAGE.into(),
            ScheduleTarget::Handoff,
            When::Daily {
                at: ClockTime::parse("19:00").unwrap(),
            },
            created,
        )
    }

    fn connected() -> HashSet<String> {
        HashSet::from([String::new()])
    }

    fn handoff(cwd: &str) -> bool {
        cwd.starts_with("/handoff")
    }

    fn pass(schedules: &mut Vec<Schedule>, agents: &[Agent], now: OffsetDateTime) -> SchedulePlan {
        plan(
            schedules,
            agents,
            &connected(),
            &handoff,
            &HashSet::new(),
            &HashSet::new(),
            now,
        )
    }

    fn sent_to(plan: &SchedulePlan) -> Vec<&str> {
        plan.sends.iter().map(|s| s.agent_id.as_str()).collect()
    }

    /// At 19:00 the resting agents in handoff folders get it; one still working
    /// is waited for, and gets it once it rests; nobody gets it twice.
    #[test]
    fn a_due_message_goes_to_resting_agents_and_waits_for_busy_ones() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let mut agents = vec![
            agent("claude-1", "s1", AgentState::Idle, "/handoff/a"),
            agent("claude-2", "s2", AgentState::Working, "/handoff/b"),
            agent("claude-3", "s3", AgentState::Idle, "/elsewhere"),
        ];
        assert!(
            pass(&mut all, &agents, kst(22, 18, 59)).sends.is_empty(),
            "not yet"
        );

        let p = pass(&mut all, &agents, kst(22, 19, 0));
        assert_eq!(sent_to(&p), ["claude-1"]);

        assert!(
            pass(&mut all, &agents, kst(22, 19, 5)).sends.is_empty(),
            "claude-2 still busy"
        );
        agents[1].state = AgentState::DoneUnread;
        let p = pass(&mut all, &agents, kst(22, 19, 10));
        assert_eq!(sent_to(&p), ["claude-2"], "claude-1 already has it");
        assert_eq!(all[0].sent, 2);
    }

    /// A target busy for the whole window is skipped for that occurrence, and
    /// the log says so; the next day starts afresh.
    #[test]
    fn a_target_busy_past_the_wait_is_skipped_and_logged() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let busy = vec![agent("claude-2", "s2", AgentState::Working, "/handoff/b")];
        assert!(pass(&mut all, &busy, kst(22, 19, 0)).sends.is_empty());
        let p = pass(&mut all, &busy, kst(22, 19, 31));
        assert!(p.sends.is_empty());
        assert!(p.notes[0].contains("skipped: claude-2"), "{:?}", p.notes);
        assert_eq!(all[0].slot, None);

        let resting = vec![agent("claude-2", "s2", AgentState::Idle, "/handoff/b")];
        assert_eq!(
            sent_to(&pass(&mut all, &resting, kst(23, 19, 0))),
            ["claude-2"]
        );
    }

    /// One pass with `held` held back by the usage guard.
    fn held_pass(
        schedules: &mut Vec<Schedule>,
        agents: &[Agent],
        held: &[&str],
        now: OffsetDateTime,
    ) -> SchedulePlan {
        let held: HashSet<String> = held.iter().map(|id| id.to_string()).collect();
        plan(
            schedules,
            agents,
            &connected(),
            &handoff,
            &HashSet::new(),
            &held,
            now,
        )
    }

    /// A target the usage guard holds past the wait is deferred, not
    /// skipped: the occurrence stays open, and the message goes once the hold
    /// lifts (#155).
    #[test]
    fn a_held_target_keeps_the_occurrence_open_until_the_hold_lifts() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let agents = vec![agent("claude-1", "s1", AgentState::Idle, "/handoff/a")];
        let held = ["claude-1"];
        assert!(held_pass(&mut all, &agents, &held, kst(22, 19, 0))
            .sends
            .is_empty());
        // Hours past the 30-minute wait, still held: still open, nothing said.
        for (h, m) in [(19, 31), (20, 0), (23, 59)] {
            let p = held_pass(&mut all, &agents, &held, kst(22, h, m));
            assert!(p.sends.is_empty(), "{h}:{m}");
            assert!(p.notes.is_empty(), "{h}:{m} {:?}", p.notes);
            assert_eq!(all[0].slot, Some(kst(22, 19, 0)), "{h}:{m}");
        }
        // The hold lifts: the message goes, once.
        let p = held_pass(&mut all, &agents, &[], kst(23, 0, 5));
        assert_eq!(sent_to(&p), ["claude-1"]);
        assert!(held_pass(&mut all, &agents, &[], kst(23, 0, 6))
            .sends
            .is_empty());
        assert_eq!(all[0].sent, 1);
        // After the hold the wait runs out as usual, and the occurrence closes.
        held_pass(&mut all, &agents, &[], kst(23, 0, 36));
        assert_eq!(
            (all[0].slot, all[0].held_at, all[0].last_slot),
            (None, None, Some(kst(22, 19, 0)))
        );
    }

    /// A hold that lifts while its target is still working leaves the full
    /// wait for it to come to rest, counted from the last held pass.
    #[test]
    fn the_wait_starts_over_when_the_hold_lifts() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let mut agents = vec![agent("claude-1", "s1", AgentState::Working, "/handoff/a")];
        held_pass(&mut all, &agents, &["claude-1"], kst(22, 19, 0));
        held_pass(&mut all, &agents, &["claude-1"], kst(22, 21, 0));
        // Released at 21:01, still working at 21:29: inside the new wait.
        let p = held_pass(&mut all, &agents, &[], kst(22, 21, 1));
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        assert!(held_pass(&mut all, &agents, &[], kst(22, 21, 29))
            .notes
            .is_empty());
        agents[0].state = AgentState::Idle;
        assert_eq!(
            sent_to(&held_pass(&mut all, &agents, &[], kst(22, 21, 29))),
            ["claude-1"]
        );
    }

    /// A daily message held for days goes once when the hold lifts: each new
    /// occurrence replaces the one held open, so they do not pile up.
    #[test]
    fn a_held_occurrence_is_replaced_by_the_next_one() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let agents = vec![agent("claude-1", "s1", AgentState::Idle, "/handoff/a")];
        let held = ["claude-1"];
        held_pass(&mut all, &agents, &held, kst(22, 19, 0));
        let p = held_pass(&mut all, &agents, &held, kst(23, 19, 0));
        assert!(p.notes[0].contains("replaces it"), "{:?}", p.notes);
        assert_eq!(all[0].slot, Some(kst(23, 19, 0)));
        // Held all night; released the next morning, long past the
        // replacement's own wait.
        held_pass(&mut all, &agents, &held, kst(24, 2, 59));
        let p = held_pass(&mut all, &agents, &[], kst(24, 3, 0));
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        assert_eq!(sent_to(&p), ["claude-1"]);
        assert_eq!(all[0].slot, Some(kst(23, 19, 0)));
        let p = held_pass(&mut all, &agents, &[], kst(24, 3, 1));
        assert!(p.sends.is_empty(), "went on the pass before, not now");
        assert_eq!(all[0].sent, 1);
    }

    /// An occurrence the supervisor was not running for is noted, not made up
    /// for hours later.
    #[test]
    fn a_missed_occurrence_is_noted_not_sent_late() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let agents = vec![agent("claude-1", "s1", AgentState::Idle, "/handoff/a")];
        let p = pass(&mut all, &agents, kst(22, 21, 0));
        assert!(p.sends.is_empty());
        assert!(p.notes[0].contains("missed 19:00"), "{:?}", p.notes);
        assert!(
            pass(&mut all, &agents, kst(22, 21, 1)).notes.is_empty(),
            "said once"
        );
    }

    /// A schedule made after today's time starts tomorrow, without a word.
    #[test]
    fn a_schedule_made_after_todays_time_starts_tomorrow() {
        let mut all = vec![daily_close(kst(22, 20, 0))];
        let agents = vec![agent("claude-1", "s1", AgentState::Idle, "/handoff/a")];
        let p = pass(&mut all, &agents, kst(22, 20, 1));
        assert_eq!(p, SchedulePlan::default());
        assert_eq!(
            sent_to(&pass(&mut all, &agents, kst(23, 19, 0))),
            ["claude-1"]
        );
    }

    /// A one-off for one session is done once that session has it.
    #[test]
    fn a_one_off_for_a_session_ends_once_delivered() {
        let mut all = vec![Schedule::new(
            "t000002".into(),
            "테스트 돌려줘".into(),
            ScheduleTarget::Session {
                id: "s1".into(),
                origin: String::new(),
            },
            When::Once { at: kst(22, 15, 0) },
            kst(22, 14, 0),
        )];
        let agents = vec![
            agent("claude-1", "s1", AgentState::Idle, "/x"),
            agent("claude-2", "s2", AgentState::Idle, "/x"),
        ];
        assert_eq!(
            sent_to(&pass(&mut all, &agents, kst(22, 15, 0))),
            ["claude-1"]
        );
        pass(&mut all, &agents, kst(22, 15, 1));
        assert!(all.is_empty(), "the one-off is over");
    }

    /// An agent an idle loop just sent to is left for the next pass.
    #[test]
    fn an_agent_the_idle_loop_just_sent_to_waits_a_pass() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let agents = vec![agent("claude-1", "s1", AgentState::Idle, "/handoff/a")];
        let busy = HashSet::from(["claude-1".to_string()]);
        let p = plan(
            &mut all,
            &agents,
            &connected(),
            &handoff,
            &busy,
            &HashSet::new(),
            kst(22, 19, 0),
        );
        assert!(p.sends.is_empty());
        assert_eq!(
            sent_to(&pass(&mut all, &agents, kst(22, 19, 0))),
            ["claude-1"]
        );
    }

    #[test]
    fn a_repeating_schedule_ends_at_its_until() {
        let mut s = daily_close(kst(20, 12, 0));
        s.until = Some(kst(21, 23, 59));
        let mut all = vec![s];
        let p = pass(&mut all, &[], kst(22, 9, 0));
        assert!(p.notes[0].contains("ended"), "{:?}", p.notes);
        assert!(all.is_empty());
    }

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn close_and_wake_are_daily_handoff_messages() {
        let now = kst(22, 12, 0);
        let close = parse_request(
            &args("19:00"),
            Some(Repeat::Daily),
            Some((tower_ui::CLOSE_MESSAGE, Who::Handoff)),
            now,
        )
        .unwrap();
        assert_eq!(
            close.when,
            When::Daily {
                at: ClockTime::parse("19:00").unwrap()
            }
        );
        assert_eq!(close.target, Who::Handoff);
        assert_eq!(close.message, tower_ui::CLOSE_MESSAGE);

        let wake = parse_request(
            &args("09:00 --weekdays"),
            Some(Repeat::Daily),
            Some((tower_ui::WAKE_MESSAGE, Who::Handoff)),
            now,
        )
        .unwrap();
        assert_eq!(
            wake.when,
            When::Weekdays {
                at: ClockTime::parse("09:00").unwrap()
            }
        );
    }

    #[test]
    fn add_reads_a_time_a_target_and_the_message() {
        let now = kst(22, 12, 0);
        let r = parse_request(
            &args("+30m --session c376295e 테스트 돌려줘"),
            None,
            None,
            now,
        )
        .unwrap();
        assert_eq!(
            r.when,
            When::Once {
                at: now + Duration::minutes(30)
            }
        );
        assert_eq!(r.target, Who::Session("c376295e".into()));
        assert_eq!(r.message, "테스트 돌려줘");

        // A time of day that has passed means tomorrow.
        let r = parse_request(&args("09:00 --handoff hi"), None, None, now).unwrap();
        assert_eq!(r.when, When::Once { at: kst(23, 9, 0) });

        let r = parse_request(
            &args("2026-09-23T08:30 --workspace work --wait 10m --until 2026-10-01 hi"),
            None,
            None,
            now,
        )
        .unwrap();
        assert_eq!(r.when, When::Once { at: kst(23, 8, 30) });
        assert_eq!(r.wait_minutes, Some(10));
        assert_eq!(r.target, Who::Workspace("work".into()));
    }

    #[test]
    fn add_says_what_is_missing() {
        let now = kst(22, 12, 0);
        for (line, want) in [
            ("--handoff", "give a time"),
            ("--handoff hi", "is not a time"),
            ("19:00 hi", "say who"),
            ("19:00 --handoff", "give the message"),
            ("+30m --daily --handoff hi", "one-off"),
            ("2026-09-01T09:00 --handoff hi", "already passed"),
            ("25:00 --handoff hi", "not a time"),
            ("19:00 --handoff --bogus hi", "unknown flag"),
        ] {
            let err = parse_request(&args(line), None, None, now).unwrap_err();
            assert!(err.contains(want), "{line:?}: {err}");
        }
    }

    #[test]
    fn the_command_adds_lists_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        let agents = vec![agent(
            "claude-1",
            "c376295e-95bf-45d2-9cd0-e26b0e837218",
            AgentState::Idle,
            "/x",
        )];
        let now = kst(22, 12, 0);
        let run = |line: &str| {
            let mut out = Vec::new();
            run_schedule(&mut out, &store, &agents, &args(line), now, &|| Ok(()))
                .map(|()| String::from_utf8(out).unwrap())
        };
        assert!(run("").unwrap().contains("nothing scheduled"));
        assert!(run("close 19:00 --weekdays")
            .unwrap()
            .contains("weekdays 19:00"));
        let added = run("add +2h --session c376295e 테스트").unwrap();
        assert!(added.contains("session c376295e"), "{added}");
        assert_eq!(
            store.list().unwrap()[1].target,
            ScheduleTarget::Session {
                id: "c376295e-95bf-45d2-9cd0-e26b0e837218".into(),
                origin: String::new(),
            },
            "the whole id is kept"
        );
        let listed = run("list").unwrap();
        assert!(
            listed.contains(tower_ui::CLOSE_MESSAGE) && listed.contains("14:00"),
            "{listed}"
        );
        assert!(run("add +2h --session nosuch hi").is_err());
        let id = store.list().unwrap()[0].id.clone();
        assert!(run(&format!("rm {id}")).unwrap().contains("removed"));
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(run("rm all").unwrap().contains("removed"));
        assert!(store.list().unwrap().is_empty());
    }
    /// A send that fails is taken back and tried again while the wait lasts;
    /// one that went is not repeated.
    #[test]
    fn a_failed_send_is_taken_back_and_tried_again() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let agents = vec![agent("claude-1", "s1", AgentState::Idle, "/handoff/a")];
        let p = pass(&mut all, &agents, kst(22, 19, 0));
        assert_eq!(all[0].sent, 1, "recorded before it is sent");
        unmark(&mut all, &p.sends[0].schedule, &p.sends[0].keys);
        assert_eq!((all[0].sent, all[0].delivered.len()), (0, 0));
        assert_eq!(
            sent_to(&pass(&mut all, &agents, kst(22, 19, 1))),
            ["claude-1"]
        );
        assert!(pass(&mut all, &agents, kst(22, 19, 2)).sends.is_empty());
    }

    /// `/clear` inside the window gives the pane a new conversation, and a
    /// fresh pane reports its first one: neither is the message's second
    /// delivery.
    #[test]
    fn a_new_conversation_in_the_same_pane_is_not_sent_to_again() {
        let mut all = vec![daily_close(kst(22, 12, 0))];
        let mut agents = vec![
            agent("claude-1", "s1", AgentState::Idle, "/handoff/a"),
            agent("claude-2", "", AgentState::Idle, "/handoff/b"),
        ];
        assert_eq!(
            sent_to(&pass(&mut all, &agents, kst(22, 19, 0))),
            ["claude-1", "claude-2"]
        );
        agents[0].session_id = "s1-after-clear".into();
        agents[1].session_id = "s2-reported".into();
        assert!(pass(&mut all, &agents, kst(22, 19, 5)).sends.is_empty());
    }

    /// Two schedules due for one agent, or two records for one pane, make one
    /// message a pass; the other schedule's comes on a later one.
    #[test]
    fn one_pane_gets_one_message_a_pass() {
        let mut wake = daily_close(kst(22, 12, 0));
        wake.id = "t000002".into();
        wake.message = tower_ui::WAKE_MESSAGE.into();
        let mut all = vec![daily_close(kst(22, 12, 0)), wake];
        let mut twin = agent("claude-1b", "s1b", AgentState::Idle, "/handoff/a");
        twin.pane.pane_id = "p-claude-1".into();
        let agents = vec![
            agent("claude-1", "s1", AgentState::Idle, "/handoff/a"),
            twin,
        ];

        let p = pass(&mut all, &agents, kst(22, 19, 0));
        assert_eq!(sent_to(&p), ["claude-1"]);
        assert_eq!(p.sends[0].schedule, "t000001");
        let p = pass(&mut all, &agents, kst(22, 19, 1));
        assert_eq!(sent_to(&p), ["claude-1"]);
        assert_eq!(p.sends[0].schedule, "t000002");
        assert!(pass(&mut all, &agents, kst(22, 19, 2)).sends.is_empty());
    }

    /// The handoff test reads files: it is asked of agents otherwise ready
    /// for the message, once per folder a pass — not of every live agent
    /// every two seconds for the whole window.
    #[test]
    fn the_handoff_test_is_asked_last_and_once_per_folder() {
        let asked = Cell::new(0);
        let counting = |cwd: &str| {
            asked.set(asked.get() + 1);
            handoff(cwd)
        };
        let mut other = daily_close(kst(22, 12, 0));
        other.id = "t000002".into();
        let mut all = vec![daily_close(kst(22, 12, 0)), other];
        let agents = vec![
            agent("claude-1", "s1", AgentState::Working, "/handoff/a"),
            agent("claude-2", "s2", AgentState::Idle, "/handoff/b"),
            agent("claude-3", "s3", AgentState::Idle, "/handoff/b"),
        ];
        let p = plan(
            &mut all,
            &agents,
            &connected(),
            &counting,
            &HashSet::new(),
            &HashSet::new(),
            kst(22, 19, 0),
        );
        // The second schedule waits: both panes have a message this pass.
        assert_eq!(sent_to(&p), ["claude-2", "claude-3"]);
        assert_eq!(asked.get(), 1, "one folder ready, asked once");
        asked.set(0);
        let p = plan(
            &mut all,
            &agents,
            &connected(),
            &counting,
            &HashSet::new(),
            &HashSet::new(),
            kst(22, 19, 5),
        );
        assert_eq!(sent_to(&p), ["claude-2", "claude-3"]);
        assert!(p.sends.iter().all(|s| s.schedule == "t000002"));
        assert_eq!(asked.get(), 1, "a new pass asks afresh, still once");
        asked.set(0);
        plan(
            &mut all,
            &agents,
            &connected(),
            &counting,
            &HashSet::new(),
            &HashSet::new(),
            kst(22, 19, 6),
        );
        assert_eq!(asked.get(), 0, "the working one is not asked about");
    }

    #[test]
    fn a_schedule_the_supervisor_cannot_send_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        let mut out = Vec::new();
        let err = run_schedule(
            &mut out,
            &store,
            &[],
            &args("close 19:00"),
            kst(22, 12, 0),
            &|| Err(std::io::Error::other("launchctl said no")),
        )
        .unwrap_err();
        assert!(err.to_string().contains("launchctl said no"), "{err}");
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn rm_all_empties_a_file_nothing_else_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        std::fs::write(dir.path().join("schedules.json"), "[{").unwrap();
        let run = |line: &str| {
            let mut out = Vec::new();
            run_schedule(&mut out, &store, &[], &args(line), kst(22, 12, 0), &|| {
                Ok(())
            })
            .map(|()| String::from_utf8(out).unwrap())
        };
        assert!(run("list").unwrap_err().to_string().contains("rm all"));
        assert!(run("rm all").unwrap().contains("emptied"));
        assert!(run("list").unwrap().contains("nothing scheduled"));
    }

    // ── the four strategies: delivery over a window, the command ────────────

    /// A deterministic stream, so a failing case replays.
    struct Seq(u64);

    impl Seq {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn some_state(seq: &mut Seq) -> AgentState {
        [
            AgentState::Idle,
            AgentState::DoneUnread,
            AgentState::Working,
            AgentState::Working,
            AgentState::Waiting,
            AgentState::Error,
        ][seq.below(6) as usize]
    }

    /// Cross, against the rule written out the slow way: every agent in a
    /// handoff folder gets the 19:00 message at the first pass inside the
    /// half hour that finds it at rest, and nobody else ever does — whatever
    /// the agents do between passes, `/clear` included.
    #[test]
    fn delivery_over_a_window_follows_the_rule() {
        let mut seq = Seq(0xd311_7e27);
        for round in 0..200 {
            let mut all = vec![daily_close(kst(22, 12, 0))];
            let n = 2 + seq.below(5) as usize;
            let mut agents: Vec<Agent> = (0..n)
                .map(|i| {
                    let cwd = if seq.below(4) == 0 {
                        "/elsewhere"
                    } else {
                        "/handoff/x"
                    };
                    agent(
                        &format!("claude-{i}"),
                        &format!("s{i}"),
                        AgentState::Idle,
                        cwd,
                    )
                })
                .collect();
            let mut expected: HashMap<String, OffsetDateTime> = HashMap::new();
            let mut got: HashMap<String, OffsetDateTime> = HashMap::new();
            let mut now = kst(22, 18, 55);
            while now < kst(22, 19, 45) {
                for a in agents.iter_mut() {
                    a.state = some_state(&mut seq);
                    if seq.below(10) == 0 {
                        a.session_id = format!("{}-cleared-{}", a.id, seq.below(1000));
                    }
                }
                let open = now >= kst(22, 19, 0) && now - kst(22, 19, 0) <= Duration::minutes(30);
                for a in &agents {
                    if open && handoff(&a.cwd) && at_rest(a) {
                        expected.entry(a.id.clone()).or_insert(now);
                    }
                }
                for send in pass(&mut all, &agents, now).sends {
                    assert!(
                        got.insert(send.agent_id.clone(), now).is_none(),
                        "round {round}: {} sent to twice",
                        send.agent_id
                    );
                }
                now += Duration::seconds(30 + seq.below(150) as i64);
            }
            assert_eq!(got, expected, "round {round}");
            assert_eq!(all[0].sent as usize, expected.len());
            assert_eq!(
                (all[0].slot, all[0].last_slot),
                (None, Some(kst(22, 19, 0)))
            );
        }
    }

    /// Property: however many schedules are due and however the records look
    /// — twins on one pane, busy agents, failures taken back — a pass sends
    /// only to agents at rest and not busy, one message a pane, and never
    /// twice to anyone for one occurrence.
    #[test]
    fn a_pass_never_doubles_up() {
        let mut seq = Seq(0x0e_9a55_u64 ^ 0x5eed);
        for round in 0..300 {
            let mut all: Vec<Schedule> = (0..1 + seq.below(3))
                .map(|i| {
                    let mut s = daily_close(kst(22, 12, 0));
                    s.id = format!("t00000{i}");
                    if seq.below(2) == 0 {
                        s.target = ScheduleTarget::Workspace {
                            name: "work".into(),
                        };
                    }
                    s
                })
                .collect();
            let agents: Vec<Agent> = (0..1 + seq.below(5))
                .map(|i| {
                    let mut a = agent(
                        &format!("claude-{i}"),
                        &format!("s{}", seq.below(3)),
                        some_state(&mut seq),
                        "/handoff/x",
                    );
                    a.pane.pane_id = format!("p{}", seq.below(3));
                    a
                })
                .collect();
            let busy: HashSet<String> = agents
                .iter()
                .filter(|_| seq.below(4) == 0)
                .map(|a| a.id.clone())
                .collect();
            let mut served: HashMap<String, HashSet<Vec<String>>> = HashMap::new();
            for minute in 0..6 {
                let p = plan(
                    &mut all,
                    &agents,
                    &connected(),
                    &handoff,
                    &busy,
                    &HashSet::new(),
                    kst(22, 19, minute),
                );
                let mut panes = HashSet::new();
                for send in &p.sends {
                    let a = agents.iter().find(|a| a.id == send.agent_id).unwrap();
                    assert!(
                        at_rest(a) && !busy.contains(&a.id),
                        "round {round}: {send:?}"
                    );
                    assert!(
                        panes.insert(send.pane_id.clone()),
                        "round {round}: pane twice"
                    );
                    let mine = served.entry(send.schedule.clone()).or_default();
                    assert!(
                        mine.iter()
                            .all(|keys| keys.iter().all(|k| !send.keys.contains(k))),
                        "round {round}: {send:?} again"
                    );
                    mine.insert(send.keys.clone());
                }
                // Some sends fail and are taken back: they may go again.
                for send in p.sends.iter().filter(|_| seq.below(5) == 0) {
                    unmark(&mut all, &send.schedule, &send.keys);
                    served.get_mut(&send.schedule).unwrap().remove(&send.keys);
                }
            }
        }
    }

    /// Round trip: a schedule added from the command line is on disk as it
    /// was typed and lists back in the same words, and `rm` takes it away.
    #[test]
    fn what_is_added_lists_back_as_typed() {
        let mut seq = Seq(0xadd_1157);
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        let now = kst(22, 12, 0);
        let run = |line: &[String]| {
            let mut out = Vec::new();
            run_schedule(&mut out, &store, &[], line, now, &|| Ok(()))
                .map(|()| String::from_utf8(out).unwrap())
        };
        for _ in 0..60 {
            let at = ClockTime {
                hour: seq.below(24) as u8,
                minute: seq.below(60) as u8,
            };
            let (flag, label) =
                [("--daily", "daily"), ("--weekdays", "weekdays")][seq.below(2) as usize];
            let message =
                ["세션 마감하자", "run the \"slow\" tests", "🗼 둘 셋"][seq.below(3) as usize];
            let mut line: Vec<String> = vec!["add".into(), at.to_string(), flag.into()];
            line.push("--handoff".into());
            line.extend(message.split(' ').map(String::from));
            let said = run(&line).unwrap();
            assert!(said.contains(&format!("{label} {at}")), "{said}");

            let kept = store.list().unwrap();
            let s = kept.last().unwrap();
            assert_eq!(s.message, message);
            assert_eq!(s.target, ScheduleTarget::Handoff);
            assert_eq!(
                s.when,
                if label == "daily" {
                    When::Daily { at }
                } else {
                    When::Weekdays { at }
                }
            );
            let listed = run(&["list".into()]).unwrap();
            let row = listed
                .lines()
                .find(|l| l.starts_with(&s.id))
                .unwrap_or_else(|| panic!("{} not in\n{listed}", s.id));
            assert!(
                row.contains(&format!("{label} {at}")) && row.ends_with(message),
                "{row}"
            );
            assert!(run(&["rm".into(), s.id.clone()])
                .unwrap()
                .contains("removed"));
            assert!(store.list().unwrap().is_empty());
        }
    }

    /// Edge: the command's own boundaries.
    #[test]
    fn the_command_at_its_edges() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        let now = kst(22, 12, 0);
        let run = |line: &str| {
            let mut out = Vec::new();
            run_schedule(&mut out, &store, &[], &args(line), now, &|| Ok(()))
                .map(|()| String::from_utf8(out).unwrap())
        };
        assert!(run("rm").unwrap_err().to_string().contains("ids"));
        assert!(run("rm t000000")
            .unwrap_err()
            .to_string()
            .contains("no schedule t000000"));
        assert!(run("rm all").unwrap().contains("nothing was scheduled"));
        assert!(run("bogus")
            .unwrap_err()
            .to_string()
            .contains("unknown schedule command"));
        assert!(
            run("close 19:00 --handoff").is_err(),
            "a preset takes no target"
        );
        assert!(run("close 19:00 hello").is_err(), "nor a message");
        assert!(run("add 19:00 --wait 90분 --handoff hi").is_ok());
        assert_eq!(store.list().unwrap()[0].wait_minutes, 90);
        assert!(run("add 19:00 --wait 2년 --handoff hi").is_err());
        assert!(run("add 19:00 --until 2026-02-30 --handoff hi").is_err());
        assert!(run("add 19:00 --session")
            .unwrap_err()
            .to_string()
            .contains("needs a value"));
        // A key two sessions share names neither.
        let twins = vec![
            agent(
                "claude-1",
                "c3762aaa-0000-4000-8000-000000000001",
                AgentState::Idle,
                "/x",
            ),
            agent(
                "claude-2",
                "c3762bbb-0000-4000-8000-000000000002",
                AgentState::Idle,
                "/x",
            ),
        ];
        let mut out = Vec::new();
        let err = run_schedule(
            &mut out,
            &store,
            &twins,
            &args("add +1h --session c3762 hi"),
            now,
            &|| Ok(()),
        );
        let err = err.unwrap_err().to_string();
        assert!(
            err.contains("more than one") || err.contains("no session"),
            "{err}"
        );
        assert_eq!(store.list().unwrap().len(), 1, "nothing added for it");
    }

    /// #124: a removal that fails leaves every schedule where it was — a
    /// stray id beside `all`, or among real ones, removes nothing.
    #[test]
    fn a_failed_removal_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        let now = kst(22, 12, 0);
        let run = |line: &str| {
            let mut out = Vec::new();
            run_schedule(&mut out, &store, &[], &args(line), now, &|| Ok(()))
                .map(|()| String::from_utf8(out).unwrap())
        };
        run("add +1h --handoff one").unwrap();
        run("add +2h --handoff two").unwrap();
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), 2);
        let body = || std::fs::read(dir.path().join("schedules.json")).ok();
        let before = body();

        for line in [
            "rm t999999 all".to_string(),
            "rm all t999999".to_string(),
            format!("rm all {}", ids[0]),
            format!("rm {} t999999", ids[0]),
            format!("rm t999999 {} {}", ids[0], ids[1]),
        ] {
            let e = run(&line).unwrap_err().to_string();
            assert!(e.contains("nothing was removed"), "{line}: {e}");
            assert_eq!(store.list().unwrap().len(), 2, "{line}");
            assert_eq!(body(), before, "{line}: the file is untouched");
        }

        // The same ids, all real, go together.
        let out = run(&format!("rm {} {}", ids[0], ids[1])).unwrap();
        assert_eq!(out.matches("removed").count(), 2, "{out}");
        assert!(store.list().unwrap().is_empty());
    }
}
