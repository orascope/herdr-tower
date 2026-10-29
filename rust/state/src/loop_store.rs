// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Durable opt-in leases for the idle-loop supervisor.
//!
//! They live beside the canonical agent store rather than in a process's
//! memory, so closing the tower does not silently turn the feature off.  A
//! separate lock keeps a supervisor update from racing a TUI/CLI toggle while
//! leaving the much hotter agent-record lock alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{write_atomic, Agent};

pub const DEFAULT_IDLE_FOR: Duration = Duration::from_secs(10 * 60);
pub const DEFAULT_LOOP_TTL: Duration = Duration::from_secs(168 * 60 * 60);

/// The editable body sent by an idle loop unless a lease carries an override.
///
/// The delivery header is deliberately not part of this text: send number and
/// remaining lifetime are live values, while this is the instruction the user
/// sees and edits when arming a loop from the tower.
pub const DEFAULT_LOOP_MESSAGE: &str =
    "Proceed with the next approved task and its tests. When complete, run herdr-tower loop done; if blocked, run herdr-tower loop blocked.";

const FILE: &str = "loops.json";
const MESSAGES_FILE: &str = "loop-messages.json";
const LOCK: &str = "loops.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopLease {
    pub agent_id: String,
    pub agent_session_id: String,
    /// Whether this lease has ever seen its agent dead.
    ///
    /// Durable because the evidence is not. A dead record is deleted after
    /// `tower_scan::DEAD_RETENTION` (24 hours) while a lease lasts up to 168,
    /// and the pause reason itself is overwritten by the `agent is missing`
    /// observation on the very next pass. herdr then hands the same
    /// deterministic pane ID to whatever arrives next, so a lease that forgot
    /// the death could adopt that agent and send this lease's autonomous
    /// instruction into it. Cleared only when the agent comes back under the
    /// lease's own session — the same conversation resumed in place, as
    /// `claude --resume` does — or when a person re-arms the lease.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub saw_dead: bool,
    /// The agent's turn count as of the last pass that looked — at arming, the
    /// count the lease was armed at.
    ///
    /// Evidence that a *settled* agent moved, as opposed to being looked at:
    /// `Store::mark_read` transitions a settled `DoneUnread` record to `Idle`
    /// and stamps `state_since`, so merely opening a row advances the
    /// timestamp without the agent doing anything. `work_seq` counts turns
    /// begun (`Agent::begin_turn`, `Agent::infer_turn`) and nothing else.
    pub seen_work_seq: u64,
    /// Empty means the default Herdr socket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    pub pane_id: String,
    pub session: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub idle_for_secs: u64,
    /// `None` is deliberately unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    #[serde(default)]
    pub sent_count: u64,
    /// A complete editable message body. Empty means [`DEFAULT_LOOP_MESSAGE`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instruction: String,
    /// The settled-state generation currently being handled.
    #[serde(
        with = "time::serde::rfc3339::option",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idle_since: Option<OffsetDateTime>,
    #[serde(default)]
    pub attempts_in_idle: u8,
    #[serde(
        with = "time::serde::rfc3339::option",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub retry_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub paused_reason: String,
}

impl LoopLease {
    pub fn new(
        agent: &Agent,
        now: OffsetDateTime,
        idle_for: Duration,
        ttl: Duration,
        max_turns: Option<u64>,
        instruction: String,
    ) -> Self {
        LoopLease {
            agent_id: agent.id.clone(),
            agent_session_id: agent.session_id.clone(),
            saw_dead: false,
            seen_work_seq: agent.work_seq,
            backend: agent.pane.backend.clone(),
            pane_id: agent.pane.pane_id.clone(),
            session: agent.pane.session.clone(),
            created_at: now,
            expires_at: now + time::Duration::try_from(ttl).unwrap_or(time::Duration::MAX),
            idle_for_secs: idle_for.as_secs(),
            max_turns,
            sent_count: 0,
            message: String::new(),
            instruction,
            idle_since: None,
            attempts_in_idle: 0,
            retry_at: None,
            paused_reason: String::new(),
        }
    }

