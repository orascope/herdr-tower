// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Plan limits: how much of each rolling window (five hours, seven days) an
//! account has used, and when each window resets.
//!
//! Two sources, each owned by the agent CLI:
//!
//! - **codex** writes the account's limits into every `token_count` line of
//!   its rollouts (`payload.rate_limits`), so they are read from disk. They
//!   are as fresh as the last codex turn, which is why [`Limits::as_of`] is
//!   kept.
//! - **claude** keeps none on disk. The figures `/usage` shows come from the
//!   account's OAuth usage endpoint; fetching it is the caller's business
//!   (this crate spawns nothing and opens no socket), and [`parse_claude_usage`]
//!   reads what came back. The plan is on disk, in the credentials claude
//!   keeps ([`claude_plan`]).
//!
//! As everywhere in this crate, a missing or unreadable source is an empty
//! value, never an error.

use std::fs::File;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::fileread::read_tail;

/// How much of a rollout's end is searched for its latest limits. A
/// `token_count` line is a kilobyte or two and every turn writes one, so the
/// last turns' are well inside this.
const TAIL: u64 = 64 * 1024;

/// One rolling window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 0–100.
    pub used_pct: f64,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
}

/// One account's windows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// As the vendor names it, made readable: "Max 5x", "Pro".
    pub plan: Option<String>,
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// When the figures were recorded, when that is known and may be old.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub as_of: Option<OffsetDateTime>,
    /// The vendor says the plan limit is reached. Paid credits may still
    /// allow use; this flag does not describe their availability.
    #[serde(default)]
    pub reached: bool,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none()
    }
}

/// Five hours and seven days, in minutes, as codex reports its windows.
const FIVE_HOURS: u64 = 5 * 60;
const SEVEN_DAYS: u64 = 7 * 24 * 60;

/// codex's latest limits, from the newest rollout that has any.
///
/// A rollout's `rate_limits` can be `null`, or an entry with no windows (a
/// `premium` bucket with `primary` and `secondary` both `null`); those are
/// passed over for the latest line that names a window. Windows are told apart
/// by their length, not by `primary`/`secondary`: which slot the weekly window
/// takes depends on the plan.
pub fn codex_limits(sessions_root: &Path) -> Limits {
    codex_limits_newer(sessions_root, None)
        .map(|(_, l)| l)
        .unwrap_or_default()
}

/// The latest line with limits among rollouts written after `after` (all of
/// them when `None`), with that rollout's modification time — so a caller that
/// keeps the answer only reads what has been written since.
pub fn codex_limits_newer(
    sessions_root: &Path,
    after: Option<SystemTime>,
) -> Option<(SystemTime, Limits)> {
    codex_limits_scan(sessions_root, after).found
}

/// What one look through the rollouts saw.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodexScan {
    /// The latest line with limits, and its rollout's modification time.
    pub found: Option<(SystemTime, Limits)>,
    /// The newest rollout looked at, found or not: the point a caller can
    /// start the next look after.
    pub newest: Option<SystemTime>,
}

/// [`codex_limits_newer`], also saying how far it looked.
///
/// 🚨 Without `newest`, a caller that found nothing had nothing to keep, and
/// the next call looked through the same rollouts again — every one of them
/// when none carried limits (a codex that only ever ran `exec`, or whose
/// limits come live): up to [`crate::codex`]'s cap of tails, on every board
/// read.
pub fn codex_limits_scan(sessions_root: &Path, after: Option<SystemTime>) -> CodexScan {
    let mut scan = CodexScan::default();
    for path in crate::codex::rollouts_by_recency(sessions_root) {
        let Ok(mut f) = File::open(&path) else {
            continue;
        };
        let Ok(meta) = f.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if after.is_some_and(|a| mtime <= a) {
            // Newest first: everything from here on is older still.
            return scan;
        }
        scan.newest = scan.newest.max(Some(mtime));
        let tail = read_tail(&mut f, meta.len(), TAIL);
        for line in tail.lines().rev() {
            if !line.contains("\"rate_limits\":{") {
                continue;
            }
            if let Some(found) = parse_codex_line(line).filter(|l| !l.is_empty()) {
                scan.found = Some((mtime, found));
                return scan;
            }
        }
    }
    scan
}

