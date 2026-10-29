// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Assembling the status card: the agents the tower supervises, as one
//! section, and the time it was updated.
//!
//! Everything here is pure: data in, [`Component`]s out. No clock, no files, no
//! network. `now` is a parameter for the same reason the rest of the crates take
//! one — a renderer that reads the clock cannot be pinned by a test, and this
//! one is pinned byte for byte against a recorded snapshot.

use std::collections::HashMap;

use time::{Duration, OffsetDateTime};
use tower_state::{Agent, AgentState, NO_WORKSPACE};
use tower_usage::{is_fable, pretty_model, CtxInfo, DefaultModels};

use crate::json::Component;

/// Everything one card is built from.
#[derive(Debug, Clone, Default)]
pub struct CardData {
    /// In store order, which groups by kind — the kind separators below assume
    /// it, and an unsorted list would draw one separator per row.
    pub agents: Vec<Agent>,
    /// Agent **ID** to model/context snapshot. Keyed by ID, not by folder: two
    /// agents of different kinds in one folder used to wear each other's model.
    pub ctx: HashMap<String, CtxInfo>,
    /// Session name to the Discord-bot mark (`⌁`, or `⌁` plus a channel label).
    pub wired: HashMap<String, String>,
    /// Working folder to worktree branch, shown as `⎇`.
    pub branches: HashMap<String, String>,
    /// The default model each CLI has pinned. `None` omits the whole line — see
    /// `default_models_line`.
    pub def_models: Option<DefaultModels>,
    /// Home directory, for abbreviating paths to `~/…`. Empty disables it.
    pub home: String,
}

fn state_emoji(s: AgentState) -> &'static str {
    match s {
        AgentState::Working => "🟢",
        AgentState::Waiting => "🟡",
        AgentState::DoneUnread => "🟠",
        AgentState::Error => "🔴",
        AgentState::Idle => "⚪",
        AgentState::Dead => "⚫",
    }
}

fn state_word(s: AgentState) -> &'static str {
    match s {
        AgentState::Working => "working",
        AgentState::Waiting => "wait",
        AgentState::DoneUnread => "new (unread)",
        AgentState::Error => "error",
        AgentState::Idle => "idle",
        AgentState::Dead => "gone",
    }
}

/// Turns `#RRGGBB` into the integer Discord wants.
///
/// Leading hex digits only, and 0 for anything unparseable. Zero is black,
/// which is a visible-but-harmless wrong answer; the alternative (refusing to
/// render) loses the whole card over a typo in a colour.
fn accent(hex: &str) -> u32 {
    let s = hex.strip_prefix('#').unwrap_or(hex);
    let digits: String = s.chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&digits, 16).unwrap_or(0)
}

