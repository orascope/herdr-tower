// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Card command handler for publishing to Discord.

use std::collections::HashMap;
use std::io::Write as _;
use time::OffsetDateTime;

use super::common::*;
use super::Result;

pub fn run_card(args: &[String]) -> Result<()> {
    // Parsed, not scanned, and this one is the sharpest of the three: `--outt`
    // read as "no flag", so a command meant only to print JSON posted to the
    // configured Discord webhook instead.
    let fs = tower_cli::helpcmd::FlagSet::new("card")
        .bool("out", false, "print the card JSON instead of posting it")
        .bool("event", false, "transition-triggered mode (coalesced)");
    let parsed = flags_only("card", &fs, args)?;
    if parsed.bool("out") {
        // Printing takes no lock: it posts nothing, so it cannot collide.
        return publish_card(true);
    }
    publish_locked()
}

/// Publishes under the card lock.
///
/// *Every* publish goes through the lock, not just `--event`. The timer
/// invocation used to publish unlocked, so a scheduled run overlapping an
/// event-triggered one had both processes load the same message ID, edit
/// Discord concurrently, and race on the one `discord-card.json.tmp` —
/// duplicate posts, or a failed rename.
fn publish_locked() -> Result<()> {
    let dir = tower_state::default_dir();
    // The lock wants an `io::Result`, and a boxed error is neither `Send` nor
    // `Sync`. Flattening to the message loses the chain, which costs nothing
    // here: the event path is fired detached from a hook, so the text is the
    // only thing anybody will ever see.
    tower_discord::run_coalesced(&dir, || {
        publish_card(false).map_err(|e| std::io::Error::other(e.to_string()))
    })
    .map_err(Into::into)
}

/// How often the supervisor refreshes the card: the five minutes of the
/// LaunchAgent (`dev.herdr.tower.card`) it replaces.
pub const CARD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// The card, refreshed every [`CARD_INTERVAL`] for as long as the supervisor
/// runs — a thread of `daemon run` instead of a process launchd starts every
/// five minutes. The hooks' `card --event` still publishes on each
/// transition; this is what keeps the card fresh between them.
///
/// Only when a card webhook is configured, and read afresh each time, so
/// setting one takes effect without a restart. A failure is logged when it
/// first happens and when it clears — not every five minutes while Discord is
/// down, into a log nothing trims.
pub fn card_timer() {
    let mut last: Option<String> = None;
    loop {
        if outbound_config().discord_webhook_url.is_some() {
            let now = std::panic::catch_unwind(|| publish_locked().map_err(|e| e.to_string()))
                .unwrap_or_else(|_| Err("the card failed unexpectedly".into()))
                .err();
            if let Some(line) = card_log_line(last.as_deref(), now.as_deref()) {
                eprintln!("herdr-tower card: {line}");
            }
            last = now;
        }
        std::thread::sleep(CARD_INTERVAL);
    }
}

/// What the timer says after a publish, given the error before it and the
/// error now: a new or changed failure, a recovery, or nothing.
fn card_log_line(before: Option<&str>, now: Option<&str>) -> Option<String> {
    match (before, now) {
        (_, Some(e)) if before != Some(e) => Some(format!("not published: {e}")),
        (Some(_), None) => Some("published again".into()),
        _ => None,
    }
}

