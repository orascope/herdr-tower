// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower board` — the dispatch board, from the advisor's pane.
//!
//! What an advisor handed to which `wt` worker, one board per repository; the
//! contract is `docs/dispatch-board.md`. Parsing, the board changes and the
//! printing live here; the socket, the process and the real directories are
//! the caller's (`main`'s `run_board`), and the delivery itself is handed in
//! as a function so every path through `send` can be tested without herdr.

use std::io::Write;
use std::path::Path;

use time::OffsetDateTime;
use tower_state::{
    column, Agent, AssignTo, Board, BoardError, BoardItem, BoardStore, Column, ItemStatus,
    PaneWorker, Progress, SendFlags, SendTo, Store,
};
use tower_wt::BoardDelivery;

pub const USAGE: &str = "usage: herdr-tower board <command>

  add \"<text>\" …               plan items (no worker yet)
  send <N> [<worker>]          deliver item N to a wt task or a pane (w2:pBR) — or,
                               with no worker, to the one it is assigned to  [--again] [--replace]
  send --new \"<text>\" <worker> add an item and deliver it in one go  [--replace]
  next <worker>                send the first item queued for a worker (board assign),
                               highest priority first
      send and next take the worker's instruction apart from the item's title:
      --message \"<text>\" or --file <path> (the worker gets the title, then it)
  prio <N> <1|2|3|->           set or clear an item's priority (1 is highest)
  move <N> --to <K>            put item N at position K
  link <N> <#PR|->             link a pull request; the board shows its state
  wake on [<pane>] | off       wake this pane (or the one named: the advisor) when an item
                               is ready for review
  assign <N> <worker>          name who has item N without sending it — a task, a pane,
                               or a label for a worker the tower cannot see  [--sent]
  done <N>…                    strike items through, after you have reviewed them
  drop <N>… [--reason \"…\"]     cancel items
  note <N> \"<text>\"            one line under an item (empty text removes it)
  rm <N>…                      remove items for good (an item out with a worker needs --force)
  prune                        remove every done, dropped and superseded item
  clear --all                  remove every item
  show                         the board and each worker's progress  [--json]
  open                         the board pane beside this one (the first send opens it too)
  --watch                      the board pane's own view  [--repo <path>]

  --repo <path> before the command works on that repository's board instead of this
  folder's.";

/// One `board` invocation, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Add(Vec<String>),
    Send {
        item: SendItem,
        /// Empty: the worker the item is assigned to.
        worker: String,
        flags: SendFlags,
        body: Option<Body>,
    },
    /// Name who has item `n`. With `sent`, the advisor sent it itself and
    /// the board takes it over as sent.
    Assign {
        n: u32,
        worker: String,
        sent: bool,
    },
    Done(Vec<u32>),
    Drop {
        ns: Vec<u32>,
        reason: String,
    },
    Note {
        n: u32,
        text: String,
    },
    Show {
        json: bool,
    },
    /// Send the first item queued for a worker.
    Next {
        worker: String,
        body: Option<Body>,
    },
    /// Set (1–3) or clear (0) an item's priority.
    Prio {
        n: u32,
        priority: u8,
    },
    /// Put item `n` at position `to`.
    Move {
        n: u32,
        to: usize,
    },
    /// Wake the calling pane — the advisor — when an item is ready for
    /// review, or stop.
    Wake {
        on: bool,
        /// The pane to wake; `None` is the caller's own.
        pane: Option<String>,
    },
    /// Link (or, with 0, unlink) a pull request.
    Link {
        n: u32,
        pr: u32,
    },
    /// Remove items for good.
    Remove {
        ns: Vec<u32>,
        force: bool,
    },
    /// Remove every closed item.
    Prune,
    /// Remove every item.
    Clear,
    /// Open the board pane beside the caller's.
    Open,
    /// The board pane's own view, for the repository given or the caller's.
    Watch {
        repo: Option<String>,
    },
}

/// The instruction a worker gets beside the item's title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Text(String),
    File(String),
}

/// Takes `--message <text>` or `--file <path>` out of `rest`, leaving the
/// other arguments. Both, or either twice, is refused.
fn take_body(rest: &[String]) -> Result<(Option<Body>, Vec<String>), String> {
    let mut body = None;
    let mut others = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let kind = match a.as_str() {
            "--message" => Body::Text,
            "--file" => Body::File,
            _ => {
                others.push(a.clone());
                continue;
            }
        };
        let value = it.next().ok_or_else(|| format!("{a} takes a value"))?;
        if body.is_some() {
            return Err("give one --message or one --file".into());
        }
        body = Some(kind(value.clone()));
    }
    Ok((body, others))
}

/// Which item a `send` delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendItem {
    Existing(u32),
    New(String),
}

/// Takes a leading `--repo <path>` off a `board` command line: the board of
/// that repository instead of the working directory's.
pub fn take_repo(args: &[String]) -> Result<(Option<String>, &[String]), String> {
    match args {
        [flag, rest @ ..] if flag == "--repo" => match rest.split_first() {
            Some((path, rest)) if !path.is_empty() && !path.starts_with('-') => {
                Ok((Some(path.clone()), rest))
            }
            _ => Err(format!("--repo takes a path\n{USAGE}")),
        },
        _ => Ok((None, args)),
    }
}

pub fn parse(args: &[String]) -> Result<Cmd, String> {
    let (sub, rest) = args.split_first().ok_or_else(|| USAGE.to_string())?;
    let bad = |why: &str| format!("board {sub}: {why}\n{USAGE}");
    match sub.as_str() {
        "add" => {
            if rest.is_empty() {
                return Err(bad("give the item text"));
            }
            Ok(Cmd::Add(rest.to_vec()))
        }
        "send" => {
            let (body, rest) = take_body(rest).map_err(|e| bad(&e))?;
            let rest = rest.as_slice();
            let mut flags = SendFlags::default();
            let mut new_text = None;
            let mut pos = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--again" => flags.again = true,
                    "--replace" => flags.replace = true,
                    "--new" => {
                        new_text = Some(
                            it.next()
                                .ok_or_else(|| bad("--new takes the item text"))?
                                .clone(),
                        )
                    }
                    f if f.starts_with("--") => return Err(bad(&format!("unknown flag {f}"))),
                    _ => pos.push(a.clone()),
                }
            }
            let (item, worker) = match (new_text, pos.as_slice()) {
                (Some(text), [worker]) => {
                    if flags.again {
                        return Err(bad("--again resends an existing item, not a new one"));
                    }
                    (SendItem::New(text), worker.clone())
                }
                (None, [n]) => (
                    SendItem::Existing(number(n).map_err(|e| bad(&e))?),
                    String::new(),
                ),
                (None, [n, worker]) => (
                    SendItem::Existing(number(n).map_err(|e| bad(&e))?),
                    worker.clone(),
                ),
                _ => {
                    return Err(bad(
                        "send <N> [<worker>], or send --new \"<text>\" <worker>",
                    ))
                }
            };
            Ok(Cmd::Send {
                item,
                worker,
                flags,
                body,
            })
        }
        "assign" => {
            let sent = rest.iter().any(|a| a == "--sent");
            let pos: Vec<&String> = rest.iter().filter(|a| *a != "--sent").collect();
            if let Some(f) = pos.iter().find(|a| a.starts_with("--")) {
                return Err(bad(&format!("unknown flag {f}")));
            }
            match pos.as_slice() {
                [n, worker] => Ok(Cmd::Assign {
                    n: number(n).map_err(|e| bad(&e))?,
                    worker: (*worker).clone(),
                    sent,
                }),
                _ => Err(bad("assign <N> <worker> [--sent]")),
            }
        }
        "done" => Ok(Cmd::Done(numbers(rest).map_err(|e| bad(&e))?)),
        "drop" => {
            let mut reason = String::new();
            let mut ns = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--reason" {
                    reason = it
                        .next()
                        .ok_or_else(|| bad("--reason takes the text"))?
                        .clone();
                } else {
                    ns.push(number(a).map_err(|e| bad(&e))?);
                }
            }
            if ns.is_empty() {
                return Err(bad("give the item numbers"));
            }
            Ok(Cmd::Drop { ns, reason })
        }
        "note" => match rest {
            [n, text] => Ok(Cmd::Note {
                n: number(n).map_err(|e| bad(&e))?,
                text: text.clone(),
            }),
            _ => Err(bad("note <N> \"<text>\"")),
        },
        "rm" => {
            let force = rest.iter().any(|a| a == "--force");
            let nums: Vec<String> = rest.iter().filter(|a| *a != "--force").cloned().collect();
            if let Some(f) = nums.iter().find(|a| a.starts_with("--")) {
                return Err(bad(&format!("unknown flag {f}")));
            }
            Ok(Cmd::Remove {
                ns: numbers(&nums).map_err(|e| bad(&e))?,
                force,
            })
        }
        "next" => {
            let (body, rest) = take_body(rest).map_err(|e| bad(&e))?;
            match rest.as_slice() {
                [worker] if !worker.starts_with("--") => Ok(Cmd::Next {
                    worker: worker.clone(),
                    body,
                }),
                _ => Err(bad("next <worker> [--message \"<text>\" | --file <path>]")),
            }
        }
        "prio" => match rest {
            [n, p] => {
                let priority = match p.as_str() {
                    "-" | "0" => 0,
                    "1" | "p1" | "P1" => 1,
                    "2" | "p2" | "P2" => 2,
                    "3" | "p3" | "P3" => 3,
                    _ => return Err(bad("priority is 1, 2, 3, or - to clear")),
                };
                Ok(Cmd::Prio {
                    n: number(n).map_err(|e| bad(&e))?,
                    priority,
                })
            }
            _ => Err(bad("prio <N> <1|2|3|->")),
        },
        "move" => match rest {
            [n, flag, k] if flag == "--to" => Ok(Cmd::Move {
                n: number(n).map_err(|e| bad(&e))?,
                to: number(k).map_err(|e| bad(&e))? as usize,
            }),
            _ => Err(bad("move <N> --to <K>")),
        },
        "wake" => match rest {
            [v] if v == "on" => Ok(Cmd::Wake {
                on: true,
                pane: None,
            }),
            [v, pane] if v == "on" && is_pane_id(pane) => Ok(Cmd::Wake {
                on: true,
                pane: Some(pane.clone()),
            }),
            [v] if v == "off" => Ok(Cmd::Wake {
                on: false,
                pane: None,
            }),
            _ => Err(bad("wake on [<pane>] | wake off")),
        },
        "link" => match rest {
            [n, pr] => {
                let pr = if pr == "-" {
                    0
                } else {
                    number(pr.trim_start_matches('#')).map_err(|e| bad(&e))?
                };
                Ok(Cmd::Link {
                    n: number(n).map_err(|e| bad(&e))?,
                    pr,
                })
            }
            _ => Err(bad("link <N> <#PR|->")),
        },
        "prune" => match rest {
            [] => Ok(Cmd::Prune),
            _ => Err(bad("prune takes no arguments")),
        },
        "clear" => match rest {
            [a] if a == "--all" => Ok(Cmd::Clear),
            _ => Err(bad("clear removes every item: say so with clear --all")),
        },
        "open" => match rest {
            [] => Ok(Cmd::Open),
            _ => Err(bad("open takes no arguments")),
        },
        "--watch" => match rest {
            [] => Ok(Cmd::Watch { repo: None }),
            [flag, repo] if flag == "--repo" => Ok(Cmd::Watch {
                repo: Some(repo.clone()),
            }),
            _ => Err(bad("--watch takes only --repo <path>")),
        },
        "show" => match rest {
            [] => Ok(Cmd::Show { json: false }),
            [j] if j == "--json" => Ok(Cmd::Show { json: true }),
            _ => Err(bad("show takes only --json")),
        },
        other => Err(format!("unknown board command: {other}\n{USAGE}")),
    }
}

