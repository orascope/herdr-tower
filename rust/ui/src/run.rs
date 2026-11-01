// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The driver: the only place that owns a terminal.
//!
//! # Why the keyboard is not a thread
//!
//! Every other producer in [`crate::feed`] runs on its own thread. Input does
//! not, and that is deliberate. A reader thread parked in `event::read()` keeps
//! consuming keystrokes while a child process owns the terminal — press `g`,
//! and the first key typed into `lazygit` is eaten by a tower that is supposed
//! to have stepped aside. Pausing such a thread correctly means a handshake
//! around every [`Action::Run`]; polling here means the loop simply stops
//! asking. Twenty wakeups a second in a foreground TUI is not a cost worth a
//! handshake.
//!
//! # Redrawing
//!
//! Only when something happened. ratatui diffs against the previous buffer, so
//! a redundant draw is cheap, but it is not free over ssh — and a tower that
//! repaints twenty times a second while nothing changes is the thing people
//! notice on a laptop battery.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self as term_event, Event as TermEvent, MouseEventKind};
use tower_state::Store;

use crate::app::{Action, App, Event, Settings};
use crate::feed::Feed;
use crate::tower::{HerdrTower, Tower};
use crate::view;

/// How long the loop waits on the keyboard before looking at the feed again.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Runs the control tower against the real backend.
///
/// `usage` is the usage board, drawn under the header and on `u`; the binary
/// that has one passes it (see [`crate::usage`]).
pub fn main(
    notice: Option<String>,
    version: String,
    usage: Option<Arc<dyn crate::usage::Usage>>,
) -> std::io::Result<()> {
    let store = Store::new(tower_state::default_dir())?;
    let mut settings = Settings::from_env();
    settings.version = version;
    settings.preview_rows = load_preview_rows(&preview_rows_path());
    let tower = Arc::new(HerdrTower::from_env());
    // The popup is only a client. Ensure the one state-store-scoped resident
    // supervisor is alive; it owns Herdr synchronization and every idle loop.
    tower
        .start_loop_supervisor(&settings.exe)
        .map_err(std::io::Error::other)?;

    let mut app = App::new(store.clone(), tower.clone(), settings.clone());
    // Carried onto the screen rather than printed before it. Anything written to
    // the terminal first is covered by the alternate screen for the whole
    // session and read only after quitting — which, for a warning about why the
    // list is empty, is exactly the wrong moment.
    //
    // Its own field, not the storage-health one: nothing that happens in here can
    // make the condition it names go away, so nothing in here may take the line
    // down either.
    if let Some(text) = notice {
        app.set_startup_note(text);
    }
    let mut feed = Feed::spawn(store, tower.clone(), settings);
    if let Some(usage) = usage {
        app.set_usage(usage.clone());
        feed.attach_usage(usage);
    }

    let mut terminal = init_terminal();
    let result = drive(&mut terminal, &mut app, &mut feed, tower);
    restore_terminal();
    feed.stop();
    result
}

fn drive(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    feed: &mut Feed,
    tower: Arc<dyn Tower>,
) -> std::io::Result<()> {
    let mut dirty = true;
    loop {
        while let Some(ev) = feed.try_recv() {
            dirty = true;
            let action = app.on_event(ev);
            match perform(action, app, feed, &tower, terminal)? {
                Flow::Continue => {}
                Flow::Quit => return Ok(()),
            }
        }

        if term_event::poll(INPUT_POLL)? {
            match term_event::read()? {
                TermEvent::Key(k) => {
                    dirty = true;
                    let action = app.on_event(Event::Key(k));
                    match perform(action, app, feed, &tower, terminal)? {
                        Flow::Continue => {}
                        Flow::Quit => return Ok(()),
                    }
                }
                // Pointer motion arrives on every cell crossed and changes
                // nothing, so it is not worth a redraw.
                TermEvent::Mouse(m) if m.kind != MouseEventKind::Moved => {
                    dirty = true;
                    let action = app.on_event(Event::Mouse(m));
                    match perform(action, app, feed, &tower, terminal)? {
                        Flow::Continue => {}
                        Flow::Quit => return Ok(()),
                    }
                }
                TermEvent::Resize(..) => dirty = true,
                _ => {}
            }
        }

        if dirty {
            // Laid out once, inside the draw: that is where the terminal takes
            // up a new size, and `get_frame()` before it still reports the old
            // one after a resize — a frame laid out for that is drawn clipped,
            // or with its key guide floating mid-screen. The one layout is
            // drawn, asked what to capture next, and remembered for the clicks
            // that land on it.
            let mut frame = view::Composed::default();
            terminal.draw(|f| {
                let area = f.area();
                app.set_size(area.width, area.height);
                frame = view::compose(app, area.width.into(), area.height.into());
                view::render_lines(f, area, std::mem::take(&mut frame.lines));
            })?;
            feed.set_preview(frame.preview.take());
            app.remember_frame(frame);
            dirty = false;
        }
    }
}