    pub fn active(&self, now: OffsetDateTime) -> bool {
        self.paused_reason.is_empty() && now < self.expires_at
    }

    pub fn idle_for(&self) -> time::Duration {
        time::Duration::seconds(i64::try_from(self.idle_for_secs).unwrap_or(i64::MAX))
    }

    /// Whole hours rounded up, so a fresh 168-hour lease says `168h` rather
    /// than `167h` as soon as the first millisecond passes.
    pub fn remaining_hours(&self, now: OffsetDateTime) -> i64 {
        let seconds = (self.expires_at - now).whole_seconds().max(0);
        seconds.saturating_add(3_599) / 3_600
    }
}

#[derive(Debug, Clone)]
pub struct LoopStore {
    dir: PathBuf,
}

impl LoopStore {
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        crate::make_state_root(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    fn messages_path(&self) -> PathBuf {
        self.dir.join(MESSAGES_FILE)
    }

    pub fn list(&self) -> std::io::Result<Vec<LoopLease>> {
        match fs::read(self.path()) {
            Ok(body) => serde_json::from_slice(&body)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn replace(&self, leases: &[LoopLease]) -> std::io::Result<()> {
        let _lock = LoopLock::acquire(&self.dir)?;
        let old = self.list()?;
        self.remember_leases(&old)?;
        self.write(leases)
    }

    pub fn update<T>(
        &self,
        f: impl FnOnce(&mut Vec<LoopLease>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let _lock = LoopLock::acquire(&self.dir)?;
        let mut leases = self.list()?;
        self.remember_leases(&leases)?;
        let value = f(&mut leases)?;
        self.write(&leases)?;
        Ok(value)
    }

    pub fn arm(&self, lease: LoopLease) -> std::io::Result<()> {
        let _lock = LoopLock::acquire(&self.dir)?;
        let mut leases = self.list()?;
        self.remember_leases(&leases)?;
        self.remember_leases(std::slice::from_ref(&lease))?;
        leases.retain(|l| l.agent_id != lease.agent_id);
        leases.push(lease);
        self.write(&leases)
    }

    pub fn disarm(&self, agent_id: &str) -> std::io::Result<bool> {
        let _lock = LoopLock::acquire(&self.dir)?;
        let mut leases = self.list()?;
        let removed: Vec<_> = leases
            .iter()
            .filter(|lease| lease.agent_id == agent_id)
            .cloned()
            .collect();
        self.remember_leases(&removed)?;
        leases.retain(|lease| lease.agent_id != agent_id);
        self.write(&leases)?;
        Ok(!removed.is_empty())
    }

    /// Removes every lease owned by one Herdr server and leaves the others
    /// untouched. The empty namespace is the default server.
    pub fn disarm_backend(&self, backend: &str) -> std::io::Result<Vec<LoopLease>> {
        let _lock = LoopLock::acquire(&self.dir)?;
        let mut leases = self.list()?;
        let removed: Vec<_> = leases
            .iter()
            .filter(|lease| lease.backend == backend)
            .cloned()
            .collect();
        self.remember_leases(&removed)?;
        leases.retain(|lease| lease.backend != backend);
        self.write(&leases)?;
        Ok(removed)
    }

    pub fn contains(&self, agent_id: &str) -> std::io::Result<bool> {
        Ok(self.list()?.iter().any(|l| l.agent_id == agent_id))
    }

    /// The last instruction explicitly armed for this native conversation.
    ///
    /// The native session ID, not a pane coordinate or repository, is the key:
    /// panes are reusable and one conversation may move between them.
    pub fn remembered_message(
        &self,
        backend: &str,
        agent_session_id: &str,
    ) -> std::io::Result<Option<String>> {
        if agent_session_id.is_empty() {
            return Ok(None);
        }
        if let Some(message) = self
            .read_messages()?
            .get(&message_key(backend, agent_session_id))
        {
            return Ok(Some(message.clone()));
        }
        Ok(None)
    }

    /// Forgets the instructions remembered for conversations that neither
    /// `keep` (backend and session ID, from the agent records) nor a current
    /// lease names, and answers how many went.
    ///
    /// A conversation with no record left — its agent dead for over a day —
    /// cannot be picked in the tower to arm a loop on again, so what it was
    /// last told is only taking room.
    pub fn prune_messages(&self, keep: &[(String, String)]) -> std::io::Result<usize> {
        let _lock = LoopLock::acquire(&self.dir)?;
        let mut messages = self.read_messages()?;
        let mut named: std::collections::HashSet<String> =
            keep.iter().map(|(b, s)| message_key(b, s)).collect();
        for lease in self.list()? {
            named.insert(message_key(&lease.backend, &lease.agent_session_id));
        }
        let before = messages.len();
        messages.retain(|key, _| named.contains(key));
        let gone = before - messages.len();
        if gone > 0 {
            let body = serde_json::to_vec_pretty(&messages)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            write_atomic(&self.messages_path(), &body)?;
        }
        Ok(gone)
    }

    fn write(&self, leases: &[LoopLease]) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(leases)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.path(), &body)
    }

    fn read_messages(&self) -> std::io::Result<BTreeMap<String, String>> {
        match fs::read(self.messages_path()) {
            Ok(body) => serde_json::from_slice(&body)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e),
        }
    }

    fn remember_leases(&self, leases: &[LoopLease]) -> std::io::Result<()> {
        let mut messages = self.read_messages()?;
        let before = messages.clone();
        for lease in leases {
            if !lease.agent_session_id.is_empty() && !lease.message.trim().is_empty() {
                messages.insert(
                    message_key(&lease.backend, &lease.agent_session_id),
                    lease.message.clone(),
                );
            }
        }
        if messages == before {
            return Ok(());
        }
        let body = serde_json::to_vec_pretty(&messages)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.messages_path(), &body)
    }
}

