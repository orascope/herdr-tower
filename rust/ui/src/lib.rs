// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The control tower's terminal screen.
//!
//! # Why there is no update/view fold
//!
//! ratatui owns nothing — you redraw from your own state each frame and read
//! input yourself — so there is no runtime to fold messages into a model and
//! carry out the commands it hands back. Three forces decided the shape, and
//! all three came from what the existing behaviour needs rather than from
//! taste:
//!
//! **1. Some key presses must sequence a side effect with a state change, and
//! the two cannot be split.** Jumping to an unread completion marks it read only
//! after focus succeeds; a failed move must leave the marker that brings the
//! user back to it. Had side effects been pushed out as messages for a driver to
//! perform and report back, that ordering would live across a channel boundary
//! where nothing enforces it. So [`App`] holds the backend ([`Tower`]) and
//! performs those effects inline, right beside the state change they belong to.
//!
//! **2. A few effects genuinely cannot be done from inside a draw loop.**
//! Handing the terminal to `lazygit` or attaching to a pane — each needs the
//! renderer suspended around a child process. Those, and quitting, are the *only* things [`App::on_key`]
//! cannot finish itself, so they are the only things [`Action`] describes.
//! `Action` is a deliberately small vocabulary, and that is the point: what is
//! left in it is what actually needs the outside world.
//!
//! **3. Everything asynchronous became a thread that owns its own clock.**
//! [`Feed`] spawns one thread per cadence (store reads, local readings, the pane
//! preview) and they push [`Event`]s down one channel.
//! This rules out a bug class: a timer that re-arms itself by returning the
//! next timer forks into two chains the moment the interval changes, a doubled
//! chain polls the backend twice forever, and the only guard is a generation
//! counter every branch has to remember to bump. A thread that loops on
//! `recv_timeout` has one interval and cannot fork. See [`feed`].
//!
//! So: an [`Event`] enum in, an [`Action`] enum out, a plain mutable [`App`] in
//! between. Returning a new model from every update would buy immutability at
//! the price of copying a struct with twelve maps in it per keystroke; there is
//! nothing here that wants a persistent history of states.
//!
//! # Where rendering was cut
//!
//! [`view`] holds no terminal. It turns `&App` into `Vec<Line>` — ratatui's
//! `Line`/`Span` are plain data, text beside style — and only [`view::render`]
//! touches a `Frame`. Every rule that would otherwise be checkable only by
//! grepping the rendered string (column widths, truncation, badge text, window
//! scrolling, group dividers) is a function in [`view`] or [`text`] with a test
//! on it.
//!
//! Style lives beside the text rather than baked into the string as escape
//! sequences, so there is no need to re-measure lines with an ANSI-aware width
//! function or to re-append `\x1b[0m` at every cut so a severed style would not
//! bleed into the next row. Width arithmetic cannot miscount an escape
//! sequence, and there is no open style to close. [`text::clamp_line`] exists
//! for the ellipsis, not for the escapes.
//!
//! # Column width
//!
//! East Asian characters occupy two terminal columns, so every pad and every
//! truncation goes through [`text`], which measures with `unicode-width` and
//! nothing else. Two width tables — one for padding, another for the clamp —
//! would disagree about ambiguous-width glyphs under a CJK locale (see [`text`]
//! for the measured numbers). Using one table everywhere is not a style
//! preference; it is the only way the padding and the truncation can agree
//! about the same string.

mod app;
mod broadcast;
mod dock;
mod feed;
mod info;
mod input;
pub mod meter;
mod remote;
mod resume;
mod run;
mod text;
mod theme;
mod tower;
pub mod usage;
pub mod view;

pub use app::{
    Action, App, Event, HostLoad, LocalInfo, Pending, Preview, Screen, Settings, DOUBLE_CLICK,
    FILTER_EXAMPLE, SCHEDULE_EXAMPLE, USAGE_REFRESH_GAP,
};
pub use broadcast::{
    has_session_handoff, is_close_message, shown, targets, Sent, CLOSE_MESSAGE, WAKE_MESSAGE,
};
pub use dock::{in_dock, needs_pin, needs_tab_label, DOCK_LABEL, DOCK_PIN_INTERVAL};
pub use feed::{Feed, HOST_INTERVAL, LOCAL_INTERVAL, POLL_INTERVAL};
pub use info::{render_info, unit_word, unit_word_for, wired_sessions, InfoData};
pub use input::TextInput;
pub use remote::{
    RemoteAction, RemoteAgent, RemoteMachine, RemoteRequest, RemoteSchedule, UnseenPane,
};
pub use resume::{resume_command, ResumePaths};
pub use run::main;
pub use usage::{ansi_line, Adjust, Usage, UsageFrame};
// The vendor's bar and colours, for the usage board: one mark for one vendor
// wherever the tower names it.
pub use text::{
    clamp_line, columns, line_text, line_width, pad_right, shorten_home, since, title_case,
    truncate_width,
};
pub use theme::{vendor_rgb, VENDOR_BAR};
pub use tower::{HerdrTower, Tower};