/// The terminal as the tower wants it: ratatui's raw alternate screen, plus
/// mouse reporting so a row can be clicked.
fn init_terminal() -> ratatui::DefaultTerminal {
    let terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    terminal
}

/// Undoes [`init_terminal`]. Mouse reporting goes first: left on, it would
/// fill the shell — or the child the terminal is handed to — with escape codes
/// on every movement.
fn restore_terminal() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
}

/// Where the preview's chosen height is kept between towers: one number, or
/// no file for the automatic split. Beside the agent store, so a tower run
/// against another store (`HERDR_TOWER_STATE_DIR`) keeps its own.
fn preview_rows_path() -> std::path::PathBuf {
    tower_state::default_dir().join("preview-rows")
}

/// The remembered height, if there is one and it reads as a number. Anything
/// else is the automatic split — a garbled file must not stop the tower.
pub fn load_preview_rows(path: &std::path::Path) -> Option<usize> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Remembers `rows`; `None` removes the file, back to automatic.
pub fn save_preview_rows(path: &std::path::Path, rows: Option<usize>) -> std::io::Result<()> {
    match rows {
        Some(n) => std::fs::write(path, format!("{n}\n")),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

enum Flow {
    Continue,
    Quit,
}

fn perform(
    action: Action,
    app: &mut App,
    feed: &Feed,
    tower: &Arc<dyn Tower>,
    terminal: &mut ratatui::DefaultTerminal,
) -> std::io::Result<Flow> {
    match action {
        Action::Nothing => Ok(Flow::Continue),
        Action::Quit => Ok(Flow::Quit),
        Action::Refresh => {
            feed.refresh();
            Ok(Flow::Continue)
        }
        Action::RefreshUsage => {
            feed.refresh_usage();
            Ok(Flow::Continue)
        }
        Action::SavePreviewRows(rows) => {
            // Best effort: a split that is not remembered costs one drag next
            // time, and is no reason to put an error on the screen.
            let _ = save_preview_rows(&preview_rows_path(), rows);
            Ok(Flow::Continue)
        }
        Action::Run { argv, focus, seen } => {
            // Both once the child *starts*, not when it ends. An attach runs for
            // as long as the person is in that pane — hours — and leaving the
            // completion unread for all of it means `status`, the card and the
            // notifications go on calling it unseen while they are looking at it.
            // A command that cannot start selects nothing and marks nothing — see
            // `App::attached`, which is that whole decision.
            let failure = run_child(&argv, terminal, || {
                app.attached(focus.as_deref(), seen.as_deref())
            })?;
            if let Some(msg) = failure {
                // Through the feed, like every other outcome, so it lands on the
                // error line and stays there. The message the child itself printed
                // went with the screen it was printed on.
                let _ = feed.notifier().send(Event::Notice(Err(msg)));
            }
            // Whatever the child did — a commit, an attached session — the
            // records may have moved underneath it.
            feed.refresh();
            Ok(Flow::Continue)
        }
        Action::Remote(request) => {
            remote_async(*request, tower.clone(), feed.notifier());
            Ok(Flow::Continue)
        }
        Action::Open(url) => {
            if let Err(e) = open_url(&url) {
                let _ = feed
                    .notifier()
                    .send(Event::Notice(Err(format!("could not open {url}: {e}"))));
            }
            Ok(Flow::Continue)
        }
    }
}

/// Hands `url` to the system's opener without waiting for the browser: the
/// tower keeps drawing, and a thread reaps the opener when it exits. A
/// machine with no opener — a server without a desktop — says so on the
/// error line, URL included, so it can still be copied.
fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(windows)]
    let mut cmd = {
        // `start`'s first quoted argument is the window title.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    let mut child = cmd
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let _ = std::thread::Builder::new()
        .name("open-url".into())
        .spawn(move || child.wait());
    Ok(())
}

/// Hands the terminal over, waits, takes it back. Yields what to report, if the
/// child failed.
///
/// `started` runs once the process exists and before the wait — see the call
/// site for why that moment matters.
///
/// The screen is restored *before* the child starts and re-initialised after, so
/// the child gets a normal terminal rather than one in raw mode on the alternate
/// screen.
///
/// Failure is reported twice on purpose, and both halves used to be missing. A
/// child that *ran* and exited nonzero — `herdr attach` on a tab that is gone,
/// `lazygit` on a broken repository — was not reported at all: the tower came
/// straight back and cleared the screen, so the child's own error message was
/// gone before it could be read, and the key press looked like it had done
/// nothing. The one case that was reported, a child that could not start, was
/// printed *after* the tower took the terminal back, onto a frame the next redraw
/// covered — invisible for a different reason.
///
/// So: the line is printed on the restored screen, where the child's own output
/// still is and is the real diagnosis, and the tower waits there for Enter rather
/// than painting over it. What survives the screen is the notice the caller
/// raises. A pause on failure only: a child that worked must not ask permission
/// to return — and none for a failure raised by `started`, where the child was
/// ended from here and left nothing on the screen to read.
fn run_child(
    argv: &[String],
    terminal: &mut ratatui::DefaultTerminal,
    started: impl FnOnce() -> Option<String>,
) -> std::io::Result<Option<String>> {
    let Some((bin, args)) = argv.split_first() else {
        return Ok(None);
    };
    restore_terminal();
    // Spawned and waited on separately, rather than `status()`, so there is a
    // moment between "it is running" and "it has finished" for the caller to act
    // in. That moment is the whole difference for an attach, which does not
    // finish until the person leaves the pane.
    //
    // `started` fires once the child has survived [`ESTABLISHED`], not the
    // instant it exists. A process that spawns and dies — `herdr session attach`
    // on a session that is gone — never reached the pane, and treating its
    // existence as arrival cleared an unread completion the person never saw.
    //
    // What `started` reports back is the arrival itself failing, which ends the
    // child: see below.

    // Whether the failure is one this code caused, in which case there is nothing
    // on the screen to read and nothing to pause over.
    let mut ours = false;
    let failure = match std::process::Command::new(bin).args(args).spawn() {
        Err(e) => Some(format!("{bin} failed to start: {e}")),
        Ok(mut child) => match established(&mut child).then(started).flatten() {
            // It started, and then what it was started *for* did not happen. A
            // child left running on that footing is worse than none: the terminal
            // stays attached to something nobody asked for, and this line waits
            // behind it until the person works out how to get back. So the child
            // ends here and the reason is on the screen a moment later.
            Some(msg) => {
                ours = true;
                let _ = child.kill();
                let _ = child.wait();
                Some(msg)
            }
            None => match child.wait() {
                Err(e) => Some(format!("{bin} failed while waiting: {e}")),
                Ok(st) if !st.success() => Some(match st.code() {
                    Some(c) => format!("{bin} failed (exit code {c})"),
                    // A signalled child has no code. Rust's own words for it beat
                    // an invented number.
                    None => format!("{bin} failed ({st})"),
                }),
                Ok(_) => None,
            },
        },
    };
    if let Some(msg) = &failure {
        if !ours {
            eprintln!("\nherdr-tower: {msg}");
            eprint!("Press Enter to continue…");
            let _ = std::io::stderr().flush();
            wait_for_enter();
        }
    }
    *terminal = init_terminal();
    let _ = terminal.clear();
    Ok(failure)
}

/// How long a child has to survive before it counts as having started.
///
/// An attach that fails does so at once — a stale session, a socket that is not
/// there — while one that works runs until the person leaves. A second
/// separates them with room to spare, and it is a second nobody waits for: the
/// child owns the terminal throughout.
const ESTABLISHED: Duration = Duration::from_secs(1);

/// Whether the child is still running after [`ESTABLISHED`].
///
/// Polled rather than timed out on, because there is no waiting-with-a-deadline
/// in the standard library — and a slice this coarse needs none.
fn established(child: &mut std::process::Child) -> bool {
    let deadline = std::time::Instant::now() + ESTABLISHED;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            // Gone already: it never arrived anywhere.
            Ok(Some(_)) => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            // Unanswerable. The wait below will say what happened; assuming it
            // started is the same answer as before this check existed.
            Err(_) => return true,
        }
    }
    true
}

