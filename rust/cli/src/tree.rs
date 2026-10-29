// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Who works under whom: the coordinator each worker pane is drawn under in
//! the tower's session tree.
//!
//! The tree used to come from herdr's layout alone — the first pane of a tab
//! over the rest of that tab. A `wt` task opens its worker in a tab of its
//! own, so every worker became a root of its own beside the coordinator that
//! made it. The relation is known elsewhere, and this reads it, strongest
//! last so it wins:
//!
//! 1. the pane `wt new` was run from (the task record's `parent`);
//! 2. a board's advisor over every worker it has an item with;
//! 3. a link somebody made with `herdr-tower tree link`.
//!
//! herdr reuses pane ids, so every source is held to the agents that were
//! there when it was written: an agent born in that pane afterwards is
//! somebody else, and the relation is dropped rather than drawn wrong. The
//! supervisor runs this every pass, so the tree follows the panes as they
//! come and go.

use std::collections::BTreeMap;

use time::OffsetDateTime;
use tower_state::{Agent, Board, TreeLink};
use tower_wt::Meta;

/// Worker to coordinator, both as `tower_mux::target_key`s. Only live agents,
/// never a pane under itself, and only within one workspace: the tree is
/// drawn under a workspace rule, and a branch cannot cross one.
pub fn parents(
    agents: &[Agent],
    metas: &[Meta],
    boards: &[Board],
    links: &[TreeLink],
) -> BTreeMap<String, String> {
    let live: Vec<&Agent> = agents.iter().filter(|a| a.is_live()).collect();
    // The live agent in a pane, if it was there by `at` (or `at` is unknown).
    let in_pane = |backend: &str, pane: &str, at: Option<OffsetDateTime>| -> Option<&Agent> {
        live.iter()
            .copied()
            .find(|a| a.pane.backend == backend && a.pane.pane_id == pane)
            .filter(|a| match (a.born_at, at) {
                (Some(born), Some(at)) => born <= at,
                _ => true,
            })
    };
    let in_worktree = |path: &str| -> Option<&Agent> {
        let mut found = live.iter().copied().filter(|a| a.cwd == path);
        let first = found.next()?;
        // Two agents in one worktree: which one is the task's is not known.
        found.next().is_none().then_some(first)
    };
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut put = |worker: &Agent, coordinator: &Agent| {
        if worker.id == coordinator.id
            || tower_state::group_key(worker) != tower_state::group_key(coordinator)
        {
            return;
        }
        out.insert(key(worker), key(coordinator));
    };

    for m in metas {
        let Some(p) = &m.parent else { continue };
        let (Some(worker), Some(coordinator)) = (
            in_worktree(&m.path),
            in_pane(&p.backend, &p.pane_id, Some(m.created_at)),
        ) else {
            continue;
        };
        put(worker, coordinator);
    }

    for b in boards {
        let advisors = advisors(b, &in_pane);
        if advisors.is_empty() {
            continue;
        }
        for item in &b.items {
            let worker = if let Some(p) = item.pane_worker() {
                in_pane(&p.backend, &p.pane_id, p.born_at.or(Some(item.created_at)))
                    .filter(|a| p.agent_id.is_empty() || a.id == p.agent_id)
            } else if !item.worktree.is_empty() {
                in_worktree(&item.worktree)
            } else {
                None
            };
            // One repository's board can be run from several workspaces:
            // the advisor in the worker's own one, else the likeliest.
            if let Some(worker) = worker {
                let advisor = advisors
                    .iter()
                    .find(|a| tower_state::group_key(a) == tower_state::group_key(worker))
                    .unwrap_or(&advisors[0]);
                put(worker, advisor);
            }
        }
    }

    for l in links {
        if let (Some(worker), Some(coordinator)) = (
            in_pane(&l.backend, &l.worker, Some(l.at)),
            in_pane(&l.backend, &l.coordinator, Some(l.at)),
        ) {
            put(worker, coordinator);
        }
    }
    out
}

/// [`parents`] from what is on disk under `state_dir`: the task records, the
/// boards and the links. Best effort: what does not read is left out.
pub fn read_parents(
    state_dir: &std::path::Path,
    agents: &[Agent],
    now: OffsetDateTime,
) -> BTreeMap<String, String> {
    let metas = tower_wt::list_metas(state_dir).unwrap_or_default();
    let boards: Vec<Board> = tower_state::BoardStore::new(state_dir)
        .ok()
        .map(|store| {
            store
                .repos()
                .unwrap_or_default()
                .iter()
                .filter_map(|repo| store.load(repo, now).ok())
                .collect()
        })
        .unwrap_or_default();
    let links = tower_state::TreeStore::new(state_dir).read();
    parents(agents, &metas, &boards, &links)
}

