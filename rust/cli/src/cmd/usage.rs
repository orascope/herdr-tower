// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower usage [--json] [--watch]` — see `tower_cli::usagecmd`,
//! `tower_cli::usagefetch` and `tower_cli::usageguard`.
//!
//! This half reaches the terminal, the config file and, for `--watch`, the
//! herdr pane it runs in. In `--watch` on a terminal, the usage guard's floors
//! are changed by dragging the sliders above the bars, or in 1% steps with
//! the arrow keys. Reaching zero turns that window's guard off.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tower_cli::usagecmd::{self, Key};
use tower_cli::usagetab;

use super::common::home;
use super::Result;

/// How often `--watch` redraws when no key is pressed.
const WATCH_EVERY: Duration = Duration::from_secs(15);
/// The least time between two `r` refreshes. Each asks claude's endpoint and
/// codex's app server; a key held down must not turn into a flood of
/// requests that gets the account throttled.
const MANUAL_GAP: Duration = Duration::from_secs(30);

pub fn run_usage(args: &[String]) -> Result<()> {
    let json_out = args.iter().any(|a| a == "--json");
    let watch = args.iter().any(|a| a == "--watch");
    if let Some(bad) = args.iter().find(|a| *a != "--json" && *a != "--watch") {
        return Err(format!("usage: herdr-tower usage [--json] [--watch]  ({bad}?)").into());
    }
    if json_out && watch {
        return Err("--json and --watch do not go together".into());
    }
    let home = home()?;
    let state_dir = tower_state::default_dir();
    let offset = tower_state::local_offset();
    if !watch {
        let board = tower_cli::usagefetch::board(&home, &state_dir);
        let text = if json_out {
            format!("{:#}\n", usagecmd::to_json(&board))
        } else {
            usagecmd::render(&board, offset, std::io::stdout().is_terminal())
        };
        tower_cli::out!("{text}")?;
        return Ok(());
    }
    watch_board(&home, &state_dir, offset)
}

