// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What Parallax Browser's own agents have handed back to the person, as the
//! resident supervisor last read it: `parallax.json` in the state directory.
//!
//! Parallax is a separate local browser with agents of its own, and a handoff
//! is one of them stopping to ask for a human. The tower's job is "one place
//! that shows who is waiting on you", so those count — but only as something
//! to *show*. The supervisor asks Parallax's local API every ten seconds or so
//! and writes this file; every surface that shows it (`status --line` in
//! herdr's tab bar every five seconds, the table, the TUI) reads the file and
//! never the API, so a slow or absent Parallax costs them nothing.
//!
//! The file is a snapshot, not a feed: it says when it was taken, and a reader
//! ignores one older than [`PARALLAX_FRESH`] rather than showing a handoff the
//! person may have answered a minute ago because the supervisor stopped.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::write_atomic;

/// The snapshot the supervisor writes.
pub const PARALLAX_FILE: &str = "parallax.json";
/// The handoff IDs already announced, so a restarted supervisor does not
/// announce them again.
pub const PARALLAX_NOTIFIED_FILE: &str = "parallax-notified.json";

/// How old a snapshot may be and still be shown.
///
/// Six polls' worth: long enough that one slow answer does not blank the tab
/// bar, short enough that a supervisor that stopped does not leave a handoff
/// on screen for good.
pub const PARALLAX_FRESH: time::Duration = time::Duration::seconds(60);

/// How much a handoff asks of the person — Parallax's own four levels.
///
/// Declared in rising order so `Ord` is urgency: sorting by it descending puts
/// the one to look at first. A level this build does not know reads as
/// `None`: a newer Parallax adding a level must not make the tower claim
/// somebody is waiting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    #[default]
    None,
    Review,
    Action,
    Urgent,
}

/// Read through [`Attention::from_wire`], so an unknown word is `None` rather
/// than a snapshot that no longer parses.
impl<'de> Deserialize<'de> for Attention {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let word = String::deserialize(d)?;
        Ok(Attention::from_wire(&word))
    }
}

impl Attention {
    /// Parallax's spelling, as its API writes it. Anything else is `None`.
    pub fn from_wire(s: &str) -> Attention {
        match s {
            "review" => Attention::Review,
            "action" => Attention::Action,
            "urgent" => Attention::Urgent,
            _ => Attention::None,
        }
    }

    /// Whether this level is somebody waiting on the person — `action` or
    /// `urgent`. `review` is a finished thing to look at, counted apart, the
    /// way the tower counts DONE apart from WAIT.
    pub fn waits(self) -> bool {
        matches!(self, Attention::Action | Attention::Urgent)
    }
}

/// One open handoff, as much of it as the tower shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallaxHandoff {
    pub id: String,
    /// The Parallax workspace's name, not its ID: this is read by a person.
    #[serde(default)]
    pub workspace: String,
    /// Parallax's status word as it came (`needs_human`, `blocked`, …), so a
    /// status this build does not know is still shown as what it is.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub attention: Attention,
    /// Empty when Parallax redacted the row: the tower is not entitled to its
    /// content, only to knowing it exists.
    #[serde(default)]
    pub title: String,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_at: Option<OffsetDateTime>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_at: Option<OffsetDateTime>,
}

impl ParallaxHandoff {
    /// The status in words, for a line a person reads.
    pub fn status_label(&self) -> &str {
        match self.status.as_str() {
            "needs_human" => "needs you",
            "blocked" => "blocked",
            "waiting_approval" => "waiting for approval",
            "approval_expired" => "approval expired",
            "ready_to_resume" => "ready to resume",
            "completed_review" => "done, to review",
            "resolved" => "resolved",
            other => other,
        }
    }

    /// What to call it on one line: its title, or its status when the title
    /// was redacted.
    pub fn headline(&self) -> &str {
        if self.title.is_empty() {
            self.status_label()
        } else {
            &self.title
        }
    }
}