/// `herdr-tower tree`'s usage.
pub const TREE_USAGE: &str = "\
usage: herdr-tower tree [show]                 who works under whom, as the tower draws it
       herdr-tower tree link <worker> [<coordinator>]
                                               put a pane under a coordinator (this pane when
                                               none is named); a worker in another workspace
                                               is moved into a new tab of the coordinator's
       herdr-tower tree unlink <worker>        take a link away
  Panes are named by id, as the tower shows them: w2:pBR.
";

/// What `tree link` is to do, worked out before anything is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPlan {
    pub worker: String,
    pub coordinator: String,
    /// The coordinator's workspace, when the worker is in another one and
    /// has to be moved there first.
    pub move_to: Option<String>,
}

/// Checks `tree link`'s panes against the live records on `backend`: both
/// hold a live agent, they are not the same pane, and the coordinator does
/// not already work under the worker (a loop draws nothing).
pub fn plan_link(
    agents: &[Agent],
    parents: &BTreeMap<String, String>,
    backend: &str,
    worker: &str,
    coordinator: &str,
) -> Result<LinkPlan, String> {
    let live = |pane: &str| {
        agents
            .iter()
            .find(|a| a.is_live() && a.pane.backend == backend && a.pane.pane_id == pane)
    };
    let w = live(worker).ok_or_else(|| format!("no agent is running in pane {worker}"))?;
    let c =
        live(coordinator).ok_or_else(|| format!("no agent is running in pane {coordinator}"))?;
    if w.id == c.id {
        return Err(format!("{worker} cannot work under itself"));
    }
    // Walk up from the coordinator: meeting the worker means a loop.
    let mut up = tower_mux::target_key(backend, coordinator);
    for _ in 0..16 {
        let Some(next) = parents.get(&up) else { break };
        if *next == tower_mux::target_key(backend, worker) {
            return Err(format!("{coordinator} already works under {worker}"));
        }
        up = next.clone();
    }
    let move_to = (tower_state::group_key(w) != tower_state::group_key(c)
        && !c.pane.workspace_id.is_empty())
    .then(|| c.pane.workspace_id.clone());
    Ok(LinkPlan {
        worker: worker.to_string(),
        coordinator: coordinator.to_string(),
        move_to,
    })
}

/// `tree show`: one line per worker, grouped under its coordinator.
pub fn show_text(parents: &BTreeMap<String, String>) -> String {
    if parents.is_empty() {
        return "no worker is known to work under anybody — the tower draws each tab on its own\n"
            .to_string();
    }
    let mut by_coordinator: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (worker, coordinator) in parents {
        by_coordinator.entry(coordinator).or_default().push(worker);
    }
    let mut out = String::new();
    for (coordinator, workers) in by_coordinator {
        out.push_str(&format!("{}\n", tower_state::clean_text(coordinator)));
        for (i, w) in workers.iter().enumerate() {
            let branch = if i + 1 == workers.len() {
                "└──"
            } else {
                "├──"
            };
            out.push_str(&format!("  {branch} {}\n", tower_state::clean_text(w)));
        }
    }
    out
}

/// A board's advisors, likeliest first: the pane it wakes on review, then
/// the ones that opened its board pane, latest first — each while the agent
/// that did it is still there.
fn advisors<'a>(
    b: &Board,
    in_pane: &impl Fn(&str, &str, Option<OffsetDateTime>) -> Option<&'a Agent>,
) -> Vec<&'a Agent> {
    let mut out: Vec<&Agent> = Vec::new();
    if let Some(w) = &b.waker {
        if let Some(a) = in_pane(&w.backend, &w.pane_id, w.born_at)
            .filter(|a| w.agent_id.is_empty() || a.id == w.agent_id)
        {
            out.push(a);
        }
    }
    let mut claims: Vec<_> = b.panes.iter().collect();
    claims.sort_by_key(|c| std::cmp::Reverse(c.at));
    for a in claims
        .into_iter()
        .filter_map(|c| in_pane(&c.backend, &c.caller_pane, Some(c.at)))
    {
        if !out.iter().any(|o| o.id == a.id) {
            out.push(a);
        }
    }
    out
}