/// The limits one rollout line carries, if it carries any.
pub fn parse_codex_line(line: &str) -> Option<Limits> {
    let v: Value = serde_json::from_str(&line[line.find('{')?..]).ok()?;
    let rl = v.pointer("/payload/rate_limits")?;
    if !rl.is_object() {
        return None;
    }
    let mut out = Limits {
        plan: rl.get("plan_type").and_then(Value::as_str).map(pretty_plan),
        as_of: v.get("timestamp").and_then(Value::as_str).and_then(|t| {
            OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok()
        }),
        ..Limits::default()
    };
    for slot in ["primary", "secondary"] {
        let Some(w) = rl.get(slot).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(used) = w.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let window = Window {
            used_pct: used.clamp(0.0, 100.0),
            resets_at: w
                .get("resets_at")
                .and_then(Value::as_i64)
                .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok()),
        };
        match w.get("window_minutes").and_then(Value::as_u64) {
            Some(FIVE_HOURS) => out.five_hour = Some(window),
            Some(SEVEN_DAYS) => out.seven_day = Some(window),
            _ => {}
        }
    }
    Some(out)
}

/// codex's live limits, from the result of `account/rateLimits/read` on
/// `codex app-server` — what codex's own `/status` shows. Also answers how
/// many free full resets the account holds.
///
/// Preferred over the rollouts: codex writes its limits there only when a
/// turn runs, so when a reached limit prevents turns the rollouts go on
/// showing the last figure before it.
pub fn parse_codex_rate_limits(result: &Value) -> Option<(Limits, u64)> {
    let rl = result
        .pointer("/rateLimitsByLimitId/codex")
        .filter(|v| v.is_object())
        .or_else(|| result.get("rateLimits").filter(|v| v.is_object()))?;
    let mut out = Limits {
        plan: rl.get("planType").and_then(Value::as_str).map(pretty_plan),
        reached: rl.get("rateLimitReachedType").is_some_and(|v| !v.is_null())
            || result.get("ordinaryUsageAllowed") == Some(&Value::Bool(false)),
        ..Limits::default()
    };
    for slot in ["primary", "secondary"] {
        let Some(w) = rl.get(slot).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(used) = w.get("usedPercent").and_then(Value::as_f64) else {
            continue;
        };
        let window = Window {
            used_pct: used.clamp(0.0, 100.0),
            resets_at: w
                .get("resetsAt")
                .and_then(Value::as_i64)
                .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok()),
        };
        match w.get("windowDurationMins").and_then(Value::as_u64) {
            Some(FIVE_HOURS) => out.five_hour = Some(window),
            Some(SEVEN_DAYS) => out.seven_day = Some(window),
            _ => {}
        }
    }
    let resets = result
        .pointer("/rateLimitResetCredits/credits")
        .and_then(Value::as_array)
        .map_or(0, |c| {
            c.iter()
                .filter(|c| c.get("status").and_then(Value::as_str) == Some("available"))
                .count() as u64
        });
    Some((out, resets))
}

/// claude's windows from the body of its OAuth usage endpoint:
/// `five_hour` and `seven_day`, each `{utilization, resets_at}`.
///
/// `None` for a body that is not that shape at all — a changed endpoint must
/// read as "unavailable", not as zero used.
pub fn parse_claude_usage(body: &[u8]) -> Option<(Option<Window>, Option<Window>)> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if v.get("five_hour").is_none() && v.get("seven_day").is_none() {
        return None;
    }
    let window = |key: &str| -> Option<Window> {
        let w = v.get(key)?;
        Some(Window {
            used_pct: w.get("utilization")?.as_f64()?.clamp(0.0, 100.0),
            resets_at: w.get("resets_at").and_then(Value::as_str).and_then(|t| {
                OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok()
            }),
        })
    };
    Some((window("five_hour"), window("seven_day")))
}

/// The plan named in claude's credentials (`~/.claude/.credentials.json`, or
/// the keychain item of the same content): "Max 5x", "Pro".
pub fn claude_plan(credentials: &Value) -> Option<String> {
    let o = credentials.get("claudeAiOauth")?;
    let sub = o.get("subscriptionType").and_then(Value::as_str)?;
    let tier = o
        .get("rateLimitTier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // "default_claude_max_5x" → "5x": the multiple is the tier's last word.
    let multiple = tier.rsplit('_').next().filter(|m| {
        m.len() > 1 && m.ends_with('x') && m[..m.len() - 1].bytes().all(|b| b.is_ascii_digit())
    });
    Some(match multiple {
        Some(m) => format!("{} {m}", pretty_plan(sub)),
        None => pretty_plan(sub),
    })
}

/// The access token in claude's credentials, if it has not expired.
pub fn claude_token(credentials: &Value, now: OffsetDateTime) -> Option<String> {
    let o = credentials.get("claudeAiOauth")?;
    let token = o.get("accessToken")?.as_str()?;
    let expires_ms = o
        .get("expiresAt")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MAX);
    let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
    (!token.is_empty() && expires_ms > now_ms).then(|| token.to_string())
}

