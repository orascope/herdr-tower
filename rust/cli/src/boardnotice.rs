// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! "Ready for review": the supervisor telling the person when a worker's turn
//! on a dispatch board item has ended.
//!
//! The board shows `✓ review` on its own, but only to someone looking at
//! it, and an advisor learns of it only when something wakes it. This sends
//! one notice per delivery — the same path as the Parallax and usage-guard
//! notices — and marks it on the item, so it is said once; a resend is a new
//! delivery and is said again. Nothing here changes an item's status.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tower_state::{column, Agent, Board, BoardStore, ItemStatus, Progress};

use crate::boardcmd::{records_for, row_text};

/// How often the boards are looked at.
pub const EVERY: Duration = Duration::from_secs(10);
/// How often linked PRs are asked about (`gh`, one call per PR).
pub const PR_EVERY: Duration = Duration::from_secs(60);

/// One item whose worker's turn on it has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub n: u32,
    pub attempt: u32,
    /// The row as the board shows it: `[w2:pBR] text`.
    pub row: String,
    pub uncertain: bool,
}

/// The items on `board` newly ready for review. Only an item read from
/// records `observed` just refreshed: a stale `DONE` is not news.
pub fn due(board: &Board, records: &[Agent], observed: &dyn Fn(&str) -> bool) -> Vec<Due> {
    ready(board, records, observed, |i| i.review_noticed)
}

/// The items the board's advisor has not been woken for yet: ready for
/// review, read from records `observed` just refreshed.
pub fn wake_due(board: &Board, records: &[Agent], observed: &dyn Fn(&str) -> bool) -> Vec<Due> {
    if board.waker.is_none() {
        return Vec::new();
    }
    ready(board, records, observed, |i| i.woken)
}

/// The sent items ready for review whose delivery `handled` (the attempt
/// last said or woken for) has not caught up with.
fn ready(
    board: &Board,
    records: &[Agent],
    observed: &dyn Fn(&str) -> bool,
    handled: impl Fn(&tower_state::BoardItem) -> u32,
) -> Vec<Due> {
    board
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Sent && handled(i) != i.attempts)
        .filter(|i| i.receipt.as_ref().is_some_and(|r| observed(&r.backend)))
        .filter_map(|i| {
            let found = records_for(i, records);
            let c = column(i, &found);
            (c.progress == Progress::Review).then(|| Due {
                n: i.n,
                attempt: i.attempts,
                row: row_text(i, &found),
                uncertain: c.uncertain,
            })
        })
        .collect()
}

/// What the advisor is told: one line per item, then what to do.
pub fn wake_message(due: &[Due]) -> String {
    let mut s = String::from("[herdr-tower board] Ready for review:");
    for d in due {
        s.push_str(&format!(
            "\n- item {}: {}",
            d.n,
            tower_text::truncate_cell(&d.row, 120)
        ));
    }
    s.push_str("\nReview each; then `herdr-tower board done N`, and `herdr-tower board next <pane>` for that worker's next item.");
    s
}

/// The notice for one item: a title and a line.
pub fn notice(repo: &str, d: &Due) -> (String, String) {
    let name = Path::new(repo)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(repo);
    let title = format!(
        "✓ {} · board item {} ready for review",
        tower_state::clean_text(name),
        d.n
    );
    let mut body = tower_text::truncate_cell(&d.row, 120);
    if d.uncertain {
        body.push_str(" (uncertain)");
    }
    (title, body)
}

/// The linked PRs' states, per repository.
type PrStates = HashMap<String, HashMap<u32, String>>;

/// Asks for the states of a repository's linked PRs.
pub type FetchPrs = fn(&str, &[u32]) -> HashMap<u32, String>;

