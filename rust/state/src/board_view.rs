// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The dispatch board's right-hand column: an item plus its worker's record,
//! read at render time, and nothing written back.
//!
//! A pure function so every row of the table in `docs/dispatch-board.md` can be
//! pinned by a test. Finding the records is the caller's part: every record —
//! dead ones included — whose `cwd` is the item's worktree on the receipt's
//! backend. What to do when the store or the backend cannot be read (keep the
//! last good column, marked stale) is the view's part too; a missing record is
//! [`Progress::Unseen`] here, never [`Progress::Gone`].

use time::OffsetDateTime;

use crate::{Agent, AgentState, BoardItem, ItemStatus};

/// What the column says about one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Planned,
    Sending,
    /// The delivery's outcome is unknown.
    SentUnknown,
    /// Delivered; the worker has not begun a turn since.
    NotStarted,
    /// A turn begun after the delivery is running.
    Working,
    /// A turn begun after the delivery has ended: the advisor's to review.
    Review,
    /// The worker is waiting for a person.
    Waiting,
    /// The worker's record says it failed.
    Errored,
    /// A different conversation holds the worktree now; its progress is not
    /// this item's. Frozen until the advisor decides.
    Replaced,
    /// The worker's record was made afresh since the delivery (a different
    /// `born_at`, or a count below the receipt's), so its turns cannot be
    /// compared. Whether it is still the same conversation is not known.
    Unclear,
    /// Delivered, but the worker's session was not known at the time, so
    /// progress is withheld rather than guessed.
    Withheld,
    /// More than one live record in the worktree: nothing is guessed.
    Ambiguous,
    /// Only dead records are left in the worktree.
    Gone,
    /// No record at all — which is not evidence of anything.
    Unseen,
    Done,
    Dropped,
    Superseded,
}

/// One item's column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub progress: Progress,
    /// Where elapsed time is counted from, when it means something.
    pub since: Option<OffsetDateTime>,
    /// The evidence behind this column is weaker than usual: the completion
    /// boundary was lost at delivery, or a superseded item's own delivery was
    /// unknown.
    pub uncertain: bool,
}

impl Column {
    fn new(progress: Progress, since: Option<OffsetDateTime>) -> Self {
        Self {
            progress,
            since,
            uncertain: false,
        }
    }

    /// The column's words, without the elapsed time: `review`, `wait`.
    pub fn label(&self) -> &'static str {
        match self.progress {
            Progress::Planned => "planned",
            Progress::Sending => "sending…",
            Progress::SentUnknown => "sent?  check the worker",
            Progress::NotStarted => "sent  no start seen",
            Progress::Working => "working",
            Progress::Review => "review",
            Progress::Waiting => "wait",
            Progress::Errored => "error",
            Progress::Replaced => "worker replaced — check",
            Progress::Unclear => "worker unclear — check",
            Progress::Withheld => "sent",
            Progress::Ambiguous => "several agents — check",
            Progress::Gone => "gone",
            Progress::Unseen => "sent  no record",
            Progress::Done => "done",
            Progress::Dropped => "dropped",
            Progress::Superseded => "superseded",
        }
    }

    /// One single-width glyph per group of columns, drawn before the label so
    /// a long board can be scanned: the label keeps the detail.
    pub fn icon(&self) -> char {
        match self.progress {
            Progress::Waiting | Progress::Errored => '!',
            Progress::Review => '✓',
            Progress::SentUnknown
            | Progress::Replaced
            | Progress::Unclear
            | Progress::Ambiguous
            | Progress::Gone
            | Progress::Unseen => '?',
            Progress::Working => '●',
            Progress::Sending | Progress::NotStarted | Progress::Withheld => '◌',
            Progress::Planned => '○',
            Progress::Done | Progress::Dropped | Progress::Superseded => '–',
        }
    }

    /// Whether the row is drawn struck through.
    pub fn struck(&self) -> bool {
        matches!(
            self.progress,
            Progress::Done | Progress::Dropped | Progress::Superseded
        )
    }
}