fn agents_container(d: &CardData, now: OffsetDateTime) -> Option<Component> {
    let shorten = |p: &str| -> String {
        if !d.home.is_empty() && p.starts_with(&d.home) {
            format!("~{}", &p[d.home.len()..])
        } else {
            p.to_string()
        }
    };

    let mut lines: Vec<String> = Vec::new();
    let mut worst: Option<f64> = None;
    let mut shown = 0usize;

    // The heading is the workspace, matching the tower's own list. It used to be
    // the vendor, and once the store grouped by workspace the vendor stopped
    // forming one run down the card — the same heading came back three rows
    // later, which is a card nobody can read as a list.
    //
    // Grouped here rather than by watching for a change between neighbours.
    // `agents` is a public field and the in-repo publisher happens to hand it
    // `Store::list`, already workspace-sorted; a caller that fills it any other
    // way would get one workspace's heading printed twice. Unlike the tower's
    // list there is no cursor indexing into this, so grouping a second time
    // cannot disagree with anything. Each group keeps the order it arrived in,
    // and the groups keep the order they first appear in, so a workspace-sorted
    // input renders exactly as it did.
    let mut groups: Vec<(tower_state::GroupKey<'_>, Vec<&Agent>)> = Vec::new();
    for a in d.agents.iter().filter(|a| a.state != AgentState::Dead) {
        let key = tower_state::group_key(a);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(a),
            None => groups.push((key, vec![a])),
        }
    }

    for (_, members) in &groups {
        let name = match members[0].pane.session.as_str() {
            "" => NO_WORKSPACE,
            s => s,
        };
        lines.push(format!("-# ── {name} ──────"));
        for a in members {
            shown += 1;
            let info = d.ctx.get(&a.id).cloned().unwrap_or_default();
            if let Some(u) = info.used_pct {
                if worst.is_none_or(|w| u.value() > w) {
                    worst = Some(u.value());
                }
            }

            let mut word = state_word(a.state).to_string();
            if a.stale(now) {
                // WORKING but long unheard from — a lost hook is likely. The TUI
                // marks the same condition `WORK?`; without the mark a wedged agent
                // is indistinguishable from a busy one.
                word.push('?');
            }
            // The vendor takes the bold slot the workspace had, because the heading
            // above now carries the workspace and a row that repeated it said the
            // same word twice while the vendor went unnamed.
            let mut line = format!("{} **{}**", state_emoji(a.state), a.kind);
            line += &format!(" `{}` — {word}", shorten(&a.cwd));
            if a.state != AgentState::Idle {
                line += &format!(" {}", since(a.state_since, now));
            }
            if let Some(w) = d.wired.get(&a.pane.session).filter(|w| !w.is_empty()) {
                line += &format!(" · {w}");
            }
            if let Some(br) = d.branches.get(&a.cwd).filter(|b| !b.is_empty()) {
                line += &format!(" · ⎇ {br}");
            }
            lines.push(line);

            let mut sub: Vec<String> = Vec::new();
            if !info.model.is_empty() {
                sub.push(info.model.clone());
            }
            if let Some(u) = info.used_pct {
                // Rounded, not truncated: this is the only number on the row a
                // person acts on, and `ctx 15%` for 15.9 reads as more headroom
                // than there is. The `~` marks a figure that was estimated rather
                // than measured — `UsedPct` carries which.
                sub.push(format!("ctx {}{}%", u.marker(), (u.value() + 0.5) as i64));
            }
            if let Some(ts) = info.ts {
                sub.push(since(ts, now));
            }
            // Read on a phone: no detail view behind it, no key to press, and the
            // row above carries the session, folder, state, wiring and branch but
            // not the name.
            let headline = a.question_first();
            if !headline.is_empty() {
                sub.push(truncate_chars(headline, 40));
            }
            if !sub.is_empty() {
                lines.push(format!("-# {}", sub.join(" · ")));
            }
        }
    }
    if shown == 0 {
        return None;
    }

    let mut head = format!("### Agents — {}", summary_line(&d.agents));
    let models = default_models_line(d.def_models.as_ref());
    if !models.is_empty() {
        head += &format!("\n{models}");
    }

    // The section's colour tracks the *fullest* context window, because that is
    // the one about to force a compaction.
    let color = match worst {
        Some(w) if w >= 80.0 => "#F04747",
        Some(w) if w >= 40.0 => "#FAA61A",
        Some(_) => "#43B581",
        None => "#565B66",
    };
    Some(Component::Container {
        accent_color: accent(color),
        components: vec![Component::text(head), Component::text(fit(lines, shown))],
    })
}

/// Discord's cap on one text component's content.
///
/// A component that exceeds it is refused with a 400, and [`crate::webhook`]
/// answers a 400 by posting a *new* card — the same oversized one — so a card
/// that grows past this once stops updating for good rather than dropping a row.
const TEXT_MAX: usize = 4000;

/// Headroom under it. The rows are built before anything knows how long the
/// closing line will be, and a budget that lands exactly on the limit has no
/// room for the line that says what was dropped.
const TEXT_BUDGET: usize = 3600;