fn number(s: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{s:?} is not an item number"))
}

fn numbers(args: &[String]) -> Result<Vec<u32>, String> {
    if args.is_empty() {
        return Err("give the item numbers".into());
    }
    args.iter().map(|a| number(a)).collect()
}

/// The repository whose board a caller in `cwd` means: the git top level —
/// or, inside a `wt` worktree, the repository that worktree was cut from, so a
/// worker and its advisor name the same board.
pub fn board_repo(state_dir: &Path, cwd: &str) -> Result<String, String> {
    let top = tower_wt::repo_root(cwd).map_err(|e| format!("not inside a git repository: {e}"))?;
    let metas =
        tower_wt::list_metas(state_dir).map_err(|e| format!("cannot read the wt tasks: {e}"))?;
    if let Some(m) = metas.into_iter().find(|m| m.path == top) {
        return Ok(m.repo);
    }
    // A tower worktree whose task record is missing or unreadable: its own
    // top level would open a second, empty board beside the real one.
    if top.replace('\\', "/").contains("/.herdr-tower/worktrees/") {
        return Err(format!(
            "{top} is a wt worktree with no readable task record — run board from the repository it was cut from"
        ));
    }
    Ok(top)
}

/// What a `board` command needs from the outside world.
pub struct Ctx<'a> {
    pub state_dir: &'a Path,
    pub repo: &'a str,
    pub boards: &'a BoardStore,
    pub store: &'a Store,
    pub pid: u32,
    pub now: OffsetDateTime,
    /// The backend whose records were refreshed just now, by the listing that
    /// refreshed them. An item whose receipt names another backend — or any,
    /// when this is `None` — is shown as last seen.
    pub observed: Option<&'a str>,
    /// The backend this command talks to (empty for the default socket); a
    /// pane id is resolved on it.
    pub backend: &'a str,
    /// How long a send to a task waits for its worker's record to appear —
    /// `wt new` returns before the supervisor has recorded the agent.
    pub settle: std::time::Duration,
    /// The pane this command runs in, if inside herdr: `board wake on` names
    /// it as the advisor to wake.
    pub caller: Option<&'a str>,
}

/// The text a worker receives for item `n`.
pub fn instruction(n: u32, text: &str, body: Option<&str>) -> String {
    match body.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => format!("[board item {n}] {text}\n\n{b}"),
        None => format!("[board item {n}] {text}"),
    }
}

/// The instruction text a `--message` or `--file` names.
fn body_text(body: &Option<Body>) -> Result<Option<String>, Failure> {
    match body {
        None => Ok(None),
        Some(Body::Text(t)) => Ok(Some(t.clone())),
        Some(Body::File(path)) => std::fs::read_to_string(path)
            .map(Some)
            .map_err(|e| Failure::Refused(format!("cannot read {path}: {e}"))),
    }
}

/// Why a `board` command failed, after anything it could print was printed.
#[derive(Debug)]
pub enum Failure {
    Refused(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused(m) => write!(f, "{m}"),
            Failure::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Failure {}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::Io(e)
    }
}

impl From<BoardError> for Failure {
    fn from(e: BoardError) -> Self {
        Failure::Refused(e.to_string())
    }
}