fn message_key(backend: &str, agent_session_id: &str) -> String {
    if backend.is_empty() {
        agent_session_id.to_string()
    } else {
        format!("{backend}:{agent_session_id}")
    }
}

struct LoopLock(#[allow(dead_code)] fs::File);

impl LoopLock {
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

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    #[test]
    fn defaults_are_ten_minutes_seven_days_and_unlimited() {
        let a = Agent::new("codex-p1", "codex", now());
        let l = LoopLease::new(
            &a,
            now(),
            DEFAULT_IDLE_FOR,
            DEFAULT_LOOP_TTL,
            None,
            String::new(),
        );
        assert_eq!(l.idle_for_secs, 600);
        assert_eq!(l.expires_at - l.created_at, time::Duration::hours(168));
        assert_eq!(l.max_turns, None);
        assert_eq!(l.remaining_hours(now() + time::Duration::seconds(1)), 168);
    }

    #[test]
    fn a_pre_message_lease_still_deserializes_to_the_safe_default_path() {
        let a = Agent::new("codex-p1", "codex", now());
        let l = LoopLease::new(
            &a,
            now(),
            DEFAULT_IDLE_FOR,
            DEFAULT_LOOP_TTL,
            None,
            String::new(),
        );
        let mut value = serde_json::to_value(l).unwrap();
        value.as_object_mut().unwrap().remove("message");
        let old: LoopLease = serde_json::from_value(value).unwrap();
        assert!(old.message.is_empty());
    }

    #[test]
    fn arm_replaces_and_disarm_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let s = LoopStore::new(d.path()).unwrap();
        let a = Agent::new("codex-p1", "codex", now());
        s.arm(LoopLease::new(
            &a,
            now(),
            DEFAULT_IDLE_FOR,
            DEFAULT_LOOP_TTL,
            None,
            "first".into(),
        ))
        .unwrap();
        let mut replacement = LoopLease::new(
            &a,
            now(),
            DEFAULT_IDLE_FOR,
            DEFAULT_LOOP_TTL,
            Some(3),
            "second".into(),
        );
        replacement.sent_count = 2;
        s.arm(replacement).unwrap();
        assert_eq!(s.list().unwrap().len(), 1);
        assert_eq!(s.list().unwrap()[0].instruction, "second");
        assert!(s.disarm(&a.id).unwrap());
        assert!(!s.disarm(&a.id).unwrap());
    }

    #[test]
    fn disarming_keeps_the_last_message_for_that_native_session() {
        let d = tempfile::tempdir().unwrap();
        let s = LoopStore::new(d.path()).unwrap();
        let mut a = Agent::new("codex-p1", "codex", now());
        a.session_id = "conversation-1".into();
        let mut lease = LoopLease::new(
            &a,
            now(),
            DEFAULT_IDLE_FOR,
            DEFAULT_LOOP_TTL,
            None,
            String::new(),
        );
        lease.message = "이 지시를 기억하세요.".into();
        s.arm(lease).unwrap();
        s.disarm(&a.id).unwrap();

        assert!(s.list().unwrap().is_empty());
        assert_eq!(
            s.remembered_message("", "conversation-1")
                .unwrap()
                .as_deref(),
            Some("이 지시를 기억하세요.")
        );
        assert_eq!(s.remembered_message("", "conversation-2").unwrap(), None);
    }

    #[test]
    fn disarming_one_backend_keeps_the_other_backends_leases_and_messages_separate() {
        let d = tempfile::tempdir().unwrap();
        let s = LoopStore::new(d.path()).unwrap();
        let mut first = Agent::new("codex-b1-p1", "codex", now());
        first.session_id = "shared-conversation".into();
        first.pane.backend = "b1".into();
        let mut second = Agent::new("codex-b2-p1", "codex", now());
        second.session_id = "shared-conversation".into();
        second.pane.backend = "b2".into();
        for (agent, message) in [(&first, "first"), (&second, "second")] {
            let mut lease = LoopLease::new(
                agent,
                now(),
                DEFAULT_IDLE_FOR,
                DEFAULT_LOOP_TTL,
                None,
                String::new(),
            );
            lease.message = message.into();
            s.arm(lease).unwrap();
        }

        let removed = s.disarm_backend("b1").unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(s.list().unwrap()[0].backend, "b2");
        assert_eq!(
            s.remembered_message("b1", "shared-conversation")
                .unwrap()
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            s.remembered_message("b2", "shared-conversation")
                .unwrap()
                .as_deref(),
            Some("second")
        );
    }

    /// #127: closing one server's loops leaves another server's lease exactly
    /// as the supervisor last wrote it, send reservation included.
    #[test]
    fn disarming_one_backend_keeps_the_others_reservation() {
        let d = tempfile::tempdir().unwrap();
        let s = LoopStore::new(d.path()).unwrap();
        for (id, backend) in [("codex-b1-p1", "b1"), ("codex-b2-p1", "b2")] {
            let mut a = Agent::new(id, "codex", now());
            a.session_id = format!("{id}-conversation");
            a.pane.backend = backend.into();
            s.arm(LoopLease::new(
                &a,
                now(),
                DEFAULT_IDLE_FOR,
                DEFAULT_LOOP_TTL,
                Some(5),
                String::new(),
            ))
            .unwrap();
        }
        // A planning pass reserves a send on b2's lease.
        let reserved_at = now() + time::Duration::minutes(10);
        s.update(|leases| {
            let l = leases.iter_mut().find(|l| l.backend == "b2").unwrap();
            l.sent_count = 3;
            l.attempts_in_idle = 1;
            l.retry_at = Some(reserved_at);
            Ok(())
        })
        .unwrap();

        let removed = s.disarm_backend("b1").unwrap();
        assert_eq!(removed.len(), 1);
        let left = s.list().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(
            (
                left[0].sent_count,
                left[0].attempts_in_idle,
                left[0].retry_at
            ),
            (3, 1, Some(reserved_at))
        );
    }

    // ── g4testadd: forgetting messages nobody can use ───────────────────

    fn lease_for(backend: &str, session_id: &str, agent_id: &str) -> LoopLease {
        let mut a = Agent::new(agent_id, "codex", now());
        a.session_id = session_id.into();
        a.pane.backend = backend.into();
        LoopLease::new(
            &a,
            now(),
            DEFAULT_IDLE_FOR,
            DEFAULT_LOOP_TTL,
            None,
            String::new(),
        )
    }

    /// A current lease keeps its conversation's message even when no record
    /// names it; the same session ID on another backend is another
    /// conversation; and with nothing to forget — or no messages file at all —
    /// nothing is written.
    #[test]
    fn pruning_keeps_what_a_lease_names_and_tells_backends_apart() {
        let d = tempfile::tempdir().unwrap();
        let s = LoopStore::new(d.path()).unwrap();
        assert_eq!(s.prune_messages(&[]).unwrap(), 0, "no file, nothing to do");
        assert!(
            !d.path().join(MESSAGES_FILE).exists(),
            "and none is created"
        );

        s.replace(&[lease_for("", "sid-leased", "codex-w2:p1")])
            .unwrap();
        fs::write(
            d.path().join(MESSAGES_FILE),
            r#"{"sid-leased": "계속 진행하세요", "sid-kept": "keep going",
                "b1:sid-kept": "the other server", "sid-gone": "long forgotten"}"#,
        )
        .unwrap();

        let keep = [("b1".to_string(), "sid-kept".to_string())];
        assert_eq!(s.prune_messages(&keep).unwrap(), 2);
        assert_eq!(
            s.remembered_message("", "sid-leased").unwrap().as_deref(),
            Some("계속 진행하세요"),
            "a lease names it"
        );
        assert_eq!(
            s.remembered_message("b1", "sid-kept").unwrap().as_deref(),
            Some("the other server")
        );
        assert_eq!(
            s.remembered_message("", "sid-kept").unwrap(),
            None,
            "kept on b1 is not kept on the default server"
        );
        assert_eq!(s.remembered_message("", "sid-gone").unwrap(), None);

        let before = fs::read(d.path().join(MESSAGES_FILE)).unwrap();
        assert_eq!(s.prune_messages(&keep).unwrap(), 0);
        assert_eq!(
            fs::read(d.path().join(MESSAGES_FILE)).unwrap(),
            before,
            "nothing forgotten, nothing rewritten"
        );
    }

    /// Cross validation over generated stores: what pruning keeps is exactly
    /// the messages whose conversation a record or a lease names, the count it
    /// answers is how many went, and a second prune forgets nothing more.
    #[test]
    fn pruning_agrees_with_a_plain_filter() {
        let mut rng = tower_testrng::Rng::new(27);
        let backends = ["", "b1", "b2"];
        let key = |b: &str, sid: &str| {
            if b.is_empty() {
                sid.to_string()
            } else {
                format!("{b}:{sid}")
            }
        };
        for round in 0..40 {
            let d = tempfile::tempdir().unwrap();
            let s = LoopStore::new(d.path()).unwrap();
            let convs: Vec<(String, String)> = (0..rng.below(8))
                .map(|i| {
                    let b = backends[rng.below(backends.len())];
                    (b.to_string(), format!("sid-{round}-{i}"))
                })
                .collect();
            let messages: BTreeMap<String, String> = convs
                .iter()
                .map(|(b, sid)| (key(b, sid), format!("message for {sid}")))
                .collect();
            let keep: Vec<(String, String)> = convs
                .iter()
                .filter(|_| rng.below(3) == 0)
                .cloned()
                .collect();
            let leased: Vec<(String, String)> = convs
                .iter()
                .filter(|_| rng.below(4) == 0)
                .cloned()
                .collect();
            let leases: Vec<LoopLease> = leased
                .iter()
                .enumerate()
                .map(|(i, (b, sid))| lease_for(b, sid, &format!("codex-w{i}:p1")))
                .collect();
            s.replace(&leases).unwrap();
            fs::write(
                d.path().join(MESSAGES_FILE),
                serde_json::to_vec(&messages).unwrap(),
            )
            .unwrap();

            let named: std::collections::HashSet<String> = keep
                .iter()
                .chain(&leased)
                .map(|(b, sid)| key(b, sid))
                .collect();
            let want: BTreeMap<String, String> = messages
                .iter()
                .filter(|(k, _)| named.contains(*k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let gone = s.prune_messages(&keep).unwrap();
            assert_eq!(gone, messages.len() - want.len(), "round {round}");
            assert_eq!(s.read_messages().unwrap(), want, "round {round}");
            assert_eq!(s.prune_messages(&keep).unwrap(), 0, "round {round}");
        }
    }
}
