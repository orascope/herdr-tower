// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Talking to the webhook, and remembering which message the card is.
//!
//! Two rules run through this module.
//!
//! **The webhook URL never appears in an error.** It contains the token, and an
//! error string ends up in logs, in a LaunchAgent's stderr file, and pasted into
//! a chat when somebody asks for help. Every failure here reports a fixed
//! sentence plus, at most, Discord's own response body — which does not carry the
//! URL. This is why the error text is deliberately vague about *which* request
//! failed.
//!
//! **The old message is deleted only after the new one lands.** A card that
//! vanishes from the channel is worse than a duplicate: the duplicate is visible
//! and self-correcting, the absence looks like the tower died.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::json::{to_sorted_json, to_sorted_json_pretty, Component};
use crate::write_private;

/// Tells Discord the message uses Components V2. Without it the `components`
/// array is rejected outright.
pub const FLAG_COMPONENTS_V2: u32 = 32768;

/// Sent as both the webhook display name and the User-Agent, so a card in the
/// channel and a request in a proxy log are attributable to the same thing.
const USER_AGENT: &str = "herdr-tower";

/// Which message to edit.
///
/// A file written by an older build may still carry `last_levels`, the limit
/// levels it used to ping about; it is read past, and the next save drops it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CardState {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message_id: String,
}

pub fn card_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("discord-card.json")
}