/// The supervisor's latest reading of Parallax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallaxSnapshot {
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Whether the API answered. `false` carries no handoffs: an unreachable
    /// Parallax says nothing about who is waiting, and showing the last list
    /// would claim it does.
    pub reachable: bool,
    /// How many workspaces were read.
    #[serde(default)]
    pub workspaces: usize,
    /// Open handoffs with any attention at all, most urgent first.
    #[serde(default)]
    pub handoffs: Vec<ParallaxHandoff>,
}

impl ParallaxSnapshot {
    /// Young enough to show; see [`PARALLAX_FRESH`]. A clock that went
    /// backwards leaves the snapshot fresh rather than hiding it.
    pub fn fresh(&self, now: OffsetDateTime) -> bool {
        now - self.updated_at <= PARALLAX_FRESH
    }

    /// The handoffs waiting on the person, most urgent first.
    pub fn waiting(&self) -> impl Iterator<Item = &ParallaxHandoff> {
        self.handoffs.iter().filter(|h| h.attention.waits())
    }

    /// How many are finished work to look at rather than a question.
    pub fn review_count(&self) -> usize {
        self.handoffs
            .iter()
            .filter(|h| h.attention == Attention::Review)
            .count()
    }
}

/// The handoffs waiting on the person in `snapshot`, or none when it is
/// missing, stale or unreachable. The one rule every surface shows by.
pub fn parallax_waiting(
    snapshot: Option<&ParallaxSnapshot>,
    now: OffsetDateTime,
) -> Vec<&ParallaxHandoff> {
    match snapshot {
        Some(s) if s.reachable && s.fresh(now) => s.waiting().collect(),
        _ => Vec::new(),
    }
}