/// The agent rows that fit, and a line saying how many did not.
///
/// Nothing bounded this text before, and grouping by workspace is what made the
/// bound necessary: an installation of one-agent workspaces now spends a whole
/// heading per agent where it used to spend one per vendor, so a card that fit
/// yesterday can exceed the limit today.
///
/// Cut at a row boundary, and never leaving a heading with nothing under it: a
/// workspace named and then not shown is worse than not naming it. The rows are
/// already ordered with what needs a person first, so what goes is what matters
/// least.
fn fit(lines: Vec<String>, shown: usize) -> String {
    // An agent is admitted with its detail line or not at all. Cutting between
    // the two kept a row and dropped the model, the context figure, the age and
    // the task under it — and the tally below would still have counted that
    // agent as shown, so nothing said the visible row had lost half of itself.
    let mut used = 0usize;
    let mut kept: Vec<String> = Vec::new();
    let mut unit: Vec<String> = Vec::new();
    let mut cost = 0usize;
    // A heading opens a unit; so does a row. A `-#` line that is not a heading
    // is the detail belonging to the row above it, and travels with it.
    let opens_a_unit = |l: &str| !l.starts_with("-# ") || l.starts_with("-# ── ");
    for l in lines {
        if opens_a_unit(&l) && !unit.is_empty() {
            if used + cost > TEXT_BUDGET {
                unit.clear();
                break;
            }
            used += cost;
            cost = 0;
            kept.append(&mut unit);
        }
        cost += l.chars().count() + 1; // the newline it joins on
        unit.push(l);
    }
    if !unit.is_empty() && used + cost <= TEXT_BUDGET {
        kept.append(&mut unit);
    }
    // A heading left at the end has nothing under it: a workspace named and
    // then not shown is worse than not naming it.
    while kept.last().is_some_and(|l| l.starts_with("-# ── ")) {
        kept.pop();
    }
    // An agent is the one line that is not a `-#` detail — its row. Counting
    // those rather than surviving lines is what makes the tally right whether
    // or not a row brought its detail line with it.
    let kept_agents = kept.iter().filter(|l| !l.starts_with("-# ")).count();
    if kept_agents < shown {
        kept.push(format!("-# … and {} more", shown - kept_agents));
    }
    let out = kept.join("\n");
    debug_assert!(out.chars().count() <= TEXT_MAX);
    out
}

/// The state tally the TUI header shows: `🟡 wait 1 · 🟢 working 2`.
///
/// Ordered by what needs a person first, and DEAD is not counted.
fn summary_line(agents: &[Agent]) -> String {
    const ORDER: [AgentState; 5] = [
        AgentState::Waiting,
        AgentState::DoneUnread,
        AgentState::Error,
        AgentState::Working,
        AgentState::Idle,
    ];
    let parts: Vec<String> = ORDER
        .iter()
        .filter_map(|s| {
            let n = agents.iter().filter(|a| a.state == *s).count();
            (n > 0).then(|| format!("{} {} {n}", state_emoji(*s), state_word(*s)))
        })
        .collect();
    if parts.is_empty() {
        return "none".into();
    }
    parts.join(" · ")
}

/// The TUI header's per-CLI default-model fragment.
///
/// `None` omits the line; `Some` with every field empty prints three "auto"s.
/// The distinction is deliberate: it is the difference between "nobody asked"
/// and "we looked and nothing is pinned", which is worth showing.
///
/// A Claude default of Fable earns a `⚠`: it is the top tier, so leaving it
/// pinned means every session anyone starts from then on burns tokens at that
/// rate.
fn default_models_line(def: Option<&DefaultModels>) -> String {
    let Some(def) = def else {
        return String::new();
    };
    let parts: Vec<String> = [
        ("claude", "Claude", def.claude.as_str()),
        ("codex", "Codex", def.codex.as_str()),
        ("gemini", "Gemini", def.gemini.as_str()),
    ]
    .into_iter()
    .map(|(key, label, v)| {
        if v.is_empty() {
            format!("{label} auto")
        } else if key == "claude" && is_fable(v) {
            format!("⚠ {label} {}", pretty_model(v))
        } else if key == "claude" {
            format!("{label} {}", pretty_model(v))
        } else {
            // Only Claude's IDs get prettified: the other two CLIs already
            // report a name a person recognizes.
            format!("{label} {v}")
        }
    })
    .collect();
    format!("-# default model {}", parts.join(" · "))
}

/// Truncates to `n` characters, not bytes: a headline cut mid-rune would put
/// replacement characters in the card.
fn truncate_chars(s: &str, n: usize) -> String {
    let mut it = s.char_indices();
    match it.nth(n) {
        Some((byte, _)) => format!("{}…", &s[..byte]),
        None => s.to_string(),
    }
}

