// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Putting the tower's numbers onto herdr's own sidebar.
//!
//! This is the one place the two tools complement rather than overlap: herdr
//! knows the agent kind and lifecycle state, the tower knows context usage,
//! the model answering, the worktree branch, and whether a Discord bot drives
//! the session. So the tower lays its half onto the surface the user is
//! already looking at.

use std::collections::HashMap;
use std::time::Duration;

use herdrx::{Herdr, Metadata, TOKEN_BRANCH, TOKEN_CTX, TOKEN_MODEL, TOKEN_WIRED};
use tower_state::{Agent, AgentState};
use tower_usage::CtxInfo;

/// How long a planted value lives if nothing refreshes it.
///
/// Three times the five-minute card interval. Long enough that skipping a
/// refresh or two does not make the number blink, short enough that a tower
/// which died at lunchtime is not still claiming `ctx 14%` at midnight — and
/// nothing else is coming along to clear it.
pub const DECORATE_TTL: Duration = Duration::from_secs(15 * 60);

/// The sidebar tokens for one agent.
///
/// Values carry their own label (`ctx 14%`, `⎇ main`) because herdr's config
/// references them as a bare `{ token = "$ctx" }`; a naked number in the
/// sidebar is a number nobody can identify. Every token is always present:
/// an empty value is herdr's "delete this token", so a value that disappears
/// takes its sidebar slot with it instead of freezing at the last reading.
pub fn agent_tokens(info: &CtxInfo, wired_mark: &str, branch: &str) -> HashMap<String, String> {
    let mut tokens: HashMap<String, String> = herdrx::ALL_TOKENS
        .iter()
        .map(|t| (t.to_string(), String::new()))
        .collect();

    if let Some(pct) = info.used_pct {
        // `UsedPct::label` already carries the `~` an estimate has to wear —
        // the same rule the TUI badge follows, so the two never disagree
        // about whether a figure was measured.
        tokens.insert(TOKEN_CTX.into(), format!("ctx {}", pct.label()));
    }
    if !info.model.is_empty() {
        tokens.insert(TOKEN_MODEL.into(), info.model.clone());
    }
    if !wired_mark.is_empty() {
        tokens.insert(TOKEN_WIRED.into(), wired_mark.to_string());
    }
    if !branch.is_empty() {
        tokens.insert(TOKEN_BRANCH.into(), format!("⎇ {branch}"));
    }
    tokens
}

/// Which panes get decorated, and with what.
///
/// Separated from the sending so the selection rules can be checked without a
/// server: a record with no coordinate has nowhere to plant anything, and a
/// DEAD record's coordinate belongs to whatever is there now — writing to it
/// would label a stranger's pane with a dead agent's context usage.
pub fn decoration_plan(
    agents: &[Agent],
    ctx: &HashMap<String, CtxInfo>,
    wired: &HashMap<String, String>,
    branches: &HashMap<String, String>,
) -> Vec<Metadata> {
    let empty = CtxInfo::default();
    agents
        .iter()
        .filter(|a| !a.pane.pane_id.is_empty() && a.state != AgentState::Dead)
        .map(|a| Metadata {
            pane_id: a.pane.pane_id.clone(),
            tokens: agent_tokens(
                ctx.get(&a.id).unwrap_or(&empty),
                // The ⌁ mark is decided per session, not per folder: a working
                // session opened in a bot's folder is not the bot.
                wired.get(&a.pane.session).map_or("", String::as_str),
                branches.get(&a.cwd).map_or("", String::as_str),
            ),
            ttl: Some(DECORATE_TTL),
            ..Metadata::default()
        })
        .collect()
}

