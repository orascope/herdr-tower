// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The usage guard: once only so much of claude's or codex's five-hour or
//! seven-day window is left (`usage_guard_5h`, `usage_guard_7d`, in 1% steps,
//! 25% unless set), the tower stops spending it for them.
//!
//! What "stops" means is deliberately narrow. The resident supervisor holds
//! what *it* sends to that vendor's agents — idle-loop continuations and
//! scheduled messages — until the window resets, and asks each agent that is
//! working at that moment, once, to finish the step it is on, write down where
//! it stopped, and wait. Nothing is interrupted: a turn in flight runs to its
//! end, and a person typing into a pane is never stopped.
//!
//! Everything here is pure: the board and the settings in, the holds out.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset};
use tower_state::{Agent, AgentState};

use crate::usagecmd::Board;

/// One AI's floors, as shares that have to be left. `None` is off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Floors {
    pub five_hour: Option<u8>,
    pub seven_day: Option<u8>,
}

impl Floors {
    /// The floor for `"5h"` or `"7d"`.
    pub fn get(&self, window: &str) -> Option<u8> {
        if window == "5h" {
            self.five_hour
        } else {
            self.seven_day
        }
    }
}

/// Each AI's floors, set apart: claude's and codex's windows are different
/// plans, and one may be worth keeping in reserve when the other is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Guard {
    pub claude: Floors,
    pub codex: Floors,
}

impl Guard {
    pub fn from_config(c: &tower_config::Config) -> Self {
        let floors = |v: &str| Floors {
            five_hour: c.usage_guard_for(v, "5h"),
            seven_day: c.usage_guard_for(v, "7d"),
        };
        Guard {
            claude: floors("claude"),
            codex: floors("codex"),
        }
    }

    /// The same floors for every AI.
    pub fn uniform(five_hour: Option<u8>, seven_day: Option<u8>) -> Self {
        let f = Floors {
            five_hour,
            seven_day,
        };
        Guard {
            claude: f,
            codex: f,
        }
    }

    /// An AI's floors; none for an AI the guard does not cover.
    pub fn floors(&self, vendor: &str) -> Floors {
        match vendor {
            "claude" => self.claude,
            "codex" => self.codex,
            _ => Floors::default(),
        }
    }

    pub fn is_off(&self) -> bool {
        [self.claude, self.codex]
            .iter()
            .all(|f| f.five_hour.is_none() && f.seven_day.is_none())
    }
}

/// One vendor held back, and until when.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub vendor: &'static str,
    /// What tripped it: "7d 10% left ≤ 25%".
    pub why: String,
    /// The latest reset among the windows that tripped it; `None` when the
    /// vendor did not say.
    pub until: Option<OffsetDateTime>,
}

