// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Event subscription — the one herdr call that keeps its connection open.
//!
//! Every other call is one request, one response, connection closed.
//! `events.subscribe` is answered once with an acknowledgement and then pushes
//! events down that same line until somebody closes it, so it needs its own
//! reader and its own deadline discipline: the acknowledgement is timed,
//! everything after it is not. A quiet session is normal, and a read deadline
//! would tear down a perfectly good subscription.

use std::io::{BufRead, BufReader, Write};

use serde::{Deserialize, Deserializer};
use serde_json::json;

use crate::{ApiError, Error, Herdr, Pane, Result};

/// The name of a lifecycle event herdr pushes.
///
/// Careful: the name you subscribe with and the name that arrives are spelled
/// differently. Subscriptions use dots (`"pane.updated"`, the `SUB_*`
/// constants); the events themselves use underscores (`"pane_updated"`). The
/// two spellings are kept apart here so they cannot be confused at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    PaneCreated,
    PaneUpdated,
    PaneClosed,
    PaneExited,
    PaneAgentDetected,
    /// Anything a newer herdr pushes.
    ///
    /// Decoding must not fail here. This client subscribes to five kinds, but a
    /// server is free to widen what it sends, and a watch that dies on the
    /// first unrecognized name would be a control tower that silently stops
    /// noticing changes after a herdr upgrade.
    Other(String),
}

impl EventKind {
    fn from_name(name: &str) -> Self {
        match name {
            "pane_created" => EventKind::PaneCreated,
            "pane_updated" => EventKind::PaneUpdated,
            "pane_closed" => EventKind::PaneClosed,
            "pane_exited" => EventKind::PaneExited,
            "pane_agent_detected" => EventKind::PaneAgentDetected,
            other => EventKind::Other(other.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(EventKind::from_name(&String::deserialize(d)?))
    }
}

/// Subscription names, in the dot spelling `events.subscribe` expects.
pub const SUB_PANE_CREATED: &str = "pane.created";
pub const SUB_PANE_UPDATED: &str = "pane.updated";
pub const SUB_PANE_CLOSED: &str = "pane.closed";
pub const SUB_PANE_EXITED: &str = "pane.exited";
pub const SUB_PANE_AGENT_DETECTED: &str = "pane.agent_detected";

/// The minimal set needed to track pane reality: discovery, coordinate updates,
/// and disappearance — everything a polling scanner used to do.
///
/// `pane.agent_status_changed` is deliberately absent: it requires a specific
/// `pane_id`, so it cannot serve as a global watch. Status changes ride along
/// anyway, inside the full pane record `pane.updated` carries.
pub const PANE_SUBSCRIPTIONS: [&str; 5] = [
    SUB_PANE_CREATED,
    SUB_PANE_UPDATED,
    SUB_PANE_CLOSED,
    SUB_PANE_EXITED,
    SUB_PANE_AGENT_DETECTED,
];

/// One delivered event.
///
/// `pane` is filled in only for the kinds that carry a whole record (created,
/// updated); the rest arrive with coordinates alone, which is all that handling
/// a disappearance needs.
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub pane: Option<Pane>,
    pub pane_id: String,
    pub workspace_id: String,
    pub agent: String,
}

#[derive(Deserialize)]
struct Ack {
    #[serde(default)]
    result: Option<AckResult>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct AckResult {
    #[serde(default)]
    r#type: String,
}

/// The outer shape of a pushed line. Unlike a response it carries no `id`.
#[derive(Deserialize)]
struct EventEnvelope {
    #[serde(default)]
    event: Option<EventKind>,
    #[serde(default)]
    data: EventData,
    /// A rejected subscription arrives as an error envelope instead.
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize, Default)]
struct EventData {
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    pane: Option<Pane>,
}

/// An open subscription.
///
/// Reading is `&mut`, so exactly one reader can exist; ending it is a separate,
/// shareable [`Closer`], which is what lets a watch be stopped from outside the
/// thread that is blocked reading it.
#[derive(Debug)]
pub struct Stream {
    reader: BufReader<crate::Conn>,
}

/// Ends a subscription from anywhere.
///
/// It holds its own descriptor for the same socket, so shutting it down wakes a
/// reader blocked in [`Stream::next_event`] on another thread. Without this a
/// quiet session would be unstoppable: the reader is parked in a syscall and
/// nothing short of closing the socket under it returns.
#[derive(Debug)]
pub struct Closer(crate::Conn);

impl Closer {
    /// Ends the subscription. A blocked reader wakes with an error.
    pub fn close(&self) -> std::io::Result<()> {
        crate::conn::shutdown(&self.0)
    }
}

impl Stream {
    /// Waits for the next event. After a close it reports the end of the stream.
    ///
    /// Named `next_event` rather than `next` because this is not an iterator:
    /// the end is an error worth surfacing, not a `None` a caller can shrug off.
    pub fn next_event(&mut self) -> Result<Event> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the herdr event stream ended",
            )));
        }
        let env: EventEnvelope = serde_json::from_str(&line)?;
        if let Some(e) = env.error {
            return Err(Error::Api(e));
        }
        let Some(kind) = env.event else {
            return Err(Error::Invalid(format!(
                "a herdr line is not an event: {line}"
            )));
        };

        let mut ev = Event {
            kind,
            pane: env.data.pane,
            pane_id: env.data.pane_id,
            workspace_id: env.data.workspace_id,
            agent: env.data.agent,
        };
        // created and updated carry the whole record and need no separate
        // pane_id, so the coordinates come from the record itself.
        if ev.pane_id.is_empty() {
            if let Some(p) = &ev.pane {
                ev.pane_id = p.pane_id.clone();
                ev.workspace_id = p.workspace_id.clone();
            }
        }
        Ok(ev)
    }

    /// Ends the subscription.
    pub fn close(&self) -> std::io::Result<()> {
        crate::conn::shutdown(self.reader.get_ref())
    }

    /// A handle that can end this subscription from another thread.
    pub fn closer(&self) -> std::io::Result<Closer> {
        Ok(Closer(self.reader.get_ref().try_clone()?))
    }
}

