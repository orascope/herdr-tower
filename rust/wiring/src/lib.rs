// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! One agent's *wiring*, collected read-only: its folder-bot registration, the
//! Discord channels and policy it answers on, a codex bridge it may sit behind,
//! and the units that start it — LaunchAgent plists on macOS, systemd user
//! units on Linux.
//!
//! Nothing here writes a file. Registering a bot and pairing a channel belong
//! to the tools that own those files; this crate only reports what is already
//! on disk. A source that is missing empties one field and no more — a control
//! tower that refuses to draw because one config file is absent is worse than
//! one that draws a blank line.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

mod bridge;
mod units;

pub use bridge::Bridge;
pub use units::session_agents;

/// One Discord channel (group) the bot answers in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Channel {
    pub id: String,
    /// The human name from config `channel_labels`. `None` means nobody has
    /// named this channel, and callers show the raw ID instead.
    pub label: Option<String>,
    pub require_mention: bool,
    /// How many users the allowlist holds. The list itself is not carried: the
    /// card shows a count, and copying user IDs around would be a leak with no
    /// reader.
    pub allow_count: usize,
}

/// The bot's Discord connection, as its own state directory describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discord {
    pub dm_policy: String,
    pub channels: Vec<Channel>,
}

/// A folder-bot registration — the one place that says which herdr session
/// a bot lives in.
///
/// Name, session and engine are one fact, not three: they are read from a
/// single entry and are either all known or all unknown, so they travel
/// together instead of as three strings that can disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registration {
    /// The key in `bots.json`.
    pub name: String,
    /// The session the bot runs in. This is the authority for the ⌁ mark.
    pub session: String,
    /// `claude` | `codex` | `gemini`, as registered. Left as text because the
    /// registry is written by another tool and may name an engine this build
    /// has never heard of.
    pub engine: String,
}

/// Everything known about one agent's wiring.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// The folder-bot registration, if this folder or session has one.
    pub bot: Option<Registration>,
    /// `None` when the folder has no `.discord-state`.
    pub discord: Option<Discord>,
    /// Set when a codex/gemini bridge points at this folder.
    pub bridge: Option<Bridge>,
    /// Labels of the launch units that mention this session or folder: plist
    /// labels on macOS, unit names on Linux.
    pub launch_units: Vec<String>,
    /// Whether any of `launch_units` matched by *session name* rather than by
    /// folder path. The ⌁ mark hangs on this — a unit found only through the
    /// folder says something about the folder, not about this session.
    pub unit_by_session: bool,
}

impl Info {
    /// Whether this folder is wired to Discord in any form at all — the
    /// question the `info` card's "Discord" line answers.
    pub fn discord_connected(&self) -> bool {
        self.discord.is_some()
            || self.bridge.is_some()
            || self.launch_units.iter().any(|u| u.contains("discord"))
    }

    /// Whether this session *is* the Discord-driven bot (the ⌁ mark).
    ///
    /// Where `discord_connected` asks "is this folder wired up", this asks "is
    /// this session the bot". A second session opened in the same folder to
    /// work in, and a dead session that was never a bot, must not carry the
    /// mark — that was the WSL2 finding of 2026-09-10, and it is why the
    /// registration's `session` outranks everything else here rather than the
    /// folder's own files: `.discord-state` left behind by a harness proves
    /// nothing about the session looking at it.
    ///
    /// Authority runs: registration (bots.json names the session) → codex
    /// bridge → a discord launch unit that spells the session name out. The
    /// bridge is matched per *folder*, so it marks every session in a bridge
    /// folder; accepted because a bridge folder is the bot's own folder.
    pub fn drives_session(&self, session: &str) -> bool {
        if session.is_empty() {
            return false;
        }
        if let Some(bot) = &self.bot {
            return bot.session == session;
        }
        if self.bridge.is_some() {
            return true;
        }
        self.unit_by_session && self.launch_units.iter().any(|u| u.contains("discord"))
    }
}

/// Where the wiring is read from.
///
/// Data rather than constants so tests can point the whole collection at a
/// temp directory. Every location is optional: a mac has no systemd user
/// directory, a Linux box has no `LaunchAgents`, and a machine with neither
/// still collects the rest.
#[derive(Debug, Clone, Default)]
pub struct Paths {
    /// `~/.config/folder-bot/bots.json`
    pub bots_json: Option<PathBuf>,
    /// `~/Library/LaunchAgents` (macOS)
    pub launch_agents_dir: Option<PathBuf>,
    /// `~/.config/systemd/user` (Linux)
    pub systemd_user_dir: Option<PathBuf>,
    /// Candidate roots of the codex-discord bridge.
    pub bridge_roots: Vec<PathBuf>,
}

impl Paths {
    /// The real locations under a given home directory.
    ///
    /// Both the macOS and the Linux directory are filled in regardless of the
    /// host: naming a directory that does not exist costs one failed
    /// `read_dir`, while guessing the platform would blind the collection on a
    /// machine that runs both (WSL2 with a mac-shaped `$HOME`, for one).
    pub fn for_home(home: impl AsRef<Path>) -> Paths {
        let home = home.as_ref();
        Paths {
            bots_json: Some(home.join(".config").join("folder-bot").join("bots.json")),
            launch_agents_dir: Some(home.join("Library").join("LaunchAgents")),
            systemd_user_dir: Some(home.join(".config").join("systemd").join("user")),
            bridge_roots: vec![
                home.join("ai-folder").join("dev").join("codex-discord"),
                home.join("codex-discord"),
            ],
        }
    }