/// The column for `item`, given every record found in its worktree on the
/// receipt's backend.
pub fn column(item: &BoardItem, records: &[&Agent]) -> Column {
    let decided = match item.status {
        ItemStatus::Planned => Some(Progress::Planned),
        ItemStatus::Sending => Some(Progress::Sending),
        ItemStatus::SentUnknown => Some(Progress::SentUnknown),
        ItemStatus::Done => Some(Progress::Done),
        ItemStatus::Dropped => Some(Progress::Dropped),
        ItemStatus::Superseded => Some(Progress::Superseded),
        ItemStatus::Sent => None,
    };
    if let Some(progress) = decided {
        let since = match progress {
            Progress::Sending => item.reservation.as_ref().map(|r| r.at),
            Progress::Done | Progress::Dropped | Progress::Superseded => item.closed_at,
            _ => None,
        };
        let mut c = Column::new(progress, since);
        c.uncertain = item.uncertain;
        return c;
    }
    let Some(receipt) = &item.receipt else {
        // `sent` without a receipt is not a state this store writes.
        return Column::new(Progress::Withheld, None);
    };
    let mut c = sent_column(receipt, records);
    // Adopted: the advisor sent it; its turn may have begun before the board
    // took it over.
    c.uncertain |= receipt.boundary_lost || receipt.adopted;
    c
}

