// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower report`: what happened while nobody was looking.
//!
//! Read from the transition journal ([`tower_state::journal`]): for each agent
//! that moved in the window, how many turns it finished, how long it worked,
//! how long it sat waiting on a person and on what, and whether it failed —
//! the question to answer in the morning being "who was blocked on me, and
//! for how long".

use std::collections::BTreeMap;
use std::io::{self, Write};

use serde::Serialize;
use time::{Duration, OffsetDateTime};
use tower_state::journal::Entry;
use tower_state::AgentState;

/// One agent's window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Line {
    pub id: String,
    pub kind: String,
    /// `session›tab`, as the tower names the place.
    pub place: String,
    /// Turns that reached DONE.
    pub turns: u32,
    pub errors: u32,
    pub work_secs: i64,
    pub wait_secs: i64,
    /// The longest single wait, and what it was asking.
    pub longest_wait_secs: i64,
    pub longest_wait_ask: String,
    /// The last task seen.
    pub task: String,
}

/// Folds the journal into one [`Line`] per agent, busiest first.
///
/// Each entry opens a stretch in its `to` state that lasts until the agent's
/// next entry, or until `now`. The stretch before an agent's first entry in
/// the window is in that entry's `from` state, from `since`.
pub fn summarize(entries: &[Entry], since: OffsetDateTime, now: OffsetDateTime) -> Vec<Line> {
    let mut by_id: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.at >= since && e.at <= now) {
        by_id.entry(&e.id).or_default().push(e);
    }
    let mut out: Vec<Line> = by_id
        .into_values()
        .map(|mut es| {
            es.sort_by_key(|e| e.at);
            let last = es[es.len() - 1];
            let mut line = Line {
                id: last.id.clone(),
                kind: last.kind.clone(),
                place: place(&last.session, &last.tab),
                task: es
                    .iter()
                    .rev()
                    .find(|e| !e.task.is_empty())
                    .map_or(String::new(), |e| e.task.clone()),
                ..Line::default()
            };
            let mut stretch =
                |state: AgentState, from: OffsetDateTime, to: OffsetDateTime, ask: &str| {
                    let secs = (to - from).max(Duration::ZERO).whole_seconds();
                    match state {
                        AgentState::Working => line.work_secs += secs,
                        AgentState::Waiting => {
                            line.wait_secs += secs;
                            if secs > line.longest_wait_secs {
                                line.longest_wait_secs = secs;
                                line.longest_wait_ask = ask.to_string();
                            }
                        }
                        _ => {}
                    }
                };
            stretch(es[0].from, since, es[0].at, "");
            for (i, e) in es.iter().enumerate() {
                let end = es.get(i + 1).map_or(now, |n| n.at);
                stretch(e.to, e.at, end, &e.ask);
            }
            line.turns = es.iter().filter(|e| e.to == AgentState::DoneUnread).count() as u32;
            line.errors = es.iter().filter(|e| e.to == AgentState::Error).count() as u32;
            line
        })
        .collect();
    out.sort_by(|a, b| {
        (b.wait_secs, b.work_secs)
            .cmp(&(a.wait_secs, a.work_secs))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

fn place(session: &str, tab: &str) -> String {
    match (session.is_empty(), tab.is_empty()) {
        (false, false) => format!("{session}›{tab}"),
        (false, true) => session.to_string(),
        (true, false) => tab.to_string(),
        (true, true) => "—".into(),
    }
}

/// `1h02m`, `14m`, `40s`, `—` for nothing.
pub fn span(secs: i64) -> String {
    match secs {
        s if s <= 0 => "—".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// A drive verdict reached in the window, as `wt drive` recorded it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub task: String,
    pub state: String,
    pub attempts: u32,
}

/// The `drive` verdicts of `wt` workers reached since `since`.
pub fn verdicts(state_dir: &std::path::Path, since: OffsetDateTime) -> Vec<Verdict> {
    tower_wt::list_metas(state_dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let d = m.drive?;
            (d.at >= since).then(|| Verdict {
                task: m.task,
                state: d.state.label().to_string(),
                attempts: d.attempts,
            })
        })
        .collect()
}

/// The report as data: what `report --json` prints and what the remote MCP
/// server's `report` tool returns. `remote` is what was done to agents over
/// MCP in the window ([`crate::mcp::audit`]).
pub fn to_json(
    lines: &[Line],
    verdicts: &[Verdict],
    remote: &[crate::mcp::audit::Entry],
    since: OffsetDateTime,
) -> serde_json::Value {
    serde_json::json!({
        "since": since.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        "agents": lines,
        "drive_verdicts": verdicts,
        "remote_actions": remote,
    })
}

/// The longest window `report` looks back over. The journal keeps days of
/// history, not decades; past this the arithmetic below would leave what a
/// timestamp can hold, and a window that long means the same as "everything".
pub const MAX_WINDOW_DAYS: u32 = 3650;

/// Where a `--since` window starts, counted back from `now`.
///
/// The one place both the command and the MCP `report` tool turn text into a
/// time, so neither can reach the overflow: `Duration::seconds_f64` panics
/// past `i64` seconds and `OffsetDateTime - Duration` panics before year
/// −9999, and `parse_hours` alone accepts any finite number.
pub fn window_start(now: OffsetDateTime, since: &str) -> Result<OffsetDateTime, String> {
    let hours = parse_hours(since)
        .ok_or_else(|| format!("--since {since:?}: expected a number and m, h or d"))?;
    if hours > f64::from(MAX_WINDOW_DAYS) * 24.0 {
        return Err(format!(
            "--since {since:?}: at most {MAX_WINDOW_DAYS}d (ten years)"
        ));
    }
    Duration::checked_seconds_f64(hours * 3600.0)
        .and_then(|d| now.checked_sub(d))
        .ok_or_else(|| format!("--since {since:?}: too far back"))
}

/// `30m`, `12h`, `2d` in hours.
pub fn parse_hours(s: &str) -> Option<f64> {
    let s = s.trim();
    // At the last character, not the last byte: `2일` must not split a char.
    let (i, _) = s.char_indices().last()?;
    let (num, unit) = s.split_at(i);
    let n: f64 = num
        .parse()
        .ok()
        .filter(|n: &f64| *n > 0.0 && n.is_finite())?;
    match unit {
        "m" => Some(n / 60.0),
        "h" => Some(n),
        "d" => Some(n * 24.0),
        _ => None,
    }
}

/// The report as text.
pub fn render(
    w: &mut impl Write,
    lines: &[Line],
    verdicts: &[Verdict],
    remote: &[crate::mcp::audit::Entry],
    since: &str,
    window: &str,
) -> io::Result<()> {
    writeln!(w, "herdr-tower report — the last {window} (since {since})")?;
    writeln!(w)?;
    if lines.is_empty() {
        writeln!(w, "no state changes in that window")?;
    } else {
        writeln!(
            w,
            "{:<34} {:<8} {:>5} {:>7} {:>7} {:>3}  LONGEST WAIT",
            "PLACE", "AGENT", "TURNS", "WORK", "WAIT", "ERR"
        )?;
        for l in lines {
            let longest = if l.longest_wait_secs > 0 {
                let ask: String = l.longest_wait_ask.chars().take(60).collect();
                if ask.is_empty() {
                    span(l.longest_wait_secs)
                } else {
                    format!("{} “{ask}”", span(l.longest_wait_secs))
                }
            } else {
                String::new()
            };
            let place: String = l.place.chars().take(34).collect();
            writeln!(
                w,
                "{:<34} {:<8} {:>5} {:>7} {:>7} {:>3}  {longest}",
                place,
                l.kind,
                l.turns,
                span(l.work_secs),
                span(l.wait_secs),
                l.errors
            )?;
        }
    }
    if !verdicts.is_empty() {
        writeln!(w)?;
        writeln!(w, "wt drive verdicts")?;
        for v in verdicts {
            writeln!(
                w,
                "  {:<24} {:<12} after {} run(s)",
                v.task, v.state, v.attempts
            )?;
        }
    }
    if !remote.is_empty() {
        writeln!(w)?;
        writeln!(w, "remote actions (MCP)")?;
        for e in remote {
            let l = e.at.to_offset(tower_state::local_offset());
            writeln!(
                w,
                "  {:02}:{:02}  {}",
                l.hour(),
                l.minute(),
                crate::mcp::audit::line(e).trim_start_matches("📱 ")
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mins: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::minutes(mins)
    }

    fn e(id: &str, mins: i64, from: AgentState, to: AgentState, ask: &str) -> Entry {
        Entry {
            at: at(mins),
            id: id.into(),
            kind: "claude".into(),
            session: "20.herdr-tower".into(),
            tab: "main".into(),
            from,
            to,
            task: "Fix login".into(),
            ask: ask.into(),
        }
    }

    use AgentState::*;

    /// Work, a wait on a question, work again, done — then a second turn that
    /// failed.
    #[test]
    fn a_nights_transitions_fold_into_one_line() {
        let es = [
            e("a", 10, Idle, Working, ""),
            e("a", 30, Working, Waiting, "Allow bash: rm -rf build?"),
            e("a", 75, Waiting, Working, ""),
            e("a", 80, Working, DoneUnread, ""),
            e("a", 90, DoneUnread, Working, ""),
            e("a", 95, Working, Error, "APIError"),
        ];
        let got = summarize(&es, at(0), at(100));
        assert_eq!(got.len(), 1);
        let l = &got[0];
        assert_eq!(l.place, "20.herdr-tower›main");
        assert_eq!(l.turns, 1);
        assert_eq!(l.errors, 1);
        assert_eq!(l.work_secs, (20 + 5 + 5) * 60);
        assert_eq!(l.wait_secs, 45 * 60);
        assert_eq!(l.longest_wait_secs, 45 * 60);
        assert_eq!(l.longest_wait_ask, "Allow bash: rm -rf build?");
    }

    /// A stretch that began before the window counts from the window's start,
    /// and one still running counts to now.
    #[test]
    fn stretches_are_clipped_to_the_window() {
        let es = [e("a", 30, Waiting, Working, "")];
        let got = summarize(&es, at(0), at(40));
        assert_eq!(got[0].wait_secs, 30 * 60, "waiting since before the window");
        assert_eq!(got[0].work_secs, 10 * 60, "still working at the end");
        assert!(
            summarize(&es, at(31), at(40)).is_empty(),
            "nothing moved in the window"
        );
    }

    /// Whoever waited longest on a person leads.
    #[test]
    fn the_longest_waiter_comes_first() {
        let es = [e("a", 0, Idle, Working, ""), e("b", 0, Idle, Waiting, "?")];
        let got = summarize(&es, at(0), at(60));
        assert_eq!(got[0].id, "b");
    }

    #[test]
    fn spans_read_at_a_glance() {
        assert_eq!(span(0), "—");
        assert_eq!(span(40), "40s");
        assert_eq!(span(14 * 60), "14m");
        assert_eq!(span(3720), "1h02m");
    }

    #[test]
    fn remote_actions_get_a_section_of_their_own_only_when_there_are_any() {
        let mut out = Vec::new();
        render(&mut out, &[], &[], &[], "since", "12h").unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("remote actions"));

        let e = crate::mcp::audit::Entry {
            at: at(0),
            grant: "g1".into(),
            client: "Claude".into(),
            tool: "send_prompt".into(),
            agent: "claude-w1:p1".into(),
            session: "api".into(),
            text: "go".into(),
            outcome: "sent".into(),
        };
        let mut out = Vec::new();
        render(&mut out, &[], &[], std::slice::from_ref(&e), "since", "12h").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("remote actions (MCP)"), "{text}");
        assert!(
            text.contains("send_prompt → api: `go` — sent (Claude)"),
            "{text}"
        );
        let json = to_json(&[], &[], &[e], at(0));
        assert_eq!(json["remote_actions"][0]["outcome"], "sent");
    }

    // ---- parse_hours: edge cases, cross validation, property

    #[test]
    fn parse_hours_accepts_only_a_positive_number_and_a_unit() {
        let cases: &[(&str, Option<f64>)] = &[
            ("30m", Some(0.5)),
            ("12h", Some(12.0)),
            ("2d", Some(48.0)),
            (" 1.5h ", Some(1.5)),
            ("0.25d", Some(6.0)),
            ("", None),
            ("h", None),
            ("0h", None),
            ("-1h", None),
            ("12", None),
            ("12x", None),
            ("12H", None),
            ("1e400h", None), // parses to infinity
            ("NaNh", None),
            ("infh", None),
            ("12 h", None),
            ("시간h", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_hours(input), *want, "{input:?}");
        }
    }

    /// The three units agree with each other: n days = 24n hours = 1440n minutes.
    #[test]
    fn parse_hours_units_agree_with_each_other() {
        for n in 1..=200u32 {
            let n = f64::from(n) / 4.0;
            let d = parse_hours(&format!("{n}d")).unwrap();
            let h = parse_hours(&format!("{}h", n * 24.0)).unwrap();
            let m = parse_hours(&format!("{}m", n * 1440.0)).unwrap();
            assert!(
                (d - h).abs() < 1e-9 && (h - m).abs() < 1e-9,
                "{n}: {d} {h} {m}"
            );
        }
    }

    /// Monotonic: a longer window never parses to fewer hours.
    #[test]
    fn parse_hours_is_monotonic_in_its_number() {
        let mut last = 0.0;
        for n in 1..=500u32 {
            let h = parse_hours(&format!("{n}m")).unwrap();
            assert!(h > last, "{n}m");
            last = h;
        }
    }

    // ---- verdicts: only drive verdicts reached inside the window

    fn meta(task: &str, drive: Option<(&str, u32, i64)>) -> tower_wt::Meta {
        let mut v = serde_json::json!({
            "task": task,
            "repo": "/r",
            "base": "main",
            "branch": format!("agent/{task}"),
            "path": format!("/r/.wt/{task}"),
            "agent": "claude",
            "created_at": "2024-10-01T00:00:00Z",
        });
        if let Some((state, attempts, mins)) = drive {
            v["drive"] = serde_json::json!({
                "state": state,
                "attempts": attempts,
                "at": at(mins).format(&time::format_description::well_known::Rfc3339).unwrap(),
            });
        }
        serde_json::from_value(v).expect("a valid task record")
    }

    #[test]
    fn verdicts_come_only_from_drives_inside_the_window() {
        let dir = tempfile::tempdir().unwrap();
        tower_wt::save_meta(dir.path(), &meta("inside", Some(("merge-ready", 1, 10)))).unwrap();
        tower_wt::save_meta(dir.path(), &meta("edge", Some(("stalled", 2, 0)))).unwrap();
        tower_wt::save_meta(dir.path(), &meta("before", Some(("merge-ready", 1, -1)))).unwrap();
        tower_wt::save_meta(dir.path(), &meta("undriven", None)).unwrap();
        let mut got = verdicts(dir.path(), at(0));
        got.sort_by(|a, b| a.task.cmp(&b.task));
        let summary: Vec<(&str, &str, u32)> = got
            .iter()
            .map(|v| (v.task.as_str(), v.state.as_str(), v.attempts))
            .collect();
        assert_eq!(
            summary,
            [("edge", "stalled", 2), ("inside", "merge-ready", 1)]
        );
        assert!(
            verdicts(&dir.path().join("nowhere"), at(0)).is_empty(),
            "no records, no verdicts"
        );
    }

    // ---- to_json: round trip through the JSON `report --json` prints

    #[test]
    fn the_report_json_carries_every_line_and_verdict_back_intact() {
        let lines = vec![Line {
            id: "claude-w1:p1".into(),
            kind: "claude".into(),
            place: "api›1".into(),
            turns: 3,
            errors: 1,
            work_secs: 120,
            wait_secs: 30,
            longest_wait_secs: 20,
            longest_wait_ask: "Proceed? “yes”".into(),
            task: "fix".into(),
        }];
        let verdicts = vec![Verdict {
            task: "t".into(),
            state: "merge-ready".into(),
            attempts: 2,
        }];
        let text = serde_json::to_string(&to_json(&lines, &verdicts, &[], at(0))).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["agents"], serde_json::to_value(&lines).unwrap());
        assert_eq!(
            back["drive_verdicts"],
            serde_json::to_value(&verdicts).unwrap()
        );
        assert_eq!(back["remote_actions"], serde_json::json!([]));
        let since = time::OffsetDateTime::parse(
            back["since"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(since, at(0));
    }

    // ---- window_start (#101): a huge --since is an error, never a panic

    #[test]
    fn window_start_refuses_windows_it_cannot_represent() {
        let now = at(0);
        for since in [
            "200000000d",
            "1e300h",
            "999999999999999999999d",
            "3651d",
            "87625h",
        ] {
            let e = window_start(now, since).unwrap_err();
            assert!(e.contains(since), "{since}: {e}");
        }
        assert!(window_start(now, "nonsense").is_err());
        assert_eq!(
            window_start(now, "3650d").unwrap(),
            now - Duration::days(3650)
        );
        assert_eq!(
            window_start(now, "90m").unwrap(),
            now - Duration::minutes(90)
        );
    }

    /// Every string the MCP schema's pattern admits — digits, an optional
    /// fraction, a unit — answers Ok or Err and never panics.
    #[test]
    fn window_start_never_panics_on_anything_the_schema_admits() {
        let now = OffsetDateTime::now_utc();
        for digits in 1..=40 {
            for lead in ["1", "9", "5"] {
                let int = format!("{lead}{}", "9".repeat(digits - 1));
                for frac in ["", ".5", ".999999"] {
                    for unit in ["m", "h", "d"] {
                        let s = format!("{int}{frac}{unit}");
                        let _ = window_start(now, &s);
                    }
                }
            }
        }
        // And near the earliest time a timestamp can hold, too.
        let early = time::PrimitiveDateTime::MIN.assume_utc() + Duration::days(1);
        assert!(window_start(early, "3650d").is_err());
    }

    /// A unit that is not one byte is refused, not split inside: text typed on
    /// a phone reaches this through the remote `report` tool.
    #[test]
    fn a_multi_byte_unit_is_refused_without_a_panic() {
        for s in ["2일", "3ｈ", "5分", "é", "1😀"] {
            assert_eq!(parse_hours(s), None, "{s}");
        }
        assert_eq!(parse_hours("2h"), Some(2.0));
    }
}
