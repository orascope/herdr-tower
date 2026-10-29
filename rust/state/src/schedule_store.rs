// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Messages sent at a set time: once at a moment, or every day (or every
//! weekday) at a time of day.
//!
//! Durable beside the agent records, like the idle-loop leases, so closing the
//! tower does not cancel them: the resident supervisor delivers them. A
//! schedule names *who* by something that outlives a pane — a session ID, a
//! workspace, or every folder that keeps a session handoff — and is resolved
//! to panes when it is due.
//!
//! Delivery waits for the agent to be at rest. An occurrence opens a window
//! (thirty minutes unless said otherwise); each target gets the message once,
//! the first time it is seen idle inside that window, and a target still busy
//! when the window closes is skipped for that occurrence — unless the usage
//! guard is holding it, which keeps the window open until the hold lifts and
//! for a full wait after (`Schedule::held_at`). The supervisor keeps
//! the bookkeeping here — the occurrence being delivered, who already has it —
//! so a restart in the middle of a window does not repeat a message.
//!
//! Delivery is at most once: a target is recorded as served just before the
//! message is written into its pane, and taken off again if the write fails.
//! A crash between the two loses that one delivery rather than sending it
//! twice.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, Time, UtcOffset, Weekday};

use crate::write_atomic;

const FILE: &str = "schedules.json";
const LOCK: &str = "schedules.lock";

/// How long an occurrence waits for its targets to come to rest.
pub const DEFAULT_SCHEDULE_WAIT_MINUTES: u32 = 30;

/// The longest span `+2h` or `--wait` takes: a year.
const MAX_SPAN_MINUTES: i64 = 366 * 24 * 60;

/// A time of day, `HH:MM`, in the machine's local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClockTime {
    pub hour: u8,
    pub minute: u8,
}

impl ClockTime {
    /// `HH:MM` or `H:MM`, 00:00 to 23:59.
    pub fn parse(s: &str) -> Option<ClockTime> {
        let (h, m) = s.trim().split_once(':')?;
        if m.len() != 2 || h.is_empty() || h.len() > 2 || !all_digits(h) || !all_digits(m) {
            return None;
        }
        let (hour, minute) = (h.parse::<u8>().ok()?, m.parse::<u8>().ok()?);
        (hour < 24 && minute < 60).then_some(ClockTime { hour, minute })
    }

    fn time(self) -> Time {
        Time::from_hms(self.hour, self.minute, 0).expect("validated when parsed")
    }
}

impl std::fmt::Display for ClockTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl TryFrom<String> for ClockTime {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        ClockTime::parse(&s).ok_or_else(|| format!("not a time of day: {s:?}"))
    }
}

impl From<ClockTime> for String {
    fn from(t: ClockTime) -> String {
        t.to_string()
    }
}

/// When a schedule fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum When {
    /// Once, at this moment.
    Once {
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Every day at this local time.
    Daily { at: ClockTime },
    /// Monday to Friday at this local time.
    Weekdays { at: ClockTime },
}

/// Who a schedule is for, resolved to panes when it is due.
///
/// There is no target by record: a record is named after a pane coordinate,
/// and herdr hands a closed pane's coordinate to the next process — which
/// would inherit the schedule. A pane is scheduled for by its conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleTarget {
    /// The agent in this conversation — whole session ID.
    Session {
        id: String,
        /// The record the tower made it from, so the row still shows it and
        /// `T` can still cancel it after the pane has moved on to another
        /// conversation (`/clear`). Delivery goes by `id` alone.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        origin: String,
    },
    /// Every agent in this herdr workspace.
    Workspace { name: String },
    /// Every agent in a folder that keeps a session handoff — the ones
    /// `wake all` and `close all` talk to.
    Handoff,
}

impl std::fmt::Display for ScheduleTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // As the SID column shows it, so the two can be matched by eye.
            ScheduleTarget::Session { id, .. } => {
                write!(f, "session {}", crate::short_session_id(id))
            }
            ScheduleTarget::Workspace { name } => write!(f, "workspace {name}"),
            ScheduleTarget::Handoff => f.write_str("every handoff session"),
        }
    }
}

/// One scheduled message and where its delivery stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub message: String,
    pub target: ScheduleTarget,
    pub when: When,
    /// How long an occurrence waits for a busy target to come to rest.
    #[serde(default = "default_wait")]
    pub wait_minutes: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// A repeating schedule stops after this moment.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub until: Option<OffsetDateTime>,
    /// The occurrence being delivered now, if its window is open.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub slot: Option<OffsetDateTime>,
    /// Who already has this occurrence's message: `s:<session id>` and
    /// `a:<record id>`, both, so a pane whose conversation changes inside the
    /// window (`/clear`, or a first session ID reported) is not sent to again.
    #[serde(default)]
    pub delivered: Vec<String>,
    /// The last time, inside the open occurrence, that a target it still owes
    /// was held by the usage guard. The wait runs from here rather than from
    /// the occurrence: a hold lasts until a window resets — hours, or days —
    /// and a target held back that long is deferred, not skipped. Kept here
    /// so a restart during a hold does not close the occurrence.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub held_at: Option<OffsetDateTime>,
    /// The last occurrence that was delivered or let go.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_slot: Option<OffsetDateTime>,
    /// Messages sent over the schedule's life.
    #[serde(default)]
    pub sent: u32,
}