/// Reads the card state, or an empty one.
///
/// A missing, unreadable or corrupt file is not an error: the worst outcome of
/// forgetting the message ID is one duplicate card, and the alternative — a
/// publisher that refuses to run until somebody deletes a file — takes the
/// dashboard down.
///
/// "Corrupt" includes a file with the right shape but a wrong *type*
/// (`"message_id": 5`): serde discards the whole document, as it does a
/// truncated one, and a fresh card is posted. Only a hand-edited file reaches
/// that branch, and losing the ID is the predictable outcome.
pub fn load_card_state(path: &Path) -> CardState {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes the card state, never leaving a half-written file behind.
///
/// A truncated state file reads as "no message" on the next run, which posts a
/// second card and orphans the first — so the temp-then-rename is not
/// belt-and-braces here, it is the thing that keeps one card one card.
pub fn save_card_state(path: &Path, s: &CardState) -> io::Result<()> {
    let body = to_sorted_json_pretty(s)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    write_private(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

/// The HTTP verbs the webhook API needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Post,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// One outbound call.
#[derive(Debug)]
pub struct Request<'a> {
    pub method: Method,
    pub url: &'a str,
    /// `None` for a request with no body, which is also how the sender knows not
    /// to set a `Content-Type` — the header goes out only with a payload.
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    /// How much of the response to keep. The body only ever feeds a diagnostic
    /// message, so a webhook answering with a gigabyte must not be able to
    /// exhaust the tower; anything past the limit is dropped, not an error.
    pub response_limit: u64,
    /// Whether the *body* is part of the answer.
    ///
    /// False for everything whose body only feeds a diagnostic string — there a
    /// truncated explanation beats turning a real HTTP status into "request
    /// failed". True for the one request whose body carries the message ID: a
    /// create whose body could not be read is an accepted message with no handle,
    /// and reporting that as success meant the next run posted a second card and
    /// could not delete the first.
    pub needs_body: bool,
}

/// What came back, with the body already read.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The message ID Discord assigns a newly created message. Empty when the
    /// response was not a JSON object with a string `id` — which is what dropping
    /// `?wait=true` looks like, and what a truncated body looks like. The caller
    /// treats an empty ID from an accepted post as a failure: without it the card
    /// is live and unreachable, and every following run posts another.
    fn id(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("id")?.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// Discord's own explanation, for the error message.
    ///
    /// The literal `<nil>` for a 3xx is not decoration: only a 4xx or 5xx
    /// carries an explanation, and `<nil>` is what this message has always
    /// printed in its place for a redirect. Kept stable so stderr can be diffed
    /// like the payloads.
    fn error_body(&self) -> String {
        if self.status >= 400 {
            String::from_utf8_lossy(&self.body).into_owned()
        } else {
            "<nil>".into()
        }
    }
}

/// How a request actually reaches the network.
///
/// Injected, like `tower_notify`'s `PostFn`, so the tests here exercise the retry
/// and delete-ordering logic without a socket. A test that needs a live webhook
/// is a test nobody runs.
pub type SendFn = Box<dyn Fn(&Request) -> io::Result<Response>>;

/// The webhook caller. One per webhook URL; it holds the connection pool.
pub struct Client {
    webhook: String,
    send: SendFn,
}

impl Client {
    /// A client that really talks to Discord.
    pub fn new(webhook: impl Into<String>) -> Self {
        let agent = crate::http::agent();
        Client {
            webhook: webhook.into(),
            send: Box::new(move |req| crate::http::send(&agent, req)),
        }
    }

    /// A client with the network replaced.
    pub fn with_sender(webhook: impl Into<String>, send: SendFn) -> Self {
        Client {
            webhook: webhook.into(),
            send,
        }
    }

    pub(crate) fn dispatch(&self, req: &Request) -> io::Result<Response> {
        (self.send)(req)
    }

    /// The configured webhook with `path` appended and `params` merged into its
    /// query.
    ///
    /// Not string concatenation. A webhook URL is allowed a query of its own —
    /// Discord's `?thread_id=…` is the ordinary case — and appending a second `?`
    /// produced `thread_id=123?wait=true`, so `wait` never took effect and the
    /// create came back without the message ID. The edit path was worse: it put
    /// `/messages/<id>` *after* the query, addressing a URL that names no message
    /// at all.
    fn url(&self, path: &str, params: &[(&str, &str)]) -> String {
        let (base, existing) = match self.webhook.split_once('?') {
            Some((b, q)) => (b, q),
            None => (self.webhook.as_str(), ""),
        };
        let mut query: Vec<String> = Vec::new();
        if !existing.is_empty() {
            query.push(existing.to_string());
        }
        for (k, v) in params {
            query.push(format!("{k}={v}"));
        }
        let mut out = format!("{}{path}", base.trim_end_matches('/'));
        if !query.is_empty() {
            out.push('?');
            out.push_str(&query.join("&"));
        }
        out
    }

    fn request(&self, method: Method, url: &str, payload: Option<&[u8]>) -> io::Result<Response> {
        self.request_for(method, url, payload, false)
    }

    /// The same, saying whether the response body is part of the answer.
    fn request_for(
        &self,
        method: Method,
        url: &str,
        payload: Option<&[u8]>,
        needs_body: bool,
    ) -> io::Result<Response> {
        self.dispatch(&Request {
            method,
            url,
            content_type: payload.map(|_| "application/json"),
            body: payload.unwrap_or(&[]),
            response_limit: 1 << 20, // a card response is a few KB; be generous
            needs_body,
        })
    }

    /// Edits the card message, creating it if there is nothing to edit.
    /// Returns the ID the card now lives at.
    ///
    /// The three ways an edit can fail are not the same failure:
    ///
    /// * **404** — somebody deleted the message. Post a new one; there is
    ///   nothing left to clean up.
    /// * **400** — Discord rejected the *content*, usually because a component
    ///   grew past a limit. Post a new one and only then delete the old, so a
    ///   failed post leaves the channel showing a stale card instead of none.
    /// * anything else — report it. Retrying a 401 or a 429 by posting would
    ///   just double the damage.
    pub fn upsert(&self, components: &[Component], mid: &str) -> io::Result<String> {
        let mut edit_was_rejected = false;
        if !mid.is_empty() {
            let body = to_sorted_json(&EditBody { components })?;
            let res = self.request(
                Method::Patch,
                &self.url(&format!("/messages/{mid}"), &[("with_components", "true")]),
                Some(&body),
            )?;
            if res.ok() {
                return Ok(mid.to_string());
            }
            match res.status {
                400 => edit_was_rejected = true,
                404 => {}
                code => {
                    return Err(io::Error::other(format!(
                        "discord edit failed (HTTP {code}): {}",
                        res.error_body()
                    )))
                }
            }
        }

        let body = to_sorted_json(&PostBody {
            allowed_mentions: NoMentions::default(),
            components,
            flags: FLAG_COMPONENTS_V2,
            username: USER_AGENT,
        })?;
        let res = self.request_for(
            Method::Post,
            &self.url("", &[("wait", "true"), ("with_components", "true")]),
            Some(&body),
            true,
        )?;
        if !res.ok() {
            return Err(io::Error::other(format!(
                "discord post failed (HTTP {}): {}",
                res.status,
                res.error_body()
            )));
        }
        let id = res.id();
        // An accepted card whose ID never arrived is not a published card. The
        // handle is the only way the next run edits *this* message instead of
        // posting another one beside it, so a missing one is reported rather than
        // stored as an empty string.
        if id.is_empty() {
            return Err(io::Error::other(
                "discord accepted the post but returned no message ID — the next run cannot edit the same card",
            ));
        }
        if edit_was_rejected && !mid.is_empty() && !id.is_empty() {
            // Best effort. A card that failed to be tidied away is a cosmetic
            // problem; failing the publish over it is not.
            let _ = self.request(
                Method::Delete,
                &self.url(&format!("/messages/{mid}"), &[]),
                None,
            );
        }
        Ok(id)
    }
}

// The two request bodies. Fields are declared in alphabetical order, which
// is wire order; see `crate::json`. Do not reorder.

#[derive(Serialize)]
struct EditBody<'a> {
    components: &'a [Component],
}

#[derive(Serialize)]
struct PostBody<'a> {
    allowed_mentions: NoMentions,
    components: &'a [Component],
    flags: u32,
    username: &'a str,
}