/// Runs one parsed command. `deliver(task, text)` performs the delivery.
pub fn run(
    out: &mut impl Write,
    ctx: &Ctx<'_>,
    cmd: &Cmd,
    deliver: impl FnOnce(&SendTo, &str) -> BoardDelivery,
) -> Result<(), Failure> {
    let (repo, now) = (ctx.repo, ctx.now);
    match cmd {
        Cmd::Add(texts) => {
            let ns = ctx.boards.update(repo, now, |b| b.add(texts, now))??;
            let list: Vec<String> = ns.iter().map(u32::to_string).collect();
            writeln!(out, "added item{} {}.", plural(ns.len()), list.join(", "))?;
        }
        Cmd::Done(ns) => {
            ctx.boards.update(repo, now, |b| b.done(ns, now))??;
            writeln!(out, "marked done: {}.", joined(ns))?;
        }
        Cmd::Drop { ns, reason } => {
            ctx.boards
                .update(repo, now, |b| b.drop_items(ns, reason, now))??;
            writeln!(out, "dropped: {}.", joined(ns))?;
        }
        Cmd::Note { n, text } => {
            ctx.boards.update(repo, now, |b| b.note(*n, text))??;
            if text.trim().is_empty() {
                writeln!(out, "note removed from item {n}.")?;
            } else {
                writeln!(out, "noted on item {n}.")?;
            }
        }
        Cmd::Show { json } => {
            let board = ctx.boards.load(repo, now)?;
            let records = ctx.store.list()?;
            if *json {
                let prs = pr_states(repo, &linked_prs(&board));
                writeln!(
                    out,
                    "{}",
                    show_json_with(&board, &records, now, ctx.observed, &prs)
                )?;
            } else {
                let prs = pr_states(repo, &linked_prs(&board));
                write!(
                    out,
                    "{}",
                    show_text_with(&board, &records, now, ctx.observed, &prs)
                )?;
            }
        }
        Cmd::Send {
            item,
            worker,
            flags,
            body,
        } => {
            let body = body_text(body)?;
            send(out, ctx, item, worker, *flags, body.as_deref(), deliver)?
        }
        Cmd::Assign { n, worker, sent } => assign(out, ctx, *n, worker, *sent)?,
        Cmd::Next { worker, body } => {
            let body = body_text(body)?;
            let n = next_for(ctx, worker)?;
            writeln!(out, "next for {worker}: item {n}.")?;
            send(
                out,
                ctx,
                &SendItem::Existing(n),
                "",
                SendFlags::default(),
                body.as_deref(),
                deliver,
            )?;
        }
        Cmd::Prio { n, priority } => {
            ctx.boards
                .update(repo, now, |b| b.set_priority(*n, *priority))??;
            match priority {
                0 => writeln!(out, "item {n} has no priority now.")?,
                p => writeln!(out, "item {n} is P{p}.")?,
            }
        }
        Cmd::Move { n, to } => {
            let at = ctx.boards.update(repo, now, |b| b.move_item(*n, *to))??;
            writeln!(out, "item {n} is at position {at}.")?;
        }
        Cmd::Wake { on, pane } => {
            if *on {
                let Some(caller) = pane.as_deref().or(ctx.caller) else {
                    return Err(Failure::Refused(
                        "board wake on names the pane it runs in: run it from the advisor's pane, or name the pane (wake on w2:pBR)"
                            .into(),
                    ));
                };
                let records = ctx.store.list()?;
                let pin = pane_worker(ctx, caller, &records)?;
                let pane = pin.pane_id.clone();
                ctx.boards.update(repo, now, |b| {
                    b.waker = Some(pin);
                    Ok(())
                })??;
                writeln!(
                    out,
                    "pane {pane} will be woken when an item is ready for review (while it is idle)."
                )?;
            } else {
                ctx.boards.update(repo, now, |b| {
                    b.waker = None;
                    Ok(())
                })??;
                writeln!(out, "nobody is woken for reviews now.")?;
            }
        }
        Cmd::Link { n, pr } => {
            ctx.boards.update(repo, now, |b| b.link(*n, *pr))??;
            match pr {
                0 => writeln!(out, "item {n} is linked to no PR now.")?,
                pr => writeln!(out, "item {n} is linked to PR #{pr}.")?,
            }
        }
        Cmd::Remove { ns, force } => {
            let gone = ctx.boards.update(repo, now, |b| b.remove(ns, *force))??;
            writeln!(out, "removed item{} {}.", plural(gone.len()), joined(&gone))?;
        }
        Cmd::Prune => {
            let gone = ctx.boards.update(repo, now, |b| Ok(b.prune()))??;
            if gone.is_empty() {
                writeln!(out, "nothing closed to remove.")?;
            } else {
                writeln!(
                    out,
                    "removed closed item{} {}.",
                    plural(gone.len()),
                    joined(&gone)
                )?;
            }
        }
        Cmd::Clear => {
            let count = ctx.boards.update(repo, now, |b| b.clear())??;
            writeln!(out, "cleared the board ({count} item{}).", plural(count))?;
        }
        // The pane and the view reach herdr and the terminal: `main` runs them.
        Cmd::Open | Cmd::Watch { .. } => {
            return Err(Failure::Refused(
                "board open and --watch run from main".into(),
            ))
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send(
    out: &mut impl Write,
    ctx: &Ctx<'_>,
    item: &SendItem,
    worker: &str,
    flags: SendFlags,
    body: Option<&str>,
    deliver: impl FnOnce(&SendTo, &str) -> BoardDelivery,
) -> Result<(), Failure> {
    let (repo, now) = (ctx.repo, ctx.now);
    let records = ctx.store.list()?;
    // Resolved before a reservation is written, so a wrong worker is refused
    // with nothing changed.
    let (to, agent_id) = if worker.is_empty() {
        let SendItem::Existing(n) = item else {
            return Err(Failure::Refused("send --new needs a worker".into()));
        };
        let board = ctx.boards.load(repo, now)?;
        let it = board.item(*n).ok_or(BoardError::NoItem(*n))?;
        match assigned(it) {
            Some(Worker::Task(t)) => task_target(ctx, &t, &records)?,
            Some(Worker::Pane(pin)) => {
                let agent = pin.agent_id.clone();
                (SendTo::Pane(pin), agent)
            }
            Some(Worker::Label(l)) => {
                return Err(Failure::Refused(format!(
                    "item {n} is assigned to {l}, a worker the tower cannot send to"
                )))
            }
            None => {
                return Err(Failure::Refused(format!(
                    "item {n} is not assigned to anybody — send {n} <worker>"
                )))
            }
        }
    } else {
        match resolve_worker(ctx, worker, &records)? {
            Worker::Task(t) => task_target(ctx, &t, &records)?,
            Worker::Pane(pin) => {
                let agent = pin.agent_id.clone();
                (SendTo::Pane(pin), agent)
            }
            Worker::Label(l) => {
                return Err(Failure::Refused(format!(
                "{l} is not a task or a pane here; the tower cannot send to it (assign it instead)"
            )))
            }
        }
    };
    // A worker `wt new` just started may not be recorded yet: give it a few
    // seconds rather than refusing a send made right after.
    let (to, agent_id) = match (&to, agent_id.is_empty()) {
        (SendTo::Task { task, .. }, true) if !ctx.settle.is_zero() => {
            let task = task.clone();
            let until = std::time::Instant::now() + ctx.settle;
            let mut found = (to, agent_id);
            while std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let records = ctx.store.list()?;
                let again = task_target(ctx, &task, &records)?;
                if !again.1.is_empty() {
                    found = again;
                    break;
                }
            }
            found
        }
        _ => (to, agent_id),
    };
    let task = match &to {
        SendTo::Task { task, .. } => task.clone(),
        SendTo::Pane(p) => p.pane_id.clone(),
    };
    let task = task.as_str();
    let (ticket, text) = ctx.boards.update(repo, now, |b| {
        let n = match item {
            SendItem::Existing(n) => *n,
            SendItem::New(text) => b.add(std::slice::from_ref(text), now)?[0],
        };
        let ticket = b.reserve_send_to(n, to.clone(), &agent_id, flags, ctx.pid, now)?;
        let text = b.item(n).map(|i| i.text.clone()).unwrap_or_default();
        Ok((ticket, text))
    })??;
    let n = ticket.n;
    let delivery = deliver(&to, &instruction(n, &text, body));
    let applied = ctx
        .boards
        .update(repo, now, |b| b.apply(ticket, delivery.outcome(), now));
    let landed = match applied {
        Ok(Ok(a)) => Ok(a),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match (&delivery, landed) {
        (_, Err(why)) => {
            // Delivered or not, the board did not take the outcome: the
            // reservation stays and recovers to `sent?` once this process ends.
            return Err(Failure::Refused(format!(
                "item {n}: {} — but the board could not record it ({why}); it will show as sent? — check {task}",
                describe(&delivery)
            )));
        }
        (_, Ok(tower_state::Applied::Stale)) => {
            return Err(Failure::Refused(format!(
                "item {n}: {} — but the item changed meanwhile (done, dropped or recovered), so the board kept that and has no receipt for this send; check {task}",
                describe(&delivery)
            )));
        }
        (
            BoardDelivery::Accepted {
                receipt,
                verdict_kept,
            },
            Ok(tower_state::Applied::Yes),
        ) => {
            writeln!(out, "sent item {n} to {task}.")?;
            if flags.again {
                writeln!(
                    out,
                    "  note: resent with --again; {task} may have received it twice."
                )?;
            }
            if receipt.boundary_lost {
                writeln!(
                    out,
                    "  note: the completion boundary was not recorded; its progress is marked uncertain."
                )?;
            }
            if receipt.session_id.is_empty() && receipt.record_born.is_none() {
                writeln!(
                    out,
                    "  note: {task}'s session and record generation are not known; its progress is marked uncertain."
                )?;
            }
            if *verdict_kept {
                writeln!(
                    out,
                    "  note: {task}'s old drive verdict could not be cleared; wt list may still call it merge-ready."
                )?;
            }
        }
        (BoardDelivery::Refused(e), Ok(tower_state::Applied::Yes)) => {
            let stays = match item {
                SendItem::New(_) => format!(" — item {n} stays on the board, planned"),
                SendItem::Existing(_) => String::new(),
            };
            return Err(Failure::Refused(format!("item {n} not sent: {e}{stays}")));
        }
        (BoardDelivery::Unknown(e), Ok(tower_state::Applied::Yes)) => {
            return Err(Failure::Refused(format!(
                "item {n}: the delivery may or may not have reached {task} ({e}). It is sent? now — look at the worker, then, if it never arrived, `board send {n} {task} --again` (add --replace if {task} holds another open item); if it did, `board done {n}` once you have reviewed the result."
            )));
        }
    }
    Ok(())
}

/// A worker, as a command names it.
enum Worker {
    Task(String),
    Pane(PaneWorker),
    Label(String),
}

/// Whether `s` has the shape of a herdr pane id (`w2:pBR`).
pub fn is_pane_id(s: &str) -> bool {
    let Some((w, p)) = s.split_once(':') else {
        return false;
    };
    let tail = |x: &str, c: char| {
        x.strip_prefix(c)
            .is_some_and(|r| !r.is_empty() && r.chars().all(|ch| ch.is_ascii_alphanumeric()))
    };
    tail(w, 'w') && tail(p, 'p')
}

/// What a worker argument names. `task:`, `pane:` and `label:` say it
/// outright; bare, a `wt` task wins, then a pane-shaped id (which must be a
/// live agent here — a typo is refused, not taken for a label), then a label.
fn resolve_worker(ctx: &Ctx<'_>, arg: &str, records: &[Agent]) -> Result<Worker, Failure> {
    let arg = arg.trim();
    if let Some(t) = arg.strip_prefix("task:") {
        return Ok(Worker::Task(t.to_string()));
    }
    if let Some(p) = arg.strip_prefix("pane:") {
        return pane_worker(ctx, p, records).map(Worker::Pane);
    }
    if let Some(l) = arg.strip_prefix("label:") {
        return label(l).map(Worker::Label);
    }
    if tower_wt::load_meta(ctx.state_dir, arg).is_ok() {
        return Ok(Worker::Task(arg.to_string()));
    }
    if is_pane_id(arg) {
        return pane_worker(ctx, arg, records).map(Worker::Pane);
    }
    label(arg).map(Worker::Label)
}

fn label(l: &str) -> Result<String, Failure> {
    let l = tower_state::clean_text(l.trim()).into_owned();
    if l.is_empty() {
        return Err(Failure::Refused("a label needs some text".into()));
    }
    Ok(l)
}

/// The live agent in pane `id` on this backend, pinned as it is now.
fn pane_worker(ctx: &Ctx<'_>, id: &str, records: &[Agent]) -> Result<PaneWorker, Failure> {
    let live: Vec<&Agent> = records
        .iter()
        .filter(|a| {
            a.pane.backend == ctx.backend
                && a.pane.pane_id == id
                && a.state != tower_state::AgentState::Dead
        })
        .collect();
    match live.as_slice() {
        [a] => Ok(PaneWorker {
            backend: ctx.backend.to_string(),
            pane_id: id.to_string(),
            agent_id: a.id.clone(),
            session_id: a.session_id.clone(),
            born_at: a.born_at,
        }),
        [] => Err(Failure::Refused(format!(
            "no agent is running in pane {id} here — label:{id} names a worker the tower cannot see"
        ))),
        _ => Err(Failure::Refused(format!(
            "more than one agent is recorded in pane {id} — try again in a few seconds"
        ))),
    }
}

/// A task send's target, with the task's agent when it is the only live one
/// in the worktree — so a task and a pane naming one agent count as one.
fn task_target(ctx: &Ctx<'_>, task: &str, records: &[Agent]) -> Result<(SendTo, String), Failure> {
    let meta =
        tower_wt::load_meta(ctx.state_dir, task).map_err(|e| Failure::Refused(e.to_string()))?;
    if meta.repo != ctx.repo {
        return Err(Failure::Refused(format!(
            "task {task} belongs to {}, not this board's repository ({})",
            meta.repo, ctx.repo
        )));
    }
    let live: Vec<&Agent> = records
        .iter()
        .filter(|a| {
            a.cwd == meta.path
                && a.pane.backend == ctx.backend
                && a.state != tower_state::AgentState::Dead
        })
        .collect();
    let agent = match live.as_slice() {
        [a] => a.id.clone(),
        _ => String::new(),
    };
    Ok((
        SendTo::Task {
            task: task.to_string(),
            worktree: meta.path,
        },
        agent,
    ))
}

/// The first planned item queued for `worker` — assigned to it, in board
/// order. The send that follows refuses while the worker still holds an item.
fn next_for(ctx: &Ctx<'_>, worker: &str) -> Result<u32, Failure> {
    let records = ctx.store.list()?;
    let who = resolve_worker(ctx, worker, &records)?;
    let board = ctx.boards.load(ctx.repo, ctx.now)?;
    let mine = |i: &BoardItem| match &who {
        Worker::Pane(p) => i
            .pane
            .as_ref()
            .is_some_and(|q| q.backend == p.backend && q.pane_id == p.pane_id),
        Worker::Task(t) => i.pane.is_none() && i.task == *t,
        Worker::Label(_) => false,
    };
    if let Worker::Label(l) = &who {
        return Err(Failure::Refused(format!(
            "{l} is not a task or a pane here; the tower cannot send to it"
        )));
    }
    if let Some(open) = board.items.iter().find(|i| i.status.is_open() && mine(i)) {
        return Err(Failure::Refused(format!(
            "{worker} still holds item {} — `board done {}` once you have reviewed it, then next",
            open.n, open.n
        )));
    }
    // Highest priority first (1 before 3, any before none), then board order.
    let rank = |i: &BoardItem| if i.priority == 0 { u8::MAX } else { i.priority };
    board
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| i.status == ItemStatus::Planned && mine(i))
        .min_by_key(|(pos, i)| (rank(i), *pos))
        .map(|(_, i)| i.n)
        .ok_or_else(|| {
            Failure::Refused(format!(
                "nothing is queued for {worker} (board assign N {worker})"
            ))
        })
}

/// Who an item is assigned to, if anybody.
fn assigned(item: &BoardItem) -> Option<Worker> {
    if let Some(p) = &item.pane {
        return Some(Worker::Pane(p.clone()));
    }
    if !item.task.is_empty() {
        return Some(Worker::Task(item.task.clone()));
    }
    (!item.label.is_empty()).then(|| Worker::Label(item.label.clone()))
}

fn assign(
    out: &mut impl Write,
    ctx: &Ctx<'_>,
    n: u32,
    worker: &str,
    sent: bool,
) -> Result<(), Failure> {
    let (repo, now) = (ctx.repo, ctx.now);
    let records = ctx.store.list()?;
    let who = resolve_worker(ctx, worker, &records)?;
    if sent {
        let Worker::Pane(pin) = who else {
            return Err(Failure::Refused(
                "--sent takes over an item sent to a pane; name the pane (w2:pBR)".into(),
            ));
        };
        let Some(agent) = records.iter().find(|a| a.id == pin.agent_id) else {
            return Err(Failure::Refused(format!(
                "no record for pane {}",
                pin.pane_id
            )));
        };
        // The item went out before the board heard of it: a turn already
        // running is taken to be its turn. Anything else is uncertain, and
        // the receipt says so.
        let work_seq = if agent.state == tower_state::AgentState::Working {
            agent.work_seq.saturating_sub(1)
        } else {
            agent.work_seq
        };
        let receipt = tower_state::Receipt {
            backend: ctx.backend.to_string(),
            session_id: if agent.born_at.is_some() {
                agent.session_id.clone()
            } else {
                String::new()
            },
            work_seq,
            agent_id: agent.id.clone(),
            adopted: true,
            record_born: agent.born_at,
            at: now,
            boundary_lost: false,
        };
        let pane = pin.pane_id.clone();
        ctx.boards
            .update(repo, now, |b| b.adopt(n, pin, receipt))??;
        writeln!(
            out,
            "item {n} is now sent to pane {pane}; progress is tracked from here, marked uncertain (it went out before the board knew)."
        )?;
        return Ok(());
    }
    let (to, said) = match who {
        Worker::Task(t) => {
            let (to, _) = task_target(ctx, &t, &records)?;
            (AssignTo::Send(to), format!("task {t}"))
        }
        Worker::Pane(pin) => {
            let said = format!("pane {}", pin.pane_id);
            (AssignTo::Send(SendTo::Pane(pin)), said)
        }
        Worker::Label(l) => {
            let said = format!("{l} (shown only; the tower cannot send to or track it)");
            (AssignTo::Label(l), said)
        }
    };
    ctx.boards.update(repo, now, |b| b.assign(n, to))??;
    writeln!(out, "assigned item {n} to {said}.")?;
    Ok(())
}

fn describe(d: &BoardDelivery) -> String {
    match d {
        BoardDelivery::Accepted { .. } => "delivered".into(),
        BoardDelivery::Refused(e) => format!("not delivered ({e})"),
        BoardDelivery::Unknown(e) => format!("delivery unknown ({e})"),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn joined(ns: &[u32]) -> String {
    ns.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}

/// Every record in the item's worktree on the backend its receipt names —
/// dead ones too, which is how `gone` is told from `no record`.
pub(crate) fn records_for<'a>(item: &BoardItem, records: &'a [Agent]) -> Vec<&'a Agent> {
    if let Some(p) = item.pane_worker() {
        return records
            .iter()
            .filter(|a| a.pane.backend == p.backend && a.pane.pane_id == p.pane_id)
            .collect();
    }
    // While a send is out, the task it goes to — as `target` and the task
    // column have it — not the one the item was with before.
    let worktree = match (&item.reservation, item.status) {
        (Some(r), ItemStatus::Sending) => r.worktree.as_str(),
        _ => item.worktree.as_str(),
    };
    if worktree.is_empty() {
        return Vec::new();
    }
    let backend = item
        .receipt
        .as_ref()
        .map(|r| r.backend.as_str())
        .unwrap_or("");
    records
        .iter()
        .filter(|a| a.cwd == worktree && a.pane.backend == backend)
        .collect()
}

/// The column's words. `stale` marks a column read from records herdr could
/// not refresh just now.
/// The `[w2:pBR]` a row starts with: the pane the item is with — named, or
/// the one live agent in its task's worktree — or its label. `None` for an
/// item nobody has.
pub(crate) fn who(item: &BoardItem, records: &[&Agent]) -> Option<String> {
    let raw = if item.pane_worker().is_none()
        && !item.label.is_empty()
        && item.status != ItemStatus::Sending
    {
        item.label.clone()
    } else {
        pane_of(item, records)?
    };
    Some(format!("[{}]", tower_state::clean_text(&raw)))
}

/// The pane [`who`] names, when it names a pane rather than a label: where a
/// click on the row in the board pane goes.
pub(crate) fn pane_of(item: &BoardItem, records: &[&Agent]) -> Option<String> {
    if let Some(p) = item.pane_worker() {
        return Some(p.pane_id.clone());
    }
    if !item.label.is_empty() && item.status != ItemStatus::Sending {
        return None;
    }
    if item.target().is_empty() {
        return None;
    }
    let live: Vec<&&Agent> = records
        .iter()
        .filter(|a| a.state != tower_state::AgentState::Dead)
        .collect();
    match live.as_slice() {
        [a] => Some(a.pane.pane_id.clone()),
        _ => None,
    }
}

/// The task column: a `wt` task's name, or empty for a pane, a label or
/// nobody — a dash there sat right before the closed icon `–`.
pub(crate) fn task_cell(item: &BoardItem) -> String {
    let task = match (&item.reservation, item.status) {
        (Some(r), ItemStatus::Sending) => r.task.as_str(),
        _ => item.task.as_str(),
    };
    tower_state::clean_text(task).into_owned()
}

/// A row's text, led by who has it.
pub(crate) fn row_text(item: &BoardItem, records: &[&Agent]) -> String {
    let text = tower_state::clean_text(&item.text);
    let lead = match (item.priority, who(item, records)) {
        (0, None) => String::new(),
        (0, Some(w)) => format!("{w} "),
        (p, None) => format!("P{p} "),
        (p, Some(w)) => format!("P{p} {w} "),
    };
    format!("{lead}{text}")
}

/// How long one `gh pr view` may take.
const PR_STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The state of each linked pull request, by number, as `gh` reports it in
/// the repository: `OPEN`, `MERGED` or `CLOSED`. Empty without `gh`; a PR it
/// cannot read is left out.
pub fn pr_states(repo: &str, prs: &[u32]) -> std::collections::HashMap<u32, String> {
    let tools = tower_usage::ToolSearch::from_env();
    let Some(gh) = tools.lookup("gh") else {
        return Default::default();
    };
    let mut wanted: Vec<u32> = prs.iter().copied().filter(|p| *p != 0).collect();
    wanted.sort_unstable();
    wanted.dedup();
    wanted
        .into_iter()
        .filter_map(|pr| {
            let mut cmd = std::process::Command::new(&gh);
            cmd.args([
                "pr",
                "view",
                &pr.to_string(),
                "--json",
                "state",
                "--jq",
                ".state",
            ])
            .current_dir(repo)
            .env("PATH", tools.extended_path())
            .env("GH_PROMPT_DISABLED", "1");
            // A stuck `gh` (the network, an auth prompt) is killed, not waited on.
            let ran = crate::mcp::run::run(cmd, PR_STATE_TIMEOUT, 4096).ok()?;
            let state = ran.stdout.trim().to_string();
            (ran.code == Some(0) && !ran.timed_out && !state.is_empty()).then_some((pr, state))
        })
        .collect()
}

/// The PRs whose state is worth asking for: linked to an item still in play.
pub fn linked_prs(board: &Board) -> Vec<u32> {
    board
        .items
        .iter()
        .filter(|i| i.pr != 0 && !i.status.is_closed())
        .map(|i| i.pr)
        .collect()
}

/// ` · PR #3231 merged`, or empty.
pub(crate) fn pr_cell(item: &BoardItem, states: &std::collections::HashMap<u32, String>) -> String {
    if item.pr == 0 {
        return String::new();
    }
    match states.get(&item.pr).map(|s| s.to_ascii_lowercase()) {
        Some(state) => format!(" · PR #{} {state}", item.pr),
        None => format!(" · PR #{}", item.pr),
    }
}

pub(crate) fn column_text(c: &Column, now: OffsetDateTime, stale: bool) -> String {
    let mut s = format!("{} {}", c.icon(), c.label());
    if let Some(since) = c.since {
        if !c.struck() {
            s.push_str("  ");
            s.push_str(&tower_text::since(since, now));
        }
    }
    if c.uncertain {
        s.push_str("  (uncertain)");
    }
    if stale {
        s.push_str("  (stale)");
    }
    s
}

/// Whether an item's column comes from worker records that were not refreshed
/// just now: read from records at all (only a `sent` item is), and from a
/// backend other than the one observed.
pub(crate) fn stale(item: &BoardItem, observed: Option<&str>) -> bool {
    item.status == ItemStatus::Sent
        && observed
            != Some(
                item.receipt
                    .as_ref()
                    .map(|r| r.backend.as_str())
                    .unwrap_or(""),
            )
}

/// The board as plain text: what the advisor reads back after every command.
pub fn show_text(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
) -> String {
    show_text_with(board, records, now, observed, &Default::default())
}

/// [`show_text`] with the linked PRs' states (see [`pr_states`]).
pub fn show_text_with(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
    prs: &std::collections::HashMap<u32, String>,
) -> String {
    let done = board
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Done)
        .count();
    let name = tower_state::clean_text(
        Path::new(&board.repo)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&board.repo),
    );
    let mut s = format!("Board · {name}  {done}/{}\n", board.items.len());
    if observed.is_none() {
        s.push_str("  (herdr did not answer — worker progress is as last seen)\n");
    }
    if let Some(w) = &board.waker {
        s.push_str(&format!(
            "  (wakes [{}] when an item is ready for review)\n",
            tower_state::clean_text(&w.pane_id)
        ));
    }
    if board.items.is_empty() {
        s.push_str("  (empty — `board add \"…\"` to plan items)\n");
        return s;
    }
    // Struck items sink below the live ones; the sort is stable, so each group
    // keeps the board's own order.
    let mut rows: Vec<(&BoardItem, bool)> = board
        .items
        .iter()
        .map(|i| (i, column(i, &records_for(i, records)).struck()))
        .collect();
    rows.sort_by_key(|(_, struck)| *struck);
    for (item, _) in rows {
        let found = records_for(item, records);
        let c = column(item, &found);
        let mark = if c.struck() { "✗" } else { " " };
        let text = tower_text::truncate_cell(&row_text(item, &found), 48);
        let task = task_cell(item);
        s.push_str(&format!(
            "{mark}{:>3}  {}  {}  {}\n",
            item.n,
            tower_text::pad_right(&text, 48),
            tower_text::pad_right(&task, 14),
            column_text(&c, now, stale(item, observed)) + &pr_cell(item, prs)
        ));
        if !item.note.is_empty() {
            s.push_str(&format!(
                "      ↳ {}\n",
                tower_state::clean_text(&item.note)
            ));
        }
        if !item.reason.is_empty() {
            s.push_str(&format!(
                "      ↳ dropped: {}\n",
                tower_state::clean_text(&item.reason)
            ));
        }
    }
    s
}