/// Blocks until Enter, on the terminal the child just used.
///
/// A closed stdin counts as Enter: a tower driven with stdin redirected must not
/// wedge on a prompt nobody can answer. Reading here is safe precisely because
/// the keyboard is not a thread — see the note at the top of this file — so
/// nothing else is competing for the line.
fn wait_for_enter() {
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
}

/// Runs an [`Action::Remote`] on a thread of its own; the answer comes back as
/// [`Event::RemoteDone`].
fn remote_async(
    request: crate::RemoteRequest,
    tower: Arc<dyn Tower>,
    notify: std::sync::mpsc::Sender<Event>,
) {
    let spare = request.clone();
    let spawned = std::thread::Builder::new()
        .name("tower_ui-remote-act".into())
        .spawn({
            let notify = notify.clone();
            move || {
                let _ = notify.send(request.run(tower.as_ref()));
            }
        });
    // Unlike a preview, the app waits for this answer — it takes no second
    // remote action until one arrives — so a thread that will not start
    // still has to send one.
    if let Err(e) = spawned {
        let _ = notify.send(spare.done(Err(format!("could not start: {e}"))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chosen height survives a round trip; automatic is no file at all;
    /// a garbled file reads as automatic rather than stopping the tower.
    #[test]
    fn the_preview_height_is_remembered_between_towers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preview-rows");
        assert_eq!(load_preview_rows(&path), None);
        save_preview_rows(&path, Some(17)).unwrap();
        assert_eq!(load_preview_rows(&path), Some(17));
        save_preview_rows(&path, None).unwrap();
        assert!(!path.exists());
        save_preview_rows(&path, None).unwrap();
        std::fs::write(&path, "tall\n").unwrap();
        assert_eq!(load_preview_rows(&path), None);
    }

    /// Any height comes back as saved, and a hand-edited file is read for
    /// the number alone: padding is forgiven, anything that is not one whole
    /// non-negative number is automatic.
    #[test]
    fn a_remembered_height_reads_back_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preview-rows");
        for n in [0, 1, 4, 17, 999, usize::MAX] {
            save_preview_rows(&path, Some(n)).unwrap();
            assert_eq!(load_preview_rows(&path), Some(n));
        }
        let cases = [
            ("17", Some(17)),
            (" 17 \n", Some(17)),
            ("\t8\r\n", Some(8)),
            ("", None),
            ("\n", None),
            ("-3", None),
            ("1.5", None),
            ("17 18", None),
            ("18446744073709551616", None),
            ("１７", None),
        ];
        for (text, want) in cases {
            std::fs::write(&path, text).unwrap();
            assert_eq!(load_preview_rows(&path), want, "{text:?}");
        }
    }
}