/// Elapsed time in the coarsest unit that still reads. Anything in the future,
/// or under a minute, is "just now".
fn since(from: OffsetDateTime, now: OffsetDateTime) -> String {
    let d = now - from;
    if d < Duration::MINUTE {
        "just now".into()
    } else if d < Duration::HOUR {
        format!("{}m", d.whole_minutes())
    } else if d < Duration::DAY {
        format!("{}h", d.whole_hours())
    } else {
        format!("{}d", d.whole_hours() / 24)
    }
}

/// Assembles the whole card: the agent section, when anything is live, and
/// the footer.
pub fn build_card(d: &CardData, now: OffsetDateTime) -> Vec<Component> {
    let mut comps: Vec<Component> = Vec::new();
    if let Some(section) = agents_container(d, now) {
        comps.push(section);
    }
    // Discord renders `<t:…:R>` as a live relative time, so the footer keeps
    // saying "2 minutes ago" without anyone re-posting the card.
    comps.push(Component::text(format!(
        "-# updated <t:{}:R>",
        now.unix_timestamp()
    )));
    comps
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tower_usage::UsedPct;

    /// The fixture's instant: 2026-08-25 12:00:00 +09:00. Shared with the
    /// webhook tests so both pin the same rendered card.
    pub(crate) fn t0() -> OffsetDateTime {
        time::Date::from_calendar_date(2026, time::Month::August, 25)
            .unwrap()
            .with_hms(12, 0, 0)
            .unwrap()
            .assume_offset(time::UtcOffset::from_hms(9, 0, 0).unwrap())
    }

    fn agent(
        id: &str,
        kind: &str,
        state: AgentState,
        session: &str,
        cwd: &str,
        age: Duration,
    ) -> Agent {
        let mut a = Agent::new(id, kind, t0() - age);
        a.state = state;
        a.state_since = t0() - age;
        a.pane.session = session.into();
        a.cwd = cwd.into();
        a
    }

    /// The fixture the recorded payloads were rendered from.
    pub(crate) fn data() -> CardData {
        let mut waiting = agent(
            "claude-7",
            "claude",
            AgentState::Waiting,
            "collab-bot",
            "/Users/jordan/ai-folder/collab",
            Duration::minutes(8),
        );
        waiting.task = "awaiting approval".into();
        // WORKING with no update for two hours: the stale mark's whole reason.
        let working = agent(
            "codex-3",
            "codex",
            AgentState::Working,
            "codex-bridge",
            "/Users/jordan/bridge",
            Duration::hours(2),
        );
        let dead = agent(
            "claude-9",
            "claude",
            AgentState::Dead,
            "",
            "/Users/jordan/gone",
            Duration::hours(1),
        );

        CardData {
            agents: vec![waiting, working, dead],
            ctx: HashMap::from([(
                "claude-7".to_string(),
                CtxInfo {
                    model: "Opus 5 (1M context)".into(),
                    used_pct: Some(UsedPct::Exact(16.0)),
                    ts: Some(t0() - Duration::minutes(3)),
                },
            )]),
            wired: HashMap::from([("collab-bot".to_string(), "⌁collab방".to_string())]),
            branches: HashMap::from([(
                "/Users/jordan/ai-folder/collab".to_string(),
                "agent/fix-card".to_string(),
            )]),
            def_models: Some(DefaultModels {
                claude: "claude-fable-5".into(),
                codex: "gpt-5.6-sol".into(),
                gemini: String::new(),
            }),
            home: "/Users/jordan".into(),
        }
    }

    pub(crate) fn card_json(d: &CardData) -> String {
        String::from_utf8(crate::json::to_sorted_json(&build_card(d, t0())).unwrap()).unwrap()
    }

    /// The payloads this card is held to.
    ///
    /// Rewritten once since they were first recorded — when the product stopped
    /// speaking Korean, which changed every line of text in them. What they
    /// still pin is the whole shape: the component nesting, the accent colours,
    /// the ordering, the separators, the trailing newline. Rewriting them is
    /// `UPDATE_EXPECTED=1 cargo test`, and it is a deliberate act to be read in
    /// the diff.
    const CARD: &str = include_str!("testdata/card.json");

    /// Rewrites the payload file above.
    #[test]
    fn update_card_fixtures() {
        if std::env::var_os("UPDATE_EXPECTED").is_none() {
            return;
        }
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata");
        std::fs::write(format!("{dir}/card.json"), card_json(&data()) + "\n").unwrap();
    }

    #[test]
    fn the_card_is_byte_identical_to_its_recorded_payload() {
        // See `update_card_fixtures`: the bytes below were embedded before it
        // wrote the new ones.
        if std::env::var_os("UPDATE_EXPECTED").is_some() {
            return;
        }
        assert_eq!(card_json(&data()), CARD.trim_end_matches('\n'));
    }

    /// The card is read on a phone, with no detail view behind it and no key to
    /// press — so a renamed pane leading with its label would leave the pending
    /// question absent from the card entirely.
    #[test]
    fn a_renamed_pane_still_says_what_it_is_waiting_for() {
        let mut d = data();
        let waiting = d
            .agents
            .iter_mut()
            .find(|a| a.state == AgentState::Waiting)
            .expect("the fixture has a waiting agent");
        waiting.label = "dbeaver".into();
        waiting.ask = "may I drop the column?".into();

        let json = card_json(&d);
        assert!(
            json.contains("may I drop the column?"),
            "the question has to be on the card somewhere:\n{json}"
        );
    }

    /// …and a pane that is *not* waiting keeps its name, because then there is
    /// no question competing for the line and nothing else on the card carries
    /// the name a person chose.
    #[test]
    fn a_renamed_pane_that_is_working_still_shows_its_name() {
        let mut d = data();
        let working = d
            .agents
            .iter_mut()
            .find(|a| a.state == AgentState::Working)
            .expect("the fixture has a working agent");
        working.label = "dbeaver".into();
        working.task = "refactor".into();

        let json = card_json(&d);
        assert!(json.contains("dbeaver"), "{json}");
    }

    /// Two agents in one workspace share one heading, and a third elsewhere
    /// opens its own.
    ///
    /// The fixture cannot show this — every one of its records is alone in its
    /// workspace — and it is the case the heading exists for. Against grouping
    /// by vendor the same heading came back further down the card once the
    /// store stopped ordering by vendor.
    #[test]
    fn one_heading_per_workspace_however_many_agents_are_under_it() {
        let mut d = data();
        d.agents.retain(|a| a.state != AgentState::Dead);
        assert!(d.agents.len() >= 2, "the fixture needs two live agents");
        for a in d.agents.iter_mut() {
            a.pane.session = "alpha".into();
        }

        let json = card_json(&d);
        assert_eq!(
            json.matches("── alpha ──────").count(),
            1,
            "two agents, one heading:\n{json}"
        );

        d.agents[1].pane.session = "beta".into();
        let json = card_json(&d);
        assert_eq!(json.matches("── alpha ──────").count(), 1, "{json}");
        assert_eq!(json.matches("── beta ──────").count(), 1, "{json}");
    }

    /// `agents` is a public field, and a caller that fills it in some other
    /// order must not get the same heading twice.
    ///
    /// `A, B, A` is the shape: watching for a change between neighbours prints
    /// A's heading, B's, then A's again, and the card reads as four workspaces
    /// where there are two.
    #[test]
    fn an_unsorted_input_still_gets_one_heading_per_workspace() {
        let mut d = data();
        d.agents.retain(|a| a.state != AgentState::Dead);
        assert!(d.agents.len() >= 2, "the fixture needs two live agents");
        let third = d.agents[0].clone();
        d.agents[0].pane.session = "alpha".into();
        d.agents[1].pane.session = "beta".into();
        let mut third = third;
        third.id = "claude-third".into();
        third.pane.session = "alpha".into();
        d.agents.push(third);

        let json = card_json(&d);
        assert_eq!(
            json.matches("── alpha ──────").count(),
            1,
            "A, B, A must not print A twice:\n{json}"
        );
        assert_eq!(json.matches("── beta ──────").count(), 1, "{json}");
    }

    /// A pane whose workspace herdr no longer reports still gets a heading.
    #[test]
    fn a_workspaceless_row_is_not_left_under_nothing() {
        let mut d = data();
        d.agents.retain(|a| a.state != AgentState::Dead);
        for a in d.agents.iter_mut() {
            a.pane.session = String::new();
        }

        let json = card_json(&d);
        assert_eq!(
            json.matches("── (no workspace) ──────").count(),
            1,
            "{json}"
        );
    }

    /// The agent text is bounded, because nothing bounded it before and
    /// grouping by workspace is what made the bound necessary.
    ///
    /// `webhook` answers a 400 by posting a *new* card — the same oversized one
    /// — so a card that grows past Discord's component limit once stops updating
    /// for good. An installation of one-agent workspaces now spends a heading
    /// per agent where it spent one per vendor, which is how a card that fit
    /// yesterday exceeds the limit today.
    #[test]
    fn the_agent_text_stays_under_the_component_limit() {
        let mut d = data();
        let one = d
            .agents
            .iter()
            .find(|a| a.state != AgentState::Dead)
            .expect("a live agent")
            .clone();
        d.agents = (0..400)
            .map(|i| {
                let mut a = one.clone();
                a.id = format!("claude-{i}");
                a.pane.session = format!("workspace-number-{i}");
                a.pane.workspace_id = format!("w{i}");
                a
            })
            .collect();

        let json = card_json(&d);
        let text = json
            .split("\"content\":")
            .find(|s| s.contains("── workspace-number-0"))
            .expect("the agent section");
        assert!(
            text.contains("… and "),
            "it says what it dropped:\n{}",
            &text[..text.len().min(400)]
        );
        for line in json.split("\"content\":\"").skip(1) {
            let content = line.split("\",").next().unwrap_or("");
            assert!(
                content.chars().count() <= 4000,
                "a {}-char component",
                content.chars().count()
            );
        }
    }

    /// A row and its detail are admitted together or not at all.
    ///
    /// Cutting between them kept the row and dropped the model, the context
    /// figure, the age and the task under it — and the tally still counted that
    /// agent as shown, so nothing said the last visible row had lost half of
    /// itself.
    #[test]
    fn a_row_never_outlives_its_detail_line() {
        let mut d = data();
        let one = d
            .agents
            .iter()
            .find(|a| a.state != AgentState::Dead)
            .expect("a live agent")
            .clone();
        d.agents = (0..400)
            .map(|i| {
                let mut a = one.clone();
                a.id = format!("claude-{i}");
                a.pane.session = format!("ws-{i}");
                a.pane.workspace_id = format!("w{i}");
                a
            })
            .collect();

        let json = card_json(&d);
        let text = json
            .split("\"content\":\"")
            .find(|s| s.contains("── ws-0"))
            .expect("the agent section");
        let body = text.split("\",").next().unwrap_or("");
        let lines: Vec<&str> = body.split("\\n").collect();
        for (i, l) in lines.iter().enumerate() {
            // A row — not a heading, not a detail — must be followed by its
            // detail when the fixture gives every agent one.
            if !l.starts_with("-# ") {
                assert!(
                    lines
                        .get(i + 1)
                        .is_some_and(|n| n.starts_with("-# ") && !n.starts_with("-# ── ")),
                    "row {i} lost its detail line:\n{body}"
                );
            }
        }
    }

    /// Gauge bars break into a grid in Discord's font, so agent rows carry the
    /// number as text.
    #[test]
    fn agent_rows_carry_no_gauge_bar() {
        let out = card_json(&data());
        assert!(!out.contains('█') && !out.contains('░'), "{out}");
        assert!(out.contains("ctx 16%"));
    }

    /// A DEAD record is history, not a row. Leaving it on the card makes the
    /// tally lie about how many agents are live.
    #[test]
    fn a_dead_agent_is_left_off_the_card() {
        let out = card_json(&data());
        assert!(!out.contains("/Users/jordan/gone"));
        assert!(out.contains("wait 1"), "the tally counts what is shown");
    }

    /// Discord answers a container holding empty text with a 400, so "no rows"
    /// has to mean "no section", not "an empty one".
    #[test]
    fn an_agent_list_with_nothing_live_yields_no_section() {
        let mut d = data();
        d.agents.retain(|a| a.state == AgentState::Dead);
        let comps = build_card(&d, t0());
        assert_eq!(comps.len(), 1, "the footer only: {comps:?}");
        assert!(
            matches!(&comps[0], Component::Text { content } if content.starts_with("-# updated"))
        );
    }

    /// Half the value of the mark is that a wedged agent stops looking busy.
    #[test]
    fn a_working_row_with_no_recent_update_is_marked_stale() {
        let out = card_json(&data());
        assert!(out.contains("working?"), "{out}");
    }

    /// Kept-bug pin. The home abbreviation is a plain prefix test with no path
    /// boundary, so a sibling folder whose name merely starts with the home
    /// directory's is mangled rather than left alone. Cosmetic, and pinned so
    /// that fixing it is a deliberate change to the card rather than a
    /// side effect somebody notices later.
    #[test]
    fn a_sibling_of_the_home_directory_is_abbreviated_wrongly() {
        let mut d = data();
        d.agents[0].cwd = "/Users/jordanian/proj".into();
        assert!(card_json(&d).contains("~ian/proj"), "{}", card_json(&d));
    }

    #[test]
    fn since_uses_the_coarsest_unit_that_still_reads() {
        let now = t0();
        assert_eq!(since(now, now), "just now");
        assert_eq!(
            since(now + Duration::minutes(5), now),
            "just now",
            "clock skew"
        );
        assert_eq!(since(now - Duration::seconds(59), now), "just now");
        assert_eq!(since(now - Duration::minutes(8), now), "8m");
        assert_eq!(since(now - Duration::hours(2), now), "2h");
        assert_eq!(since(now - Duration::hours(49), now), "2d");
    }

    /// `None` means nobody looked; every field empty means we looked and nothing
    /// is pinned. The line has to say which.
    #[test]
    fn an_unread_default_model_set_prints_no_line_at_all() {
        assert_eq!(default_models_line(None), "");
        assert_eq!(
            default_models_line(Some(&DefaultModels::default())),
            "-# default model Claude auto · Codex auto · Gemini auto"
        );
    }

    /// Fable left pinned as the default means every session anyone starts runs
    /// at the top tier, which is why it earns a warning rather than a name.
    #[test]
    fn a_fable_default_is_flagged() {
        let def = DefaultModels {
            claude: "claude-fable-5".into(),
            ..DefaultModels::default()
        };
        assert!(default_models_line(Some(&def)).contains("⚠ Claude Fable 5"));
    }

    /// Only Claude's value is shortened, and only Claude's earns the warning.
    ///
    /// The other two CLIs already report a name a person recognizes, so putting
    /// their values through the shortener would mangle them — and a codex model
    /// whose name happens to carry "fable" is not the pinning that burns Claude
    /// tokens, so it must not raise the flag that says it is.
    ///
    /// The right-hand column is a hand-written table rather than a second call
    /// to `pretty_model`, so a change in the shortener shows up here as a
    /// disagreement instead of being echoed back.
    #[test]
    fn only_claudes_default_is_shortened_and_only_claudes_is_flagged() {
        for (raw, want) in [
            ("claude-opus-5", "Claude Opus 5"),
            ("claude-opus-5[1m]", "Claude Opus 5 (1M)"),
            ("claude-fable-5", "⚠ Claude Fable 5"),
            ("opusplan", "Claude OpusPlan"),
            // An alias the CLI writes for "no pinning of my own".
            ("default", "Claude auto"),
            // A display name rather than an ID — still recognized as Fable.
            ("Fable", "⚠ Claude Fable"),
            // Unknown and non-ASCII values pass through as themselves: a model
            // nobody has taught this about must show up, not disappear.
            ("gpt-5.6-sol", "Claude gpt-5.6-sol"),
            ("모델", "Claude 모델"),
        ] {
            let def = DefaultModels {
                claude: raw.into(),
                ..DefaultModels::default()
            };
            let line = default_models_line(Some(&def));
            assert!(line.contains(want), "{raw:?} should give {want:?}: {line}");
        }

        for raw in ["claude-opus-5", "claude-fable-5", "Fable", "gpt-5.6-sol"] {
            let def = DefaultModels {
                codex: raw.into(),
                gemini: raw.into(),
                ..DefaultModels::default()
            };
            let line = default_models_line(Some(&def));
            assert!(line.contains(&format!("Codex {raw}")), "{line}");
            assert!(line.contains(&format!("Gemini {raw}")), "{line}");
            assert!(
                !line.contains('⚠'),
                "only Claude's own pinning burns Claude tokens: {line}"
            );
        }
    }

    /// Whatever the three CLIs have pinned, the line names all three.
    ///
    /// The line answers "what will the next session anyone starts run", and a
    /// CLI silently missing from it reads as if nobody asked about that one —
    /// which is the same picture as `None`, the case the line is careful to
    /// tell apart. The warning is present exactly when Claude's own pinning is
    /// Fable, and the line is empty exactly when nobody looked.
    #[test]
    fn every_pinning_is_named_and_the_warning_tracks_claude_alone() {
        const VOCAB: [&str; 8] = [
            "",
            "default",
            "opusplan",
            "claude-fable-5",
            "claude-opus-5[1m]",
            "gpt-5.6-sol",
            "Gemini 3.6 Flash (Medium)",
            "모델",
        ];
        let mut rng = tower_testrng::Rng::new(0xCA5E_0007);
        // The generator is [`tower_testrng`]'s: xorshift64*, whose output
        // multiply matters here because `% n` reads the low bits and every
        // draw in this test is a small `n`.
        let mut next = move |n: u64| rng.below(n as usize) as u64;
        for step in 0..200 {
            let def = DefaultModels {
                claude: VOCAB[next(8) as usize].into(),
                codex: VOCAB[next(8) as usize].into(),
                gemini: VOCAB[next(8) as usize].into(),
            };
            let line = default_models_line(Some(&def));
            assert!(line.starts_with("-# default model "), "step {step}: {line}");
            assert_eq!(line.split(" · ").count(), 3, "step {step}: {line}");
            for label in ["Claude ", "Codex ", "Gemini "] {
                assert!(line.contains(label), "step {step}: no {label:?} in {line}");
            }
            assert_eq!(
                line.contains('⚠'),
                is_fable(&def.claude),
                "step {step}: {line}"
            );
        }
        assert!(default_models_line(None).is_empty());
    }

    /// The heading carries the default-model line and nothing else now, so with
    /// nobody having looked there is no second line to carry.
    ///
    /// Joining an empty fragment on would leave a blank row under the heading,
    /// which is a visible hole on a card read on a phone — and it was reachable
    /// while the heading joined two fragments of which either could be empty.
    #[test]
    fn a_heading_with_no_default_models_is_the_tally_alone() {
        let heading = |d: &CardData| -> String {
            let Some(Component::Container { components, .. }) = agents_container(d, t0()) else {
                panic!("the fixture has live agents, so it has a section");
            };
            let Component::Text { content } = &components[0] else {
                panic!("the heading is the section's first component");
            };
            content.clone()
        };

        let mut bare = data();
        bare.def_models = None;
        assert_eq!(heading(&bare), "### Agents — 🟡 wait 1 · 🟢 working 1");

        let full = heading(&data());
        assert_eq!(full.lines().count(), 2, "{full}");
        assert!(
            full.lines()
                .nth(1)
                .unwrap()
                .starts_with("-# default model "),
            "{full}"
        );
    }

    #[test]
    fn an_unparseable_colour_is_black_rather_than_a_lost_card() {
        assert_eq!(accent("#F04747"), 0x00F0_4747);
        assert_eq!(accent("F04747"), 0x00F0_4747);
        assert_eq!(accent("#zz"), 0);
        assert_eq!(accent(""), 0);
    }

    #[test]
    fn truncation_counts_characters_so_no_rune_is_cut_in_half() {
        assert_eq!(truncate_chars("짧다", 40), "짧다");
        assert_eq!(truncate_chars("가나다라", 2), "가나…");
    }
}