/// The board as JSON: every item, its column, and the evidence behind it.
pub fn show_json(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
) -> String {
    show_json_with(board, records, now, observed, &Default::default())
}

/// [`show_json`] with the linked PRs' states.
pub fn show_json_with(
    board: &Board,
    records: &[Agent],
    now: OffsetDateTime,
    observed: Option<&str>,
    prs: &std::collections::HashMap<u32, String>,
) -> String {
    let items: Vec<serde_json::Value> = board
        .items
        .iter()
        .map(|item| {
            let found = records_for(item, records);
            let c = column(item, &found);
            let evidence: Vec<serde_json::Value> = found
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "id": a.id,
                        "backend": a.pane.backend,
                        "pane_id": a.pane.pane_id,
                        "state": a.state,
                        "session_id": a.session_id,
                        "work_seq": a.work_seq,
                        "born_at": a.born_at.and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()),
                        "state_since": a.state_since.format(&time::format_description::well_known::Rfc3339).ok(),
                        "updated_at": a.updated_at.format(&time::format_description::well_known::Rfc3339).ok(),
                    })
                })
                .collect();
            serde_json::json!({
                "item": item,
                "records": evidence,
                "column": {
                    "progress": format!("{:?}", c.progress),
                    "label": c.label(),
                    "icon": c.icon().to_string(),
                    "since": c.since.and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()),
                    "elapsed": c.since.map(|t| tower_text::since(t, now)),
                    "uncertain": c.uncertain,
                    "struck": c.struck(),
                    "stale": stale(item, observed),
                    "pr_state": prs.get(&item.pr),
                },
            })
        })
        .collect();
    serde_json::to_string_pretty(
        &serde_json::json!({ "repo": board.repo, "observed": observed.is_some(), "observed_backend": observed, "items": items }),
    )
    .unwrap_or_default()
}

