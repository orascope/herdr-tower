// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Which WAITs have gone unanswered long enough to announce again.
//!
//! Kept in the supervisor's memory rather than on the records: a reminder is a
//! courtesy, and a supervisor restart that sends one more is a far smaller
//! fault than a record field every writer has to carry along.

use std::collections::HashMap;
use std::time::Duration;

use time::OffsetDateTime;
use tower_state::{Agent, AgentState};

/// The WAITs already reminded of, by record ID, each keyed on when that wait
/// began — a new wait on the same record is a new question.
#[derive(Debug, Default)]
pub struct Reminders {
    sent: HashMap<String, OffsetDateTime>,
}

impl Reminders {
    /// The records to remind of now, each once per wait.
    pub fn due<'a>(
        &mut self,
        agents: &'a [Agent],
        after: Duration,
        now: OffsetDateTime,
    ) -> Vec<&'a Agent> {
        // Forget waits that are over, so the map holds only live ones.
        self.sent.retain(|id, since| {
            agents
                .iter()
                .any(|a| &a.id == id && a.state == AgentState::Waiting && a.state_since == *since)
        });
        let mut out = Vec::new();
        for a in agents {
            if a.state != AgentState::Waiting {
                continue;
            }
            let waited = now - a.state_since;
            if waited < after || self.sent.get(&a.id) == Some(&a.state_since) {
                continue;
            }
            self.sent.insert(a.id.clone(), a.state_since);
            out.push(a);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mins: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::minutes(mins)
    }

    fn waiting(id: &str, since: i64) -> Agent {
        let mut a = Agent::new(id, "claude", at(0));
        a.state = AgentState::Waiting;
        a.state_since = at(since);
        a
    }

    const AFTER: Duration = Duration::from_secs(15 * 60);

    #[test]
    fn a_long_wait_is_reminded_once() {
        let mut r = Reminders::default();
        let agents = [waiting("a", 0)];
        assert!(r.due(&agents, AFTER, at(10)).is_empty(), "not yet");
        assert_eq!(r.due(&agents, AFTER, at(15)).len(), 1);
        assert!(r.due(&agents, AFTER, at(40)).is_empty(), "once per wait");
    }

    /// Answered and asked again: the new question gets its own reminder.
    #[test]
    fn a_new_wait_on_the_same_record_is_reminded_again() {
        let mut r = Reminders::default();
        assert_eq!(r.due(&[waiting("a", 0)], AFTER, at(15)).len(), 1);
        let mut working = waiting("a", 0);
        working.state = AgentState::Working;
        assert!(r.due(&[working], AFTER, at(16)).is_empty());
        assert_eq!(r.due(&[waiting("a", 17)], AFTER, at(32)).len(), 1);
    }

    #[test]
    fn only_waits_are_reminded() {
        let mut r = Reminders::default();
        let mut done = waiting("a", 0);
        done.state = AgentState::DoneUnread;
        assert!(r.due(&[done], AFTER, at(60)).is_empty());
    }
}