impl Herdr {
    /// Opens a subscription and returns once the server has acknowledged it.
    ///
    /// `types` are dot-spelled names — see [`PANE_SUBSCRIPTIONS`].
    pub fn subscribe(&self, types: &[&str]) -> Result<Stream> {
        if types.is_empty() {
            return Err(Error::Invalid(
                "a herdr subscription needs at least one event type".into(),
            ));
        }
        let path = match &self.client.socket {
            Some(p) => p.clone(),
            None => crate::default_socket_path().ok_or(Error::NoSocket)?,
        };
        let timeout = self.client.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);

        // The same bounded connect the ordinary calls use. A plain `connect`
        // blocks with no limit of its own, and `Feed::spawn` opens this
        // subscription synchronously *before* the terminal is initialized — so a
        // server whose accept backlog is full hung the tower at startup, with no
        // screen drawn and nothing printed, despite the advertised deadline.
        let conn = crate::connect_within(&path, Some(timeout))?;
        conn.set_read_timeout(Some(timeout))?;
        conn.set_write_timeout(Some(timeout))?;

        let subs: Vec<_> = types.iter().map(|t| json!({ "type": t })).collect();
        let body = serde_json::to_vec(&crate::Request {
            id: crate::next_id(),
            method: "events.subscribe",
            params: json!({ "subscriptions": subs }),
        })?;
        {
            let mut w = &conn;
            w.write_all(&body)?;
            w.write_all(b"\n")?;
            w.flush()?;
        }

        // The buffer outlives the acknowledgement: bytes of a first event can
        // already be sitting in it, and dropping the reader would drop them.
        let mut reader = BufReader::with_capacity(64 * 1024, conn);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "no herdr subscription acknowledgement",
            )));
        }
        let ack: Ack = serde_json::from_str(&line)?;
        if let Some(e) = ack.error {
            return Err(Error::Api(e));
        }
        let started = ack.result.map(|r| r.r#type).unwrap_or_default();
        if started != "subscription_started" {
            return Err(Error::Invalid(format!(
                "the herdr subscription did not start: {started}"
            )));
        }

        // The acknowledgement was the only thing worth timing. Waiting is the
        // point from here on, so the deadline goes away.
        reader.get_ref().set_read_timeout(None)?;
        Ok(Stream { reader })
    }

    /// Opens a pane watch and takes the baseline listing, in that order.
    ///
    /// The order is the whole point. Since herdr 0.9.0 a subscription no longer
    /// replays retained history (0.8.2 did), so a snapshot taken first would
    /// permanently lose whatever happened between it and the subscription.
    /// Subscribing first can only duplicate an event, which is harmless.
    pub fn watch_panes(&self) -> Result<(Stream, Vec<Pane>)> {
        let stream = self.subscribe(&PANE_SUBSCRIPTIONS)?;
        match self.list_panes() {
            Ok(panes) => Ok((stream, panes)),
            Err(e) => {
                let _ = stream.close();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two spellings must not be confused: subscribing with the event name
    /// (or reading an event by the subscription name) matches nothing, and the
    /// watch silently never fires.
    #[test]
    fn subscription_and_event_spellings_differ() {
        assert_eq!(SUB_PANE_UPDATED, "pane.updated");
        assert_eq!(EventKind::from_name("pane_updated"), EventKind::PaneUpdated);
        assert_eq!(
            EventKind::from_name("pane.updated"),
            EventKind::Other("pane.updated".into()),
            "the dot spelling is not an event name"
        );
    }

    /// A herdr that grows a sixth event kind must not blind the watch.
    #[test]
    fn an_unknown_kind_decodes_instead_of_failing() {
        let k: EventKind = serde_json::from_str("\"tab_renamed\"").unwrap();
        assert_eq!(k, EventKind::Other("tab_renamed".into()));
    }

    /// Nothing that needs a pane_id belongs in a global watch.
    #[test]
    fn the_global_set_has_no_per_pane_subscription() {
        assert!(!PANE_SUBSCRIPTIONS.contains(&"pane.agent_status_changed"));
        assert_eq!(PANE_SUBSCRIPTIONS.len(), 5);
    }
}