fn default_wait() -> u32 {
    DEFAULT_SCHEDULE_WAIT_MINUTES
}

impl Schedule {
    pub fn new(
        id: String,
        message: String,
        target: ScheduleTarget,
        when: When,
        now: OffsetDateTime,
    ) -> Schedule {
        Schedule {
            id,
            message,
            target,
            when,
            wait_minutes: DEFAULT_SCHEDULE_WAIT_MINUTES,
            created_at: now,
            until: None,
            slot: None,
            delivered: Vec::new(),
            held_at: None,
            last_slot: None,
            sent: 0,
        }
    }

    /// How long an occurrence stays open for busy targets.
    pub fn wait(&self) -> Duration {
        Duration::minutes(i64::from(self.wait_minutes))
    }

    /// The latest occurrence at or before `now`.
    ///
    /// `now` carries the local offset, which is what a time of day is read
    /// in. Across a daylight-saving change the day before is placed with
    /// today's offset, an hour out for that one occurrence at most.
    pub fn latest(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        match self.when {
            When::Once { at } => (at <= now).then_some(at),
            When::Daily { at } => {
                let today = now.replace_time(at.time());
                Some(if today <= now {
                    today
                } else {
                    today - Duration::days(1)
                })
            }
            When::Weekdays { at } => {
                let mut day = now.replace_time(at.time());
                if day > now {
                    day -= Duration::days(1);
                }
                while is_weekend(day.weekday()) {
                    day -= Duration::days(1);
                }
                Some(day)
            }
        }
    }

    /// The next occurrence after `now`, if there is one.
    pub fn next(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        let next = match self.when {
            When::Once { at } => (at > now).then_some(at),
            When::Daily { at } => {
                let today = now.replace_time(at.time());
                Some(if today > now {
                    today
                } else {
                    today + Duration::days(1)
                })
            }
            When::Weekdays { at } => {
                let mut day = now.replace_time(at.time());
                if day <= now {
                    day += Duration::days(1);
                }
                while is_weekend(day.weekday()) {
                    day += Duration::days(1);
                }
                Some(day)
            }
        }?;
        match self.until {
            Some(until) if next > until => None,
            _ => Some(next),
        }
    }

    /// `once 09-23 09:00`, `daily 19:00`, `weekdays 09:00`, in `offset`.
    pub fn when_label(&self, offset: UtcOffset) -> String {
        match self.when {
            When::Once { at } => format!("once {}", dated(at.to_offset(offset))),
            When::Daily { at } => format!("daily {at}"),
            When::Weekdays { at } => format!("weekdays {at}"),
        }
    }

    /// Whether this schedule is for one agent by name — its session, or the
    /// record it was made from — rather than by workspace or folder.
    pub fn names(&self, session_id: &str, agent_id: &str) -> bool {
        match &self.target {
            ScheduleTarget::Session { id, origin } => {
                (!session_id.is_empty() && id == session_id)
                    || (!origin.is_empty() && origin == agent_id)
            }
            _ => false,
        }
    }
}

/// `19:00` when `at` falls on `now`'s day, `09-23 19:00` on another — read in
/// `now`'s offset, which is the local one.
pub fn clock_label(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let at = at.to_offset(now.offset());
    if at.date() == now.date() {
        format!("{:02}:{:02}", at.hour(), at.minute())
    } else {
        dated(at)
    }
}

/// `09-23 19:00`.
fn dated(at: OffsetDateTime) -> String {
    format!(
        "{:02}-{:02} {:02}:{:02}",
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

fn is_weekend(day: Weekday) -> bool {
    matches!(day, Weekday::Saturday | Weekday::Sunday)
}

/// How a schedule repeats, as asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Once,
    Daily,
    Weekdays,
}

impl Repeat {
    /// In the order `tab` goes round them in the tower.
    pub const ALL: [Repeat; 3] = [Repeat::Once, Repeat::Daily, Repeat::Weekdays];

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Once => "once",
            Repeat::Daily => "daily",
            Repeat::Weekdays => "weekdays",
        }
    }
}