fn sent_column(receipt: &crate::Receipt, records: &[&Agent]) -> Column {
    let live: Vec<&Agent> = records
        .iter()
        .copied()
        .filter(|a| a.state != AgentState::Dead)
        .collect();
    let a = match live.as_slice() {
        [] if records.is_empty() => return Column::new(Progress::Unseen, Some(receipt.at)),
        [] => return Column::new(Progress::Gone, None),
        [one] => *one,
        _ => return Column::new(Progress::Ambiguous, None),
    };
    // A worker just started has no session id yet, and that is the usual
    // first dispatch. Its record's generation (`born_at`) still pins which
    // agent the count belongs to, so progress is shown; only a receipt with
    // neither leaves the identity to the count alone, and says so.
    let no_session = receipt.session_id.is_empty();
    // Only a different id is evidence of a different conversation. A count
    // below the receipt's is not: the scanner starts a record over at zero when
    // it reuses a pane id, even for the same conversation resumed there.
    if !no_session && !a.session_id.is_empty() && a.session_id != receipt.session_id {
        return Column::new(Progress::Replaced, None);
    }
    // Another record than the one the receipt read: its count started over,
    // so no count comparison means anything any more. A receipt read from a
    // record older than `born_at` has none, and such a record only ever gains
    // one by being born anew — so a `born_at` appearing is a change too. When
    // neither has one, the count going backwards is the only sign left.
    let reborn = receipt.record_born != a.born_at;
    if reborn || a.work_seq < receipt.work_seq {
        return Column::new(Progress::Unclear, None);
    }
    let started = a.work_seq > receipt.work_seq;
    // A record with no id yet may be the same conversation or a new one.
    let unsure = a.session_id.is_empty() || (no_session && receipt.record_born.is_none());
    let mut c = match a.state {
        // A person is needed whoever's turn it is, so these show either way —
        // marked uncertain when this item's turn was not seen to begin.
        AgentState::Waiting => Column::new(Progress::Waiting, Some(a.state_since)),
        AgentState::Error => Column::new(Progress::Errored, Some(a.state_since)),
        AgentState::Working if started => Column::new(Progress::Working, Some(a.state_since)),
        // `state_since` is the completion only while it is unread: reading the
        // row, or a dispatch acknowledging it, restamps it.
        AgentState::DoneUnread if started => Column::new(Progress::Review, Some(a.state_since)),
        AgentState::Idle if started => Column::new(Progress::Review, None),
        // No start seen. A `WORK` here is a turn that was already running when
        // the item went out (the check-then-prompt race); a hookless worker can
        // also start and finish between two polls without one being counted.
        AgentState::Working | AgentState::DoneUnread | AgentState::Idle => {
            Column::new(Progress::NotStarted, Some(receipt.at))
        }
        AgentState::Dead => unreachable!("filtered above"),
    };
    c.uncertain =
        unsure || (!started && matches!(c.progress, Progress::Waiting | Progress::Errored));
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Board, Outcome, Receipt, SendFlags};

    fn at(mins: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::minutes(mins)
    }

    /// Read from a record born at minute 0, like every [`worker`] below.
    fn receipt(session: &str, work_seq: u64) -> Receipt {
        Receipt {
            backend: String::new(),
            session_id: session.into(),
            work_seq,
            agent_id: String::new(),
            adopted: false,
            record_born: Some(at(0)),
            at: at(0),
            boundary_lost: false,
        }
    }

    /// Item 1, delivered with `r`.
    fn sent_item(r: Receipt) -> BoardItem {
        let mut b = Board::new("/r");
        b.add(&["a".into()], at(0)).unwrap();
        let t = b
            .reserve_send(1, "t", "/r/wt/t", SendFlags::default(), 1, at(0))
            .unwrap();
        b.apply(t, Outcome::Accepted(r), at(0)).unwrap();
        b.item(1).unwrap().clone()
    }

    fn worker(state: AgentState, session: &str, work_seq: u64) -> Agent {
        let mut a = Agent::new("codex-w1:p1", "codex", at(0));
        a.state = state;
        a.session_id = session.into();
        a.work_seq = work_seq;
        a.state_since = at(7);
        a
    }

    fn progress(item: &BoardItem, records: &[Agent]) -> Progress {
        let refs: Vec<&Agent> = records.iter().collect();
        column(item, &refs).progress
    }

    #[test]
    fn a_settled_worker_with_no_new_turn_has_not_started() {
        let item = sent_item(receipt("S1", 5));
        for state in [AgentState::Idle, AgentState::DoneUnread] {
            let c = column(&item, &[&worker(state, "S1", 5)]);
            assert_eq!(c.progress, Progress::NotStarted, "{state:?}");
            assert_eq!(c.since, Some(at(0)), "counted from the delivery");
        }
    }

    #[test]
    fn a_stale_done_from_the_previous_item_is_not_this_items_review() {
        // The worker was DONE with its last item when this one went out; that
        // DONE is still standing, but no turn has begun since.
        let item = sent_item(receipt("S1", 5));
        assert_eq!(
            progress(&item, &[worker(AgentState::DoneUnread, "S1", 5)]),
            Progress::NotStarted
        );
    }

    #[test]
    fn a_turn_begun_after_the_delivery_is_work_then_review() {
        let item = sent_item(receipt("S1", 5));
        let c = column(&item, &[&worker(AgentState::Working, "S1", 6)]);
        assert_eq!(c.progress, Progress::Working);
        assert_eq!(c.since, Some(at(7)));
        for state in [AgentState::DoneUnread, AgentState::Idle] {
            // Reading the row turns DONE_UNREAD into IDLE; still review.
            assert_eq!(
                progress(&item, &[worker(state, "S1", 6)]),
                Progress::Review,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_turn_that_was_already_running_is_not_this_items_work() {
        let item = sent_item(receipt("S1", 5));
        assert_eq!(
            progress(&item, &[worker(AgentState::Working, "S1", 5)]),
            Progress::NotStarted
        );
    }

    #[test]
    fn waiting_and_error_show_either_way_uncertain_before_a_start_was_seen() {
        let item = sent_item(receipt("S1", 5));
        for state in [AgentState::Waiting, AgentState::Error] {
            let before = column(&item, &[&worker(state, "S1", 5)]);
            let after = column(&item, &[&worker(state, "S1", 6)]);
            assert_eq!(before.progress, after.progress, "{state:?}");
            assert!(before.uncertain, "{state:?}: maybe the previous turn's");
            assert!(!after.uncertain, "{state:?}");
        }
    }

    #[test]
    fn another_conversation_in_the_worktree_is_replaced_not_progress() {
        let item = sent_item(receipt("S1", 5));
        assert_eq!(
            progress(&item, &[worker(AgentState::DoneUnread, "S2", 9)]),
            Progress::Replaced
        );
    }

    #[test]
    fn a_count_that_went_backwards_is_unclear_not_replaced() {
        // The scanner restarts a record at zero on a reused pane id, even for
        // the same conversation resumed there.
        let item = sent_item(receipt("S1", 5));
        for session in ["S1", ""] {
            assert_eq!(
                progress(&item, &[worker(AgentState::Working, session, 1)]),
                Progress::Unclear,
                "{session:?}"
            );
        }
    }

    #[test]
    fn a_record_made_afresh_is_unclear_even_once_its_count_catches_up() {
        // Same conversation resumed on a reused pane: the new record's count
        // started at zero and has since passed the receipt's.
        let item = sent_item(receipt("S1", 5));
        let mut fresh = worker(AgentState::DoneUnread, "S1", 6);
        fresh.born_at = Some(at(30));
        assert_eq!(progress(&item, &[fresh]), Progress::Unclear);
    }

    #[test]
    fn a_receipt_from_before_born_at_falls_back_to_the_count() {
        let mut r = receipt("S1", 5);
        r.record_born = None;
        let item = sent_item(r);
        let old = |seq| {
            let mut a = worker(AgentState::DoneUnread, "S1", seq);
            a.born_at = None;
            a
        };
        assert_eq!(progress(&item, &[old(6)]), Progress::Review);
        assert_eq!(progress(&item, &[old(2)]), Progress::Unclear);
        // The record gained a `born_at` since: it was born anew (or revived
        // without evidence), so its count is not the receipt's.
        assert_eq!(
            progress(&item, &[worker(AgentState::DoneUnread, "S1", 6)]),
            Progress::Unclear
        );
    }

    #[test]
    fn a_record_without_a_session_yet_is_judged_by_its_count_but_uncertain() {
        let item = sent_item(receipt("S1", 5));
        let c = column(&item, &[&worker(AgentState::Working, "", 6)]);
        assert_eq!(c.progress, Progress::Working);
        assert!(c.uncertain);
    }

    #[test]
    fn review_time_counts_only_while_the_completion_is_unread() {
        let item = sent_item(receipt("S1", 5));
        let unread = column(&item, &[&worker(AgentState::DoneUnread, "S1", 6)]);
        assert_eq!(unread.since, Some(at(7)));
        // Reading it restamps state_since, which is then not the completion.
        let read = column(&item, &[&worker(AgentState::Idle, "S1", 6)]);
        assert_eq!(read.progress, Progress::Review);
        assert_eq!(read.since, None);
    }

    #[test]
    fn a_first_dispatch_before_the_session_is_known_still_shows_progress() {
        // The worker had no session id when the item went out — a worker just
        // started. Its record generation pins the agent, so this is certain.
        let item = sent_item(receipt("", 5));
        let c = column(&item, &[&worker(AgentState::DoneUnread, "S1", 6)]);
        assert_eq!(c.progress, Progress::Review);
        assert!(!c.uncertain);
        assert_eq!(column(&item, &[]).progress, Progress::Unseen);
        // A new record in the worktree is still caught, by its generation.
        let mut fresh = worker(AgentState::Working, "S2", 7);
        fresh.born_at = Some(at(30));
        assert_eq!(progress(&item, &[fresh]), Progress::Unclear);
    }

    #[test]
    fn with_neither_a_session_nor_a_generation_progress_is_uncertain() {
        let mut r = receipt("", 0);
        r.record_born = None;
        let item = sent_item(r);
        let mut a = worker(AgentState::DoneUnread, "S1", 1);
        a.born_at = None;
        let c = column(&item, &[&a]);
        assert_eq!(c.progress, Progress::Review);
        assert!(c.uncertain);
    }

    #[test]
    fn gone_needs_a_dead_record_and_missing_is_only_unseen() {
        let item = sent_item(receipt("S1", 5));
        assert_eq!(
            progress(&item, &[worker(AgentState::Dead, "S1", 6)]),
            Progress::Gone
        );
        assert_eq!(progress(&item, &[]), Progress::Unseen);
    }

    #[test]
    fn a_dead_record_beside_the_live_one_does_not_matter() {
        let item = sent_item(receipt("S1", 5));
        assert_eq!(
            progress(
                &item,
                &[
                    worker(AgentState::Dead, "S0", 99),
                    worker(AgentState::Working, "S1", 6)
                ]
            ),
            Progress::Working
        );
    }

    #[test]
    fn two_live_records_are_ambiguous() {
        let item = sent_item(receipt("S1", 5));
        assert_eq!(
            progress(
                &item,
                &[
                    worker(AgentState::Idle, "S1", 6),
                    worker(AgentState::Working, "S1", 6)
                ]
            ),
            Progress::Ambiguous
        );
    }

    #[test]
    fn a_lost_boundary_marks_the_column_uncertain() {
        let mut r = receipt("S1", 5);
        r.boundary_lost = true;
        let item = sent_item(r);
        let c = column(&item, &[&worker(AgentState::DoneUnread, "S1", 6)]);
        assert_eq!(c.progress, Progress::Review);
        assert!(c.uncertain);
    }

    #[test]
    fn decided_statuses_ignore_the_worker() {
        let mut b = Board::new("/r");
        b.add(&["a".into(), "b".into(), "c".into()], at(0)).unwrap();
        b.done(&[2], at(3)).unwrap();
        b.drop_items(&[3], "", at(4)).unwrap();
        let busy = worker(AgentState::Working, "S1", 9);
        assert_eq!(
            progress(b.item(1).unwrap(), std::slice::from_ref(&busy)),
            Progress::Planned
        );
        let done = column(b.item(2).unwrap(), &[&busy]);
        assert_eq!(done.progress, Progress::Done);
        assert_eq!(done.since, Some(at(3)));
        assert!(done.struck());
        assert!(column(b.item(3).unwrap(), &[&busy]).struck());
        assert!(!column(b.item(1).unwrap(), &[&busy]).struck());
    }

    #[test]
    fn sending_and_sent_question_come_from_the_item() {
        let mut b = Board::new("/r");
        b.add(&["a".into(), "b".into()], at(0)).unwrap();
        b.reserve_send(1, "t", "/r/wt/t", SendFlags::default(), 1, at(2))
            .unwrap();
        let t = b
            .reserve_send(2, "u", "/r/wt/u", SendFlags::default(), 1, at(0))
            .unwrap();
        b.apply(t, Outcome::Unknown, at(0)).unwrap();
        let sending = column(b.item(1).unwrap(), &[]);
        assert_eq!(sending.progress, Progress::Sending);
        assert_eq!(sending.since, Some(at(2)));
        assert_eq!(
            column(b.item(2).unwrap(), &[]).progress,
            Progress::SentUnknown
        );
    }

    #[test]
    fn an_uncertain_supersede_is_marked() {
        let mut b = Board::new("/r");
        b.add(&["a".into(), "b".into()], at(0)).unwrap();
        let t = b
            .reserve_send(1, "t", "/r/wt/t", SendFlags::default(), 1, at(0))
            .unwrap();
        b.apply(t, Outcome::Unknown, at(0)).unwrap();
        let replace = SendFlags {
            again: false,
            replace: true,
        };
        let t = b
            .reserve_send(2, "t", "/r/wt/t", replace, 1, at(0))
            .unwrap();
        b.apply(t, Outcome::Accepted(receipt("S1", 5)), at(1))
            .unwrap();
        let c = column(b.item(1).unwrap(), &[]);
        assert_eq!(c.progress, Progress::Superseded);
        assert!(c.uncertain);
    }

    #[test]
    fn every_progress_has_words() {
        use Progress::*;
        for p in [
            Planned,
            Sending,
            SentUnknown,
            NotStarted,
            Working,
            Review,
            Waiting,
            Errored,
            Replaced,
            Unclear,
            Withheld,
            Ambiguous,
            Gone,
            Unseen,
            Done,
            Dropped,
            Superseded,
        ] {
            assert!(!Column::new(p, None).label().is_empty(), "{p:?}");
        }
    }

    #[test]
    fn icons_follow_the_groups() {
        use Progress::*;
        let groups: [(&[Progress], char); 7] = [
            (&[Waiting, Errored], '!'),
            (&[Review], '✓'),
            (
                &[SentUnknown, Replaced, Unclear, Ambiguous, Gone, Unseen],
                '?',
            ),
            (&[Working], '●'),
            (&[Sending, NotStarted, Withheld], '◌'),
            (&[Planned], '○'),
            (&[Done, Dropped, Superseded], '–'),
        ];
        let mut seen = 0;
        for (members, icon) in groups {
            for &p in members {
                let c = Column::new(p, None);
                assert_eq!(c.icon(), icon, "{p:?}");
                assert_eq!(c.struck(), icon == '–', "{p:?}");
                seen += 1;
            }
        }
        assert_eq!(seen, 17, "every progress belongs to one group");
    }
}
