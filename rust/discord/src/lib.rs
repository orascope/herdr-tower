// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The Discord status card: one message in a channel, kept up to date.
//!
//! The card is a single webhook message that is *edited*, never re-posted, so a
//! channel someone leaves open shows the tower's current state instead of a
//! scrollback of dead snapshots. Everything in this crate serves that: the
//! renderer builds the components (`card`), the coalescer keeps a burst of
//! triggers from becoming a burst of edits (`coalesce`), and the client
//! upserts the message and remembers which one it is (`webhook`).
//!
//! Three rules hold across the whole crate:
//!
//! **The bytes are the contract.** The payloads posted to the webhook and the
//! `discord-card.json` state file have one exact layout — sorted keys, HTML
//! escapes and all. See the `json` module for what that costs and why. Tests pin
//! the bytes against recorded payloads.
//!
//! **The webhook URL never appears in an error.** It carries the token.
//!
//! **The network is injected.** [`SendFn`] is the only door out, the way
//! `tower_notify` does it with `PostFn`, so every ordering rule here is tested
//! without a socket.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

mod card;
mod coalesce;
mod http;
mod json;
mod webhook;

pub use card::{build_card, CardData};
pub use coalesce::run_coalesced;
pub use json::{
    encode_sorted_json, to_sorted_json, to_sorted_json_pretty, Component, TYPE_CONTAINER, TYPE_TEXT,
};
pub use webhook::{
    card_state_path, load_card_state, save_card_state, CardState, Client, Method, Request,
    Response, SendFn, FLAG_COMPONENTS_V2,
};

/// Opens a file for reading and writing, creating it 0600.
///
/// The mode matters on the lock file only inasmuch as it sits in the state
/// directory beside records that name folders and tasks; there is no reason for
/// anything this crate creates to be world-readable. On a platform without Unix
/// permissions the mode is simply not expressible, and the file is still
/// created — the tower runs on macOS and Linux, and a Windows build failing over
/// a mode bit would be the wrong trade.
pub(crate) fn open_private(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Writes `data` to a fresh 0600 file, replacing whatever was there.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.sync_all()
}
