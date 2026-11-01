// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Which release is the newest on GitHub, for the tower's header: the
//! supervisor asks every [`CHECK_EVERY`] and puts the tag in `daemon.json`
//! (`latest_release`), and the tower offers it once it is newer than the
//! supervisor itself.
//!
//! One unauthenticated GET of the public releases API, carrying nothing but
//! the user agent. `update_check = false` turns it off, and then nothing is
//! asked at all.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The public API answer naming the newest release that is not a draft or a
/// prerelease.
pub const LATEST_URL: &str = "https://api.github.com/repos/orascope/herdr-tower/releases/latest";

/// Releases come days apart; a few requests a day find one the same day.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// After a failed request — offline, rate-limited — sooner than a full
/// period, without hammering a network that is down.
pub const RETRY_EVERY: Duration = Duration::from_secs(60 * 60);

const TIMEOUT: Duration = Duration::from_secs(10);

/// A tag longer than this is not one of ours.
const TAG_MAX: usize = 32;

/// The newest tag as last read, kept by a thread of its own.
#[derive(Clone, Default)]
pub struct Refresher {
    latest: Arc<Mutex<String>>,
}

impl Refresher {
    /// Starts the thread. It reads the setting before every request, so
    /// turning the check off stops the requests without a restart, and
    /// forgets the tag it had.
    pub fn spawn(version: String) -> Self {
        let r = Refresher::default();
        let latest = r.latest.clone();
        let _ = std::thread::Builder::new()
            .name("release-check".into())
            .spawn(move || loop {
                // `None` keeps the tag already known: a failed request — offline
                // for an hour — is no news about the releases.
                let (found, wait) = if tower_config::load().update_check_enabled() {
                    match fetch(&version) {
                        Some(tag) => (Some(tag), CHECK_EVERY),
                        None => (None, RETRY_EVERY),
                    }
                } else {
                    (Some(String::new()), CHECK_EVERY)
                };
                if let (Some(tag), Ok(mut l)) = (found, latest.lock()) {
                    *l = tag;
                }
                std::thread::sleep(wait);
            });
        r
    }

    /// The newest tag, or empty before the first answer and while the check
    /// is off.
    pub fn latest(&self) -> String {
        self.latest.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

fn fetch(version: &str) -> Option<String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build(),
    );
    let ua = format!("herdr-tower/{}", version.trim_start_matches('v'));
    let body = agent
        .get(LATEST_URL)
        .header("User-Agent", &ua)
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_body()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .ok()?;
    tag_of(&body)
}

/// The release's tag from the API's answer, when it looks like one of ours
/// (`v` and three numbers).
pub fn tag_of(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    (tag.len() <= TAG_MAX && tower_state::release_triple(tag).is_some()).then(|| tag.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "asks the real GitHub API; run by hand"]
    fn the_real_api_answers_with_a_tag() {
        let tag = fetch("v0.0.0").expect("an answer");
        assert!(tower_state::release_triple(&tag).is_some(), "{tag}");
    }

    #[test]
    fn the_tag_comes_from_tag_name() {
        let body = r#"{"tag_name":"v0.61.7","name":"v0.61.7","draft":false}"#;
        assert_eq!(tag_of(body).as_deref(), Some("v0.61.7"));
    }

    /// Round trip: any tag `release_triple` accepts, put in an API answer
    /// by a real JSON writer beside the fields GitHub sends, comes back
    /// unchanged; and every tag `tag_of` returns is one `release_triple`
    /// accepts (cross validation of the two gates).
    #[test]
    fn a_tag_survives_the_api_answer() {
        let mut rng = tower_testrng::Rng::new(0x7461_675f_6f66);
        for _ in 0..500 {
            let tag = format!(
                "v{}.{}.{}",
                rng.below(1000),
                rng.below(1000),
                rng.below(1000)
            );
            let body = serde_json::json!({
                "url": "https://api.github.com/repos/orascope/herdr-tower/releases/1",
                "tag_name": tag,
                "name": tag,
                "draft": false,
                "prerelease": false,
                "assets": [{"name": "checksums.txt", "download_count": 4}],
            })
            .to_string();
            let got = tag_of(&body);
            assert_eq!(got.as_deref(), Some(tag.as_str()));
            assert!(tower_state::release_triple(got.as_deref().unwrap()).is_some());
        }
    }

    /// Edge: the length cap is inclusive at 32, and a suffix the triple
    /// reader would drop (`-rc1`) is still the tag as GitHub named it.
    #[test]
    fn the_length_cap_is_inclusive() {
        let at_cap = format!("v1.2.{}", "9".repeat(32 - "v1.2.".len()));
        assert_eq!(at_cap.len(), 32);
        let over = format!("{at_cap}9");
        let body = |t: &str| serde_json::json!({ "tag_name": t }).to_string();
        // Thirty digits overflow u64, so the cap is not what refuses it.
        assert_eq!(tag_of(&body(&at_cap)), None);
        let short_at_cap = format!("v1.2.3-{}", "x".repeat(32 - "v1.2.3-".len()));
        assert_eq!(short_at_cap.len(), 32);
        assert_eq!(
            tag_of(&body(&short_at_cap)).as_deref(),
            Some(short_at_cap.as_str())
        );
        assert_eq!(tag_of(&body(&format!("{short_at_cap}x"))), None);
        assert_eq!(tag_of(&body(&over)), None);
        assert_eq!(tag_of(&body("v1.2.3-rc1")).as_deref(), Some("v1.2.3-rc1"));
    }

    #[test]
    fn anything_but_a_version_tag_is_refused() {
        for body in [
            r#"{"tag_name":"nightly"}"#,
            r#"{"tag_name":"v1.2"}"#,
            r#"{"tag_name":"v1.2.3\u001b[31m"}"#,
            r#"{"tag_name":7}"#,
            r#"{"message":"API rate limit exceeded"}"#,
            "not json",
            "",
        ] {
            assert_eq!(tag_of(body), None, "{body}");
        }
        let long = format!(r#"{{"tag_name":"v1.2.{}"}}"#, "9".repeat(40));
        assert_eq!(tag_of(&long), None);
    }
}
