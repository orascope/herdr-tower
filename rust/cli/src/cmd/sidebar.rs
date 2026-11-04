// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The supervisor's thread that keeps herdr's sidebar decorated.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use tower_state::Agent;

use super::common::*;
use super::Result;

/// How often the sidebar's context, model and branch are refreshed.
pub const DECORATE_INTERVAL: Duration = Duration::from_secs(60);

/// Plants the sidebar's values every [`DECORATE_INTERVAL`] for as long as the
/// supervisor runs.
///
/// Reads the records the supervisor's own pass keeps current rather than scanning
/// panes a second time: this thread never writes a record, so it cannot race the
/// pass over one. A sandboxed store's records are not the user's panes, so a test
/// supervisor leaves them alone.
///
/// A failure is logged when it first happens and when it clears, not every minute
/// while it lasts, into a log nothing trims.
pub fn decorate_timer() {
    if store_is_sandboxed() {
        return;
    }
    let mut cache = tower_usage::CtxCache::default();
    let mut last: Option<String> = None;
    loop {
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            decorate_once(&mut cache).map_err(|e| e.to_string())
        }));
        let now = result
            .unwrap_or_else(|_| Err("the sidebar pass failed unexpectedly".into()))
            .err();
        if let Some(line) = log_line(last.as_deref(), now.as_deref()) {
            eprintln!("herdr-tower decorate: {line}");
        }
        last = now;
        std::thread::sleep(DECORATE_INTERVAL);
    }
}

/// One refresh, of this supervisor's default backend's live agents.
fn decorate_once(cache: &mut tower_usage::CtxCache) -> Result<()> {
    let agents = store()?.list()?;
    let agents = on_backend(agents, &tower_cli::muxsel::default_backend_id());
    let home = home()?;
    let ctx = tower_usage::agent_ctx_cached(
        &agents,
        &tower_usage::codex_sessions_root(&home),
        &tower_usage::gemini_dir(&home),
        &tower_usage::claude_dir(&home),
        cache,
    );
    let branches = tower_wt::live_branches(&tower_state::default_dir(), &folders_of(&agents));
    let h = tower_cli::muxsel::default_mux();
    tower_cli::decorate::decorate_agents(&h, &agents, &ctx, &branches);
    Ok(())
}

/// The live agents whose coordinates belong to `backend`.
///
/// A pane ID is unique only inside one backend, so another's `w2:p1` here would
/// label a stranger's pane. A record with no backend predates the field and was
/// written by the default one.
fn on_backend(agents: Vec<Agent>, backend: &str) -> Vec<Agent> {
    agents
        .into_iter()
        .filter(|a| a.is_live() && (a.pane.backend.is_empty() || a.pane.backend == backend))
        .collect()
}

fn folders_of(agents: &[Agent]) -> HashSet<&str> {
    agents.iter().map(|a| a.cwd.as_str()).collect()
}

/// What the timer says after a pass, given the error before it and the error
/// now: a new or changed failure, a recovery, or nothing.
fn log_line(before: Option<&str>, now: Option<&str>) -> Option<String> {
    match (before, now) {
        (_, Some(e)) if before != Some(e) => Some(format!("not decorated: {e}")),
        (Some(_), None) => Some("decorating again".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use tower_state::AgentState;

    fn agent(id: &str, backend: &str, cwd: &str, state: AgentState) -> Agent {
        let mut a = Agent::new(id, "claude", OffsetDateTime::UNIX_EPOCH);
        a.pane.backend = backend.into();
        a.pane.pane_id = "w1:p1".into();
        a.cwd = cwd.into();
        a.state = state;
        a
    }

    #[test]
    fn a_failure_is_logged_once_and_so_is_its_recovery() {
        assert_eq!(log_line(None, None), None);
        assert_eq!(
            log_line(None, Some("store")).as_deref(),
            Some("not decorated: store")
        );
        assert_eq!(log_line(Some("store"), Some("store")), None);
        assert_eq!(
            log_line(Some("store"), Some("home")).as_deref(),
            Some("not decorated: home")
        );
        assert_eq!(
            log_line(Some("store"), None).as_deref(),
            Some("decorating again")
        );
    }

    /// Dead records and other backends' panes are neither labelled nor asked
    /// about: the folders handed on are what the branch reader may touch.
    #[test]
    fn only_this_backends_live_agents_are_decorated_or_read() {
        let agents = vec![
            agent("mine", "local", "/here", AgentState::Working),
            agent("old-record", "", "/older", AgentState::Idle),
            agent("dead", "local", "/volume/finished", AgentState::Dead),
            agent("theirs", "other", "/volume/theirs", AgentState::Working),
        ];
        let kept = on_backend(agents, "local");
        let ids: Vec<&str> = kept.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["mine", "old-record"]);
        assert_eq!(folders_of(&kept), HashSet::from(["/here", "/older"]));
    }

    /// A refresh or two can be missed before a value expires and blinks.
    #[test]
    fn the_interval_is_shorter_than_the_values_life() {
        assert!(DECORATE_INTERVAL * 3 <= tower_cli::decorate::DECORATE_TTL);
    }
}