/// "max" → "Max", "team_premium" → "Team Premium".
fn pretty_plan(raw: &str) -> String {
    raw.split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(rate_limits: Value) -> String {
        json!({
            "timestamp": "2026-09-24T05:10:00.000Z",
            "type": "event_msg",
            "payload": {"type": "token_count", "info": null, "rate_limits": rate_limits}
        })
        .to_string()
    }

    fn window(used: f64, minutes: u64, resets: i64) -> Value {
        json!({"used_percent": used, "window_minutes": minutes, "resets_at": resets})
    }

    #[test]
    fn codex_windows_are_told_apart_by_length_not_slot() {
        // The weekly window in the primary slot, as a Pro plan reports it.
        let l = parse_codex_line(&line(json!({
            "limit_id": "codex", "primary": window(90.0, 10080, 1_790_411_738),
            "secondary": null, "plan_type": "pro"
        })))
        .unwrap();
        assert_eq!(l.five_hour, None);
        assert_eq!(l.seven_day.as_ref().unwrap().used_pct, 90.0);
        assert_eq!(
            l.seven_day.unwrap().resets_at.unwrap().unix_timestamp(),
            1_790_411_738
        );
        assert_eq!(l.plan.as_deref(), Some("Pro"));
        assert_eq!(l.as_of.unwrap().unix_timestamp(), 1_790_226_600);

        let l = parse_codex_line(&line(json!({
            "primary": window(12.5, 300, 1), "secondary": window(27.0, 10080, 2)
        })))
        .unwrap();
        assert_eq!(l.five_hour.unwrap().used_pct, 12.5);
        assert_eq!(l.seven_day.unwrap().used_pct, 27.0);
    }

    #[test]
    fn codex_lines_without_windows_say_nothing() {
        assert_eq!(parse_codex_line(&line(Value::Null)), None);
        let premium = parse_codex_line(&line(json!({
            "limit_id": "premium", "primary": null, "secondary": null, "plan_type": null
        })))
        .unwrap();
        assert!(premium.is_empty());
        // An unknown window length is not guessed at.
        assert!(
            parse_codex_line(&line(json!({"primary": window(5.0, 60, 1)})))
                .unwrap()
                .is_empty()
        );
        assert_eq!(parse_codex_line("not json"), None);
        // Out-of-range percentages are held to 0–100.
        let l = parse_codex_line(&line(json!({"primary": window(140.0, 300, 1)}))).unwrap();
        assert_eq!(l.five_hour.unwrap().used_pct, 100.0);
    }

    #[test]
    fn codex_takes_the_latest_line_with_windows_across_rollouts() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/09/24");
        std::fs::create_dir_all(&day).unwrap();
        let old = dir.path().join("2026/09/20");
        std::fs::create_dir_all(&old).unwrap();
        // The older rollout has the windows; the newer one only nulls.
        std::fs::write(
            old.join("rollout-a.jsonl"),
            [
                line(json!({"primary": window(10.0, 300, 1), "plan_type": "pro"})),
                line(json!({"primary": window(20.0, 300, 2), "secondary": window(30.0, 10080, 3), "plan_type": "pro"})),
            ]
            .join("\n"),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            day.join("rollout-b.jsonl"),
            [
                line(Value::Null),
                line(json!({"limit_id": "premium", "primary": null, "secondary": null})),
            ]
            .join("\n"),
        )
        .unwrap();
        let l = codex_limits(dir.path());
        assert_eq!(
            l.five_hour.unwrap().used_pct,
            20.0,
            "the later line of the two"
        );
        assert_eq!(l.seven_day.unwrap().used_pct, 30.0);
        assert_eq!(l.plan.as_deref(), Some("Pro"));

        // Only what was written after a known reading is read again.
        let b_mtime = std::fs::metadata(day.join("rollout-b.jsonl"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(codex_limits_newer(dir.path(), Some(b_mtime)), None);
        let (at, _) = codex_limits_newer(dir.path(), None).unwrap();
        assert!(at < b_mtime, "the answer names the rollout it came from");

        // The scan says how far it looked even when it found nothing, so the
        // next one can start after the rollout without limits.
        let nothing = codex_limits_scan(dir.path(), Some(at));
        assert_eq!(nothing.found, None);
        assert_eq!(nothing.newest, Some(b_mtime));
        let found = codex_limits_scan(dir.path(), None);
        assert_eq!(
            found.newest,
            Some(b_mtime),
            "the newest looked at, not the one found"
        );
        assert_eq!(found.found.map(|f| f.0), Some(at));
        assert_eq!(
            codex_limits_scan(dir.path(), Some(b_mtime)),
            CodexScan::default()
        );

        // Nothing anywhere: empty, not an error.
        assert_eq!(codex_limits(&dir.path().join("none")), Limits::default());
    }

    /// The answer a Pro account at its weekly limit gave, 2026-09-24.
    #[test]
    fn codex_live_limits_say_when_the_limit_is_reached() {
        let result = json!({
            "ordinaryUsageAllowed": false,
            "rateLimits": {"limitId": "codex", "primary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1_790_411_738_i64}, "secondary": null, "planType": "pro", "rateLimitReachedType": "rate_limit_reached"},
            "rateLimitsByLimitId": {"codex": {"limitId": "codex", "primary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1_790_411_738_i64}, "secondary": null, "planType": "pro", "rateLimitReachedType": "rate_limit_reached"}},
            "rateLimitResetCredits": {"availableCount": 1, "credits": [{"id": "x", "status": "available"}]}
        });
        let (l, resets) = parse_codex_rate_limits(&result).unwrap();
        assert!(l.reached);
        assert_eq!(l.plan.as_deref(), Some("Pro"));
        assert_eq!(l.five_hour, None);
        assert_eq!(l.seven_day.as_ref().unwrap().used_pct, 100.0);
        assert_eq!(
            l.seven_day.unwrap().resets_at.unwrap().unix_timestamp(),
            1_790_411_738
        );
        assert_eq!(resets, 1);

        // Under the limit, both windows, nothing reached, no credits.
        let result = json!({
            "ordinaryUsageAllowed": true,
            "rateLimits": {"primary": {"usedPercent": 12.5, "windowDurationMins": 300, "resetsAt": 1},
                           "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 2},
                           "planType": "plus", "rateLimitReachedType": null}
        });
        let (l, resets) = parse_codex_rate_limits(&result).unwrap();
        assert!(!l.reached);
        assert_eq!(l.five_hour.unwrap().used_pct, 12.5);
        assert_eq!(l.seven_day.unwrap().used_pct, 40.0);
        assert_eq!(resets, 0);
        // Not that shape at all: nothing.
        assert_eq!(parse_codex_rate_limits(&json!({"error": 1})), None);
    }

    #[test]
    fn claude_usage_reads_both_windows() {
        let body = json!({
            "five_hour": {"utilization": 14.0, "resets_at": "2026-09-24T12:39:59.712889+00:00"},
            "seven_day": {"utilization": 33.0, "resets_at": "2026-09-30T17:59:59.712910+00:00"},
            "seven_day_opus": null,
        })
        .to_string();
        let (five, seven) = parse_claude_usage(body.as_bytes()).unwrap();
        assert_eq!(five.as_ref().unwrap().used_pct, 14.0);
        assert_eq!(
            five.unwrap().resets_at.unwrap().unix_timestamp(),
            1_790_253_599
        );
        assert_eq!(seven.unwrap().used_pct, 33.0);

        // A window the account does not have is absent, not zero.
        let (five, seven) =
            parse_claude_usage(br#"{"five_hour":null,"seven_day":{"utilization":1}}"#).unwrap();
        assert_eq!(five, None);
        assert_eq!(seven.unwrap().resets_at, None);
        // A changed endpoint is unavailable, not "0% used".
        assert_eq!(parse_claude_usage(br#"{"error":"nope"}"#), None);
        assert_eq!(parse_claude_usage(b"<html>"), None);
    }

    #[test]
    fn claude_plan_names_the_tier_multiple() {
        let creds = |sub: &str, tier: &str| json!({"claudeAiOauth": {"subscriptionType": sub, "rateLimitTier": tier}});
        let cases = [
            ("max", "default_claude_max_5x", Some("Max 5x")),
            ("max", "default_claude_max_20x", Some("Max 20x")),
            ("pro", "default_claude_pro", Some("Pro")),
            ("team_premium", "", Some("Team Premium")),
            ("max", "weird_x", Some("Max")),
        ];
        for (sub, tier, want) in cases {
            assert_eq!(
                claude_plan(&creds(sub, tier)).as_deref(),
                want,
                "{sub} {tier}"
            );
        }
        assert_eq!(claude_plan(&json!({})), None);
    }

    #[test]
    fn an_expired_claude_token_is_not_used() {
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let creds = |exp: i64| json!({"claudeAiOauth": {"accessToken": "t", "expiresAt": exp}});
        assert_eq!(claude_token(&creds(1_000_001), now).as_deref(), Some("t"));
        assert_eq!(
            claude_token(&creds(1_000_000), now),
            None,
            "expiring now is expired"
        );
        assert_eq!(
            claude_token(&json!({"claudeAiOauth": {"accessToken": ""}}), now),
            None
        );
    }
}