    /// The real locations for this user, or `None` when the home directory is
    /// unknown. Resolved from `$HOME`, the conventional source of the home
    /// directory on Unix.
    pub fn detect() -> Option<Paths> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(Paths::for_home)
    }
}

#[derive(Deserialize)]
struct BotEntry {
    #[serde(default)]
    engine: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    session: String,
}

#[derive(Deserialize)]
struct AccessFile {
    #[serde(default, rename = "dmPolicy")]
    dm_policy: String,
    #[serde(default)]
    groups: BTreeMap<String, AccessGroup>,
}

#[derive(Deserialize)]
struct AccessGroup {
    #[serde(default, rename = "requireMention")]
    require_mention: bool,
    #[serde(default, rename = "allowFrom")]
    allow_from: Vec<String>,
}

/// Collects the wiring of the agent whose working directory is `folder` and
/// whose session is `session`.
///
/// `folder` is text, not a `Path`, on purpose: the same string is compared
/// byte-for-byte against `bots.json` and then used as a needle inside unit
/// files, so normalising it as a path here would only make the two comparisons
/// disagree with each other.
pub fn collect(
    paths: &Paths,
    folder: &str,
    session: &str,
    labels: &HashMap<String, String>,
) -> Info {
    let mut info = Info {
        bot: registration(paths, folder, session),
        discord: discord_for(folder, labels),
        bridge: bridge::detect(&paths.bridge_roots, folder),
        ..Info::default()
    };

    let session_re = units::session_needle(session);
    let mut path_res = Vec::new();
    // An empty folder would be a needle that matches every unit ever written.
    if !folder.is_empty() {
        path_res.push(path_needle(folder));
    }
    if let Some(b) = &info.bridge {
        path_res.push(path_needle(&b.dir.to_string_lossy()));
    }

    for u in units::unit_texts(paths) {
        let by_session = session_re.as_ref().is_some_and(|re| re.is_match(&u.text));
        if by_session {
            info.unit_by_session = true;
        }
        if by_session || path_res.iter().any(|re| re.is_match(&u.text)) {
            info.launch_units.push(u.label);
        }
    }
    info
}

/// The folder-bot entry for this folder or session.
///
/// A worktree runs from a different directory than the one registered, so a
/// session-name match counts too. Entries are read into a sorted map: with an
/// unordered map, a file where two entries match would resolve differently
/// from run to run; here the first name in sort order wins, which is at least
/// reproducible and reports the same bot twice in a row.
fn registration(paths: &Paths, folder: &str, session: &str) -> Option<Registration> {
    let raw = read_text(paths.bots_json.as_ref()?)?;
    let bots: BTreeMap<String, BotEntry> = serde_json::from_str(&raw).ok()?;
    bots.into_iter()
        .find(|(_, e)| e.folder == folder || (!session.is_empty() && e.session == session))
        .map(|(name, e)| Registration {
            name,
            session: e.session,
            engine: e.engine,
        })
}

/// The Discord wiring the folder carries in `.discord-state/access.json`.
///
/// Channels come out sorted by ID so the card does not reshuffle between two
/// runs that read the same file.
fn discord_for(folder: &str, labels: &HashMap<String, String>) -> Option<Discord> {
    let path = Path::new(folder).join(".discord-state").join("access.json");
    let raw = read_text(&path)?;
    let af: AccessFile = serde_json::from_str(&raw).ok()?;
    Some(Discord {
        dm_policy: af.dm_policy,
        channels: af
            .groups
            .into_iter()
            .map(|(id, g)| Channel {
                label: labels.get(&id).cloned(),
                id,
                require_mention: g.require_mention,
                allow_count: g.allow_from.len(),
            })
            .collect(),
    })
}

/// A folder path matches a unit only up to the path's own end boundary.
///
/// Plain containment lets `/a/b` match the unit of a sibling (`/a/b-x`) or of a
/// bot living one level down (`/a/b/c`), and the agent sitting in the parent
/// folder then reports "Discord connected" while owning nothing. That was a
/// real false positive, not a hypothetical one.
fn path_needle(needle: &str) -> Regex {
    Regex::new(&format!(r"{}($|[^A-Za-z0-9_\-./])", regex::escape(needle)))
        .expect("an escaped literal always compiles")
}

/// Shortens a channel ID for display.
///
/// Discord snowflakes share their leading timestamp bits, so two channels
/// created the same week are indistinguishable by their prefix alone — the tail
/// is what tells them apart, and both ends have to be shown.
///
/// Sliced by characters rather than bytes: these IDs are ASCII digits in
/// practice, but a display helper must not panic on the day one is not.
pub fn short_id(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 10 {
        return id.to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Reads a file as text, tolerating bytes that are not UTF-8.
///
/// Everything read here is matched against, never re-emitted, so one stray
/// byte in a plist must not make a whole launcher invisible.
pub(crate) fn read_text(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Entry names of a directory, sorted.
///
/// The collection leans on a stable order — which unit is reported first, and
/// which `.env` wins when two of them point at one folder. `fs::read_dir`
/// promises no order at all, so the sort has to be spelled out.
pub(crate) fn sorted_dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
