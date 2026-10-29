// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Notifications for state transitions.
//!
//! The caller decides *whether* something happened — this crate only delivers.
//! Failures are swallowed on purpose: a notification that cannot be sent must
//! never interrupt the hook that triggered it, because that hook is running
//! inside the agent's own process.

use tower_config::Config;
use tower_state::{Agent, AgentState};

/// How a notification reaches the user. Injected so tests never shell out or
/// touch the network.
#[derive(Default)]
pub struct Sender {
    /// Desktop notification. `None` when the platform has no way to show one.
    pub notify: Option<NotifyFn>,
    /// Discord webhook POST.
    pub post_json: Option<PostFn>,
}

/// Shows a desktop notification: title, then body.
pub type NotifyFn = Box<dyn Fn(&str, &str) -> std::io::Result<()>>;

/// Posts a JSON body to a webhook URL.
pub type PostFn = Box<dyn Fn(&str, &[u8]) -> std::io::Result<()>>;

/// Which transitions are worth interrupting someone for.
///
/// Only the ones that need a person: a finished turn they have not seen, a
/// question, or an error. WORKING and IDLE are the tower's job to display, not
/// to announce — notifying on those would train the user to ignore all of them.
pub fn notifiable(to: AgentState) -> bool {
    matches!(
        to,
        AgentState::DoneUnread | AgentState::Waiting | AgentState::Error
    )
}

/// The notification title.
///
/// Named by session rather than by kind, because a user with four Claude
/// sessions needs to know *which* one is asking.
///
/// English, unlike the rest of what this tool says. A desktop notification is
/// the one message that arrives where the tool is not: on a lock screen, in a
/// notification centre, over somebody's shoulder in a screen share. The body
/// beside it is the person's own task text and stays whatever they wrote.
pub fn title(a: &Agent, to: AgentState) -> String {
    let name = if a.pane.session.is_empty() {
        &a.kind
    } else {
        &a.pane.session
    };
    match to {
        AgentState::DoneUnread => format!("✔ {name} done"),
        AgentState::Waiting => format!("◆ {name} needs input"),
        _ => format!("✖ {name} error"),
    }
}

/// Delivers a transition notification, if it is one worth sending.
///
/// A transition to the state it was already in is not a transition; sending on
/// every heartbeat would make the notifications worthless.
pub fn notify(cfg: &Config, s: &Sender, a: &Agent, prev: AgentState, to: AgentState) {
    if prev == to || !notifiable(to) {
        return;
    }
    deliver(cfg, s, a, &title(a, to));
}

/// The second announcement of a WAIT that has gone unanswered for `waited`.
///
/// Said as a reminder in the title, so it does not read as a new question.
pub fn remind(cfg: &Config, s: &Sender, a: &Agent, waited: std::time::Duration) {
    let name = if a.pane.session.is_empty() {
        &a.kind
    } else {
        &a.pane.session
    };
    let mins = (waited.as_secs() / 60).max(1);
    deliver(cfg, s, a, &format!("◆ {name} still needs input ({mins}m)"));
}

/// A notice about the tower itself rather than one agent — the usage guard
/// holding or releasing an AI, its floors changing: a desktop notification
/// and a line in the notification channel, as the settings allow.
pub fn announce(cfg: &Config, s: &Sender, title: &str, body: &str) {
    if cfg.macos_enabled() {
        if let Some(f) = &s.notify {
            let _ = f(title, body);
        }
    }
    if cfg.notify_discord {
        if let (Some(url), Some(post)) = (cfg.notify_url(), &s.post_json) {
            let payload = serde_json::json!({
                "username": "herdr-tower",
                "content": format!("{title} — {body}"),
                "allowed_mentions": { "parse": [] },
            });
            if let Ok(body) = serde_json::to_vec(&payload) {
                let _ = post(url, &body);
            }
        }
    }
}