/// The supervisor's side: at most every [`EVERY`], look at every board, mark
/// what is newly due and hand each to `say`.
pub struct Watch {
    last: Option<Instant>,
    last_prs: Option<Instant>,
    /// The PR states being fetched. `gh` is a network call: it runs on a
    /// thread of its own, one fetch at a time, and the pass that finds its
    /// answer acts on it — the pass itself never waits for it.
    prs: Option<mpsc::Receiver<PrStates>>,
    fetch_prs: FetchPrs,
}

impl Default for Watch {
    fn default() -> Self {
        Watch::with_fetch(crate::boardcmd::pr_states)
    }
}

/// The items whose linked PR has merged and was not yet announced.
pub fn merged(board: &Board, states: &std::collections::HashMap<u32, String>) -> Vec<(u32, u32)> {
    board
        .items
        .iter()
        .filter(|i| i.pr != 0 && !i.pr_noticed && !i.status.is_closed())
        .filter(|i| states.get(&i.pr).is_some_and(|s| s == "MERGED"))
        .map(|i| (i.n, i.pr))
        .collect()
}

/// The notice for an item whose PR merged.
pub fn merged_notice(repo: &str, n: u32, pr: u32, row: &str) -> (String, String) {
    let name = Path::new(repo)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(repo);
    (
        format!(
            "✓ {} · board item {n}: PR #{pr} merged",
            tower_state::clean_text(name)
        ),
        format!("{} — board done {n}", tower_text::truncate_cell(row, 100)),
    )
}

impl Watch {
    pub fn with_fetch(fetch_prs: FetchPrs) -> Self {
        Watch {
            last: None,
            last_prs: None,
            prs: None,
            fetch_prs,
        }
    }

    /// The PR states a fetch started earlier has come back with, if it has;
    /// starts the next fetch when one is due and none is running.
    fn pr_states(&mut self, boards: &BoardStore, now: OffsetDateTime) -> Option<PrStates> {
        let mut fresh = None;
        if let Some(rx) = &self.prs {
            match rx.try_recv() {
                Ok(states) => {
                    // Kept for the board panes, which draw these instead of
                    // each asking `gh` for the same PRs every minute.
                    for (repo, got) in &states {
                        let _ = boards.save_pr_states(repo, got, now);
                    }
                    fresh = Some(states);
                    self.prs = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.prs = None,
            }
        }
        let due = self.last_prs.is_none_or(|t| t.elapsed() >= PR_EVERY);
        if due && self.prs.is_none() {
            self.last_prs = Some(Instant::now());
            let wanted: Vec<(String, Vec<u32>)> = boards
                .repos()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|repo| {
                    let linked = crate::boardcmd::linked_prs(&boards.load(&repo, now).ok()?);
                    (!linked.is_empty()).then_some((repo, linked))
                })
                .collect();
            if !wanted.is_empty() {
                let (tx, rx) = mpsc::channel();
                let fetch = self.fetch_prs;
                let spawned =
                    std::thread::Builder::new()
                        .name("board-prs".into())
                        .spawn(move || {
                            let states: PrStates = wanted
                                .into_iter()
                                .map(|(repo, prs)| {
                                    let got = fetch(&repo, &prs);
                                    (repo, got)
                                })
                                .collect();
                            let _ = tx.send(states);
                        });
                if spawned.is_ok() {
                    self.prs = Some(rx);
                }
            }
        }
        fresh
    }