fn watch_board(home: &Path, state_dir: &Path, offset: time::UtcOffset) -> Result<()> {
    let color = std::io::stdout().is_terminal();
    let pane = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|p| !p.is_empty());
    // Keys only on a terminal; the guard is still shown without one.
    let tty = Path::new(tower_cli::cbreak::CONTROLLING_TTY);
    let keys = (std::io::stdin().is_terminal())
        .then(|| key_reader(tty))
        .flatten();
    let mut editing: Option<usize> = keys.as_ref().map(|_| 0);
    // Clicks and drags on the sliders, while the keys are read.
    let _mouse = keys.as_ref().map(|_| MouseReporting::on());
    // `r`: ask every source again now, at most once per MANUAL_GAP.
    let mut refresh_now = false;
    let mut last_refresh: Option<std::time::Instant> = None;
    let mut status: Option<String> = None;
    loop {
        let board = if std::mem::take(&mut refresh_now) {
            tower_cli::usagefetch::board_refreshed(home, state_dir)
        } else {
            tower_cli::usagefetch::board(home, state_dir)
        };
        let mut drawn = usagecmd::render_editing(&board, offset, color, editing);
        // A word on the last `r`, once, under the board.
        if let Some(line) = status.take() {
            drawn.text.push_str(&format!("\n{line}\n"));
        }
        // Keep the choice on a slider that is still drawn.
        if let Some(e) = editing {
            if !drawn.sliders.is_empty() && e >= drawn.sliders.len() {
                editing = Some(drawn.sliders.len() - 1);
            }
        }
        {
            let mut out = std::io::stdout().lock();
            // Home and clear, then the board: one write, so it never flickers
            // half-drawn.
            let _ = out.write_all(format!("\x1b[H\x1b[2J{}", drawn.text).as_bytes());
            let _ = out.flush();
        }
        if let Some(pane) = &pane {
            claim_pane(pane);
        }
        let Some((_mode, rx)) = &keys else {
            std::thread::sleep(WATCH_EVERY);
            continue;
        };
        let floor_of = |s: &usagecmd::SliderRow| board.guard.floors(s.vendor).get(s.window);
        match rx.recv_timeout(WATCH_EVERY) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The terminal is gone and keys no longer come: the board goes on
            // at its pace rather than spinning on a channel that answers at once.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                editing = None;
                std::thread::sleep(WATCH_EVERY);
            }
            Ok(bytes) => {
                let n = drawn.sliders.len();
                // Floors saved from this read, by slider: the board was loaded
                // before it, and three arrows in one read are three steps.
                let mut saved: Vec<(usize, Option<u8>)> = Vec::new();
                let current = |saved: &[(usize, Option<u8>)], which: usize| {
                    saved
                        .iter()
                        .rev()
                        .find(|(w, _)| *w == which)
                        .map_or_else(|| floor_of(&drawn.sliders[which]), |(_, f)| *f)
                };
                // A floor that cannot be saved is said under the board; the
                // board stays up.
                let save = |saved: &mut Vec<(usize, Option<u8>)>,
                            status: &mut Option<String>,
                            which: usize,
                            next| {
                    match save_floor(&drawn.sliders[which], next) {
                        Ok(()) => saved.push((which, next)),
                        Err(e) => *status = Some(format!("could not save the floor: {e}")),
                    }
                };
                for key in usagecmd::parse_keys(&bytes) {
                    match key {
                        Key::Quit => return Ok(()),
                        Key::Up if n > 0 => editing = Some((editing.unwrap_or(0) + n - 1) % n),
                        Key::Down if n > 0 => editing = Some((editing.unwrap_or(0) + 1) % n),
                        Key::Up | Key::Down | Key::WheelUp | Key::WheelDown => {}
                        Key::Refresh => {
                            let now = std::time::Instant::now();
                            match usagecmd::refresh_wait(last_refresh, now, MANUAL_GAP) {
                                Some(wait) => {
                                    status =
                                        Some(format!("refreshed a moment ago — again in {wait}s"));
                                }
                                None => {
                                    last_refresh = Some(now);
                                    refresh_now = true;
                                    let l = time::OffsetDateTime::now_utc().to_offset(offset);
                                    status = Some(format!(
                                        "refreshed at {:02}:{:02}:{:02}",
                                        l.hour(),
                                        l.minute(),
                                        l.second()
                                    ));
                                }
                            }
                        }
                        Key::Mouse { row, col } => {
                            let Some(which) = drawn.sliders.iter().position(|s| s.row == row)
                            else {
                                continue;
                            };
                            editing = Some(which);
                            let next = usagecmd::floor_at_column(col, usagecmd::BAR_WIDTH);
                            if next != current(&saved, which) {
                                save(&mut saved, &mut status, which, next);
                            }
                        }
                        Key::Left | Key::Right | Key::Toggle => {
                            let Some(which) = editing.filter(|&e| e < n) else {
                                continue;
                            };
                            let now = current(&saved, which);
                            let next =
                                usagecmd::step_floor(now, key, tower_config::DEFAULT_USAGE_GUARD);
                            if next != now {
                                save(&mut saved, &mut status, which, next);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The terminal's SGR mouse reporting, on for as long as this lives. Turned
/// off however the view ends, so the shell after it does not receive mouse
/// escapes as input.
pub(super) struct MouseReporting;

impl MouseReporting {
    /// Button presses and drags: the usage sliders follow a drag.
    fn on() -> Self {
        Self::with(b"\x1b[?1002h\x1b[?1006h")
    }

    /// Button presses only: the board pane acts on a click, and a drag
    /// reported as more presses would act again.
    pub(super) fn clicks() -> Self {
        Self::with(b"\x1b[?1000h\x1b[?1006h")
    }

    fn with(on: &[u8]) -> Self {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(on);
        let _ = out.flush();
        MouseReporting
    }
}

impl Drop for MouseReporting {
    fn drop(&mut self) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1006l");
        let _ = out.flush();
    }
}

fn save_floor(slider: &usagecmd::SliderRow, floor: Option<u8>) -> Result<()> {
    usagecmd::save_floor(slider, floor)?;
    Ok(())
}

/// Key presses from the terminal, in cbreak mode for as long as the returned
/// guard lives. Ctrl-C arrives as a key too (`-isig`), so leaving always
/// restores the terminal — a signal would kill the board with the terminal
/// still silent.
pub(super) fn key_reader(
    tty: &Path,
) -> Option<(tower_cli::cbreak::Cbreak, mpsc::Receiver<Vec<u8>>)> {
    let mode = tower_cli::cbreak::enter(tty).ok()?;
    let _ = tower_cli::cbreak::keys_not_signals(tty);
    let mut input = std::fs::File::open(tty).ok()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok(n) = input.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    Some((mode, rx))
}

/// Marks this pane as the usage board's, for as long as the board keeps
/// saying so (the token expires; see `usagetab::TOKEN_TTL`). Best effort: a
/// board with no herdr to tell still draws.
fn claim_pane(pane_id: &str) {
    let h = tower_cli::muxsel::default_mux();
    let _ = h.report_metadata(&herdrx::Metadata {
        pane_id: pane_id.to_string(),
        source: Some(usagetab::SOURCE.to_string()),
        tokens: [(usagetab::TOKEN.to_string(), "1".to_string())].into(),
        title: None,
        ttl: Some(usagetab::TOKEN_TTL),
        seq: 0,
    });
}