/// `{"parse": []}`: nothing in the message pings anyone.
///
/// A new message is what raises notifications, and the one this crate posts
/// carries text an agent chose — the card shows a pending question, the task,
/// the folder and the branch. A repository that gets an agent to ask something
/// with `@everyone` or `<@&role>` in it would otherwise ping the whole channel
/// from the user's webhook. An empty `parse` list turns off every kind of
/// mention, including the ones spelled out in the text. `tower_notify` sends
/// the same value.
#[derive(Serialize, Default)]
struct NoMentions {
    parse: [&'static str; 0],
}

#[cfg(test)]
mod tests {

    /// A webhook URL may carry a query of its own — Discord's `thread_id` is the
    /// ordinary case — and the extra parameters have to *merge* with it.
    ///
    /// Appending a second `?` produced `thread_id=123?wait=true`, so `wait` never
    /// took effect and the create came back with no message ID; the edit path put
    /// `/messages/<id>` after the query and addressed nothing.
    #[test]
    fn card_parameters_merge_into_a_webhook_that_already_has_a_query() {
        let plain = Client::new("https://discord.com/api/webhooks/1/tok");
        assert_eq!(
            plain.url("", &[("wait", "true")]),
            "https://discord.com/api/webhooks/1/tok?wait=true"
        );
        assert_eq!(
            plain.url("/messages/42", &[]),
            "https://discord.com/api/webhooks/1/tok/messages/42"
        );

        let threaded = Client::new("https://discord.com/api/webhooks/1/tok?thread_id=9");
        assert_eq!(
            threaded.url("", &[("wait", "true"), ("with_components", "true")]),
            "https://discord.com/api/webhooks/1/tok?thread_id=9&wait=true&with_components=true"
        );
        assert_eq!(
            threaded.url("/messages/42", &[("with_components", "true")]),
            "https://discord.com/api/webhooks/1/tok/messages/42?thread_id=9&with_components=true"
        );

        // A trailing slash is not a path segment.
        let slashed = Client::new("https://discord.com/api/webhooks/1/tok/");
        assert_eq!(
            slashed.url("/messages/42", &[]),
            "https://discord.com/api/webhooks/1/tok/messages/42"
        );
    }
    use super::*;
    use crate::card::tests::{data, t0};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What the client asked for, and what it was told.
    type Calls = Rc<RefCell<Vec<String>>>;