/// The line an idle loop adds for an advisor whose repository has a board:
/// what is out, what waits for review or for a person, what is still planned.
/// `None` when nothing on the board is open or planned.
///
/// `observed(backend)` says whether that backend's records were refreshed
/// just now; an item read from any other backend's records is marked `?` too.
pub fn loop_summary(
    board: &Board,
    records: &[Agent],
    observed: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let mut groups: Vec<(&str, Vec<String>)> = [
        "needs you",
        "review",
        "check",
        "working",
        "not started",
        "sent",
        "planned",
    ]
    .into_iter()
    .map(|g| (g, Vec::new()))
    .collect();
    for item in &board.items {
        let c = column(item, &records_for(item, records));
        let group = match c.progress {
            Progress::Waiting | Progress::Errored => "needs you",
            Progress::Review => "review",
            Progress::SentUnknown
            | Progress::Replaced
            | Progress::Unclear
            | Progress::Ambiguous
            | Progress::Gone
            | Progress::Unseen => "check",
            Progress::Working => "working",
            Progress::NotStarted => "not started",
            Progress::Withheld | Progress::Sending => "sent",
            Progress::Planned => "planned",
            Progress::Done | Progress::Dropped | Progress::Superseded => continue,
        };
        if let Some((_, ns)) = groups.iter_mut().find(|(g, _)| *g == group) {
            // `?`: the column's evidence is weaker than usual — `show` says
            // `(uncertain)` on the same row.
            let unseen = item.status == ItemStatus::Sent
                && !observed(
                    item.receipt
                        .as_ref()
                        .map(|r| r.backend.as_str())
                        .unwrap_or(""),
                );
            ns.push(if c.uncertain || unseen {
                format!("{}?", item.n)
            } else {
                item.n.to_string()
            });
        }
    }
    let parts: Vec<String> = groups
        .into_iter()
        .filter(|(_, ns)| !ns.is_empty())
        .map(|(g, ns)| format!("{g} {}", ns.join(", ")))
        .collect();
    (!parts.is_empty()).then(|| {
        format!(
            "Dispatch board (herdr-tower board show): {}. Strike an item through with `herdr-tower board done N` once you have reviewed it — that is separate from `loop done`.",
            parts.join(" · ")
        )
    })
}