fn deliver(cfg: &Config, s: &Sender, a: &Agent, t: &str) {
    // `attention`, not `headline`: the row can lead with what the pane calls
    // itself because the state column is beside it, and a notification cannot. The
    // title here already says which session and which state, so the body is the
    // only room left to say what needs doing.
    let body = if a.attention().is_empty() {
        a.cwd.as_str()
    } else {
        a.attention()
    };
    // Through `announce`, so the agent's own text cannot mention anyone in
    // the channel either.
    announce(cfg, s, t, body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn at() -> time::OffsetDateTime {
        time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    fn agent(session: &str) -> Agent {
        let mut a = Agent::new("claude-1", "claude", at());
        a.pane.session = session.into();
        a.cwd = "/proj".into();
        a
    }

    /// What one channel recorded, shared between the sender and the assertion.
    type Log = Rc<RefCell<Vec<String>>>;

    /// Records what each channel was asked to send.
    fn recorder() -> (Sender, Log, Log) {
        let desktop = Rc::new(RefCell::new(Vec::new()));
        let posts = Rc::new(RefCell::new(Vec::new()));
        let d = desktop.clone();
        let p = posts.clone();
        let s = Sender {
            notify: Some(Box::new(move |t, b| {
                d.borrow_mut().push(format!("{t}|{b}"));
                Ok(())
            })),
            post_json: Some(Box::new(move |url, body| {
                p.borrow_mut()
                    .push(format!("{url}|{}", String::from_utf8_lossy(body)));
                Ok(())
            })),
        };
        (s, desktop, posts)
    }

    /// Only what needs a person. Announcing WORKING would train the user to
    /// ignore every notification.
    #[test]
    fn only_transitions_that_need_a_person() {
        assert!(notifiable(AgentState::DoneUnread));
        assert!(notifiable(AgentState::Waiting));
        assert!(notifiable(AgentState::Error));
        assert!(!notifiable(AgentState::Working));
        assert!(!notifiable(AgentState::Idle));
        assert!(!notifiable(AgentState::Dead));
    }

    /// A heartbeat is not a transition.
    #[test]
    fn same_state_sends_nothing() {
        let (s, desktop, _) = recorder();
        let cfg = Config::default();
        notify(
            &cfg,
            &s,
            &agent("dev"),
            AgentState::Waiting,
            AgentState::Waiting,
        );
        assert!(desktop.borrow().is_empty());
    }

    /// The title says which session and which state; the body is the only room
    /// left to say what needs doing.
    ///
    /// What the pane calls itself — its terminal title, or a `/rename` label —
    /// leads the tower's row, because the state column is beside it and the
    /// detail card is one key away. A notification on a phone has neither, so a
    /// permission prompt reading `dbeaver` would have spent its one line on what
    /// the title already said.
    #[test]
    fn the_body_says_what_needs_doing_not_what_the_pane_is_called() {
        let (s, desktop, _) = recorder();
        let cfg = Config {
            notify_macos: Some(true),
            ..Config::default()
        };

        let mut a = agent("collab-bot");
        a.label = "dbeaver".into();
        a.ask = "awaiting approval".into();
        notify(&cfg, &s, &a, AgentState::Working, AgentState::Waiting);
        assert_eq!(
            desktop.borrow().as_slice(),
            ["◆ collab-bot needs input|awaiting approval"],
            "the row would say `dbeaver` here, and a notification must not"
        );

        // With nothing to act on, the label still beats the bare folder path.
        let mut quiet = agent("collab-bot");
        quiet.label = "dbeaver".into();
        notify(
            &cfg,
            &s,
            &quiet,
            AgentState::Working,
            AgentState::DoneUnread,
        );
        assert_eq!(desktop.borrow()[1], "✔ collab-bot done|dbeaver");
    }

    /// Four Claude sessions need distinguishing by session, not by kind.
    #[test]
    fn title_names_the_session() {
        let a = agent("collab-bot");
        assert_eq!(title(&a, AgentState::Waiting), "◆ collab-bot needs input");
        assert_eq!(title(&a, AgentState::DoneUnread), "✔ collab-bot done");

        let nameless = agent("");
        assert_eq!(
            title(&nameless, AgentState::Error),
            "✖ claude error",
            "falls back to the kind"
        );
    }

    #[test]
    fn desktop_can_be_turned_off() {
        let (s, desktop, _) = recorder();
        let cfg: Config = serde_json::from_str(r#"{"notify_macos":false}"#).unwrap();
        notify(
            &cfg,
            &s,
            &agent("dev"),
            AgentState::Working,
            AgentState::Waiting,
        );
        assert!(desktop.borrow().is_empty());
    }

    /// Discord is opt-in, and silent without a webhook even when enabled.
    #[test]
    fn discord_needs_both_the_flag_and_a_webhook() {
        let (s, _, posts) = recorder();

        let flag_only: Config = serde_json::from_str(r#"{"notify_discord":true}"#).unwrap();
        notify(
            &flag_only,
            &s,
            &agent("dev"),
            AgentState::Working,
            AgentState::Waiting,
        );
        assert!(posts.borrow().is_empty(), "no webhook configured");

        let both: Config =
            serde_json::from_str(r#"{"notify_discord":true,"discord_webhook_url":"https://hook"}"#)
                .unwrap();
        notify(
            &both,
            &s,
            &agent("dev"),
            AgentState::Working,
            AgentState::Waiting,
        );
        assert_eq!(posts.borrow().len(), 1);
        assert!(posts.borrow()[0].contains("needs input"));
    }

    /// A record with no task still says something useful.
    #[test]
    fn body_falls_back_to_the_folder() {
        let (s, desktop, _) = recorder();
        notify(
            &Config::default(),
            &s,
            &agent("dev"),
            AgentState::Working,
            AgentState::DoneUnread,
        );
        assert!(desktop.borrow()[0].ends_with("|/proj"));
    }

    /// A reminder names the session and how long it has waited, and carries
    /// the question like the first notification did.
    #[test]
    fn a_reminder_says_it_is_one() {
        let seen = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
        let log = seen.clone();
        let s = Sender {
            notify: Some(Box::new(move |t, b| {
                log.borrow_mut().push((t.into(), b.into()));
                Ok(())
            })),
            post_json: None,
        };
        let mut a = agent("20.herdr-tower");
        a.ask = "Allow bash: ls?".into();
        remind(
            &Config::default(),
            &s,
            &a,
            std::time::Duration::from_secs(15 * 60),
        );
        let got = seen.borrow();
        assert_eq!(got[0].0, "◆ 20.herdr-tower still needs input (15m)");
        assert_eq!(got[0].1, "Allow bash: ls?");
    }

    /// The agent's own text goes into the channel, so it can mention no one:
    /// every post, a transition or a reminder, turns mentions off.
    #[test]
    fn a_post_mentions_no_one() {
        let (s, _, posts) = recorder();
        let cfg: Config =
            serde_json::from_str(r#"{"notify_discord":true,"discord_webhook_url":"https://hook"}"#)
                .unwrap();
        let mut a = agent("dev");
        a.ask = "@everyone <@123> look".into();
        notify(&cfg, &s, &a, AgentState::Working, AgentState::Waiting);
        remind(&cfg, &s, &a, std::time::Duration::from_secs(900));
        announce(&cfg, &s, "t", "b");
        let posts = posts.borrow();
        assert_eq!(posts.len(), 3);
        for p in posts.iter() {
            assert!(p.contains(r#""allowed_mentions":{"parse":[]}"#), "{p}");
        }
    }
}