/// The vendor whose limits an agent of this kind spends. Only the vendors
/// the board reads limits for; any other kind is never held.
pub fn vendor_of(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

/// The vendors to hold back now.
///
/// A window trips its guard when no more than its floor is left and it has
/// not reset yet. A window whose reset has passed says nothing about now, so
/// an old reading never holds anything past the time it was good for. Only
/// enabled floors hold sends: a vendor's reached flag is informational,
/// since paid credits can still allow work after a plan window is exhausted.
pub fn holds(board: &Board, guard: &Guard, now: OffsetDateTime) -> Vec<Hold> {
    if guard.is_off() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for v in &board.vendors {
        let Some(vendor) = vendor_of(v.name) else {
            continue;
        };
        let mut why = Vec::new();
        let mut until: Option<OffsetDateTime> = None;
        let current = |at: Option<OffsetDateTime>| at.is_none_or(|t| t > now);
        let floors = guard.floors(vendor);
        for (label, limit, window) in [
            ("5h", floors.five_hour, &v.limits.five_hour),
            ("7d", floors.seven_day, &v.limits.seven_day),
        ] {
            let (Some(limit), Some(w)) = (limit, window) else {
                continue;
            };
            let left = 100.0 - w.used_pct;
            if left <= f64::from(limit) && current(w.resets_at) {
                why.push(format!("{label} {left:.0}% left ≤ {limit}%"));
                if let Some(t) = w.resets_at {
                    until = Some(until.map_or(t, |u| u.max(t)));
                }
            }
        }
        if !why.is_empty() {
            out.push(Hold {
                vendor,
                why: why.join(", "),
                until,
            });
        }
    }
    out
}

/// "09-26 Sat 17:35", in local time.
pub fn until_label(until: Option<OffsetDateTime>, offset: UtcOffset) -> String {
    match until {
        Some(t) => crate::usagecmd::dated(t, offset),
        None => "the reset".into(),
    }
}

/// What a working agent is asked, once, when its vendor is held. In
/// English, whatever language the person uses: it is an instruction to an
/// agent, and English is the one every agent CLI reads the same way.
pub fn wrap_up_message(hold: &Hold, offset: UtcOffset) -> String {
    format!(
        "[herdr-tower usage guard] {}'s usage has reached the floor you set ({}). \
         Finish the step you are on, write down where you stopped (in SESSION.md if this folder keeps one), \
         then stop and wait. Do not start new work before the reset at {}.",
        hold.vendor,
        hold.why,
        until_label(hold.until, offset)
    )
}

/// The ids of agents whose vendor is held.
pub fn held_agents(holds: &[Hold], agents: &[Agent]) -> HashSet<String> {
    agents
        .iter()
        .filter(|a| vendor_of(&a.kind).is_some_and(|v| holds.iter().any(|h| h.vendor == v)))
        .map(|a| a.id.clone())
        .collect()
}

/// What changed on a pass.
#[derive(Debug, Clone)]
pub enum Event {
    Held(Hold),
    /// A hold ended: `why` says whether the window reset or the floor moved.
    Released {
        vendor: &'static str,
        why: String,
    },
    /// Ask this working agent, once, to wrap up.
    WrapUp {
        agent: Box<Agent>,
        message: String,
    },
    /// Tell an agent that was asked to wait that it may go on.
    Resume {
        agent: Box<Agent>,
        message: String,
    },
    /// An AI's floors changed, and stayed changed for [`SETTLE`]: for the
    /// person, never for the agents.
    FloorsChanged {
        vendor: &'static str,
        change: String,
    },
}

/// How long the floors must stay put before a change counts. A drag on a
/// slider changes them several times a second; a hold must not begin and
/// end with each step, or agents would be told to stop and go on over and
/// over.
pub const SETTLE: std::time::Duration = std::time::Duration::from_secs(4);

/// "5h 25% → 20% · 7d off → 30%": what moved between two sets of floors.
pub fn change_text(from: Floors, to: Floors) -> Option<String> {
    let show = |f: Option<u8>| f.map_or("off".to_string(), |f| format!("{f}%"));
    let parts: Vec<String> = [
        ("5h", from.five_hour, to.five_hour),
        ("7d", from.seven_day, to.seven_day),
    ]
    .into_iter()
    .filter(|(_, a, b)| a != b)
    .map(|(w, a, b)| format!("{w} {} → {}", show(a), show(b)))
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What an agent that was asked to wait is told when the hold on its AI ends.
pub fn resume_message(vendor: &str, why: &str) -> String {
    format!(
        "[herdr-tower usage guard] The hold on {vendor} is lifted ({why}). \
         You may continue: pick up where you stopped (see SESSION.md if this folder keeps one)."
    )
}

/// One hold as it began: until when, and the floors it began under.
#[derive(Debug, Clone)]
struct Active {
    until: Option<OffsetDateTime>,
    floors: Floors,
}

/// The guard's memory between passes: which AIs are held, who was asked to
/// wait, and the floors as last settled.
#[derive(Debug, Default)]
pub struct State {
    active: HashMap<&'static str, Active>,
    /// Who was asked to wrap up, per AI, and under which reset — so a hold
    /// whose reset flickers back to an earlier reading does not ask again.
    /// 🚨 Dropped with the hold, like `active` and `waiting`: the keys carry
    /// each episode's reset, which is new every time, so a set kept past the
    /// release would grow for the whole life of the supervisor.
    asked: HashMap<&'static str, HashSet<(String, Option<OffsetDateTime>)>>,
    /// The agents told to wait, per AI, to be told when it is over. Each
    /// once: a hold that moves to a later reset asks again, but the agent
    /// still waits for one release and is told it may go on once.
    waiting: HashMap<&'static str, Vec<String>>,
    /// The floors as last settled; `None` before the first pass.
    settled: Option<Guard>,
    /// A change still moving: the floors it moved to, and when.
    moving: Option<(Guard, std::time::Instant)>,
}

impl State {
    /// The pass's events.
    ///
    /// - Holds that began (or moved to a later reset), with the agents
    ///   working then asked, once, to wrap up. An agent that starts working
    ///   later, while held, is someone's own doing and is left alone.
    /// - Holds that ended, with the agents that were asked to wait told they
    ///   may go on.
    /// - Floors that changed and settled, for the person.
    ///
    /// While the floors are still moving nothing about holds is decided: the
    /// holds of a half-finished drag are not the person's choice yet.
    pub fn update(
        &mut self,
        holds: &[Hold],
        agents: &[Agent],
        guard: Guard,
        now: std::time::Instant,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        let offset = tower_state::local_offset();
        let settled = *self.settled.get_or_insert(guard);
        let latest = self.moving.map_or(settled, |(g, _)| g);
        if guard != latest {
            self.moving = Some((guard, now));
        }
        if let Some((to, since)) = self.moving {
            if now.duration_since(since) < SETTLE {
                return out;
            }
            for vendor in ["claude", "codex"] {
                if let Some(change) = change_text(settled.floors(vendor), to.floors(vendor)) {
                    out.push(Event::FloorsChanged { vendor, change });
                }
            }
            self.settled = Some(to);
            self.moving = None;
        }
        let guard = self.settled.unwrap_or(guard);
        for h in holds {
            if self
                .active
                .get(h.vendor)
                .is_some_and(|a| a.until == h.until)
            {
                continue;
            }
            self.active.insert(
                h.vendor,
                Active {
                    until: h.until,
                    floors: guard.floors(h.vendor),
                },
            );
            out.push(Event::Held(h.clone()));
            for a in agents
                .iter()
                .filter(|a| a.state == AgentState::Working && vendor_of(&a.kind) == Some(h.vendor))
            {
                let asked = self.asked.entry(h.vendor).or_default();
                if asked.insert((a.id.clone(), h.until)) {
                    let waiting = self.waiting.entry(h.vendor).or_default();
                    if !waiting.contains(&a.id) {
                        waiting.push(a.id.clone());
                    }
                    out.push(Event::WrapUp {
                        agent: Box::new(a.clone()),
                        message: wrap_up_message(h, offset),
                    });
                }
            }
        }
        let gone: Vec<&'static str> = self
            .active
            .keys()
            .filter(|v| !holds.iter().any(|h| h.vendor == **v))
            .copied()
            .collect();
        for vendor in gone {
            let began = self.active.remove(vendor).map(|a| a.floors);
            // The episode is over: a later hold is a new one, and asks anew.
            self.asked.remove(vendor);
            let why = match began.and_then(|f| change_text(f, guard.floors(vendor))) {
                Some(change) => format!("the floor changed: {change}"),
                None => "the window has reset".to_string(),
            };
            for id in self.waiting.remove(vendor).unwrap_or_default() {
                if let Some(a) = agents
                    .iter()
                    .find(|a| a.id == id && a.state != AgentState::Dead)
                {
                    out.push(Event::Resume {
                        agent: Box::new(a.clone()),
                        message: resume_message(vendor, &why),
                    });
                }
            }
            out.push(Event::Released { vendor, why });
        }
        out
    }
}

/// How often the guard's board is refreshed. The sources themselves are
/// asked no more than every five minutes (`usagefetch`); this only picks up
/// what they last said, and a changed setting.
pub const REFRESH_EVERY: Duration = Duration::from_secs(60);

/// The latest board, kept by a thread of its own.
#[derive(Clone, Default)]
pub struct Refresher {
    latest: Arc<Mutex<Option<Board>>>,
}

impl Refresher {
    /// Starts the thread. It reads nothing while every guard is off, so a
    /// person who turned the guard off pays for no requests.
    pub fn spawn(home: PathBuf, state_dir: PathBuf) -> Self {
        let r = Refresher::default();
        let latest = r.latest.clone();
        let _ = std::thread::Builder::new()
            .name("usage-guard".into())
            .spawn(move || loop {
                let guard = Guard::from_config(&tower_config::load());
                let board = (!guard.is_off()).then(|| crate::usagefetch::board(&home, &state_dir));
                if let Ok(mut l) = latest.lock() {
                    *l = board;
                }
                std::thread::sleep(REFRESH_EVERY);
            });
        r
    }

    pub fn latest(&self) -> Option<Board> {
        self.latest.lock().ok().and_then(|l| l.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usagecmd::Vendor;
    use tower_usage::{Limits, Window};

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }
    const NOW: i64 = 1_790_211_600;

    fn vendor(name: &'static str, five: Option<(f64, i64)>, seven: Option<(f64, i64)>) -> Vendor {
        let w = |x: Option<(f64, i64)>| {
            x.map(|(used, reset)| Window {
                used_pct: used,
                resets_at: Some(at(NOW + reset)),
            })
        };
        Vendor {
            name,
            limits: Limits {
                five_hour: w(five),
                seven_day: w(seven),
                ..Limits::default()
            },
            note: None,
        }
    }

    fn board(vendors: Vec<Vendor>) -> Board {
        Board {
            vendors,
            at: at(NOW),
            guard: Guard::default(),
        }
    }

    fn guard(five: Option<u8>, seven: Option<u8>) -> Guard {
        Guard::uniform(five, seven)
    }

    #[test]
    fn a_window_trips_its_guard_at_the_floor_not_before() {
        // Hold at 15% left on 5h, 10% left on 7d.
        let g = guard(Some(15), Some(10));
        let now = at(NOW);
        let cases = [
            (84.9, 10.0, false),
            (85.0, 10.0, true),
            (10.0, 89.0, false),
            (10.0, 90.0, true),
        ];
        for (five, seven, want) in cases {
            let b = board(vec![vendor(
                "claude",
                Some((five, 3600)),
                Some((seven, 86400)),
            )]);
            assert_eq!(!holds(&b, &g, now).is_empty(), want, "{five} {seven}");
        }
    }

    #[test]
    fn fine_floors_use_the_exact_share_and_ignore_reached_flags() {
        for name in ["claude", "codex"] {
            for (floor, used, reset, expected) in [
                (Some(1), 98.99, 3600, false),
                (Some(1), 99.0, 3600, true),
                (Some(3), 96.99, 3600, false),
                (Some(3), 97.0, 3600, true),
                (Some(100), 0.0, 3600, true),
                (Some(1), 100.0, 0, false),
                (None, 100.0, 3600, false),
            ] {
                for reached in [false, true] {
                    let mut v = vendor(name, Some((used, reset)), None);
                    v.limits.reached = reached;
                    let b = board(vec![v]);
                    let h = holds(&b, &guard(floor, None), at(NOW));
                    assert_eq!(!h.is_empty(), expected, "{name} {floor:?} {used} {reset}");
                    assert_eq!(h, holds(&b, &guard(floor, None), at(NOW)), "stable reading");
                }
            }
        }
    }

    #[test]
    fn a_hold_says_why_and_lasts_until_the_latest_tripped_reset() {
        let g = guard(Some(50), Some(50));
        let b = board(vec![vendor(
            "codex",
            Some((60.0, 3600)),
            Some((70.0, 86400)),
        )]);
        let h = holds(&b, &g, at(NOW));
        assert_eq!(
            h,
            [Hold {
                vendor: "codex",
                why: "5h 40% left ≤ 50%, 7d 30% left ≤ 50%".into(),
                until: Some(at(NOW + 86400)),
            }]
        );
    }

    #[test]
    fn nothing_is_held_when_off_or_after_the_reset() {
        let b = board(vec![vendor("claude", Some((100.0, 3600)), None)]);
        assert!(holds(&b, &Guard::default(), at(NOW)).is_empty(), "off");
        // One second past the reset, the reading no longer holds.
        let g = guard(Some(50), None);
        assert!(holds(&b, &g, at(NOW + 3601)).is_empty());
        assert!(!holds(&b, &g, at(NOW + 3599)).is_empty());
        // A guard on the other window only does not look at this one.
        assert!(holds(&b, &guard(None, Some(50)), at(NOW)).is_empty());
    }

    #[test]
    fn a_reached_limit_respects_each_windows_off_setting() {
        for name in ["codex", "claude"] {
            for reached in [false, true] {
                for (five, seven, off, on, reason, reset) in [
                    (
                        10.0,
                        100.0,
                        guard(Some(25), None),
                        guard(Some(25), Some(25)),
                        "7d 0% left ≤ 25%",
                        86400,
                    ),
                    (
                        100.0,
                        10.0,
                        guard(None, Some(25)),
                        guard(Some(25), Some(25)),
                        "5h 0% left ≤ 25%",
                        3600,
                    ),
                ] {
                    let mut v = vendor(name, Some((five, 3600)), Some((seven, 86400)));
                    v.limits.reached = reached;
                    let b = board(vec![v]);
                    assert!(
                        holds(&b, &off, at(NOW)).is_empty(),
                        "{name} {reached} {off:?}"
                    );
                    assert_eq!(
                        holds(&b, &on, at(NOW)),
                        [Hold {
                            vendor: name,
                            why: reason.into(),
                            until: Some(at(NOW + reset)),
                        }]
                    );
                    assert!(holds(&b, &Guard::default(), at(NOW)).is_empty());
                    assert!(holds(&b, &on, at(NOW + 86401)).is_empty());
                }
            }
        }
    }

    #[test]
    fn a_reached_flag_without_windows_does_not_override_the_floors() {
        let mut v = vendor("codex", None, None);
        v.limits.reached = true;
        assert!(holds(&board(vec![v]), &guard(Some(25), Some(25)), at(NOW)).is_empty());
    }

    /// Each AI is held by its own floors only.
    #[test]
    fn each_ai_is_held_by_its_own_floor() {
        let b = board(vec![
            vendor("claude", Some((70.0, 3600)), None),
            vendor("codex", Some((70.0, 3600)), None),
        ]);
        // 30% left on both; claude keeps 40% back, codex only 20%.
        let g = Guard {
            claude: Floors {
                five_hour: Some(40),
                seven_day: None,
            },
            codex: Floors {
                five_hour: Some(20),
                seven_day: None,
            },
        };
        let h = holds(&b, &g, at(NOW));
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].vendor, "claude");
        assert_eq!(h[0].why, "5h 30% left ≤ 40%");
        // codex's floors off: its reached limit does not hold it.
        let mut v = vendor("codex", None, Some((100.0, 86400)));
        v.limits.reached = true;
        let g = Guard {
            claude: g.claude,
            codex: Floors::default(),
        };
        assert!(holds(&board(vec![v]), &g, at(NOW)).is_empty());
    }

    #[test]
    fn other_vendors_are_never_held() {
        let b = board(vec![vendor("gemini", Some((100.0, 60)), Some((100.0, 60)))]);
        assert!(holds(&b, &guard(Some(95), Some(95)), at(NOW)).is_empty());
        assert_eq!(vendor_of("opencode"), None);
        assert_eq!(vendor_of("codex"), Some("codex"));
    }

    fn working(id: &str, kind: &str, state: AgentState) -> Agent {
        let mut a = Agent::new(id, kind, at(NOW));
        a.state = state;
        a
    }

    fn t0() -> std::time::Instant {
        std::time::Instant::now()
    }

    fn secs(start: std::time::Instant, n: u64) -> std::time::Instant {
        start + std::time::Duration::from_secs(n)
    }

    fn codex_hold(until: i64) -> Hold {
        Hold {
            vendor: "codex",
            why: "7d 0% left ≤ 25%".into(),
            until: Some(at(NOW + until)),
        }
    }

    fn names(ev: &[Event], resume: bool) -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                Event::WrapUp { agent, .. } if !resume => Some(agent.id.clone()),
                Event::Resume { agent, .. } if resume => Some(agent.id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn working_agents_are_asked_once_and_told_when_it_is_over() {
        let mut agents = vec![
            working("codex-1", "codex", AgentState::Working),
            working("codex-2", "codex", AgentState::Idle),
            working("claude-1", "claude", AgentState::Working),
        ];
        let g = Guard::uniform(Some(25), Some(25));
        let start = t0();
        let hold = codex_hold(86400);
        let mut st = State::default();
        let ev = st.update(std::slice::from_ref(&hold), &agents, g, start);
        assert!(matches!(ev[0], Event::Held(_)));
        assert_eq!(
            names(&ev, false),
            ["codex-1"],
            "only the working codex agent"
        );
        // The next pass, the same hold: nothing new.
        assert!(st
            .update(std::slice::from_ref(&hold), &agents, g, secs(start, 2))
            .is_empty());
        // The reset: the one told to wait is told it may go on, and only it.
        agents[0].state = AgentState::Idle;
        let ev = st.update(&[], &agents, g, secs(start, 4));
        assert_eq!(names(&ev, true), ["codex-1"]);
        assert!(ev.iter().any(|e| matches!(
            e,
            Event::Released { vendor: "codex", why } if why == "the window has reset"
        )));
        // A new hold (a later reset) asks again.
        let later = codex_hold(2 * 86400);
        agents[0].state = AgentState::Working;
        let ev = st.update(&[later], &agents, g, secs(start, 6));
        assert_eq!(names(&ev, false), ["codex-1"]);
    }

    /// A hold that moves to a later reset while the agent is still working
    /// asks it again, but it waits for one release and is told it may go on
    /// once (#154).
    #[test]
    fn a_hold_moved_to_a_later_reset_resumes_the_agent_once() {
        let agents = [working("codex-1", "codex", AgentState::Working)];
        let g = Guard::uniform(Some(25), Some(25));
        let start = t0();
        let mut st = State::default();
        let ev = st.update(&[codex_hold(86400)], &agents, g, start);
        assert_eq!(names(&ev, false), ["codex-1"]);
        // The window reset under a turn in flight: the hold moved.
        let ev = st.update(&[codex_hold(2 * 86400)], &agents, g, secs(start, 2));
        assert_eq!(names(&ev, false), ["codex-1"], "a moved hold asks again");
        assert_eq!(st.waiting["codex"], ["codex-1"]);
        let ev = st.update(&[], &agents, g, secs(start, 4));
        assert_eq!(
            names(&ev, true),
            ["codex-1"],
            "one resume, not one per move"
        );
    }

    /// What was asked under a hold is forgotten when it is released, so the
    /// guard's memory does not grow with every episode (#156).
    #[test]
    fn a_released_hold_leaves_nothing_asked_behind() {
        let mut agents = vec![working("codex-1", "codex", AgentState::Working)];
        let g = Guard::uniform(Some(25), Some(25));
        let start = t0();
        let mut st = State::default();
        let first = codex_hold(86400);
        st.update(std::slice::from_ref(&first), &agents, g, start);
        assert!(st.asked["codex"].contains(&("codex-1".to_string(), first.until)));
        agents[0].state = AgentState::Idle;
        st.update(&[], &agents, g, secs(start, 2));
        assert!(!st.asked.contains_key("codex"), "{:?}", st.asked);
        // Taken again, under the next reset: only that episode is kept.
        agents[0].state = AgentState::Working;
        let second = codex_hold(8 * 86400);
        let ev = st.update(std::slice::from_ref(&second), &agents, g, secs(start, 4));
        assert_eq!(names(&ev, false), ["codex-1"]);
        let asked = &st.asked["codex"];
        assert!(
            !asked.iter().any(|(_, until)| *until == first.until),
            "{asked:?}"
        );
        assert_eq!(asked.len(), 1);
    }

    /// Lowering the floor below what is left lifts the hold, and the agents
    /// are told why.
    #[test]
    fn a_lowered_floor_lifts_the_hold_and_says_so() {
        let agents = [working("codex-1", "codex", AgentState::Working)];
        let start = t0();
        let mut st = State::default();
        let before = Guard::uniform(Some(25), Some(25));
        st.update(&[codex_hold(86400)], &agents, before, start);
        let after = Guard {
            claude: before.claude,
            codex: Floors {
                five_hour: Some(25),
                seven_day: None,
            },
        };
        // Moving: nothing is decided yet.
        assert!(st.update(&[], &agents, after, secs(start, 1)).is_empty());
        // Settled.
        let ev = st.update(&[], &agents, after, secs(start, 1 + SETTLE.as_secs()));
        let changed: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                Event::FloorsChanged { vendor, change } => {
                    assert_eq!(*vendor, "codex");
                    Some(change.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(changed, ["7d 25% → off"]);
        let why = ev
            .iter()
            .find_map(|e| match e {
                Event::Released { why, .. } => Some(why.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(why, "the floor changed: 7d 25% → off");
        let told = ev
            .iter()
            .find_map(|e| match e {
                Event::Resume { message, .. } => Some(message.clone()),
                _ => None,
            })
            .unwrap();
        assert!(told.contains("The hold on codex is lifted (the floor changed: 7d 25% → off)"));
        assert!(!told.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)));
    }

    #[test]
    fn turning_off_an_exhausted_weeks_guard_resumes_its_agents() {
        for name in ["claude", "codex"] {
            let mut v = vendor(name, Some((10.0, 3600)), Some((100.0, 86400)));
            v.limits.reached = true;
            let b = board(vec![v]);
            let agents = [working("agent-1", name, AgentState::Working)];
            let start = t0();
            let mut st = State::default();
            let before = guard(Some(25), Some(25));
            let mut after = before;
            match name {
                "claude" => after.claude.seven_day = None,
                "codex" => after.codex.seven_day = None,
                _ => unreachable!(),
            }
            let ev = st.update(&holds(&b, &before, at(NOW)), &agents, before, start);
            assert_eq!(names(&ev, false), ["agent-1"]);
            let released = holds(&b, &after, at(NOW));
            assert!(held_agents(&released, &agents).is_empty(), "{name}");
            assert!(st
                .update(&released, &agents, after, secs(start, 1))
                .is_empty());
            let ev = st.update(&released, &agents, after, secs(start, 1) + SETTLE);
            assert_eq!(names(&ev, true), ["agent-1"], "{name}");
            assert!(ev
                .iter()
                .any(|e| matches!(e, Event::Released { vendor, why }
                if *vendor == name && why == "the floor changed: 7d 25% → off")));
            assert!(st
                .update(&released, &agents, after, secs(start, 10))
                .is_empty());
        }
    }

    /// A drag steps through the floors several times a second: until it
    /// stops, no hold begins or ends, and a drag that comes back where it
    /// started says nothing at all.
    #[test]
    fn a_drag_decides_nothing_until_it_settles() {
        let agents = [working("codex-1", "codex", AgentState::Working)];
        let start = t0();
        let mut st = State::default();
        let base = Guard::uniform(Some(25), Some(25));
        assert!(st.update(&[], &agents, base, start).is_empty());
        let mut now = start;
        for floor in [30, 35, 40, 35, 30, 25] {
            now += std::time::Duration::from_millis(500);
            // Mid-drag the floors trip a hold; it must not be acted on.
            let g = Guard::uniform(Some(floor), Some(floor));
            assert!(
                st.update(&[codex_hold(86400)], &agents, g, now).is_empty(),
                "{floor}"
            );
        }
        // Back where it began and still: no floors changed, and the hold the
        // board shows now is decided as usual.
        let ev = st.update(&[], &agents, base, now + SETTLE);
        assert!(ev.is_empty(), "{ev:?}");
    }

    #[test]
    fn a_change_names_only_what_moved() {
        let f = |a: Option<u8>, b: Option<u8>| Floors {
            five_hour: a,
            seven_day: b,
        };
        assert_eq!(
            change_text(f(Some(25), Some(25)), f(Some(25), Some(25))),
            None
        );
        assert_eq!(
            change_text(f(Some(25), Some(25)), f(Some(20), Some(25))).as_deref(),
            Some("5h 25% → 20%")
        );
        assert_eq!(
            change_text(f(None, Some(25)), f(Some(10), Some(30))).as_deref(),
            Some("5h off → 10% · 7d 25% → 30%")
        );
    }

    #[test]
    fn only_the_held_vendors_agents_are_held() {
        let agents = [
            working("codex-1", "codex", AgentState::Idle),
            working("claude-1", "claude", AgentState::Idle),
            working("gemini-1", "gemini", AgentState::Idle),
        ];
        let holds = [Hold {
            vendor: "claude",
            why: String::new(),
            until: None,
        }];
        assert_eq!(
            held_agents(&holds, &agents),
            HashSet::from(["claude-1".to_string()])
        );
        assert!(held_agents(&[], &agents).is_empty());
    }

    #[test]
    fn the_wrap_up_message_names_the_vendor_the_reason_and_the_reset() {
        let kst = UtcOffset::from_hms(9, 0, 0).unwrap();
        let h = Hold {
            vendor: "codex",
            why: "7d 0% left ≤ 25%".into(),
            // 2026-09-26 17:35 KST, a Saturday.
            until: Some(at(1_790_411_738)),
        };
        let m = wrap_up_message(&h, kst);
        assert!(m.contains("codex"), "{m}");
        assert!(m.contains("7d 0% left ≤ 25%"), "{m}");
        assert!(m.contains("09-26 Sat 17:35"), "{m}");
        assert!(!m.contains('\n'), "one line: a newline would submit early");
        assert!(
            !m.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)),
            "English, no Hangul: {m}"
        );
        assert!(
            m.starts_with("[herdr-tower usage guard] codex's usage has reached"),
            "{m}"
        );
    }
}