/// [`loop_summary`] for an agent working in `cwd`, reading its repository's
/// board. `None` when `cwd` is not in a repository or its board is empty.
pub fn loop_line(
    state_dir: &Path,
    cwd: &str,
    records: &[Agent],
    now: OffsetDateTime,
    observed: &dyn Fn(&str) -> bool,
) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    // For the advisor only: a worker's loop runs in a `wt` worktree, and the
    // board is not its to act on.
    let top = tower_wt::repo_root(cwd).ok()?;
    let metas = tower_wt::list_metas(state_dir).ok()?;
    if metas.iter().any(|m| m.path == top) || top.contains("/.herdr-tower/worktrees/") {
        return None;
    }
    let repo = top;
    let boards = BoardStore::new(state_dir).ok()?;
    if !boards.path_for(&repo).exists() {
        return None;
    }
    let board = boards.load(&repo, now).ok()?;
    loop_summary(&board, records, observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_state::{AgentState, Receipt};

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(
            parse(&args(&["add", "a", "b"])),
            Ok(Cmd::Add(args(&["a", "b"])))
        );
        assert_eq!(
            parse(&args(&["send", "3", "i-e", "--replace"])),
            Ok(Cmd::Send {
                item: SendItem::Existing(3),
                worker: "i-e".into(),
                flags: SendFlags {
                    again: false,
                    replace: true
                },
                body: None,
            })
        );
        assert_eq!(
            parse(&args(&["send", "--new", "fix it", "i-e"])),
            Ok(Cmd::Send {
                item: SendItem::New("fix it".into()),
                worker: "i-e".into(),
                flags: SendFlags::default(),
                body: None,
            })
        );
        assert_eq!(parse(&args(&["done", "1", "2"])), Ok(Cmd::Done(vec![1, 2])));
        assert_eq!(
            parse(&args(&["drop", "4", "--reason", "not needed"])),
            Ok(Cmd::Drop {
                ns: vec![4],
                reason: "not needed".into()
            })
        );
        assert_eq!(
            parse(&args(&["note", "2", "tests failing"])),
            Ok(Cmd::Note {
                n: 2,
                text: "tests failing".into()
            })
        );
        assert_eq!(
            parse(&args(&["show", "--json"])),
            Ok(Cmd::Show { json: true })
        );
        assert_eq!(parse(&args(&["open"])), Ok(Cmd::Open));
        assert_eq!(parse(&args(&["--watch"])), Ok(Cmd::Watch { repo: None }));
        assert_eq!(
            parse(&args(&["--watch", "--repo", "/w/r"])),
            Ok(Cmd::Watch {
                repo: Some("/w/r".into())
            })
        );
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        for bad in [
            vec![],
            vec!["add"],
            vec!["send"],
            vec!["assign", "3"],
            vec!["assign", "x", "w2:p1"],
            vec!["assign", "3", "w2:p1", "--bogus"],
            vec!["send", "x", "i-e"],
            vec!["send", "0", "i-e"],
            vec!["send", "--new", "t", "i-e", "--again"],
            vec!["send", "3", "i-e", "--bogus"],
            vec!["done"],
            vec!["done", "two"],
            vec!["drop", "--reason"],
            vec!["note", "1"],
            vec!["show", "extra"],
            vec!["open", "x"],
            vec!["--watch", "--repo"],
            vec!["frobnicate"],
        ] {
            assert!(parse(&args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_worker_is_told_which_item_it_is() {
        assert_eq!(
            instruction(3, "fix the parser", None),
            "[board item 3] fix the parser"
        );
    }

    // --- running, against real stores and a stand-in delivery -------------

    /// A repository with one commit and a `wt` task `name` cut from it.
    fn task_in_repo(name: &str) -> (tempfile::TempDir, tempfile::TempDir, tower_wt::Meta) {
        let git = |dir: &Path, a: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(a)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {a:?}");
        };
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.name", "fixture"]);
        git(
            repo.path(),
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "init"]);
        let state = tempfile::tempdir().unwrap();
        let meta = tower_wt::new_task(
            state.path(),
            &tower_wt::NewOptions {
                task: name,
                repo: repo.path().to_str().unwrap(),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        (repo, state, meta)
    }

    #[test]
    fn a_worker_in_its_worktree_names_its_repositorys_board() {
        let (_repo, state, meta) = task_in_repo("i-e");
        assert_eq!(board_repo(state.path(), &meta.path).unwrap(), meta.repo);
        assert_eq!(board_repo(state.path(), &meta.repo).unwrap(), meta.repo);
    }

    struct World {
        _repo: tempfile::TempDir,
        state: tempfile::TempDir,
        boards: BoardStore,
        store: Store,
        meta: tower_wt::Meta,
    }

    impl World {
        fn new() -> World {
            let (repo, state, meta) = task_in_repo("i-e");
            let boards = BoardStore::new(state.path()).unwrap();
            let store = Store::new(state.path().join("agents")).unwrap();
            World {
                _repo: repo,
                state,
                boards,
                store,
                meta,
            }
        }

        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                state_dir: self.state.path(),
                repo: &self.meta.repo,
                boards: &self.boards,
                store: &self.store,
                pid: std::process::id(),
                now: now(),
                observed: Some(""),
                backend: "",
                settle: std::time::Duration::ZERO,
                caller: None,
            }
        }

        fn run(&self, a: &[&str], d: BoardDelivery) -> (String, Result<(), Failure>) {
            let mut out = Vec::new();
            let cmd = parse(&args(a)).unwrap();
            let r = run(&mut out, &self.ctx(), &cmd, |_, _| d);
            (String::from_utf8(out).unwrap(), r)
        }

        /// Runs a step the test only sets up with; it has to succeed.
        fn must(&self, a: &[&str], d: BoardDelivery) {
            let (out, r) = self.run(a, d);
            assert!(r.is_ok(), "{a:?}: {r:?}\n{out}");
        }

        fn status(&self, n: u32) -> ItemStatus {
            self.boards
                .load(&self.meta.repo, now())
                .unwrap()
                .item(n)
                .unwrap()
                .status
        }
    }

    fn accepted() -> BoardDelivery {
        BoardDelivery::Accepted {
            receipt: Receipt {
                backend: String::new(),
                session_id: "S1".into(),
                work_seq: 5,
                agent_id: String::new(),
                adopted: false,
                record_born: Some(now()),
                at: now(),
                boundary_lost: false,
            },
            verdict_kept: false,
        }
    }

    fn unused() -> BoardDelivery {
        BoardDelivery::Unknown(tower_wt::Error::NoTask)
    }

    #[test]
    fn add_send_done_round_trip() {
        let w = World::new();
        let (out, r) = w.run(&["add", "parser", "tests"], unused());
        assert!(r.is_ok());
        assert_eq!(out, "added items 1, 2.\n");

        let mut told = String::new();
        let mut buf = Vec::new();
        let cmd = parse(&args(&["send", "1", "i-e"])).unwrap();
        run(&mut buf, &w.ctx(), &cmd, |to, text| {
            let SendTo::Task { task, .. } = to else {
                panic!("{to:?}")
            };
            told = format!("{task}: {text}");
            accepted()
        })
        .unwrap();
        assert_eq!(told, "i-e: [board item 1] parser");
        assert_eq!(String::from_utf8(buf).unwrap(), "sent item 1 to i-e.\n");
        assert_eq!(w.status(1), ItemStatus::Sent);

        let (_, r) = w.run(&["done", "1"], unused());
        assert!(r.is_ok());
        assert_eq!(w.status(1), ItemStatus::Done);
    }

    #[test]
    fn send_new_adds_and_delivers_in_one_go() {
        let w = World::new();
        let (out, r) = w.run(&["send", "--new", "benchmark", "i-e"], accepted());
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(out, "sent item 1 to i-e.\n");
        assert_eq!(w.status(1), ItemStatus::Sent);
    }

    #[test]
    fn a_refused_delivery_leaves_the_item_planned_and_fails() {
        let w = World::new();
        w.must(&["add", "parser"], unused());
        let refused = BoardDelivery::Refused(tower_wt::Error::WorkerBusy {
            task: "i-e".into(),
            status: "working",
        });
        let (_, r) = w.run(&["send", "1", "i-e"], refused);
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("not sent") && msg.contains("working"), "{msg}");
        assert_eq!(w.status(1), ItemStatus::Planned);
    }

    #[test]
    fn a_refused_send_new_still_keeps_the_new_item_planned() {
        // The add and the reservation land together; the refusal then only
        // takes the reservation back.
        let w = World::new();
        let refused = BoardDelivery::Refused(tower_wt::Error::NoAgentIn("/w".into()));
        let (_, r) = w.run(&["send", "--new", "x", "i-e"], refused);
        assert!(r.is_err());
        assert_eq!(w.status(1), ItemStatus::Planned);
    }

    #[test]
    fn an_unknown_delivery_says_how_to_resolve_it() {
        let w = World::new();
        w.must(&["add", "parser"], unused());
        let (_, r) = w.run(&["send", "1", "i-e"], unused());
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("--again") && msg.contains("board done 1"),
            "{msg}"
        );
        assert_eq!(w.status(1), ItemStatus::SentUnknown);
    }

    #[test]
    fn a_board_refusal_never_calls_the_delivery() {
        let w = World::new();
        w.must(&["add", "a", "b"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        let mut called = false;
        let cmd = parse(&args(&["send", "2", "i-e"])).unwrap();
        let r = run(&mut Vec::new(), &w.ctx(), &cmd, |_, _| {
            called = true;
            accepted()
        });
        assert!(r.unwrap_err().to_string().contains("already holds item 1"));
        assert!(!called);
    }

    #[test]
    fn a_task_from_another_repository_is_refused_before_anything() {
        let w = World::new();
        w.must(&["add", "a"], unused());
        let other = World::new();
        let mut ctx = other.ctx();
        ctx.state_dir = w.state.path(); // w's task i-e, other's repository
        let cmd = parse(&args(&["send", "1", "i-e"])).unwrap();
        let r = run(&mut Vec::new(), &ctx, &cmd, |_, _| {
            panic!("must not deliver")
        });
        assert!(r.unwrap_err().to_string().contains("belongs to"));
    }

    #[test]
    fn show_puts_progress_beside_each_item() {
        let w = World::new();
        w.must(&["add", "parser", "tests", "bench"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        w.must(&["drop", "3", "--reason", "later"], unused());
        w.must(&["note", "1", "check the grammar"], unused());
        let mut a = Agent::new("codex-w1:p1", "codex", now());
        a.cwd = w.meta.path.clone();
        a.session_id = "S1".into();
        a.work_seq = 6;
        a.state = AgentState::Working;
        a.state_since = now() - time::Duration::minutes(12);
        w.store.save(&a).unwrap();

        let (out, r) = w.run(&["show"], unused());
        assert!(r.is_ok());
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with("Board · ") && lines[0].ends_with("0/3"),
            "{out}"
        );
        assert!(
            lines[1].contains("parser")
                && lines[1].contains("i-e")
                && lines[1].contains("● working  12m"),
            "{out}"
        );
        assert!(lines[2].contains("check the grammar"), "{out}");
        assert!(
            lines[3].contains("tests") && lines[3].contains("○ planned"),
            "{out}"
        );
        assert!(
            lines[4].starts_with('✗') && lines[4].contains("dropped"),
            "{out}"
        );
        assert!(lines[5].contains("dropped: later"), "{out}");
    }

    #[test]
    fn column_text_leads_with_a_one_cell_icon() {
        use tower_state::{Column, Progress};
        let c = Column {
            progress: Progress::Waiting,
            since: Some(now() - time::Duration::minutes(3)),
            uncertain: true,
        };
        assert_eq!(column_text(&c, now(), false), "! wait  3m  (uncertain)");
        let struck = Column {
            progress: Progress::Done,
            since: Some(now()),
            uncertain: false,
        };
        assert_eq!(column_text(&struck, now(), true), "– done  (stale)");
        for icon in ['!', '✓', '?', '●', '◌', '○', '–'] {
            assert_eq!(tower_text::width(&icon.to_string()), 1, "{icon}");
        }
    }

    #[test]
    fn show_json_carries_the_column_and_its_evidence() {
        let w = World::new();
        w.must(&["add", "parser"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        let (out, _) = w.run(&["show", "--json"], unused());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let item = &v["items"][0];
        assert_eq!(item["item"]["status"], "sent");
        assert_eq!(item["item"]["receipt"]["session_id"], "S1");
        assert_eq!(item["column"]["progress"], "Unseen");
    }

    #[test]
    fn show_marks_worker_progress_stale_when_herdr_did_not_answer() {
        let w = World::new();
        w.must(&["add", "parser", "tests"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        let out = show_text(&board, &[], now(), None);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("did not answer"), "{out}");
        assert!(lines[2].ends_with("(stale)"), "{out}");
        assert!(
            !lines[3].contains("stale"),
            "a planned item is not read from records: {out}"
        );
        let v: serde_json::Value =
            serde_json::from_str(&show_json(&board, &[], now(), None)).unwrap();
        assert_eq!(v["observed"], false);
        assert_eq!(v["items"][0]["column"]["stale"], true);
        assert_eq!(v["items"][1]["column"]["stale"], false);
    }

    #[test]
    fn an_item_sent_on_another_backend_is_stale_even_when_this_one_answered() {
        let w = World::new();
        w.must(&["add", "parser"], unused());
        w.must(&["send", "1", "i-e"], accepted()); // receipt backend: default
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        let out = show_text(&board, &[], now(), Some("b0123456789abcdef"));
        assert!(!out.contains("did not answer"), "{out}");
        assert!(out.lines().nth(1).unwrap().ends_with("(stale)"), "{out}");
        assert!(!show_text(&board, &[], now(), Some("")).contains("stale"));
    }

    #[test]
    fn show_json_carries_the_records_it_judged_by() {
        let w = World::new();
        w.must(&["add", "parser"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        let mut a = Agent::new("codex-w1:p1", "codex", now());
        a.cwd = w.meta.path.clone();
        a.session_id = "S1".into();
        a.work_seq = 6;
        w.store.save(&a).unwrap();
        let (out, _) = w.run(&["show", "--json"], unused());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let r = &v["items"][0]["records"][0];
        assert_eq!(r["session_id"], "S1");
        assert_eq!(r["work_seq"], 6);
        assert!(r["born_at"].is_string());
    }

    #[test]
    fn a_stale_outcome_fails_the_send() {
        // `done` lands while the prompt is in flight: the delivery's outcome is
        // discarded, and the advisor is told rather than shown a success.
        let w = World::new();
        w.must(&["add", "parser"], unused());
        let cmd = parse(&args(&["send", "1", "i-e"])).unwrap();
        let ctx = w.ctx();
        let r = run(&mut Vec::new(), &ctx, &cmd, |_, _| {
            w.boards
                .update(&w.meta.repo, now(), |b| b.done(&[1], now()))
                .unwrap()
                .unwrap();
            accepted()
        });
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("changed meanwhile"), "{msg}");
        assert_eq!(w.status(1), ItemStatus::Done);
    }

    #[test]
    fn again_says_the_worker_may_have_it_twice() {
        let w = World::new();
        w.must(&["add", "parser"], unused());
        let _ = w.run(&["send", "1", "i-e"], unused());
        let (out, r) = w.run(&["send", "1", "i-e", "--again"], accepted());
        assert!(r.is_ok(), "{r:?}");
        assert!(out.contains("received it twice"), "{out}");
    }

    #[test]
    fn a_refused_send_new_says_the_item_stays() {
        let w = World::new();
        let refused = BoardDelivery::Refused(tower_wt::Error::NoAgentIn("/w".into()));
        let (_, r) = w.run(&["send", "--new", "x", "i-e"], refused);
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("stays on the board, planned"));
    }

    #[test]
    fn a_worktree_without_a_task_record_is_refused_not_given_its_own_board() {
        let (_repo, state, meta) = task_in_repo("i-e");
        std::fs::remove_dir_all(state.path().join("worktrees")).unwrap();
        let err = board_repo(state.path(), &meta.path).unwrap_err();
        assert!(err.contains("no readable task record"), "{err}");
    }

    #[test]
    fn the_loop_line_groups_what_needs_the_advisor_first() {
        let w = World::new();
        w.must(&["add", "a", "b", "c", "d"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        w.must(&["done", "2"], unused());
        let mut a = Agent::new("codex-w1:p1", "codex", now());
        a.cwd = w.meta.path.clone();
        a.session_id = "S1".into();
        a.work_seq = 6;
        a.state = AgentState::DoneUnread;
        w.store.save(&a).unwrap();
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        let records = w.store.list().unwrap();
        assert_eq!(
            loop_summary(&board, &records, &|_| true).unwrap(),
            "Dispatch board (herdr-tower board show): review 1 · planned 3, 4. Strike an item through with `herdr-tower board done N` once you have reviewed it — that is separate from `loop done`."
        );
        assert_eq!(
            loop_line(w.state.path(), &w.meta.repo, &records, now(), &|_| true).unwrap(),
            loop_summary(&board, &records, &|_| true).unwrap(),
            "the advisor's cwd finds its board"
        );
    }

    #[test]
    fn an_uncertain_column_is_marked_in_the_loop_line() {
        let w = World::new();
        w.must(&["add", "a"], unused());
        let mut d = accepted();
        if let BoardDelivery::Accepted { receipt, .. } = &mut d {
            receipt.boundary_lost = true;
        }
        w.must(&["send", "1", "i-e"], d);
        let mut a = Agent::new("codex-w1:p1", "codex", now());
        a.cwd = w.meta.path.clone();
        a.session_id = "S1".into();
        a.work_seq = 6;
        a.state = AgentState::DoneUnread;
        w.store.save(&a).unwrap();
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        let line = loop_summary(&board, &w.store.list().unwrap(), &|_| true).unwrap();
        assert!(line.contains("review 1?"), "{line}");
    }

    #[test]
    fn an_item_on_a_backend_not_read_this_pass_is_marked() {
        let w = World::new();
        w.must(&["add", "a"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        let line = loop_summary(&board, &[], &|b| !b.is_empty()).unwrap();
        assert!(line.contains("check 1?"), "{line}");
    }

    #[test]
    fn no_board_or_nothing_open_adds_no_loop_line() {
        let w = World::new();
        assert_eq!(
            loop_line(w.state.path(), &w.meta.repo, &[], now(), &|_| true),
            None
        );
        w.must(&["add", "a"], unused());
        w.must(&["done", "1"], unused());
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        assert_eq!(loop_summary(&board, &[], &|_| true), None);
        assert_eq!(loop_line(w.state.path(), "", &[], now(), &|_| true), None);
        // A worker's loop, in its worktree, gets no board line.
        w.must(&["add", "b"], unused());
        assert!(loop_line(w.state.path(), &w.meta.repo, &[], now(), &|_| true).is_some());
        assert_eq!(
            loop_line(w.state.path(), &w.meta.path, &[], now(), &|_| true),
            None
        );
    }

    #[test]
    fn pane_ids_are_told_by_their_shape() {
        for yes in ["w2:pBR", "w42:p1", "wF:p18"] {
            assert!(is_pane_id(yes), "{yes}");
        }
        for no in [
            "i-3205",
            "w2:",
            ":p1",
            "w2p1",
            "buildbox w5:p2",
            "w2:p-1",
            "x2:p1",
        ] {
            assert!(!is_pane_id(no), "{no}");
        }
    }

    #[test]
    fn assign_and_bare_send_parse() {
        assert_eq!(
            parse(&args(&["assign", "3", "w2:pBR"])),
            Ok(Cmd::Assign {
                n: 3,
                worker: "w2:pBR".into(),
                sent: false
            })
        );
        assert_eq!(
            parse(&args(&["assign", "3", "w2:pBR", "--sent"])),
            Ok(Cmd::Assign {
                n: 3,
                worker: "w2:pBR".into(),
                sent: true
            })
        );
        assert_eq!(
            parse(&args(&["send", "3"])),
            Ok(Cmd::Send {
                item: SendItem::Existing(3),
                worker: String::new(),
                flags: SendFlags::default(),
                body: None,
            })
        );
    }

    /// A long-lived worker in pane `pane` (default backend), `work_seq` turns in.
    fn pane_agent(w: &World, pane: &str, state: AgentState, work_seq: u64) -> Agent {
        let mut a = Agent::new(format!("claude-{pane}"), "claude", now());
        a.pane.pane_id = pane.into();
        a.cwd = "/somewhere/else".into();
        a.session_id = format!("S-{pane}");
        a.work_seq = work_seq;
        a.state = state;
        w.store.save(&a).unwrap();
        a
    }

    fn pane_receipt(a: &Agent) -> BoardDelivery {
        BoardDelivery::Accepted {
            receipt: Receipt {
                backend: String::new(),
                session_id: a.session_id.clone(),
                work_seq: a.work_seq,
                agent_id: a.id.clone(),
                adopted: false,
                record_born: a.born_at,
                at: now(),
                boundary_lost: false,
            },
            verdict_kept: false,
        }
    }

    #[test]
    fn items_queue_on_a_pane_and_show_it_first() {
        let w = World::new();
        pane_agent(&w, "w2:pBR", AgentState::Idle, 3);
        w.must(&["add", "#3148", "#3239", "bench"], unused());
        let (out, r) = w.run(&["assign", "1", "w2:pBR"], unused());
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(out, "assigned item 1 to pane w2:pBR.\n");
        w.must(&["assign", "2", "w2:pBR"], unused()); // a queue is fine
        let (out, _) = w.run(&["assign", "3", "buildbox w5:p2"], unused());
        assert!(out.contains("buildbox w5:p2 (shown only"), "{out}");
        let (out, _) = w.run(&["show"], unused());
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[1].contains("[w2:pBR] #3148") && lines[1].contains("planned"),
            "{out}"
        );
        assert!(lines[2].contains("[w2:pBR] #3239"), "{out}");
        assert!(lines[3].contains("[buildbox w5:p2] bench"), "{out}");
    }

    #[test]
    fn a_pane_typo_is_refused_not_taken_for_a_label() {
        let w = World::new();
        w.must(&["add", "a"], unused());
        let (_, r) = w.run(&["assign", "1", "w2:pZZ"], unused());
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("no agent is running in pane w2:pZZ") && msg.contains("label:w2:pZZ"),
            "{msg}"
        );
        // Said outright, it is a label.
        let (out, r) = w.run(&["assign", "1", "label:w2:pZZ"], unused());
        assert!(r.is_ok() && out.contains("w2:pZZ (shown only"), "{out}");
    }

    #[test]
    fn a_bare_send_goes_to_the_assigned_pane() {
        let w = World::new();
        let a = pane_agent(&w, "w2:pBR", AgentState::Idle, 3);
        w.must(&["add", "#3148"], unused());
        w.must(&["assign", "1", "w2:pBR"], unused());
        let mut told = None;
        let mut buf = Vec::new();
        let cmd = parse(&args(&["send", "1"])).unwrap();
        run(&mut buf, &w.ctx(), &cmd, |to, text| {
            told = Some((to.clone(), text.to_string()));
            pane_receipt(&a)
        })
        .unwrap();
        let (to, text) = told.unwrap();
        let SendTo::Pane(pin) = to else {
            panic!("{to:?}")
        };
        assert_eq!(pin.pane_id, "w2:pBR");
        assert_eq!(pin.agent_id, a.id);
        assert_eq!(text, "[board item 1] #3148");
        assert_eq!(String::from_utf8(buf).unwrap(), "sent item 1 to w2:pBR.\n");
        assert_eq!(w.status(1), ItemStatus::Sent);
    }

    #[test]
    fn a_label_or_nobody_cannot_be_sent_to() {
        let w = World::new();
        w.must(&["add", "a", "b"], unused());
        w.must(&["assign", "1", "label:buildbox"], unused());
        let (_, r) = w.run(&["send", "1"], unused());
        assert!(r.unwrap_err().to_string().contains("cannot send to"));
        let (_, r) = w.run(&["send", "2"], unused());
        assert!(r.unwrap_err().to_string().contains("not assigned"));
    }

    #[test]
    fn one_agent_named_two_ways_still_holds_one_open_item() {
        // The task's worker and the pane are the same agent: a second item
        // sent by pane id is refused while the first, sent by task, is open.
        let w = World::new();
        let mut a = pane_agent(&w, "w2:pBR", AgentState::Idle, 3);
        a.cwd = w.meta.path.clone();
        w.store.save(&a).unwrap();
        w.must(&["add", "a", "b"], unused());
        w.must(&["send", "1", "i-e"], pane_receipt(&a));
        let (_, r) = w.run(&["send", "2", "w2:pBR"], unused());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("already holds item 1"), "{msg}");
    }

    #[test]
    fn assign_sent_takes_over_a_running_turn_as_this_items() {
        let w = World::new();
        let a = pane_agent(&w, "w2:pBR", AgentState::Working, 7);
        w.must(&["add", "#3239"], unused());
        let (out, r) = w.run(&["assign", "1", "w2:pBR", "--sent"], unused());
        assert!(r.is_ok(), "{r:?}");
        assert!(out.contains("tracked from here"), "{out}");
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        let item = board.item(1).unwrap();
        assert_eq!(item.status, ItemStatus::Sent);
        let rc = item.receipt.as_ref().unwrap();
        assert!(rc.adopted);
        assert_eq!(rc.work_seq, 6, "the running turn counts as this item's");
        assert_eq!(rc.agent_id, a.id);
        let (out, _) = w.run(&["show"], unused());
        assert!(
            out.contains("[w2:pBR] #3239")
                && out.contains("working")
                && out.contains("(uncertain)"),
            "{out}"
        );
        // A second one for the same agent is refused.
        w.must(&["add", "more"], unused());
        let (_, r) = w.run(&["assign", "2", "w2:pBR", "--sent"], unused());
        assert!(r.unwrap_err().to_string().contains("already holds item 1"));
    }

    #[test]
    fn an_item_out_cannot_be_reassigned() {
        let w = World::new();
        let a = pane_agent(&w, "w2:pBR", AgentState::Idle, 3);
        pane_agent(&w, "w2:pBS", AgentState::Idle, 3);
        w.must(&["add", "a"], unused());
        w.must(&["send", "1", "w2:pBR"], pane_receipt(&a));
        let (_, r) = w.run(&["assign", "1", "w2:pBS"], unused());
        assert!(r.unwrap_err().to_string().contains("only a planned item"));
    }

    #[test]
    fn a_task_row_shows_the_pane_its_worker_is_in() {
        let w = World::new();
        let mut a = pane_agent(&w, "w2:pBQ", AgentState::Working, 4);
        a.cwd = w.meta.path.clone();
        w.store.save(&a).unwrap();
        w.must(&["add", "a"], unused());
        w.must(&["send", "1", "i-e"], pane_receipt(&a));
        let (out, _) = w.run(&["show"], unused());
        let row = out.lines().nth(1).unwrap();
        assert!(row.contains("[w2:pBQ] a") && row.contains("i-e"), "{out}");
    }

    #[test]
    fn removing_items_prunes_and_clears() {
        let w = World::new();
        w.must(&["add", "a", "b", "c", "d"], unused());
        w.must(&["done", "1"], unused());
        w.must(&["drop", "2"], unused());
        let (out, r) = w.run(&["rm", "4"], unused());
        assert!(r.is_ok() && out == "removed item 4.\n", "{out}");
        let (out, _) = w.run(&["prune"], unused());
        assert_eq!(out, "removed closed items 1, 2.\n");
        let (out, _) = w.run(&["prune"], unused());
        assert_eq!(out, "nothing closed to remove.\n");
        let (out, _) = w.run(&["add", "e"], unused());
        assert_eq!(out, "added item 5.\n", "numbers are not reused");
        let (out, _) = w.run(&["clear", "--all"], unused());
        assert_eq!(out, "cleared the board (2 items).\n");
        let (out, _) = w.run(&["show"], unused());
        assert!(out.contains("0/0"), "{out}");
    }

    #[test]
    fn an_item_out_with_a_worker_needs_force() {
        let w = World::new();
        w.must(&["add", "a"], unused());
        w.must(&["send", "1", "i-e"], accepted());
        let (_, r) = w.run(&["rm", "1"], unused());
        assert!(r.unwrap_err().to_string().contains("--force"));
        let (out, r) = w.run(&["rm", "1", "--force"], unused());
        assert!(r.is_ok(), "{out}");
    }

    #[test]
    fn remove_commands_parse_strictly() {
        assert_eq!(
            parse(&args(&["rm", "2", "3", "--force"])),
            Ok(Cmd::Remove {
                ns: vec![2, 3],
                force: true
            })
        );
        assert_eq!(parse(&args(&["prune"])), Ok(Cmd::Prune));
        assert_eq!(parse(&args(&["clear", "--all"])), Ok(Cmd::Clear));
        for bad in [
            vec!["rm"],
            vec!["rm", "x"],
            vec!["rm", "1", "--bogus"],
            vec!["prune", "1"],
            vec!["clear"],
            vec!["clear", "1"],
        ] {
            assert!(parse(&args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn next_sends_the_first_queued_item_and_waits_for_done() {
        let w = World::new();
        let a = pane_agent(&w, "w2:pBR", AgentState::Idle, 3);
        pane_agent(&w, "w2:pBS", AgentState::Idle, 3);
        w.must(&["add", "one", "two", "three"], unused());
        w.must(&["assign", "2", "w2:pBR"], unused());
        w.must(&["assign", "3", "w2:pBR"], unused());
        w.must(&["assign", "1", "w2:pBS"], unused());
        let mut sent = Vec::new();
        let mut buf = Vec::new();
        let cmd = parse(&args(&["next", "w2:pBR"])).unwrap();
        run(&mut buf, &w.ctx(), &cmd, |_, text| {
            sent.push(text.to_string());
            pane_receipt(&a)
        })
        .unwrap();
        assert_eq!(sent, vec!["[board item 2] two"]);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("next for w2:pBR: item 2.\n"), "{out}");
        let (_, r) = w.run(&["next", "w2:pBR"], unused());
        assert!(r.unwrap_err().to_string().contains("still holds item 2"));
        w.must(&["done", "2"], unused());
        w.must(&["next", "w2:pBR"], pane_receipt(&a));
        assert_eq!(w.status(3), ItemStatus::Sent);
        let (_, r) = w.run(&["next", "w2:pBR"], unused());
        assert!(r.unwrap_err().to_string().contains("still holds item 3"));
        w.must(&["done", "3"], unused());
        let (_, r) = w.run(&["next", "w2:pBR"], unused());
        assert!(r.unwrap_err().to_string().contains("nothing is queued"));
        assert_eq!(
            w.status(1),
            ItemStatus::Planned,
            "another worker's queue untouched"
        );
    }

    #[test]
    fn next_parses_one_worker() {
        assert_eq!(
            parse(&args(&["next", "w2:pBR"])),
            Ok(Cmd::Next {
                worker: "w2:pBR".into(),
                body: None,
            })
        );
        assert!(parse(&args(&["next"])).is_err());
        assert!(parse(&args(&["next", "a", "b"])).is_err());
    }

    #[test]
    fn the_worker_gets_the_title_then_the_instruction() {
        assert_eq!(
            instruction(3, "fix the parser", Some("  Use the grammar in docs/ \n")),
            "[board item 3] fix the parser\n\nUse the grammar in docs/"
        );
        assert_eq!(instruction(3, "t", Some("   ")), "[board item 3] t");
    }

    #[test]
    fn send_and_next_take_a_message_or_a_file() {
        let w = World::new();
        let a = pane_agent(&w, "w2:pBR", AgentState::Idle, 3);
        w.must(&["add", "parser", "tests"], unused());
        let told = std::cell::RefCell::new(Vec::new());
        let mut buf = Vec::new();
        let cmd = parse(&args(&[
            "send",
            "1",
            "w2:pBR",
            "--message",
            "Read REPORT.md first.",
        ]))
        .unwrap();
        run(&mut buf, &w.ctx(), &cmd, |_, text| {
            told.borrow_mut().push(text.to_string());
            pane_receipt(&a)
        })
        .unwrap();
        w.must(&["done", "1"], unused());
        w.must(&["assign", "2", "w2:pBR"], unused());
        let file = w.state.path().join("brief.md");
        std::fs::write(&file, "Long brief\nline two\n").unwrap();
        let cmd = parse(&args(&["next", "w2:pBR", "--file", file.to_str().unwrap()])).unwrap();
        run(&mut buf, &w.ctx(), &cmd, |_, text| {
            told.borrow_mut().push(text.to_string());
            pane_receipt(&a)
        })
        .unwrap();
        assert_eq!(
            *told.borrow(),
            vec![
                "[board item 1] parser\n\nRead REPORT.md first.".to_string(),
                "[board item 2] tests\n\nLong brief\nline two".to_string(),
            ]
        );
        let (_, r) = w.run(
            &["send", "--new", "x", "w2:pBR", "--file", "/no/such/file"],
            unused(),
        );
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("cannot read /no/such/file"));
    }

    #[test]
    fn message_flags_are_refused_twice_or_bare() {
        for bad in [
            vec!["send", "1", "--message"],
            vec!["send", "1", "--message", "a", "--file", "b"],
            vec!["next", "w2:p1", "--file"],
        ] {
            assert!(parse(&args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn priority_order_and_links_parse_and_apply() {
        assert_eq!(
            parse(&args(&["prio", "3", "1"])),
            Ok(Cmd::Prio { n: 3, priority: 1 })
        );
        assert_eq!(
            parse(&args(&["prio", "3", "-"])),
            Ok(Cmd::Prio { n: 3, priority: 0 })
        );
        assert_eq!(
            parse(&args(&["move", "3", "--to", "1"])),
            Ok(Cmd::Move { n: 3, to: 1 })
        );
        assert_eq!(
            parse(&args(&["link", "3", "#3231"])),
            Ok(Cmd::Link { n: 3, pr: 3231 })
        );
        assert_eq!(
            parse(&args(&["link", "3", "-"])),
            Ok(Cmd::Link { n: 3, pr: 0 })
        );
        for bad in [
            vec!["prio", "3", "4"],
            vec!["prio", "3"],
            vec!["move", "3", "1"],
            vec!["link", "3", "#x"],
        ] {
            assert!(parse(&args(&bad)).is_err(), "{bad:?}");
        }
        let w = World::new();
        w.must(&["add", "a", "b", "c"], unused());
        let (out, _) = w.run(&["prio", "2", "1"], unused());
        assert_eq!(out, "item 2 is P1.\n");
        let (out, _) = w.run(&["move", "3", "--to", "1"], unused());
        assert_eq!(out, "item 3 is at position 1.\n");
        let (out, _) = w.run(&["link", "1", "#3231"], unused());
        assert_eq!(out, "item 1 is linked to PR #3231.\n");
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        let states: std::collections::HashMap<u32, String> = [(3231, "MERGED".into())].into();
        let out = show_text_with(&board, &[], now(), Some(""), &states);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("  3  c"), "moved to the top: {out}");
        assert!(lines[2].contains("PR #3231 merged"), "{out}");
        assert!(lines[3].contains("P1 b"), "{out}");
    }

    #[test]
    fn next_takes_the_highest_priority_first() {
        let w = World::new();
        let a = pane_agent(&w, "w2:pBR", AgentState::Idle, 3);
        w.must(&["add", "low", "high", "none"], unused());
        for n in ["1", "2", "3"] {
            w.must(&["assign", n, "w2:pBR"], unused());
        }
        w.must(&["prio", "1", "3"], unused());
        w.must(&["prio", "2", "1"], unused());
        let mut sent = Vec::new();
        let cmd = parse(&args(&["next", "w2:pBR"])).unwrap();
        run(&mut Vec::new(), &w.ctx(), &cmd, |_, text| {
            sent.push(text.to_string());
            pane_receipt(&a)
        })
        .unwrap();
        assert_eq!(sent, vec!["[board item 2] high"]);
        w.must(&["done", "2"], unused());
        w.must(&["next", "w2:pBR"], pane_receipt(&a));
        assert_eq!(w.status(1), ItemStatus::Sent, "P3 before no priority");
    }

    #[test]
    fn a_send_right_after_wt_new_waits_for_the_workers_record() {
        let w = World::new();
        w.must(&["add", "a"], unused());
        let mut ctx = w.ctx();
        ctx.settle = std::time::Duration::from_secs(5);
        let store_dir = w.state.path().join("agents");
        let worktree = w.meta.path.clone();
        // The supervisor records the new agent a moment later.
        let late = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(900));
            let store = Store::new(store_dir).unwrap();
            let mut a = Agent::new("codex-w9:p1", "codex", now());
            a.pane.pane_id = "w9:p1".into();
            a.cwd = worktree;
            store.save(&a).unwrap();
        });
        let cmd = parse(&args(&["send", "1", "i-e"])).unwrap();
        run(&mut Vec::new(), &ctx, &cmd, |_, _| accepted()).unwrap();
        late.join().unwrap();
        assert_eq!(
            w.status(1),
            ItemStatus::Sent,
            "not refused for want of a record"
        );
    }

    #[test]
    fn wake_on_pins_the_calling_pane_and_off_forgets_it() {
        let w = World::new();
        pane_agent(&w, "w2:p9B", AgentState::Idle, 3);
        let (_, r) = w.run(&["wake", "on"], unused());
        assert!(
            r.unwrap_err().to_string().contains("advisor's pane"),
            "no caller"
        );
        let mut ctx = w.ctx();
        ctx.caller = Some("w2:p9B");
        let mut out = Vec::new();
        run(
            &mut out,
            &ctx,
            &parse(&args(&["wake", "on"])).unwrap(),
            |_, _| unused(),
        )
        .unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("pane w2:p9B will be woken"));
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        assert_eq!(board.waker.as_ref().unwrap().agent_id, "claude-w2:p9B");
        let (out, _) = w.run(&["show"], unused());
        assert!(out.contains("wakes [w2:p9B]"), "{out}");
        w.must(&["wake", "off"], unused());
        assert!(w.boards.load(&w.meta.repo, now()).unwrap().waker.is_none());
        assert!(parse(&args(&["wake"])).is_err());
        assert!(parse(&args(&["wake", "maybe"])).is_err());
        assert!(parse(&args(&["wake", "on", "advisor"])).is_err());
        assert!(parse(&args(&["wake", "off", "w2:p9B"])).is_err());
        // A named pane stands in for the caller: from outside herdr (MCP).
        w.must(&["wake", "on", "w2:p9B"], unused());
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        assert_eq!(board.waker.as_ref().unwrap().agent_id, "claude-w2:p9B");
    }

    #[test]
    fn wake_on_a_named_pane_needs_a_live_agent_there_and_beats_the_caller() {
        let w = World::new();
        pane_agent(&w, "w2:p9B", AgentState::Idle, 3);
        pane_agent(&w, "w2:pC1", AgentState::Idle, 4);
        // No agent in the named pane: refused, and nothing is pinned.
        let (_, r) = w.run(&["wake", "on", "w7:p1"], unused());
        assert!(r.is_err(), "no agent in w7:p1");
        assert!(w.boards.load(&w.meta.repo, now()).unwrap().waker.is_none());
        // Named from another pane: the named one is pinned, not the caller.
        let mut ctx = w.ctx();
        ctx.caller = Some("w2:pC1");
        run(
            &mut Vec::new(),
            &ctx,
            &parse(&args(&["wake", "on", "w2:p9B"])).unwrap(),
            |_, _| unused(),
        )
        .unwrap();
        let board = w.boards.load(&w.meta.repo, now()).unwrap();
        assert_eq!(board.waker.as_ref().unwrap().agent_id, "claude-w2:p9B");
    }

    #[test]
    fn take_repo_leaves_every_rest_untouched() {
        // Property: whatever follows `--repo <path>` comes back as it was,
        // and a line without a leading --repo is not touched at all.
        let words = [
            "show", "--json", "send", "3", "w2:pBR", "--repo", "", "한 글",
        ];
        for len in 0..5 {
            for seed in 0..40usize {
                let rest: Vec<String> = (0..len)
                    .map(|i| words[(seed * 7 + i * 3) % words.len()].to_string())
                    .collect();
                let mut line = args(&["--repo", "/w/r"]);
                line.extend(rest.iter().cloned());
                let (repo, got) = take_repo(&line).unwrap();
                assert_eq!(repo.as_deref(), Some("/w/r"));
                assert_eq!(got, &rest[..]);
                if rest.first().is_some_and(|w| w != "--repo") {
                    let (repo, got) = take_repo(&rest).unwrap();
                    assert_eq!((repo, got), (None, &rest[..]));
                }
            }
        }
        assert!(take_repo(&args(&["--repo", ""])).is_err(), "an empty path");
    }

    #[test]
    fn a_row_being_sent_to_another_task_names_that_tasks_pane() {
        let mut b = tower_state::Board::new("/w/r");
        b.add(&["x".into()], now()).unwrap();
        b.items[0].task = "a".into();
        b.items[0].worktree = "/w/wt/a".into();
        b.reserve_send(1, "b", "/w/wt/b", SendFlags::default(), 1, now())
            .unwrap();
        let item = &b.items[0];
        assert_eq!(item.status, ItemStatus::Sending);
        let mut in_a = Agent::new("claude-w2:pA", "claude", now());
        in_a.cwd = "/w/wt/a".into();
        in_a.pane.pane_id = "w2:pA".into();
        let mut in_b = Agent::new("claude-w2:pB", "claude", now());
        in_b.cwd = "/w/wt/b".into();
        in_b.pane.pane_id = "w2:pB".into();
        let records = [in_a, in_b];
        let found = records_for(item, &records);
        assert_eq!(who(item, &found).as_deref(), Some("[w2:pB]"));
        assert_eq!(task_cell(item), "b");
    }

    #[test]
    fn a_leading_repo_is_taken_off_the_command_line() {
        let line = args(&["--repo", "/w/r", "show", "--json"]);
        let (repo, rest) = take_repo(&line).unwrap();
        assert_eq!(repo.as_deref(), Some("/w/r"));
        assert_eq!(rest, &args(&["show", "--json"])[..]);
        let plain = args(&["show"]);
        let (repo, rest) = take_repo(&plain).unwrap();
        assert_eq!((repo, rest), (None, &plain[..]));
        assert!(take_repo(&args(&["--repo"])).is_err());
        assert!(take_repo(&args(&["--repo", "--json"])).is_err());
    }

    #[test]
    fn an_empty_board_says_how_to_start() {
        let w = World::new();
        let (out, _) = w.run(&["show"], unused());
        assert!(out.contains("board add"), "{out}");
    }
}