/// Puts the most urgent first, then the one waiting longest, then by ID so
/// two equal rows keep one order from poll to poll and the screen does not
/// shuffle.
pub fn sort_handoffs(handoffs: &mut [ParallaxHandoff]) {
    handoffs.sort_by(|a, b| {
        b.attention
            .cmp(&a.attention)
            .then_with(|| match (a.created_at, b.created_at) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// One announced handoff and when it was announced, so the list can be
/// trimmed by age rather than growing for ever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifiedHandoff {
    pub id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// `parallax.json` and `parallax-notified.json` in one state directory.
#[derive(Debug, Clone)]
pub struct ParallaxStore {
    dir: PathBuf,
}

impl ParallaxStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    /// The snapshot, or `None` when there is none. A file that does not parse
    /// is `None` too: every reader of this is a display, and a display with
    /// nothing to add is better than one that fails over a half-edited file.
    pub fn read(&self) -> Option<ParallaxSnapshot> {
        let body = fs::read(self.dir.join(PARALLAX_FILE)).ok()?;
        serde_json::from_slice(&body).ok()
    }

    /// Written whole through a temp file and a rename, owner-only: a handoff
    /// title is what one of the person's agents said, as private as a record.
    pub fn write(&self, snapshot: &ParallaxSnapshot) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.dir.join(PARALLAX_FILE), &body)
    }

    /// Removes the snapshot — `parallax` turned off. Already gone is fine.
    pub fn remove(&self) -> std::io::Result<()> {
        match fs::remove_file(self.dir.join(PARALLAX_FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The handoffs already announced. A missing or broken file is an empty
    /// list: the worst that does is one repeated notice.
    pub fn read_notified(&self) -> Vec<NotifiedHandoff> {
        fs::read(self.dir.join(PARALLAX_NOTIFIED_FILE))
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok())
            .unwrap_or_default()
    }

    pub fn write_notified(&self, notified: &[NotifiedHandoff]) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(notified)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.dir.join(PARALLAX_NOTIFIED_FILE), &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::seconds(secs)
    }

    fn handoff(id: &str, attention: Attention, created: Option<i64>) -> ParallaxHandoff {
        ParallaxHandoff {
            id: id.into(),
            workspace: "Default".into(),
            status: "needs_human".into(),
            attention,
            title: String::new(),
            created_at: created.map(at),
            updated_at: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_snapshot_round_trips_owner_only_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        assert_eq!(store.read(), None, "nothing written yet");
        let snapshot = ParallaxSnapshot {
            updated_at: at(0),
            reachable: true,
            workspaces: 3,
            handoffs: vec![handoff("h1", Attention::Urgent, Some(5))],
        };
        store.write(&snapshot).unwrap();
        assert_eq!(store.read(), Some(snapshot.clone()));
        let mode = fs::metadata(dir.path().join(PARALLAX_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(snapshot.fresh(at(60)));
        assert!(!snapshot.fresh(at(61)));
        assert_eq!(parallax_waiting(Some(&snapshot), at(60)).len(), 1);
        assert!(parallax_waiting(Some(&snapshot), at(61)).is_empty());
        store.remove().unwrap();
        store.remove().unwrap();
        assert_eq!(store.read(), None);
    }

    #[test]
    fn a_broken_file_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PARALLAX_FILE), b"{not json").unwrap();
        fs::write(dir.path().join(PARALLAX_NOTIFIED_FILE), b"[1,2").unwrap();
        let store = ParallaxStore::new(dir.path());
        assert_eq!(store.read(), None);
        assert!(store.read_notified().is_empty());
    }

    #[test]
    fn only_action_and_urgent_wait_and_review_counts_apart() {
        let snapshot = ParallaxSnapshot {
            updated_at: at(0),
            reachable: true,
            workspaces: 1,
            handoffs: vec![
                handoff("a", Attention::Action, None),
                handoff("u", Attention::Urgent, None),
                handoff("r", Attention::Review, None),
                handoff("n", Attention::None, None),
            ],
        };
        let ids: Vec<_> = snapshot.waiting().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["a", "u"]);
        assert_eq!(snapshot.review_count(), 1);
        let unreachable = ParallaxSnapshot {
            reachable: false,
            ..snapshot
        };
        assert!(parallax_waiting(Some(&unreachable), at(0)).is_empty());
        assert!(parallax_waiting(None, at(0)).is_empty());
    }

    #[test]
    fn an_unknown_attention_level_reads_as_none() {
        let h: ParallaxHandoff =
            serde_json::from_str(r#"{"id":"x","attention":"apocalyptic"}"#).unwrap();
        assert_eq!(h.attention, Attention::None);
        assert_eq!(Attention::from_wire("apocalyptic"), Attention::None);
        assert_eq!(Attention::from_wire("urgent"), Attention::Urgent);
    }

    #[test]
    fn the_most_urgent_then_the_oldest_comes_first() {
        let mut v = vec![
            handoff("late-action", Attention::Action, Some(30)),
            handoff("review", Attention::Review, Some(1)),
            handoff("undated-urgent", Attention::Urgent, None),
            handoff("early-action", Attention::Action, Some(10)),
            handoff("urgent", Attention::Urgent, Some(50)),
        ];
        sort_handoffs(&mut v);
        let ids: Vec<_> = v.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "urgent",
                "undated-urgent",
                "early-action",
                "late-action",
                "review"
            ]
        );
    }

    #[test]
    fn a_redacted_handoff_is_named_by_its_status() {
        let mut h = handoff("x", Attention::Urgent, None);
        h.status = "waiting_approval".into();
        assert_eq!(h.headline(), "waiting for approval");
        h.title = "Approve the upload".into();
        assert_eq!(h.headline(), "Approve the upload");
        h.status = "brand_new".into();
        assert_eq!(h.status_label(), "brand_new");
    }

    #[test]
    fn the_notified_list_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let list = vec![NotifiedHandoff {
            id: "h1".into(),
            at: at(0),
        }];
        store.write_notified(&list).unwrap();
        assert_eq!(store.read_notified(), list);
    }

    const LEVELS: [Attention; 4] = [
        Attention::None,
        Attention::Review,
        Attention::Action,
        Attention::Urgent,
    ];

    /// A generated handoff list: few IDs, levels and times, so ties on every
    /// key but the last are common.
    fn generated(rng: &mut tower_testrng::Rng) -> Vec<ParallaxHandoff> {
        (0..rng.below(12))
            .map(|_| {
                let created = if rng.one_in(4) {
                    None
                } else {
                    Some(rng.below(5) as i64)
                };
                let mut h = handoff(
                    &format!("h{}", rng.below(8)),
                    *rng.pick(&LEVELS).unwrap(),
                    created,
                );
                h.title = ["", "승인", "x".repeat(300).as_str(), "a\"b\\c"][rng.below(4)].into();
                h
            })
            .collect()
    }

    /// Properties over generated lists: the sort is a permutation, it is
    /// idempotent, it does not depend on the input order, and it agrees with
    /// a naive sort by the documented key (attention descending, dated before
    /// undated, oldest first, then ID).
    #[test]
    fn sorting_is_a_permutation_in_key_order_whatever_the_input_order() {
        fn ids(v: &[ParallaxHandoff]) -> Vec<String> {
            let mut ids: Vec<String> = v.iter().map(|h| h.id.clone()).collect();
            ids.sort();
            ids
        }
        let mut rng = tower_testrng::Rng::new(0x5027_AB1E);
        for _ in 0..500 {
            let input = generated(&mut rng);
            let mut sorted = input.clone();
            sort_handoffs(&mut sorted);
            assert_eq!(ids(&sorted), ids(&input), "a permutation");
            assert_eq!(sorted.len(), input.len());

            let mut naive = input.clone();
            naive.sort_by_key(|h| {
                (
                    std::cmp::Reverse(h.attention),
                    h.created_at.is_none(),
                    h.created_at,
                    h.id.clone(),
                )
            });
            let key = |v: &[ParallaxHandoff]| -> Vec<(Attention, Option<OffsetDateTime>, String)> {
                v.iter()
                    .map(|h| (h.attention, h.created_at, h.id.clone()))
                    .collect()
            };
            assert_eq!(key(&sorted), key(&naive));
            assert!(sorted.windows(2).all(|w| w[0].attention >= w[1].attention));

            let mut again = sorted.clone();
            sort_handoffs(&mut again);
            assert_eq!(again, sorted, "idempotent");

            let mut reversed: Vec<_> = input.iter().rev().cloned().collect();
            sort_handoffs(&mut reversed);
            assert_eq!(key(&reversed), key(&sorted), "input order does not matter");
        }
        let mut empty: Vec<ParallaxHandoff> = Vec::new();
        sort_handoffs(&mut empty);
        assert!(empty.is_empty());
    }

    /// Edges of freshness: a nanosecond past a minute is stale, a clock that
    /// went backwards (a snapshot "from the future") is fresh, and freshness
    /// is monotone — fresh at some moment means fresh at every earlier one.
    #[test]
    fn freshness_boundaries_and_monotonicity() {
        let s = ParallaxSnapshot {
            updated_at: at(0),
            reachable: true,
            workspaces: 0,
            handoffs: vec![],
        };
        assert!(s.fresh(at(60)));
        assert!(!s.fresh(at(60) + time::Duration::nanoseconds(1)));
        assert!(s.fresh(at(60) - time::Duration::nanoseconds(1)));
        assert!(s.fresh(at(-3600)), "a clock that went back keeps it fresh");
        let mut last = true;
        for secs in -5..=130 {
            let now = s.fresh(at(secs));
            assert!(last || !now, "stale at an earlier moment, fresh at {secs}");
            last = now;
        }
    }

    /// Property over generated snapshots: `parallax_waiting` is exactly the
    /// action and urgent handoffs, in their stored order, of a reachable and
    /// fresh snapshot — a subset whose size a naive count predicts — and
    /// nothing otherwise.
    #[test]
    fn parallax_waiting_is_the_waiting_subset_in_order() {
        let mut rng = tower_testrng::Rng::new(0x03A1_71D6);
        for _ in 0..300 {
            let snapshot = ParallaxSnapshot {
                updated_at: at(0),
                reachable: !rng.one_in(4),
                workspaces: rng.below(4),
                handoffs: generated(&mut rng),
            };
            let now = at(rng.below(120) as i64);
            let got = parallax_waiting(Some(&snapshot), now);
            if !snapshot.reachable || !snapshot.fresh(now) {
                assert!(got.is_empty());
                continue;
            }
            let naive: Vec<&ParallaxHandoff> = snapshot
                .handoffs
                .iter()
                .filter(|h| matches!(h.attention, Attention::Action | Attention::Urgent))
                .collect();
            assert_eq!(got, naive);
            assert!(got.iter().all(|h| h.attention.waits()));
            let reviews = snapshot
                .handoffs
                .iter()
                .filter(|h| h.attention == Attention::Review)
                .count();
            assert_eq!(snapshot.review_count(), reviews);
            assert!(got.len() + reviews <= snapshot.handoffs.len());
        }
    }

    /// Round trip over generated snapshots — unicode, quotes, a 300-character
    /// title, missing dates, every level including `None` — and over generated
    /// notified lists: what is written reads back equal.
    #[test]
    fn generated_snapshots_and_notified_lists_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let mut rng = tower_testrng::Rng::new(0x0005_704E);
        for i in 0..100 {
            let mut handoffs = generated(&mut rng);
            for h in &mut handoffs {
                h.updated_at = h.created_at.map(|t| t + time::Duration::milliseconds(123));
                h.workspace = ["", "Default", "작업 공간", "w\n"][rng.below(4)].into();
            }
            let snapshot = ParallaxSnapshot {
                updated_at: at(i) + time::Duration::nanoseconds(i * 7),
                reachable: rng.one_in(2),
                workspaces: rng.below(9),
                handoffs,
            };
            store.write(&snapshot).unwrap();
            assert_eq!(store.read(), Some(snapshot));
            let notified: Vec<NotifiedHandoff> = (0..rng.below(5))
                .map(|k| NotifiedHandoff {
                    id: format!("n{k}-{}", rng.below(1000)),
                    at: at(k as i64),
                })
                .collect();
            store.write_notified(&notified).unwrap();
            assert_eq!(store.read_notified(), notified);
        }
    }

    /// Round trip through the file for words this build does not know, and
    /// for the words it does: an unknown or differently-cased level reads as
    /// `None` and so does not wait; missing optional fields take defaults.
    #[test]
    fn a_file_with_unknown_levels_and_missing_fields_reads_back_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParallaxStore::new(dir.path());
        let body = r#"{"updated_at":"2024-10-06T00:00:00Z","reachable":true,"extra":1,
            "handoffs":[{"id":"a","attention":"apocalyptic"},{"id":"b","attention":"URGENT"},
            {"id":"c","attention":""},{"id":"d","attention":"urgent"},{"id":"e"}]}"#;
        fs::write(dir.path().join(PARALLAX_FILE), body).unwrap();
        let s = store.read().unwrap();
        let levels: Vec<Attention> = s.handoffs.iter().map(|h| h.attention).collect();
        assert_eq!(
            levels,
            [
                Attention::None,
                Attention::None,
                Attention::None,
                Attention::Urgent,
                Attention::None
            ]
        );
        assert_eq!(s.workspaces, 0);
        assert_eq!(s.handoffs[0].workspace, "");
        assert_eq!(s.waiting().count(), 1);
        for level in LEVELS {
            let word = serde_json::to_string(&level).unwrap();
            let back: Attention = serde_json::from_str(&word).unwrap();
            assert_eq!(back, level, "{word}");
        }
        fs::write(dir.path().join(PARALLAX_FILE), r#"{"reachable":true}"#).unwrap();
        assert_eq!(store.read(), None, "no updated_at is no snapshot");
    }
}