/// A time as a person types it, read against `now` (local): `19:00` — the
/// next one, for a one-off; the time of day, for a daily or weekday schedule —
/// `2026-09-23T09:00`, or `+30m` / `+2h` for a one-off.
pub fn parse_when(s: &str, repeat: Repeat, now: OffsetDateTime) -> Result<When, String> {
    if let Some(rest) = s.strip_prefix('+') {
        if repeat != Repeat::Once {
            return Err("+30m and +2h are for one-off messages; give a time of day for --daily or --weekdays".into());
        }
        let at = now
            .checked_add(parse_span(rest)?)
            .ok_or_else(|| format!("{s:?} is too far away"))?;
        return Ok(When::Once { at });
    }
    if let Some(at) = ClockTime::parse(s) {
        return Ok(match repeat {
            Repeat::Daily => When::Daily { at },
            Repeat::Weekdays => When::Weekdays { at },
            Repeat::Once => {
                let one = Schedule::new(
                    String::new(),
                    String::new(),
                    ScheduleTarget::Handoff,
                    When::Daily { at },
                    now,
                );
                When::Once {
                    at: one.next(now).expect("a daily time always comes round"),
                }
            }
        });
    }
    if repeat != Repeat::Once {
        return Err(format!(
            "{s:?}: --daily and --weekdays take a time of day, like 19:00"
        ));
    }
    let (date, clock) = s
        .split_once('T')
        .ok_or_else(|| format!("{s:?} is not a time — 19:00, 2026-09-23T09:00 or +30m"))?;
    let date = parse_date(date)
        .ok_or_else(|| format!("{s:?} is not a date and time — 2026-09-23T09:00"))?;
    let at = ClockTime::parse(clock).ok_or_else(|| format!("{s:?}: the time is not HH:MM"))?;
    let at = date
        .with_hms(at.hour, at.minute, 0)
        .expect("validated")
        .assume_offset(now.offset());
    if at <= now {
        return Err(format!("{s:?} has already passed"));
    }
    Ok(When::Once { at })
}

/// Digits and nothing else: `parse` also takes a sign, which in `+026` or
/// `9:+5` is not what anyone typing a date or a time meant.
fn all_digits(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit())
}

/// `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Option<time::Date> {
    let mut parts = s.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    if ![y, m, d].into_iter().all(all_digits) {
        return None;
    }
    let month = time::Month::try_from(m.parse::<u8>().ok()?).ok()?;
    time::Date::from_calendar_date(y.parse().ok()?, month, d.parse().ok()?).ok()
}

/// `30m`, `2h`, `90m` — or `30분`, `2시간` — up to a year.
///
/// Not the duration reader the flags use: that one takes `1h30m`, seconds
/// and fractions, none of which a whole-minute schedule has a use for, and
/// no Korean units.
pub fn parse_span(s: &str) -> Result<Duration, String> {
    let bad = || format!("{s:?} is not a span of time — 30m or 2h");
    // A char boundary: where the digits end is where the unit begins.
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(digits);
    let n: i64 = n.parse().map_err(|_| bad())?;
    let minutes = match unit {
        "m" | "min" | "분" => Some(n),
        "h" | "시간" => n.checked_mul(60),
        _ => return Err(bad()),
    };
    match minutes {
        Some(m) if m <= 0 => Err(bad()),
        Some(m) if m <= MAX_SPAN_MINUTES => Ok(Duration::minutes(m)),
        _ => Err(format!("{s:?} is longer than a year")),
    }
}

/// A short, fresh ID: `t` and six hex digits.
pub fn new_schedule_id(now: OffsetDateTime, taken: &[Schedule]) -> String {
    let mut seed = now.unix_timestamp_nanos() as u64;
    loop {
        seed = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0x2545_F491);
        let id = format!("t{:06x}", (seed >> 40) & 0xff_ffff);
        if !taken.iter().any(|s| s.id == id) {
            return id;
        }
    }
}

/// The machine's local offset from UTC, now.
///
/// The `time` crate declines to read it in a process that has threads, on
/// Unix, and the tower has several; `date +%z` answers the same question from
/// outside. UTC is the last resort, and it is said on the screen as `+00:00`
/// times rather than guessed at.
pub fn local_offset() -> UtcOffset {
    if let Ok(offset) = UtcOffset::current_local_offset() {
        return offset;
    }
    std::process::Command::new(date_program(Path::new(SYSTEM_DATE)))
        .arg("+%z")
        .output()
        .ok()
        .and_then(|out| parse_offset(String::from_utf8_lossy(&out.stdout).trim()))
        .unwrap_or(UtcOffset::UTC)
}

/// Where `date` is on every Unix this runs on.
const SYSTEM_DATE: &str = "/bin/date";

/// The `date` to run: `system` by its absolute path when it exists, a `PATH`
/// lookup only when it does not.
///
/// Absolute first because the supervisor runs this with whatever `PATH` the
/// shell that started it had, and a `date` earlier on it — a project's `bin/`,
/// a directory another account can write — would run as this user every time
/// the offset is asked for. The lookup stays as the fallback for a system that
/// keeps `date` somewhere else, where the old behaviour is the only one there
/// is.
fn date_program(system: &Path) -> &std::ffi::OsStr {
    if system.exists() {
        system.as_os_str()
    } else {
        std::ffi::OsStr::new("date")
    }
}