/// Plants the plan, and reports how many panes took it.
///
/// Failures are swallowed. This is display, not supervision — a sidebar that
/// refuses an update must not stop a card from being posted or a status table
/// from being printed. The count comes back so a caller that wants to say
/// something about it can.
pub fn decorate_agents(
    h: &Herdr,
    agents: &[Agent],
    ctx: &HashMap<String, CtxInfo>,
    wired: &HashMap<String, String>,
    branches: &HashMap<String, String>,
) -> usize {
    decoration_plan(agents, ctx, wired, branches)
        .iter()
        .filter(|m| h.report_metadata(m).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use tower_usage::UsedPct;

    fn agent(
        id: &str,
        kind: &str,
        state: AgentState,
        session: &str,
        pane: &str,
        cwd: &str,
    ) -> Agent {
        let mut a = Agent::new(id, kind, OffsetDateTime::UNIX_EPOCH);
        a.state = state;
        a.pane.session = session.into();
        a.pane.pane_id = pane.into();
        a.cwd = cwd.into();
        a
    }

    fn fixture() -> Vec<Agent> {
        vec![
            agent(
                "claude-1",
                "claude",
                AgentState::Working,
                "dev",
                "w2:p1",
                "/proj",
            ),
            agent("codex-2", "codex", AgentState::Idle, "dev", "w2:p2", "/wt"),
            // Neither of these is a target: the first's coordinate belongs to
            // whatever lives there now, the second has no coordinate at all.
            agent("claude-3", "claude", AgentState::Dead, "dev", "w2:p3", ""),
            agent("claude-4", "claude", AgentState::Idle, "", "", ""),
        ]
    }

    fn plan_for(id: &str, plan: &[Metadata], agents: &[Agent]) -> HashMap<String, String> {
        let pane = &agents.iter().find(|a| a.id == id).unwrap().pane.pane_id;
        plan.iter()
            .find(|m| &m.pane_id == pane)
            .unwrap_or_else(|| panic!("{id} is not in the plan"))
            .tokens
            .clone()
    }

    #[test]
    fn every_value_carries_its_own_label() {
        let info = CtxInfo {
            model: "Opus 5".into(),
            used_pct: Some(UsedPct::Exact(14.0)),
            ts: None,
        };
        let got = agent_tokens(&info, "⌁테스트방", "hero-bold");
        assert_eq!(got[TOKEN_CTX], "ctx 14%");
        assert_eq!(got[TOKEN_MODEL], "Opus 5");
        assert_eq!(got[TOKEN_WIRED], "⌁테스트방");
        assert_eq!(got[TOKEN_BRANCH], "⎇ hero-bold");
    }

    /// An estimate wears a `~`, so a reconstructed percentage is never read as
    /// a measured one.
    #[test]
    fn an_estimated_percentage_is_marked_as_one() {
        let info = CtxInfo {
            used_pct: Some(UsedPct::approx(8.0)),
            ..CtxInfo::default()
        };
        assert_eq!(agent_tokens(&info, "", "")[TOKEN_CTX], "ctx ~8%");
    }

    /// A value that goes away has to be sent as empty, not omitted: herdr
    /// keeps the last value it was told, so an omitted token stays on screen
    /// forever.
    #[test]
    fn a_value_that_disappeared_is_sent_as_a_deletion() {
        let got = agent_tokens(&CtxInfo::default(), "", "");
        assert_eq!(
            got.len(),
            herdrx::ALL_TOKENS.len(),
            "every token has to be sent for the missing ones to clear: {got:?}"
        );
        assert!(got.values().all(String::is_empty), "{got:?}");
    }

    #[test]
    fn dead_and_coordinateless_records_are_left_alone() {
        let agents = fixture();
        let plan = decoration_plan(&agents, &HashMap::new(), &HashMap::new(), &HashMap::new());
        let panes: Vec<&str> = plan.iter().map(|m| m.pane_id.as_str()).collect();
        assert_eq!(panes, ["w2:p1", "w2:p2"], "{panes:?}");
    }

    #[test]
    fn each_agents_own_context_branch_and_mark_reach_its_own_pane() {
        let agents = fixture();
        let ctx = HashMap::from([
            (
                "claude-1".to_string(),
                CtxInfo {
                    model: "Opus 5".into(),
                    used_pct: Some(UsedPct::Exact(14.0)),
                    ts: None,
                },
            ),
            (
                "codex-2".to_string(),
                CtxInfo {
                    model: "gpt-5".into(),
                    ..CtxInfo::default()
                },
            ),
        ]);
        let wired = HashMap::from([("dev".to_string(), "⌁".to_string())]);
        let branches = HashMap::from([("/wt".to_string(), "wingman-2b".to_string())]);

        let plan = decoration_plan(&agents, &ctx, &wired, &branches);
        assert_eq!(plan_for("claude-1", &plan, &agents)[TOKEN_CTX], "ctx 14%");
        assert_eq!(
            plan_for("codex-2", &plan, &agents)[TOKEN_BRANCH],
            "⎇ wingman-2b"
        );
    }

    /// The ⌁ is a property of the session, so both records in one session wear
    /// it — including the one whose own folder has no bot wiring.
    #[test]
    fn the_bot_mark_reaches_every_record_in_the_marked_session() {
        let agents = fixture();
        let wired = HashMap::from([("dev".to_string(), "⌁".to_string())]);
        let plan = decoration_plan(&agents, &HashMap::new(), &wired, &HashMap::new());
        for id in ["claude-1", "codex-2"] {
            assert_eq!(plan_for(id, &plan, &agents)[TOKEN_WIRED], "⌁", "{id}");
        }
    }

    /// Without a TTL, a tower that dies strands its last reading on the
    /// sidebar and nothing else will ever clear it.
    #[test]
    fn every_planted_value_can_expire_on_its_own() {
        let agents = fixture();
        let plan = decoration_plan(&agents, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert!(!plan.is_empty());
        for m in &plan {
            assert_eq!(m.ttl, Some(DECORATE_TTL), "{}", m.pane_id);
            assert!(m.validate().is_ok(), "{}: {:?}", m.pane_id, m.validate());
        }
    }
}