/// Assembles one card and upserts it.
fn publish_card(out_only: bool) -> Result<()> {
    let store = store()?;
    let now = OffsetDateTime::now_utc();
    let h = sync_panes(&store, now)?;
    let agents = store.list()?;
    let home = home()?;

    let ctx = tower_usage::agent_ctx(
        &agents,
        &tower_usage::codex_sessions_root(&home),
        &tower_usage::gemini_dir(&home),
        &tower_usage::claude_dir(&home),
    );
    let cfg = outbound_config();
    let paths =
        tower_wiring::Paths::detect().unwrap_or_else(|| tower_wiring::Paths::for_home(&home));
    let wired = tower_ui::wired_sessions(&agents, &paths, &cfg.channel_labels);

    let branches: HashMap<String, String> = tower_wt::live_branches(&tower_state::default_dir());

    // The backend has its own screen, so what the tower computed is put back on
    // it. Failures are swallowed inside; a sidebar decoration must not be able
    // to stop the card.
    tower_cli::decorate::decorate_agents(&h, &agents, &ctx, &wired, &branches);

    let card = tower_discord::build_card(
        &tower_discord::CardData {
            agents,
            ctx,
            wired,
            branches,
            def_models: Some(tower_usage::DefaultModels::read(&home)),
            home: home.to_string_lossy().into_owned(),
        },
        now,
    );

    if out_only {
        // `encode_sorted_json`, not `to_sorted_json_pretty`: the encoder terminates with
        // a newline. The distinction lives in `tower_discord` so there is one
        // place for it and one place for the golden to check.
        let body = tower_discord::encode_sorted_json(&card)?;
        std::io::stdout().write_all(&body)?;
        return Ok(());
    }

    let Some(url) = cfg.discord_webhook_url.as_deref() else {
        if tower_config::load().discord_webhook_url.is_some() {
            return Err(
                "HERDR_TOWER_STATE_DIR names a store of its own, so the config's \
                        webhook is not used; set HERDR_TOWER_CONFIG to post from it"
                    .into(),
            );
        }
        return Err(format!(
            "discord_webhook_url is not in the config: {}",
            tower_config::path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(config path unknown)".into())
        )
        .into());
    };
    let state_path = tower_discord::card_state_path(&tower_state::default_dir());
    let mut cs = tower_discord::load_card_state(&state_path);
    let client = tower_discord::Client::new(url);
    cs.message_id = client.upsert(&card, &cs.message_id)?;
    tower_discord::save_card_state(&state_path, &cs)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timer_logs_a_failure_once_and_its_recovery_once() {
        assert_eq!(card_log_line(None, None), None);
        assert_eq!(
            card_log_line(None, Some("403")).as_deref(),
            Some("not published: 403")
        );
        assert_eq!(
            card_log_line(Some("403"), Some("403")),
            None,
            "not again every five minutes"
        );
        assert_eq!(
            card_log_line(Some("403"), Some("timeout")).as_deref(),
            Some("not published: timeout")
        );
        assert_eq!(
            card_log_line(Some("403"), None).as_deref(),
            Some("published again")
        );
    }

    /// Every argument list here has to be refused *before* `publish_card` — which
    /// is also why they are the only lists a unit test may hand `run_card`:
    /// anything the parser accepts goes on to the real store, the real config and
    /// the real Discord webhook.
    ///
    /// `--outt` is the regression itself: scanned rather than parsed, it read as
    /// "no flag", so a command meant only to print JSON posted the card instead.
    /// The near misses around it — a single dash, a value that is not a boolean,
    /// a word that is not a flag at all — all end the same way.
    #[test]
    fn a_misspelled_flag_is_refused_before_anything_is_posted() {
        let cases: [(&[&str], &str); 9] = [
            (&["--outt"], "flag provided but not defined: -outt"),
            (&["-outt"], "flag provided but not defined: -outt"),
            (&["--out-json"], "flag provided but not defined: -out-json"),
            (&["--events"], "flag provided but not defined: -events"),
            (
                &["--out=maybe"],
                "invalid boolean value \"maybe\" for -out: parse error",
            ),
            (
                &["--event="],
                "invalid boolean value \"\" for -event: parse error",
            ),
            (&["---out"], "bad flag syntax: ---out"),
            (
                &["out"],
                "unknown argument: \"out\" — card takes no positional arguments",
            ),
            // The flag parses; the word after it does not belong to `card`, and
            // is not silently dropped.
            (
                &["--out", "extra"],
                "unknown argument: \"extra\" — card takes no positional arguments",
            ),
        ];
        for (args, want) in cases {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let e = run_card(&args).expect_err(&format!("{args:?} is not a valid card invocation"));
            assert_eq!(e.to_string(), want, "for {args:?}");
        }
        // `--help` is a refusal too: it prints the usage and posts nothing.
        assert_eq!(
            run_card(&["--help".to_string()])
                .expect_err("help is not a publish")
                .to_string(),
            "flag: help requested"
        );
    }

    /// Property: a flag spelling `card` does not define is never read as "no
    /// flag", and the message names the spelling the user typed, whatever it is.
    /// The generated flags are all mutations of the two real ones, so each is a
    /// plausible typo; each is one character longer than its base, so none can
    /// collide with `out`, `event`, `help` or `h` — a collision would post the
    /// card.
    #[test]
    fn a_flag_card_does_not_define_is_never_read_as_no_flag() {
        let mut seed: u64 = 20_260_922;
        let mut next = |bound: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % bound
        };
        let alphabet = b"abcdeouvt0189_";
        for _ in 0..200 {
            let base = ["out", "event"][next(2)];
            let at = next(base.len() + 1);
            let mut name = String::from(&base[..at]);
            name.push(alphabet[next(alphabet.len())] as char);
            name.push_str(&base[at..]);
            assert!(name.len() > base.len(), "a mutation, not the flag itself");
            let e = run_card(&[format!("--{name}")]).expect_err("an undefined flag");
            assert_eq!(
                e.to_string(),
                format!("flag provided but not defined: -{name}"),
                "`--{name}` has to be refused by that spelling"
            );
        }
    }

    /// Cross validation against the help text, which is where a user learns what
    /// `card` takes and is written by hand in another crate: every flag the help
    /// line offers has to be a flag this command defines, and a boolean at that.
    /// An undefined boolean answers "not defined" — the same sentence `--outt`
    /// earned, except that here the user's spelling would be the right one.
    ///
    /// The independent reading is the help line itself; `--name=not-a-bool` is
    /// the probe, because it is refused by every flag that exists and so posts
    /// nothing.
    #[test]
    fn every_flag_the_help_offers_for_card_is_a_defined_boolean() {
        let help = tower_cli::helpcmd::help_text();
        let line = help
            .lines()
            .find(|line| line.trim_start().starts_with("card "))
            .expect("the help text still has a `card` line");
        let flags: Vec<&str> = line
            .split_whitespace()
            .map(|word| word.trim_matches(|c| c == '[' || c == ']'))
            .filter_map(|word| word.strip_prefix("--"))
            .collect();
        assert_eq!(
            flags,
            ["out", "event"],
            "the help line for `card` now reads: {line}"
        );
        for flag in flags {
            let e = run_card(&[format!("--{flag}=not-a-bool")])
                .expect_err("a value that is not a boolean");
            assert_eq!(
                e.to_string(),
                format!("invalid boolean value \"not-a-bool\" for -{flag}: parse error"),
                "the help offers `--{flag}`, so `card` has to define it as a boolean"
            );
        }
    }
}