/// `+0900` / `-0430` to an offset.
fn parse_offset(s: &str) -> Option<UtcOffset> {
    // Sliced by bytes below, so ASCII only: anything else is not an offset,
    // and would split a character.
    if s.len() != 5 || !s.is_ascii() || !all_digits(&s[1..]) {
        return None;
    }
    let sign = match &s[..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let hours: i8 = s[1..3].parse().ok()?;
    let minutes: i8 = s[3..5].parse().ok()?;
    UtcOffset::from_hms(sign * hours, sign * minutes, 0).ok()
}

/// The schedules, beside the agent records.
#[derive(Debug, Clone)]
pub struct ScheduleStore {
    dir: PathBuf,
}

impl ScheduleStore {
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        crate::make_state_root(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    pub fn list(&self) -> std::io::Result<Vec<Schedule>> {
        match fs::read(self.path()) {
            Ok(body) => serde_json::from_slice(&body)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Reads, changes and writes the schedules under one lock, so the
    /// supervisor's delivery bookkeeping and a person adding or cancelling one
    /// cannot overwrite each other.
    ///
    /// Written only when `f` changed something: the supervisor comes through
    /// here every two seconds, and a daily schedule is idle for all but half
    /// an hour of the day.
    pub fn update<T>(
        &self,
        f: impl FnOnce(&mut Vec<Schedule>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let _lock = ScheduleLock::acquire(&self.dir)?;
        let mut all = self.list()?;
        let before = all.clone();
        let value = f(&mut all)?;
        if all != before {
            self.write(&all)?;
        }
        Ok(value)
    }

    fn write(&self, all: &[Schedule]) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(all)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.path(), &body)
    }

    /// Adds the schedule `build` makes — it is handed the ones there are, to
    /// pick a fresh ID against — and answers with it.
    pub fn add(&self, build: impl FnOnce(&[Schedule]) -> Schedule) -> std::io::Result<Schedule> {
        self.update(|all| {
            let schedule = build(all);
            all.push(schedule.clone());
            Ok(schedule)
        })
    }

    /// Removes every schedule. A file that cannot be read — cut short, edited
    /// by hand, written by a newer build — is emptied all the same, which is
    /// how it gets fixed: answers `None` then, for there is nothing to name.
    pub fn clear(&self) -> std::io::Result<Option<Vec<Schedule>>> {
        let _lock = ScheduleLock::acquire(&self.dir)?;
        let gone = match self.list() {
            Ok(all) => Some(all),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => None,
            Err(e) => return Err(e),
        };
        self.write(&[])?;
        Ok(gone)
    }

    /// Removes the schedules `remove` picks, and answers with them.
    pub fn remove_where(
        &self,
        remove: impl Fn(&Schedule) -> bool,
    ) -> std::io::Result<Vec<Schedule>> {
        self.update(|all| {
            let (gone, kept): (Vec<_>, Vec<_>) = all.drain(..).partition(|s| remove(s));
            *all = kept;
            Ok(gone)
        })
    }
}

struct ScheduleLock(#[allow(dead_code)] fs::File);

impl ScheduleLock {
    fn acquire(dir: &Path) -> std::io::Result<Self> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK))?;
        file.lock()?;
        Ok(Self(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// September 2026 at `h:m`, KST. The 18th is a Friday, the 21st a Monday.
    fn kst(day: u8, h: u8, m: u8) -> OffsetDateTime {
        time::Date::from_calendar_date(2026, time::Month::September, day)
            .unwrap()
            .with_hms(h, m, 0)
            .unwrap()
            .assume_offset(UtcOffset::from_hms(9, 0, 0).unwrap())
    }

    fn at(s: &str) -> ClockTime {
        ClockTime::parse(s).unwrap()
    }

    /// `date` is run by its absolute path when there is one, and looked up on
    /// `PATH` only when there is not.
    #[test]
    fn date_is_run_by_absolute_path_when_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let present = tmp.path().join("date");
        fs::write(&present, "").unwrap();
        assert_eq!(date_program(&present), present.as_os_str());
        let missing = tmp.path().join("missing");
        assert_eq!(date_program(&missing), "date");
    }

    fn schedule(when: When) -> Schedule {
        Schedule::new(
            "t000001".into(),
            "세션 마감하자".into(),
            ScheduleTarget::Handoff,
            when,
            kst(1, 0, 0),
        )
    }

    #[test]
    fn a_time_of_day_is_hours_and_minutes() {
        assert_eq!(
            ClockTime::parse("19:00"),
            Some(ClockTime {
                hour: 19,
                minute: 0
            })
        );
        assert_eq!(
            ClockTime::parse("9:05"),
            Some(ClockTime { hour: 9, minute: 5 })
        );
        for bad in ["24:00", "7:5", "19:60", "ab", "", "19", "123:00"] {
            assert_eq!(ClockTime::parse(bad), None, "{bad:?}");
        }
        assert_eq!(at("9:05").to_string(), "09:05");
    }

    #[test]
    fn a_daily_schedule_fires_at_its_time_every_day() {
        let s = schedule(When::Daily { at: at("19:00") });
        assert_eq!(s.latest(kst(22, 18, 0)), Some(kst(21, 19, 0)));
        assert_eq!(s.next(kst(22, 18, 0)), Some(kst(22, 19, 0)));
        assert_eq!(
            s.latest(kst(22, 19, 0)),
            Some(kst(22, 19, 0)),
            "the minute itself"
        );
        assert_eq!(s.next(kst(22, 20, 0)), Some(kst(23, 19, 0)));
    }

    /// Friday evening's next weekday nine o'clock is Monday's; Monday morning's
    /// last one was Friday's.
    #[test]
    fn a_weekday_schedule_skips_the_weekend() {
        let s = schedule(When::Weekdays { at: at("09:00") });
        assert_eq!(s.next(kst(18, 20, 0)), Some(kst(21, 9, 0)));
        assert_eq!(s.next(kst(19, 12, 0)), Some(kst(21, 9, 0)), "from Saturday");
        assert_eq!(s.latest(kst(21, 8, 0)), Some(kst(18, 9, 0)));
        assert_eq!(s.latest(kst(22, 10, 0)), Some(kst(22, 9, 0)));
    }

    #[test]
    fn a_one_off_schedule_fires_once() {
        let s = schedule(When::Once { at: kst(22, 19, 0) });
        assert_eq!(s.latest(kst(22, 18, 0)), None);
        assert_eq!(s.next(kst(22, 18, 0)), Some(kst(22, 19, 0)));
        assert_eq!(s.latest(kst(22, 20, 0)), Some(kst(22, 19, 0)));
        assert_eq!(s.next(kst(22, 20, 0)), None);
    }

    #[test]
    fn a_schedule_has_no_next_occurrence_past_its_end() {
        let mut s = schedule(When::Daily { at: at("19:00") });
        s.until = Some(kst(22, 23, 0));
        assert_eq!(s.next(kst(22, 18, 0)), Some(kst(22, 19, 0)));
        assert_eq!(s.next(kst(22, 20, 0)), None);
    }

    #[test]
    fn the_label_reads_in_local_time() {
        let kst9 = UtcOffset::from_hms(9, 0, 0).unwrap();
        assert_eq!(
            schedule(When::Daily { at: at("19:00") }).when_label(kst9),
            "daily 19:00"
        );
        assert_eq!(
            schedule(When::Once { at: kst(23, 9, 30) }).when_label(kst9),
            "once 09-23 09:30"
        );
    }

    #[test]
    fn a_local_offset_is_read_from_date() {
        assert_eq!(parse_offset("+0900"), UtcOffset::from_hms(9, 0, 0).ok());
        assert_eq!(parse_offset("-0430"), UtcOffset::from_hms(-4, -30, 0).ok());
        assert_eq!(parse_offset("0900"), None);
    }

    #[test]
    fn schedules_are_kept_on_disk_and_can_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        assert!(store.list().unwrap().is_empty());
        let fresh = |when| {
            move |taken: &[Schedule]| {
                let mut s = schedule(when);
                s.id = new_schedule_id(kst(22, 0, 0), taken);
                s
            }
        };
        let close = store
            .add(fresh(When::Weekdays { at: at("19:00") }))
            .unwrap();
        let wake = store.add(fresh(When::Daily { at: at("09:00") })).unwrap();
        assert_ne!(
            close.id, wake.id,
            "the second ID is picked against the first"
        );
        assert_eq!(store.list().unwrap(), vec![close.clone(), wake.clone()]);

        let body = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(body.contains(r#""at": "19:00""#), "{body}");

        let gone = store.remove_where(|s| s.id == close.id).unwrap();
        assert_eq!(gone, vec![close]);
        assert_eq!(store.list().unwrap(), vec![wake.clone()]);

        assert_eq!(store.clear().unwrap(), Some(vec![wake]));
        assert!(store.list().unwrap().is_empty());
    }

    /// Nothing changed, nothing written: the supervisor passes through
    /// `update` every two seconds.
    #[test]
    fn an_update_that_changes_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        store
            .add(|_| schedule(When::Daily { at: at("19:00") }))
            .unwrap();
        let path = dir.path().join(FILE);
        // A mark no write of ours would leave: if it survives, nothing wrote.
        let body = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{body}\n\n")).unwrap();
        store.update(|_| Ok(())).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("\n\n"));

        store
            .update(|all| {
                all[0].sent = 1;
                Ok(())
            })
            .unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().ends_with("\n\n"));
    }

    /// A file nobody can read is still emptied by `clear` — the way out of
    /// it — while everything else refuses to guess at it.
    #[test]
    fn an_unreadable_file_can_still_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(FILE),
            r#"[{"id": "t1", "target": {"kind": "fleet"}}]"#,
        )
        .unwrap();
        assert!(store.list().is_err());
        assert!(store.update(|_| Ok(())).is_err());
        assert_eq!(store.clear().unwrap(), None);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_span_is_minutes_or_hours_in_either_language() {
        for (s, minutes) in [
            ("30m", 30),
            ("90min", 90),
            ("2h", 120),
            ("30분", 30),
            ("2시간", 120),
        ] {
            assert_eq!(parse_span(s), Ok(Duration::minutes(minutes)), "{s:?}");
        }
        // The last character is more than one byte in the Korean units; a
        // split by bytes panicked there.
        for bad in [
            "", "m", "30", "0m", "-5m", "3.5h", "30 m", "분", "30초", "1h30m",
        ] {
            assert!(parse_span(bad).is_err(), "{bad:?}");
        }
        for huge in ["9999999999999999h", "99999999h", "527041m"] {
            assert!(parse_span(huge).is_err(), "{huge:?}");
        }
        assert!(parse_span("527040m").is_ok(), "a year is the most");
    }

    #[test]
    fn a_one_off_too_far_away_is_refused_not_a_crash() {
        let now = kst(22, 12, 0);
        assert!(parse_when("+99999999h", Repeat::Once, now).is_err());
        assert!(parse_when("+30분", Repeat::Once, now).is_ok());
    }

    /// The row keeps a schedule made from it after `/clear` gives the pane a
    /// new conversation — to show it and to cancel it — though it is not
    /// delivered there.
    #[test]
    fn a_session_schedule_is_named_by_its_session_or_the_row_it_came_from() {
        let mut s = schedule(When::Daily { at: at("19:00") });
        s.target = ScheduleTarget::Session {
            id: "sid-a".into(),
            origin: "claude-1".into(),
        };
        assert!(s.names("sid-a", "claude-9"));
        assert!(s.names("sid-b", "claude-1"));
        assert!(!s.names("sid-b", "claude-9"));
        assert!(!s.names("", "claude-9"), "no session is no match");
        s.target = ScheduleTarget::Handoff;
        assert!(!s.names("sid-a", "claude-1"));
    }

    #[test]
    fn a_clock_reads_the_date_only_on_another_day() {
        assert_eq!(clock_label(kst(22, 19, 0), kst(22, 12, 0)), "19:00");
        assert_eq!(clock_label(kst(23, 9, 5), kst(22, 12, 0)), "09-23 09:05");
    }

    // ── the four strategies: calendar, parsing, storage ─────────────────────

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

    /// Somewhere in 2026–2028 — a leap day and two year ends among them — to
    /// the second, in one of a few offsets, a half-hour one included.
    fn some_now(seq: &mut Seq) -> OffsetDateTime {
        let offsets = [(9, 0), (0, 0), (-4, -30), (5, 45)];
        let (h, m) = offsets[seq.below(offsets.len() as u64) as usize];
        time::Date::from_calendar_date(2026, time::Month::January, 1)
            .unwrap()
            .midnight()
            .assume_offset(UtcOffset::from_hms(h, m, 0).unwrap())
            + Duration::seconds(seq.below(3 * 366 * 24 * 3600) as i64)
    }

    fn some_clock(seq: &mut Seq) -> ClockTime {
        ClockTime {
            hour: seq.below(24) as u8,
            minute: seq.below(60) as u8,
        }
    }

    fn some_repeating(seq: &mut Seq) -> When {
        let at = some_clock(seq);
        if seq.below(2) == 0 {
            When::Daily { at }
        } else {
            When::Weekdays { at }
        }
    }

    /// Whether the minute starting at `t` is an occurrence — the definition,
    /// read in `t`'s own offset.
    fn fires_at(when: When, t: OffsetDateTime) -> bool {
        let on = |at: ClockTime| t.hour() == at.hour && t.minute() == at.minute && t.second() == 0;
        match when {
            When::Daily { at } => on(at),
            When::Weekdays { at } => on(at) && !is_weekend(t.weekday()),
            When::Once { at } => t == at,
        }
    }

    fn floor_minute(t: OffsetDateTime) -> OffsetDateTime {
        t.replace_second(0).unwrap().replace_nanosecond(0).unwrap()
    }

    /// Minute by minute backwards from `now`: the slow, obvious answer.
    fn brute_latest(when: When, now: OffsetDateTime) -> OffsetDateTime {
        let mut t = floor_minute(now);
        while !fires_at(when, t) {
            t -= Duration::minutes(1);
        }
        t
    }

    fn brute_next(when: When, now: OffsetDateTime) -> OffsetDateTime {
        let mut t = floor_minute(now) + Duration::minutes(1);
        while !fires_at(when, t) {
            t += Duration::minutes(1);
        }
        t
    }

    /// Cross: `latest` and `next` agree with a minute-by-minute walk, across
    /// weekends, month and year ends, the leap day and odd offsets.
    #[test]
    fn occurrences_agree_with_a_minute_by_minute_walk() {
        let mut seq = Seq(0x005e_ed0f_7133);
        for _ in 0..150 {
            let now = some_now(&mut seq);
            let s = schedule(some_repeating(&mut seq));
            assert_eq!(
                s.latest(now),
                Some(brute_latest(s.when, now)),
                "{:?} latest at {now}",
                s.when
            );
            assert_eq!(
                s.next(now),
                Some(brute_next(s.when, now)),
                "{:?} next at {now}",
                s.when
            );
        }
        // The minute itself, to the nanosecond: it has come, and is not next.
        let s = schedule(When::Daily { at: at("19:00") });
        let now = kst(22, 19, 0);
        assert_eq!(s.latest(now), Some(brute_latest(s.when, now)));
        assert_eq!(s.next(now), Some(brute_next(s.when, now)));
    }

    /// Property: the latest occurrence is at or before now and the next one
    /// after it, a day apart (three over a weekend), on the clock time, never
    /// on a weekend for a weekday schedule; and each is the other's neighbour.
    #[test]
    fn latest_and_next_bracket_now() {
        let mut seq = Seq(0xb4ac_e7ed);
        for _ in 0..2000 {
            let now = some_now(&mut seq);
            let s = schedule(some_repeating(&mut seq));
            let (latest, next) = (s.latest(now).unwrap(), s.next(now).unwrap());
            assert!(latest <= now && now < next, "{:?} at {now}", s.when);
            let at = match s.when {
                When::Daily { at } | When::Weekdays { at } => at,
                When::Once { .. } => unreachable!(),
            };
            for t in [latest, next] {
                assert_eq!((t.hour(), t.minute(), t.second()), (at.hour, at.minute, 0));
                assert_eq!(t.offset(), now.offset(), "read in now's offset");
                if matches!(s.when, When::Weekdays { .. }) {
                    assert!(!is_weekend(t.weekday()), "{t} for {:?}", s.when);
                }
            }
            let gap = next - latest;
            match s.when {
                When::Daily { .. } => assert_eq!(gap, Duration::days(1)),
                _ => assert!(
                    gap == Duration::days(1) || gap == Duration::days(3),
                    "{gap} at {now}"
                ),
            }
            assert_eq!(
                s.latest(next),
                Some(next),
                "an occurrence is its own latest"
            );
            assert_eq!(s.next(latest), Some(next), "nothing between them");
        }
    }

    /// Property: a one-off is latest from its moment on and next before it,
    /// and `until` cuts off exactly the occurrences after it.
    #[test]
    fn a_one_off_and_an_end_cut_where_they_say() {
        let mut seq = Seq(0x0ff_0ff);
        for _ in 0..500 {
            let (a, b) = (some_now(&mut seq), some_now(&mut seq));
            let s = schedule(When::Once { at: a });
            assert_eq!(s.latest(b), (a <= b).then_some(a));
            assert_eq!(s.next(b), (a > b).then_some(a));

            let mut s = schedule(some_repeating(&mut seq));
            let open = s.next(b).unwrap();
            s.until = Some(a);
            assert_eq!(s.next(b), (open <= a).then_some(open), "until {a}, at {b}");
        }
    }

    /// Round trip: every time of day prints as `HH:MM` and reads back, bare
    /// and as JSON; a single-digit hour is the same time.
    #[test]
    fn every_time_of_day_reads_back() {
        for hour in 0..24 {
            for minute in 0..60 {
                let t = ClockTime { hour, minute };
                assert_eq!(ClockTime::parse(&t.to_string()), Some(t));
                let json = serde_json::to_string(&t).unwrap();
                assert_eq!(json, format!("\"{t}\""));
                assert_eq!(serde_json::from_str::<ClockTime>(&json).unwrap(), t);
                if hour < 10 {
                    assert_eq!(ClockTime::parse(&format!("{hour}:{minute:02}")), Some(t));
                }
            }
        }
    }

    fn some_schedule(seq: &mut Seq) -> Schedule {
        let now = some_now(seq);
        let maybe = |seq: &mut Seq| (seq.below(2) == 0).then(|| some_now(seq));
        let when = match seq.below(3) {
            0 => When::Once {
                at: some_now(seq) + Duration::nanoseconds(seq.below(1_000_000_000) as i64),
            },
            _ => some_repeating(seq),
        };
        let target = match seq.below(4) {
            0 => ScheduleTarget::Session {
                id: format!("{:08x}-95bf-45d2-9cd0-e26b0e837218", seq.next() as u32),
                origin: String::new(),
            },
            1 => ScheduleTarget::Session {
                id: format!("0199{:04x}-aaaa-7bbb-8ccc-dddddddddddd", seq.next() as u16),
                origin: format!("claude-b{:x}-p{}", seq.below(99), seq.below(9)),
            },
            2 => ScheduleTarget::Workspace {
                name: ["dbstore", "한글이름", "a \"quoted\" one", ""][seq.below(4) as usize].into(),
            },
            _ => ScheduleTarget::Handoff,
        };
        let messages = [
            "세션 마감하자",
            "line one\nline two",
            "a \"quote\" \\ back",
            "🗼",
            "",
        ];
        let mut s = Schedule::new(
            new_schedule_id(now, &[]),
            messages[seq.below(messages.len() as u64) as usize].into(),
            target,
            when,
            now,
        );
        s.wait_minutes = seq.below(24 * 60) as u32;
        s.until = maybe(seq);
        s.slot = maybe(seq);
        s.last_slot = maybe(seq);
        s.held_at = maybe(seq);
        s.delivered = (0..seq.below(4))
            .map(|i| ["a:claude-1", "p:b/p1", "s:sid"][i as usize % 3].to_string())
            .collect();
        s.sent = seq.below(1000) as u32;
        s
    }

    /// Round trip: whatever a schedule holds is what comes back from the
    /// file, through the store as through serde.
    #[test]
    fn a_schedule_reads_back_as_it_was_written() {
        let mut seq = Seq(0x70_7219);
        let all: Vec<Schedule> = (0..200).map(|_| some_schedule(&mut seq)).collect();
        for s in &all {
            let json = serde_json::to_string(s).unwrap();
            let back: Schedule = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, s, "{json}");
            if let ScheduleTarget::Session { origin, .. } = &s.target {
                assert_eq!(json.contains("origin"), !origin.is_empty(), "{json}");
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path()).unwrap();
        store
            .update(|kept| {
                kept.extend(all.iter().cloned());
                Ok(())
            })
            .unwrap();
        assert_eq!(store.list().unwrap(), all);
    }

    /// Edge: a file written before the delivery bookkeeping existed — or by
    /// hand, with only what matters — reads with the defaults.
    #[test]
    fn a_bare_schedule_reads_with_defaults() {
        let body = r#"[{"id": "t1", "message": "hi",
            "target": {"kind": "session", "id": "sid"},
            "when": {"kind": "daily", "at": "9:05"},
            "created_at": "2026-09-22T12:00:00+09:00"}]"#;
        let all: Vec<Schedule> = serde_json::from_str(body).unwrap();
        let s = &all[0];
        assert_eq!(s.wait_minutes, DEFAULT_SCHEDULE_WAIT_MINUTES);
        assert_eq!(
            (s.until, s.slot, s.last_slot, s.held_at, s.sent),
            (None, None, None, None, 0)
        );
        assert!(s.delivered.is_empty());
        assert_eq!(
            s.target,
            ScheduleTarget::Session {
                id: "sid".into(),
                origin: String::new()
            }
        );
        assert_eq!(s.when, When::Daily { at: at("09:05") });
        for bad in [
            r#"{"kind": "daily", "at": "24:00"}"#,
            r#"{"kind": "hourly", "at": "09:00"}"#,
            r#"{"kind": "once", "at": "tomorrow"}"#,
        ] {
            assert!(serde_json::from_str::<When>(bad).is_err(), "{bad}");
        }
    }

    /// Edge: what a person types for a time, at the boundaries.
    #[test]
    fn a_typed_time_at_its_edges() {
        let now = kst(22, 19, 0);
        // The minute it is now has come already: the next one is tomorrow's.
        assert_eq!(
            parse_when("19:00", Repeat::Once, now),
            Ok(When::Once { at: kst(23, 19, 0) })
        );
        assert_eq!(
            parse_when("19:01", Repeat::Once, now),
            Ok(When::Once { at: kst(22, 19, 1) })
        );
        assert!(parse_when("2026-09-22T19:00", Repeat::Once, now)
            .unwrap_err()
            .contains("already passed"));
        assert_eq!(
            parse_when("2028-02-29T09:00", Repeat::Once, now),
            Ok(When::Once {
                at: time::Date::from_calendar_date(2028, time::Month::February, 29)
                    .unwrap()
                    .with_hms(9, 0, 0)
                    .unwrap()
                    .assume_offset(now.offset())
            })
        );
        for (bad, why) in [
            ("2026-02-29T09:00", "not a date"),
            ("2026-09-23T24:00", "not HH:MM"),
            ("2026-09-23", "not a time"),
            ("+30m", "one-off"),
        ] {
            let repeat = if bad == "+30m" {
                Repeat::Daily
            } else {
                Repeat::Once
            };
            let err = parse_when(bad, repeat, now).unwrap_err();
            assert!(err.contains(why), "{bad:?}: {err}");
        }
        assert!(parse_when("2026-09-23T09:00", Repeat::Weekdays, now)
            .unwrap_err()
            .contains("time of day"));
        assert_eq!(
            parse_when("09:00", Repeat::Weekdays, now),
            Ok(When::Weekdays { at: at("09:00") })
        );
    }

    /// Edge: digits only. A sign is a number to `parse`, not to a person
    /// typing a date or a time of day.
    #[test]
    fn dates_and_times_take_digits_only() {
        assert_eq!(
            parse_date("2026-09-23"),
            time::Date::from_calendar_date(2026, time::Month::September, 23).ok()
        );
        for bad in [
            "2026-9-23",
            "26-09-23",
            "2026-09-23-01",
            "2026-13-01",
            "2026-00-10",
            "2026-09-31",
            "+026-09-23",
            "2026-+9-23",
            "",
        ] {
            assert_eq!(parse_date(bad), None, "{bad:?}");
        }
        for bad in ["+9:05", "9:+5", "-1:00", "9:0 ", " :05"] {
            assert_eq!(ClockTime::parse(bad), None, "{bad:?}");
        }
    }

    /// Edge: `date +%z` output, at its extremes, and junk never panics.
    #[test]
    fn an_offset_reads_at_its_edges_and_junk_is_refused() {
        assert_eq!(parse_offset("+0000"), Some(UtcOffset::UTC));
        assert_eq!(parse_offset("-0000"), Some(UtcOffset::UTC));
        assert_eq!(parse_offset("+1400"), UtcOffset::from_hms(14, 0, 0).ok());
        assert_eq!(parse_offset("+0545"), UtcOffset::from_hms(5, 45, 0).ok());
        for bad in [
            "", "+09", "+09:00", "09000", "+0960x", "+0é0", "+é00", "×0900",
        ] {
            assert_eq!(parse_offset(bad), None, "{bad:?}");
        }
    }

    /// Property: a fresh ID is `t` and six hex digits, never one taken — even
    /// when the first pick is.
    #[test]
    fn a_fresh_id_is_never_one_already_taken() {
        let mut seq = Seq(0x1d_1d);
        for _ in 0..200 {
            let now = some_now(&mut seq);
            let mut taken = Vec::new();
            for _ in 0..8 {
                let id = new_schedule_id(now, &taken);
                assert!(
                    id.len() == 7
                        && id.starts_with('t')
                        && id[1..].chars().all(|c| c.is_ascii_hexdigit()),
                    "{id}"
                );
                assert!(!taken.iter().any(|s: &Schedule| s.id == id), "{id} reused");
                let mut s = schedule(When::Daily { at: at("19:00") });
                s.id = id;
                taken.push(s);
            }
        }
    }
}
