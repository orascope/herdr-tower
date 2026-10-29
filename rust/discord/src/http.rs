// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The one place this crate touches the network.
//!
//! Everything above it goes through [`crate::SendFn`], so the upsert ordering,
//! the multipart layout and the payload bytes are all testable without a socket.
//! This module is the part a test cannot cover, which is why it is kept as small
//! as it is: build a request, read a response, and translate ureq's errors into
//! the two sentences that are safe to show a user.

use std::io::Read;
use std::time::Duration;

use ureq::typestate::WithBody;
use ureq::{Agent, RequestBuilder};

use crate::webhook::{Method, Request, Response};

/// Discord's own name for us, in the request as well as on the message.
const USER_AGENT: &str = "herdr-tower";

/// How long to wait before giving up on the whole exchange.
///
/// A publisher fires from a hook inside the agent's own process and from a
/// five-minute timer. Neither has anyone watching it, so a hung connection has
/// to end by itself.
const TIMEOUT: Duration = Duration::from_secs(30);

/// One agent per [`crate::Client`], so repeated publishes reuse the connection.
pub(crate) fn agent() -> Agent {
    Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        // A 4xx is an answer, not a transport failure: Discord explains a
        // rejected card in the body, and `upsert` decides what a 400 versus a
        // 404 means. Letting ureq raise them as errors would throw that away.
        .http_status_as_error(false)
        .build()
        .into()
}

pub(crate) fn send(agent: &Agent, req: &Request) -> std::io::Result<Response> {
    let sent = match req.method {
        Method::Post => with_body(agent.post(req.url), req),
        Method::Patch => with_body(agent.patch(req.url), req),
        Method::Delete => agent
            .delete(req.url)
            .header("User-Agent", USER_AGENT)
            .call(),
    };
    let mut sent = sent.map_err(translate)?;
    let status = sent.status().as_u16();

    // A read error is swallowed *unless the body is the answer*. For a
    // diagnostic, a truncated explanation beats turning a real HTTP status into
    // "request failed"; for a create, the body carries the message ID and a
    // truncated one is an accepted message nobody can edit again.
    let mut body = Vec::new();
    let read = sent
        .body_mut()
        .as_reader()
        .take(req.response_limit)
        .read_to_end(&mut body);
    if req.needs_body {
        read?;
    }
    Ok(Response { status, body })
}

fn with_body(
    b: RequestBuilder<WithBody>,
    req: &Request,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let mut b = b.header("User-Agent", USER_AGENT);
    if let Some(ct) = req.content_type {
        b = b.header("Content-Type", ct);
    }
    b.send(req.body)
}

/// Turns a ureq failure into one of the two fixed sentences.
///
/// Fixed, and deliberately uninformative, because the webhook URL contains the
/// token and an error string travels: into a log file, into a LaunchAgent's
/// stderr, into a paste when somebody asks for help. ureq puts the URL in its
/// own `Display` output, so the error is never forwarded — only classified.
///
/// Two messages, because a request that could not be *built* (a malformed URL
/// — a configuration mistake) is a different problem from one that could not be
/// *sent*. ureq only notices a malformed URL when the call happens, so the
/// distinction is drawn from the error kind instead of from where it was raised.
fn translate(e: ureq::Error) -> std::io::Error {
    match e {
        ureq::Error::BadUri(_) | ureq::Error::Http(_) => {
            std::io::Error::other("could not build the discord request")
        }
        _ => std::io::Error::other("the discord request failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the fixed strings: a webhook token in an error is a
    /// token in a log file. ureq's own `Display` prints the URL it was given, so
    /// what matters is that none of it is forwarded.
    ///
    /// Driven through [`translate`] rather than a real call: opening even a
    /// doomed socket would make this test depend on the machine it runs on.
    #[test]
    fn a_transport_failure_never_names_the_webhook() {
        let secret = "https://discord.com/api/webhooks/123/SECRET";
        for e in [
            ureq::Error::BadUri(secret.into()),
            ureq::Error::Io(std::io::Error::other(secret)),
            ureq::Error::HostNotFound,
            ureq::Error::ConnectionFailed,
        ] {
            let msg = translate(e).to_string();
            assert!(!msg.contains("SECRET"), "{msg}");
            assert!(
                msg == "could not build the discord request" || msg == "the discord request failed",
                "{msg}"
            );
        }
    }

    /// And the classification survives the wiring: a URL ureq cannot parse comes
    /// back as the build failure without a socket being opened.
    #[test]
    fn a_malformed_url_is_reported_as_a_build_failure() {
        let err = send(
            &agent(),
            &Request {
                method: Method::Post,
                url: "not a url",
                content_type: None,
                body: b"",
                response_limit: 1024,
                needs_body: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(err, "could not build the discord request");
    }
}