    /// `wake(advisor, message)` delivers to the board's advisor and answers
    /// whether it did — `false` while the advisor is busy or gone, and the
    /// items stay due for a later pass.
    pub fn tick(
        &mut self,
        state_dir: &Path,
        records: &[Agent],
        observed: &dyn Fn(&str) -> bool,
        say: &dyn Fn(&str, &str),
        wake: &dyn Fn(&tower_state::PaneWorker, &str) -> bool,
    ) -> Vec<String> {
        if self.last.is_some_and(|t| t.elapsed() < EVERY) {
            return Vec::new();
        }
        self.last = Some(Instant::now());
        let mut log = Vec::new();
        let Ok(boards) = BoardStore::new(state_dir) else {
            return log;
        };
        let now = OffsetDateTime::now_utc();
        let pr_states = self.pr_states(&boards, now);
        for repo in boards.repos().unwrap_or_default() {
            let Ok(board) = boards.load(&repo, now) else {
                continue;
            };
            if let Some(states) = pr_states.as_ref().and_then(|m| m.get(&repo)) {
                let done = merged(&board, states);
                if !done.is_empty() {
                    let marked = boards.update(&repo, now, |b| {
                        Ok(done
                            .iter()
                            .filter(|(n, pr)| b.note_pr_merged(*n, *pr))
                            .copied()
                            .collect::<Vec<_>>())
                    });
                    if let Ok(Ok(said)) = marked {
                        for (n, pr) in said {
                            let row = board
                                .item(n)
                                .map(|i| row_text(i, &records_for(i, records)))
                                .unwrap_or_default();
                            let (title, body) = merged_notice(&repo, n, pr, &row);
                            say(&title, &body);
                            log.push(format!("board: item {n} PR #{pr} merged ({repo})"));
                        }
                    }
                }
            }
            let to_wake = wake_due(&board, records, observed);
            if let (Some(advisor), false) = (&board.waker, to_wake.is_empty()) {
                if wake(advisor, &wake_message(&to_wake)) {
                    let _ = boards.update(&repo, now, |b| {
                        for d in &to_wake {
                            b.note_woken(d.n, d.attempt);
                        }
                        Ok(())
                    });
                    let ns: Vec<String> = to_wake.iter().map(|d| d.n.to_string()).collect();
                    log.push(format!(
                        "board: woke {} for item {} ({repo})",
                        advisor.pane_id,
                        ns.join(", ")
                    ));
                }
            }
            let found = due(&board, records, observed);
            if found.is_empty() {
                continue;
            }
            // Marked under the lock, and said only when the mark was ours: two
            // passes, or two supervisors, cannot say it twice.
            let marked = boards.update(&repo, now, |b| {
                Ok(found
                    .iter()
                    .filter(|d| b.note_review(d.n, d.attempt))
                    .cloned()
                    .collect::<Vec<_>>())
            });
            match marked {
                Ok(Ok(said)) => {
                    for d in said {
                        let (title, body) = notice(&repo, &d);
                        say(&title, &body);
                        log.push(format!("board: item {} ready for review ({repo})", d.n));
                    }
                }
                Ok(Err(e)) => log.push(format!("board notice for {repo}: {e}")),
                Err(e) => log.push(format!("board notice for {repo}: {e}")),
            }
        }
        log
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_state::{AgentState, Outcome, Receipt, SendFlags};

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    fn sent_board() -> Board {
        let mut b = Board::new("/w/dbstore");
        b.add(&["#3212 jsonb".into(), "other".into()], now())
            .unwrap();
        let t = b
            .reserve_send(1, "i-e", "/w/wt/i-e", SendFlags::default(), 1, now())
            .unwrap();
        b.apply(
            t,
            Outcome::Accepted(Receipt {
                backend: String::new(),
                session_id: "S1".into(),
                work_seq: 5,
                agent_id: "codex-w2:pBR".into(),
                adopted: false,
                record_born: Some(now()),
                at: now(),
                boundary_lost: false,
            }),
            now(),
        )
        .unwrap();
        b
    }

    fn worker(state: AgentState, work_seq: u64) -> Agent {
        let mut a = Agent::new("codex-w2:pBR", "codex", now());
        a.cwd = "/w/wt/i-e".into();
        a.pane.pane_id = "w2:pBR".into();
        a.session_id = "S1".into();
        a.work_seq = work_seq;
        a.state = state;
        a
    }

    #[test]
    fn only_a_finished_turn_on_an_observed_backend_is_due() {
        let b = sent_board();
        assert!(due(&b, &[worker(AgentState::Working, 6)], &|_| true).is_empty());
        assert!(
            due(&b, &[worker(AgentState::DoneUnread, 5)], &|_| true).is_empty(),
            "no start"
        );
        assert!(
            due(&b, &[worker(AgentState::DoneUnread, 6)], &|_| false).is_empty(),
            "stale"
        );
        let d = due(&b, &[worker(AgentState::DoneUnread, 6)], &|_| true);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].n, 1);
        assert_eq!(d[0].row, "[w2:pBR] #3212 jsonb");
    }

    #[test]
    fn a_noticed_delivery_is_not_due_again() {
        let mut b = sent_board();
        assert!(b.note_review(1, 1));
        assert!(due(&b, &[worker(AgentState::DoneUnread, 6)], &|_| true).is_empty());
    }

    #[test]
    fn the_notice_names_the_repo_the_item_and_its_pane() {
        let d = Due {
            n: 3,
            attempt: 1,
            row: "[w2:pBR] #3212 jsonb".into(),
            uncertain: true,
        };
        let (title, body) = notice("/Users/alice/dbstore_go_v2", &d);
        assert_eq!(title, "✓ dbstore_go_v2 · board item 3 ready for review");
        assert_eq!(body, "[w2:pBR] #3212 jsonb (uncertain)");
    }

    #[test]
    fn a_merged_pr_is_due_once_and_only_for_an_item_in_play() {
        let mut b = sent_board();
        b.link(1, 3231).unwrap();
        b.link(2, 3240).unwrap();
        let states: std::collections::HashMap<u32, String> =
            [(3231, "MERGED".to_string()), (3240, "OPEN".to_string())].into();
        assert_eq!(merged(&b, &states), vec![(1, 3231)]);
        assert!(b.note_pr_merged(1, 3231));
        assert!(merged(&b, &states).is_empty());
        let (title, body) = merged_notice("/w/dbstore", 1, 3231, "[w2:pBR] #3212 jsonb");
        assert_eq!(title, "✓ dbstore · board item 1: PR #3231 merged");
        assert_eq!(body, "[w2:pBR] #3212 jsonb — board done 1");
    }

    #[test]
    fn the_advisor_is_woken_once_it_takes_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let boards = BoardStore::new(dir.path()).unwrap();
        let mut b = sent_board();
        b.waker = Some(tower_state::PaneWorker {
            backend: String::new(),
            pane_id: "w2:p9B".into(),
            agent_id: "claude-w2:p9B".into(),
            session_id: String::new(),
            born_at: None,
        });
        boards
            .update(&b.repo.clone(), now(), |x| {
                *x = b.clone();
                Ok(())
            })
            .unwrap()
            .unwrap();
        let records = [worker(AgentState::DoneUnread, 6)];
        let told = std::cell::RefCell::new(Vec::new());
        let busy = |_: &tower_state::PaneWorker, _: &str| false;
        let idle = |p: &tower_state::PaneWorker, m: &str| {
            told.borrow_mut().push((p.pane_id.clone(), m.to_string()));
            true
        };
        // Busy: nothing marked, still due next pass.
        Watch::default().tick(dir.path(), &records, &|_| true, &|_, _| {}, &busy);
        Watch::default().tick(dir.path(), &records, &|_| true, &|_, _| {}, &idle);
        Watch::default().tick(dir.path(), &records, &|_| true, &|_, _| {}, &idle);
        let told = told.borrow();
        assert_eq!(told.len(), 1, "once");
        assert_eq!(told[0].0, "w2:p9B");
        assert!(
            told[0].1.contains("- item 1: [w2:pBR] #3212 jsonb"),
            "{}",
            told[0].1
        );
        assert!(told[0].1.contains("board done N"));
    }

    #[test]
    fn a_slow_pr_fetch_does_not_hold_the_pass_and_its_answer_is_acted_on() {
        fn slow_merged(_: &str, prs: &[u32]) -> HashMap<u32, String> {
            std::thread::sleep(Duration::from_millis(400));
            prs.iter().map(|p| (*p, "MERGED".to_string())).collect()
        }
        let dir = tempfile::tempdir().unwrap();
        let boards = BoardStore::new(dir.path()).unwrap();
        let mut b = sent_board();
        b.link(1, 77).unwrap();
        boards
            .update(&b.repo.clone(), now(), |x| {
                *x = b.clone();
                Ok(())
            })
            .unwrap()
            .unwrap();
        let said = std::cell::RefCell::new(Vec::new());
        let say = |t: &str, _: &str| said.borrow_mut().push(t.to_string());
        let mut w = Watch::with_fetch(slow_merged);
        let started = Instant::now();
        w.tick(dir.path(), &[], &|_| true, &say, &|_, _| false);
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the pass waited for the fetch"
        );
        assert!(said.borrow().is_empty(), "no answer yet");
        let deadline = Instant::now() + Duration::from_secs(10);
        while said.borrow().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            w.last = None; // past the throttle
            w.tick(dir.path(), &[], &|_| true, &say, &|_, _| false);
        }
        assert_eq!(said.borrow().len(), 1, "announced once the answer came");
        assert!(
            said.borrow()[0].contains("PR #77 merged"),
            "{:?}",
            said.borrow()
        );
        assert!(w.prs.is_none(), "no second fetch before PR_EVERY");
    }

    #[test]
    fn no_advisor_no_wake() {
        let b = sent_board();
        assert!(wake_due(&b, &[worker(AgentState::DoneUnread, 6)], &|_| true).is_empty());
    }

    /// The states a fetch comes back with are kept for the board panes,
    /// per repository, as soon as the pass that receives them runs.
    #[test]
    fn fetched_pr_states_are_kept_for_the_board_panes() {
        fn open_and_merged(_: &str, prs: &[u32]) -> HashMap<u32, String> {
            prs.iter()
                .map(|p| (*p, if *p == 77 { "MERGED" } else { "OPEN" }.to_string()))
                .collect()
        }
        let dir = tempfile::tempdir().unwrap();
        let boards = BoardStore::new(dir.path()).unwrap();
        let mut b = sent_board();
        b.link(1, 77).unwrap();
        let repo = b.repo.clone();
        boards
            .update(&repo, now(), |x| {
                *x = b.clone();
                Ok(())
            })
            .unwrap()
            .unwrap();
        let mut w = Watch::with_fetch(open_and_merged);
        let deadline = Instant::now() + Duration::from_secs(10);
        while boards.pr_states(&repo).is_empty() && Instant::now() < deadline {
            w.last = None;
            w.tick(dir.path(), &[], &|_| true, &|_, _| {}, &|_, _| false);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            boards.pr_states(&repo),
            HashMap::from([(77, "MERGED".to_string())])
        );
    }

    #[test]
    fn the_watch_says_each_once_and_marks_it() {
        let dir = tempfile::tempdir().unwrap();
        let boards = BoardStore::new(dir.path()).unwrap();
        let b = sent_board();
        boards
            .update(&b.repo.clone(), now(), |x| {
                *x = b.clone();
                Ok(())
            })
            .unwrap()
            .unwrap();
        let said = std::cell::RefCell::new(Vec::new());
        let say = |t: &str, _: &str| said.borrow_mut().push(t.to_string());
        let records = [worker(AgentState::DoneUnread, 6)];
        let mut w = Watch::default();
        let log = w.tick(dir.path(), &records, &|_| true, &say, &|_, _| false);
        assert_eq!(said.borrow().len(), 1, "{log:?}");
        // Throttled, and then already marked.
        w.tick(dir.path(), &records, &|_| true, &say, &|_, _| false);
        let mut fresh = Watch::default();
        fresh.tick(dir.path(), &records, &|_| true, &say, &|_, _| false);
        assert_eq!(said.borrow().len(), 1);
    }
}