fn key(a: &Agent) -> String {
    tower_mux::target_key(&a.pane.backend, &a.pane.pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_state::AgentState;

    fn t(min: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(min)
    }

    fn agent(pane: &str, cwd: &str, born: i64) -> Agent {
        let mut a = Agent::new(format!("claude-{pane}"), "claude", t(born));
        a.pane.pane_id = pane.into();
        a.pane.workspace_id = pane.split(':').next().unwrap().into();
        a.cwd = cwd.into();
        a.state = AgentState::Idle;
        a.born_at = Some(t(born));
        a
    }

    fn meta(path: &str, parent: &str, created: i64) -> Meta {
        Meta {
            task: path.rsplit('/').next().unwrap().into(),
            repo: "/r".into(),
            base: "main".into(),
            branch: "agent/x".into(),
            path: path.into(),
            agent: "claude".into(),
            test_cmd: String::new(),
            test_pass: None,
            test_at: None,
            created_at: t(created),
            drive: None,
            parent: Some(tower_wt::Parent {
                backend: String::new(),
                pane_id: parent.into(),
            }),
        }
    }

    fn link(worker: &str, coordinator: &str, at: i64) -> TreeLink {
        TreeLink {
            backend: String::new(),
            worker: worker.into(),
            coordinator: coordinator.into(),
            at: t(at),
        }
    }

    fn pairs(m: &BTreeMap<String, String>) -> Vec<(&str, &str)> {
        m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }

    #[test]
    fn a_task_is_drawn_under_the_pane_that_made_it() {
        let agents = [agent("w4:pC", "/r", 0), agent("w4:pY", "/wt/i-1", 5)];
        let got = parents(&agents, &[meta("/wt/i-1", "w4:pC", 3)], &[], &[]);
        assert_eq!(pairs(&got), [("w4:pY", "w4:pC")]);
    }

    #[test]
    fn a_pane_reused_since_is_somebody_else() {
        // The coordinator's pane now holds an agent born after the task.
        let agents = [agent("w4:pC", "/r", 9), agent("w4:pY", "/wt/i-1", 5)];
        assert!(parents(&agents, &[meta("/wt/i-1", "w4:pC", 3)], &[], &[]).is_empty());
        // So does a link's worker pane.
        let agents = [agent("w4:pC", "/r", 0), agent("w4:pY", "/r", 9)];
        assert!(parents(&agents, &[], &[], &[link("w4:pY", "w4:pC", 5)]).is_empty());
    }

    #[test]
    fn a_link_wins_and_a_branch_never_crosses_a_workspace() {
        let agents = [
            agent("w4:pC", "/r", 0),
            agent("w4:pD", "/r", 0),
            agent("w4:pY", "/wt/i-1", 1),
            agent("w7:pK", "/r", 0),
        ];
        let metas = [meta("/wt/i-1", "w4:pC", 3)];
        let got = parents(&agents, &metas, &[], &[link("w4:pY", "w4:pD", 5)]);
        assert_eq!(pairs(&got), [("w4:pY", "w4:pD")]);
        let got = parents(&agents, &metas, &[], &[link("w4:pY", "w7:pK", 5)]);
        assert_eq!(pairs(&got), [("w4:pY", "w4:pC")], "w7 is another workspace");
        let got = parents(&agents, &[], &[], &[link("w4:pC", "w4:pC", 5)]);
        assert!(got.is_empty(), "never under itself");
    }

    #[test]
    fn a_boards_advisor_takes_its_workers() {
        let agents = [
            agent("w4:pC", "/r", 0),
            agent("w4:pS", "/wt/i-2", 1),
            agent("w4:pZ", "/r", 1),
        ];
        let mut b = Board::new("/r");
        b.add(&["a".into(), "b".into()], t(2)).unwrap();
        b.items[0].worktree = "/wt/i-2".into();
        b.items[0].task = "i-2".into();
        b.assign(
            2,
            tower_state::AssignTo::Send(tower_state::SendTo::Pane(tower_state::PaneWorker {
                backend: String::new(),
                pane_id: "w4:pZ".into(),
                agent_id: "claude-w4:pZ".into(),
                session_id: String::new(),
                born_at: Some(t(1)),
            })),
        )
        .unwrap();
        b.claim_pane("", "w4:pC", 1, t(3));
        let got = parents(&agents, &[], &[b], &[]);
        assert_eq!(pairs(&got), [("w4:pS", "w4:pC"), ("w4:pZ", "w4:pC")]);
    }

    // ── edge cases ───────────────────────────────────────────────────────

    #[test]
    fn dead_agents_and_shared_worktrees_place_nothing() {
        let mut dead = agent("w4:pC", "/r", 0);
        dead.state = AgentState::Dead;
        let metas = [meta("/wt/i-1", "w4:pC", 3)];
        let got = parents(&[dead, agent("w4:pY", "/wt/i-1", 5)], &metas, &[], &[]);
        assert!(got.is_empty(), "a dead coordinator is nobody");
        // Two live agents in the task's worktree: which is its worker is unknown.
        let agents = [
            agent("w4:pC", "/r", 0),
            agent("w4:pY", "/wt/i-1", 5),
            agent("w4:pX", "/wt/i-1", 5),
        ];
        assert!(parents(&agents, &metas, &[], &[]).is_empty());
        // No record of the parent at all: an old task.
        let mut old = meta("/wt/i-1", "w4:pC", 3);
        old.parent = None;
        let agents = [agent("w4:pC", "/r", 0), agent("w4:pY", "/wt/i-1", 5)];
        assert!(parents(&agents, &[old], &[], &[]).is_empty());
        assert!(parents(&[], &[], &[], &[]).is_empty(), "nothing at all");
    }

    #[test]
    fn a_board_wakes_its_advisor_first_then_one_in_the_workers_workspace() {
        let agents = [
            agent("w4:pC", "/r", 0),
            agent("w2:pA", "/r", 0),
            agent("w2:pW", "/wt/i-2", 1),
            agent("w4:pV", "/wt/i-3", 1),
        ];
        let mut b = Board::new("/r");
        b.add(&["x".into(), "y".into()], t(2)).unwrap();
        b.items[0].worktree = "/wt/i-2".into();
        b.items[1].worktree = "/wt/i-3".into();
        b.claim_pane("", "w2:pA", 1, t(3));
        b.claim_pane("", "w4:pC", 1, t(4));
        let got = parents(&agents, &[], &[b.clone()], &[]);
        assert_eq!(
            pairs(&got),
            [("w2:pW", "w2:pA"), ("w4:pV", "w4:pC")],
            "each worker under the advisor in its own workspace"
        );
        // The pane the board wakes comes first, where it can.
        b.waker = Some(tower_state::PaneWorker {
            backend: String::new(),
            pane_id: "w4:pC".into(),
            agent_id: "claude-w4:pC".into(),
            session_id: String::new(),
            born_at: Some(t(0)),
        });
        b.panes.clear();
        let got = parents(&agents, &[], &[b.clone()], &[]);
        assert_eq!(
            pairs(&got),
            [("w4:pV", "w4:pC")],
            "w2's worker has no advisor in w2"
        );
        // A waker whose pane now holds another agent is not the advisor.
        b.waker.as_mut().unwrap().agent_id = "codex-w4:pC".into();
        assert!(parents(&agents, &[], &[b], &[]).is_empty());
    }

    #[test]
    fn plan_link_refuses_what_cannot_be_drawn() {
        let agents = [
            agent("w4:pC", "/r", 0),
            agent("w4:pY", "/r", 0),
            agent("w7:pK", "/r", 0),
        ];
        let none = BTreeMap::new();
        // (name, worker, coordinator, Ok(the workspace it moves to) or the refusal)
        type Case<'a> = (&'a str, &'a str, &'a str, Result<Option<&'a str>, &'a str>);
        let cases: [Case; 5] = [
            ("same workspace", "w4:pY", "w4:pC", Ok(None)),
            ("another workspace moves", "w4:pY", "w7:pK", Ok(Some("w7"))),
            (
                "no such worker",
                "w4:p9",
                "w4:pC",
                Err("no agent is running in pane w4:p9"),
            ),
            (
                "no such coordinator",
                "w4:pY",
                "",
                Err("no agent is running in pane "),
            ),
            (
                "itself",
                "w4:pC",
                "w4:pC",
                Err("w4:pC cannot work under itself"),
            ),
        ];
        for (name, worker, coordinator, want) in cases {
            let got = plan_link(&agents, &none, "", worker, coordinator);
            match want {
                Ok(move_to) => {
                    let plan = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                    assert_eq!(plan.move_to.as_deref(), move_to, "{name}");
                    assert_eq!(
                        (plan.worker.as_str(), plan.coordinator.as_str()),
                        (worker, coordinator)
                    );
                }
                Err(e) => assert_eq!(got.unwrap_err(), e, "{name}"),
            }
        }
        // Another backend's pane of the same id is not this one.
        assert!(plan_link(&agents, &none, "b0123456789abcdef", "w4:pY", "w4:pC").is_err());
        // A loop, however long, is refused.
        let chain: BTreeMap<String, String> = [("w4:pC".to_string(), "w4:pY".to_string())].into();
        assert_eq!(
            plan_link(&agents, &chain, "", "w4:pY", "w4:pC").unwrap_err(),
            "w4:pC already works under w4:pY"
        );
    }

    #[test]
    fn show_text_groups_workers_under_each_coordinator() {
        assert!(show_text(&BTreeMap::new()).starts_with("no worker is known"));
        let m: BTreeMap<String, String> = [
            ("w4:pY", "w4:pC"),
            ("w4:pZ", "w4:pC"),
            ("w7:pN", "w7:pK"),
            ("w4:p\x1b[2J", "w4:pC"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let text = show_text(&m);
        assert!(
            !text.contains('\x1b'),
            "a pane id cannot move the cursor: {text:?}"
        );
        assert!(
            text.contains("w4:pC\n  ├── w4:p[2J\n  ├── w4:pY\n  └── w4:pZ\nw7:pK\n  └── w7:pN\n"),
            "{text}"
        );
    }

    // ── property and cross-check ─────────────────────────────────────────

    /// Over generated panes and links (seeded): every relation is between two
    /// live agents of one workspace, never a pane under itself — and with
    /// links alone it is exactly what a naive reading of those rules gives.
    #[test]
    fn links_alone_agree_with_a_naive_reading() {
        let mut rng = tower_testrng::Rng::new(0x7EE_5EED);
        let panes = ["w1:p1", "w1:p2", "w1:p3", "w2:p1", "w2:p2"];
        for round in 0..400 {
            let mut agents: Vec<Agent> = Vec::new();
            for p in panes {
                if rng.below(5) == 0 {
                    continue;
                }
                let mut a = agent(p, "/r", rng.below(10) as i64);
                if rng.below(6) == 0 {
                    a.state = AgentState::Dead;
                }
                agents.push(a);
            }
            let links: Vec<TreeLink> = (0..rng.below(6))
                .map(|_| {
                    link(
                        rng.pick(&panes).unwrap(),
                        rng.pick(&panes).unwrap(),
                        rng.below(10) as i64,
                    )
                })
                .collect();
            let got = parents(&agents, &[], &[], &links);

            let fine = |pane: &str, at: OffsetDateTime| {
                agents.iter().find(|a| {
                    a.is_live() && a.pane.pane_id == pane && a.born_at.is_none_or(|b| b <= at)
                })
            };
            let mut naive = BTreeMap::new();
            for l in &links {
                if let (Some(w), Some(c)) = (fine(&l.worker, l.at), fine(&l.coordinator, l.at)) {
                    if w.id != c.id && w.pane.workspace_id == c.pane.workspace_id {
                        naive.insert(l.worker.clone(), l.coordinator.clone());
                    }
                }
            }
            assert_eq!(got, naive, "round {round}");
            for (w, c) in &got {
                assert_ne!(w, c, "round {round}");
                assert_eq!(w.split(':').next(), c.split(':').next(), "round {round}");
            }
        }
    }

    /// What `plan_link` accepts and `parents` then reads back from the link
    /// is the same pair: planned, linked, drawn.
    #[test]
    fn a_planned_link_reads_back_as_planned() {
        let agents = [agent("w4:pC", "/r", 0), agent("w4:pY", "/r", 1)];
        let plan = plan_link(&agents, &BTreeMap::new(), "", "w4:pY", "w4:pC").unwrap();
        let l = link(&plan.worker, &plan.coordinator, 2);
        let got = parents(&agents, &[], &[], &[l]);
        assert_eq!(pairs(&got), [("w4:pY", "w4:pC")]);
        let again = plan_link(&agents, &got, "", "w4:pY", "w4:pC").unwrap();
        assert_eq!(again, plan, "linking again is the same plan");
    }
}