    /// A client whose network is a script of `(status, body)` answers, in order.
    fn scripted(webhook: &str, answers: Vec<(u16, &'static str)>) -> (Client, Calls) {
        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let log = calls.clone();
        let answers = RefCell::new(answers.into_iter());
        let client = Client::with_sender(
            webhook,
            Box::new(move |req| {
                log.borrow_mut().push(format!(
                    "{} {}\n{}",
                    req.method.as_str(),
                    req.url,
                    String::from_utf8_lossy(req.body)
                ));
                let (status, body) = answers.borrow_mut().next().unwrap_or((200, "{}"));
                Ok(Response {
                    status,
                    body: body.as_bytes().to_vec(),
                })
            }),
        );
        (client, calls)
    }

    fn components() -> Vec<Component> {
        vec![Component::text("hi")]
    }

    /// The exact POST body sent for the shared fixture.
    const POST_BODY: &str = include_str!("testdata/post_body.json");
    /// The request bodies this client is held to — recorded, and rewritten
    /// once, when the product stopped speaking Korean. What they still pin is
    /// the envelope: the field order, the flags, the escaping.
    /// `UPDATE_EXPECTED=1 cargo test` rewrites them, deliberately.
    const PATCH_BODY: &str = include_str!("testdata/patch_body.json");

    #[test]
    fn the_webhook_payload_is_byte_identical_to_its_recorded_body() {
        let comps = crate::card::build_card(&data(), t0());

        let (client, calls) = scripted("https://hook", vec![(200, r#"{"id":"999"}"#)]);
        client.upsert(&comps, "").unwrap();
        let post = calls.borrow()[0].clone();
        let (head, body) = post.split_once('\n').unwrap();
        assert_eq!(head, "POST https://hook?wait=true&with_components=true");
        if std::env::var_os("UPDATE_EXPECTED").is_some() {
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata");
            std::fs::write(format!("{dir}/post_body.json"), format!("{body}\n")).unwrap();
        } else {
            assert_eq!(body, POST_BODY.trim_end_matches('\n'));
        }

        let (client, calls) = scripted("https://hook", vec![(200, "{}")]);
        client.upsert(&comps, "42").unwrap();
        let patch = calls.borrow()[0].clone();
        let (head, body) = patch.split_once('\n').unwrap();
        assert_eq!(head, "PATCH https://hook/messages/42?with_components=true");
        let updating = std::env::var_os("UPDATE_EXPECTED").is_some();
        if updating {
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata");
            std::fs::write(format!("{dir}/patch_body.json"), format!("{body}\n")).unwrap();
        } else {
            assert_eq!(body, PATCH_BODY.trim_end_matches('\n'));
        }
    }

    /// The common case: one message, edited forever.
    #[test]
    fn a_successful_edit_keeps_the_message_it_edited() {
        let (client, calls) = scripted("https://hook", vec![(200, "{}")]);
        assert_eq!(client.upsert(&components(), "123").unwrap(), "123");
        assert_eq!(calls.borrow().len(), 1, "no post after a good edit");
    }

    /// Somebody deleted the card. Post a new one and adopt its ID.
    #[test]
    fn a_deleted_message_is_posted_again() {
        let (client, calls) = scripted("https://hook", vec![(404, ""), (200, r#"{"id":"999"}"#)]);
        assert_eq!(client.upsert(&components(), "123").unwrap(), "999");
        let calls = calls.borrow();
        assert_eq!(calls.len(), 2, "no delete: there is nothing there");
        assert!(calls[0].starts_with("PATCH "));
        assert!(calls[1].starts_with("POST "));
    }

    /// The ordering rule, stated as a test: the delete comes last, and only
    /// because the post succeeded.
    #[test]
    fn a_rejected_edit_deletes_the_old_card_only_after_the_new_one_lands() {
        let (client, calls) = scripted(
            "https://hook",
            vec![(400, "too long"), (200, r#"{"id":"999"}"#)],
        );
        assert_eq!(client.upsert(&components(), "123").unwrap(), "999");
        let calls = calls.borrow();
        assert_eq!(calls.len(), 3);
        assert!(
            calls[2].starts_with("DELETE https://hook/messages/123"),
            "{:?}",
            calls[2]
        );
    }

    /// If the replacement never landed, the old card is the only one the channel
    /// has. Deleting it would leave the dashboard blank.
    #[test]
    fn a_rejected_edit_and_a_failed_post_leave_the_old_card_alone() {
        let (client, calls) = scripted("https://hook", vec![(400, "too long"), (500, "boom")]);
        assert!(client.upsert(&components(), "123").is_err());
        let calls = calls.borrow();
        assert_eq!(calls.len(), 2, "no delete: {calls:?}");
    }

    /// A 401 means the webhook is wrong, not that the message is. Posting would
    /// double the damage.
    #[test]
    fn an_unexpected_edit_failure_is_reported_rather_than_worked_around() {
        let (client, calls) = scripted("https://hook", vec![(401, "unauthorized")]);
        let err = client.upsert(&components(), "123").unwrap_err();
        assert!(err.to_string().contains("HTTP 401"), "{err}");
        assert!(err.to_string().contains("unauthorized"), "{err}");
        assert_eq!(calls.borrow().len(), 1);
    }

    /// The error text lands in logs and gets pasted into chats. The token must
    /// not travel with it.
    #[test]
    fn no_failure_ever_names_the_webhook() {
        let url = "https://discord.com/api/webhooks/123/SECRET";
        for answers in [
            vec![(401, "nope")],
            vec![(404, ""), (500, "nope")],
            vec![(500, "nope")],
        ] {
            let (client, _) = scripted(url, answers);
            let err = client.upsert(&components(), "123").unwrap_err().to_string();
            assert!(!err.contains("SECRET"), "{err}");
        }
    }

    /// A post Discord accepted without answering with an ID is a failure, not an
    /// empty success.
    ///
    /// It used to return `""`, and the caller stored that: the card was live and
    /// unreachable, so every following run posted another one beside it and could
    /// delete none of them. Reporting it leaves one orphan and a message saying
    /// why, instead of one per cycle in silence.
    #[test]
    fn a_post_with_no_id_in_the_answer_is_reported() {
        let (client, _) = scripted("https://hook", vec![(204, "")]);
        let err = client.upsert(&components(), "").unwrap_err().to_string();
        assert!(err.contains("no message ID"), "{err}");
    }

    const STATE: &str = include_str!("testdata/card_state.json");
    const STATE_EMPTY: &str = include_str!("testdata/card_state_empty.json");

    /// The bytes are the contract, not just the fields.
    #[test]
    fn the_state_file_is_byte_identical_to_its_recorded_shape() {
        let s = CardState {
            message_id: "42".into(),
        };
        let out = String::from_utf8(to_sorted_json_pretty(&s).unwrap()).unwrap();
        assert_eq!(out, STATE.trim_end_matches('\n'));
        let empty =
            String::from_utf8(to_sorted_json_pretty(&CardState::default()).unwrap()).unwrap();
        assert_eq!(empty, STATE_EMPTY.trim_end_matches('\n'));
    }

    #[test]
    fn the_state_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = card_state_path(dir.path());
        let s = CardState {
            message_id: "42".into(),
        };
        save_card_state(&path, &s).unwrap();
        assert_eq!(load_card_state(&path), s);
    }

    /// An older build's file, with the limit levels it used to keep, still
    /// names the message to edit.
    #[test]
    fn an_older_state_file_keeps_its_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = card_state_path(dir.path());
        std::fs::write(
            &path,
            r#"{"message_id": "42", "last_levels": {"claude": "green"}}"#,
        )
        .unwrap();
        assert_eq!(load_card_state(&path).message_id, "42");
    }

    /// Forgetting the message ID costs one duplicate card. Refusing to publish
    /// costs the dashboard.
    #[test]
    fn a_missing_or_corrupt_state_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = card_state_path(dir.path());
        assert_eq!(load_card_state(&path), CardState::default());
        std::fs::write(&path, b"{\"message_id\": \"42\"").unwrap();
        assert_eq!(load_card_state(&path), CardState::default());
    }

    /// A crash mid-write must not leave a file that reads as "no message" — that
    /// orphans the card in the channel.
    #[test]
    fn the_state_file_is_replaced_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = card_state_path(dir.path());
        save_card_state(
            &path,
            &CardState {
                message_id: "1".into(),
            },
        )
        .unwrap();
        save_card_state(
            &path,
            &CardState {
                message_id: "2".into(),
            },
        )
        .unwrap();
        assert_eq!(load_card_state(&path).message_id, "2");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
