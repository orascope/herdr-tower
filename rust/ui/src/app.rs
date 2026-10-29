// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The tower's state, and what happens when a key is pressed.
//!
//! See the crate doc for why this is a mutable struct with methods rather than
//! a `Model` folded by `Update`. The short version: three key presses have to
//! sequence a side effect with a state change (spawn a tab, *then* delete the
//! record only if the spawn worked), and that ordering cannot survive being
//! split across a message boundary.
//!
//! Nothing here spawns a thread or draws anything. Events arrive from
//! [`crate::feed`], drawing happens in [`crate::view`], and the handful of
//! effects that need the terminal to step aside leave as an [`Action`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use time::OffsetDateTime;
use tower_state::{
    local_offset, new_schedule_id, parse_when, Agent, AgentState, DaemonSnapshot, FieldHints,
    HintStore, LoopLease, LoopStore, Repeat, Schedule, ScheduleStore, ScheduleTarget, Store,
    DEFAULT_LOOP_MESSAGE, DEFAULT_LOOP_TTL,
};
use tower_usage::{CtxInfo, DefaultModels};

use crate::broadcast::{has_session_handoff, shown, targets, CLOSE_MESSAGE, WAKE_MESSAGE};
use crate::info::{render_info, InfoData};
use crate::resume::ResumePaths;
use crate::tower::Tower;
use crate::TextInput;

/// The last value if there is one, else the example.
fn or_example<'a>(last: &'a str, example: &'a str) -> &'a str {
    if last.is_empty() {
        example
    } else {
        last
    }
}

/// How close two clicks on one row must be to count as a double click.
///
/// macOS's default, and slow enough for a trackpad tap.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// A broadcast longer than this is a paste accident, not an instruction.
const BROADCAST_LIMIT: usize = 500;
/// A filter is a few words, not a paste.
const FILTER_LIMIT: usize = 80;

/// A loop prompt is deliberately larger than a broadcast: the safe default is
/// already several paragraphs and the user may add task-specific constraints.
const LOOP_MESSAGE_LIMIT: usize = 4_096;

/// Tower presets. The CLI remains the escape hatch for arbitrary durations.
const LOOP_IDLE_PRESETS: [Duration; 4] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
];
const DEFAULT_LOOP_IDLE_INDEX: usize = 1;

/// A scheduled message is a prompt, as long as a loop's.
const SCHEDULE_MESSAGE_LIMIT: usize = LOOP_MESSAGE_LIMIT;

/// What the `T` field shows, dim, before any schedule has been made: the
/// shape of the line it wants — a time, then the message — as one that
/// [`parse_when`] takes as it stands, so `→` `enter` on it is a working
/// schedule rather than an error.
pub const SCHEDULE_EXAMPLE: &str = "19:00 check the build and report back";
/// What the `/` field shows, dim, before any filter has been used: a word the
/// list really matches on (the agent's kind), short enough to read as an
/// example rather than a value.
pub const FILTER_EXAMPLE: &str = "codex";

/// Said when `enter` finds a hinted field empty. The hint is not input — that
/// is what keeps a stray `enter` from arming a loop nobody chose — so the
/// field stays open and says how to take it.
const LOOP_EMPTY: &str = "type the loop message, or → to take the one shown";
const SCHEDULE_EMPTY: &str = "type a time and the message, or → to take the one shown";

#[derive(Debug, Clone)]
enum Composer {
    Broadcast(TextInput),
    /// `/`: the text rows must contain to stay on the list, applied as it is
    /// typed.
    Filter(TextInput),
    /// `T` on a saved machine's row: a time and a message, sent by that
    /// machine's own herdr-tower.
    RemoteSchedule {
        input: TextInput,
        label: String,
        id: String,
        repeat: usize,
    },
    /// `m` in remote mode: a message for an agent on a saved machine.
    RemoteSend {
        input: TextInput,
        label: String,
        id: String,
    },
    Loop {
        input: TextInput,
        agent_id: String,
        agent_session_id: String,
        idle_index: usize,
    },
    /// `T`: a time and a message for the selected session.
    Schedule {
        input: TextInput,
        target: ScheduleTarget,
        /// What the notice calls the session.
        who: String,
        repeat: usize,
    },
}

impl Composer {
    fn input(&self) -> &TextInput {
        match self {
            Composer::Broadcast(input)
            | Composer::Filter(input)
            | Composer::RemoteSend { input, .. }
            | Composer::RemoteSchedule { input, .. }
            | Composer::Loop { input, .. }
            | Composer::Schedule { input, .. } => input,
        }
    }

    fn input_mut(&mut self) -> &mut TextInput {
        match self {
            Composer::Broadcast(input)
            | Composer::Filter(input)
            | Composer::RemoteSend { input, .. }
            | Composer::RemoteSchedule { input, .. }
            | Composer::Loop { input, .. }
            | Composer::Schedule { input, .. } => input,
        }
    }

    fn idle_for(&self) -> Option<Duration> {
        match self {
            Composer::Loop { idle_index, .. } => Some(LOOP_IDLE_PRESETS[*idle_index]),
            Composer::Broadcast(_)
            | Composer::Filter(_)
            | Composer::RemoteSend { .. }
            | Composer::RemoteSchedule { .. }
            | Composer::Schedule { .. } => None,
        }
    }

    fn repeat(&self) -> Option<Repeat> {
        match self {
            Composer::Schedule { repeat, .. } | Composer::RemoteSchedule { repeat, .. } => {
                Some(Repeat::ALL[*repeat])
            }
            _ => None,
        }
    }

    /// `tab`: the next idle interval for a loop, the next repetition for a
    /// schedule.
    fn cycle(&mut self, backwards: bool) {
        let (index, len) = match self {
            Composer::Loop { idle_index, .. } => (idle_index, LOOP_IDLE_PRESETS.len()),
            Composer::Schedule { repeat, .. } | Composer::RemoteSchedule { repeat, .. } => {
                (repeat, Repeat::ALL.len())
            }
            Composer::Broadcast(_) | Composer::Filter(_) | Composer::RemoteSend { .. } => return,
        };
        *index = if backwards {
            (*index + len - 1) % len
        } else {
            (*index + 1) % len
        };
    }
}

/// Which screen is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    List,
    /// The detail card, already rendered — it reads several files, so it is
    /// built once when `i` is pressed rather than on every frame.
    Info(String),
}

/// A destructive action waiting for `y`.
///
/// One enum rather than a command name plus a separate broadcast text, two
/// fields that would have to be kept consistent by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Tell every session to resume its handoff.
    Wake,
    /// Tell every session to close out.
    Close,
    /// Send this text to every session.
    Broadcast(String),
    /// Press Escape in an agent on a saved machine: its machine label and id.
    RemoteInterrupt { label: String, id: String },
}

/// The only things [`App`] cannot finish by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "an action that is not carried out is a key press that did nothing"]
pub enum Action {
    Nothing,
    /// Re-read the store now instead of waiting for the next poll.
    Refresh,
    /// Ask the usage board's sources again now.
    RefreshUsage,
    /// Remember the preview's chosen height (`None`: automatic) for the next
    /// tower — a file write, so the driver does it.
    SavePreviewRows(Option<usize>),
    /// Leave the screen and end the program.
    Quit,
    /// Restore the terminal, run this argv to completion, come back, refresh.
    ///
    /// It covers attaching to a pane and `lazygit` — both of which want the
    /// whole terminal and neither of which can share it with a draw loop.
    ///
    /// `focus` is the pane to select once that child is running, and `seen` the
    /// record to mark read after it. Neither happens for a child that cannot
    /// start — a missing `herdr`, a command that exits at once. The marker
    /// because the person never reached the pane, and it is the only thing that
    /// would bring them back to it; the selection because it moves the view of
    /// *every* client attached to the session, and moving somebody else's screen
    /// for an attach that never happened is the one part of that this code can
    /// decline to do.
    Run {
        argv: Vec<String>,
        focus: Option<String>,
        seen: Option<String>,
    },
    /// Ask a saved machine to do something, off the redraw path.
    ///
    /// It is an ssh round trip, and one to a machine that has stopped
    /// answering takes as long as the timeout: run on the key press, it froze
    /// the whole tower — every row, the preview, the clock — for up to eight
    /// seconds. The answer comes back as [`Event::RemoteDone`].
    Remote(Box<crate::RemoteRequest>),
}

/// Fast local information, collected together because it all comes from reading
/// files and none of it is worth its own round trip.
#[derive(Debug, Clone, Default)]
pub struct LocalInfo {
    /// Agent ID to model and context share.
    pub ctx: HashMap<String, CtxInfo>,
    /// Session name to its Discord mark (`⌁`, or `⌁` plus a channel label).
    pub wired: HashMap<String, String>,
    /// `None` until the first sweep. An empty map would mean both "nothing
    /// pinned" and "not looked yet", and the header would then announce that
    /// all three CLIs were on automatic before it had read a single file.
    pub def_models: Option<DefaultModels>,
}

/// How busy the machine the tower runs on is, for the header.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostLoad {
    /// The processor's busy share over the last interval, 0–100. `None`
    /// before the second reading, and where the system has no counters.
    pub cpu_pct: Option<f64>,
    pub memory: Option<tower_os::host::Memory>,
}

/// The name `@local`'s band goes by, among the saved machines' labels.
pub const LOCAL_MACHINE: &str = "local";

/// Something that happened, from wherever it happened.
///
/// There is no variant for re-arming a timer. The producers are threads that
/// own their own clocks, so "time passed" is not news the [`App`] has to act on
/// — it arrives as [`Event::Records`] with a fresh `now`, which is all a tick
/// would carry once a watch is open.
#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    /// A click or a wheel turn. See [`App::on_mouse`].
    Mouse(MouseEvent),
    /// The store was re-read. `now` refreshes every elapsed label on screen.
    Records {
        agents: Vec<Agent>,
        /// Worktree folder to branch name.
        branches: HashMap<String, String>,
        /// The first pane in each live Herdr tab, taken from its layout.
        tab_roots: HashSet<String>,
        /// Worker pane to the coordinator it works under (`target_key`s), as
        /// the supervisor worked it out.
        parents: BTreeMap<String, String>,
        /// The resident supervisor heartbeat that produced this store view.
        daemon: Option<DaemonSnapshot>,
        now: OffsetDateTime,
    },
    Local(LocalInfo),
    /// The machine's processor and memory were read again.
    Host(HostLoad),
    /// The usage board read its figures again.
    Usage,
    /// What was read from the machines herdr has saved.
    Remote(Vec<crate::RemoteMachine>),
    /// Folder to the number it works on: its branch's open PR, else the
    /// issue its branch is named after.
    Prs(HashMap<String, u64>),
    Preview {
        pane_id: String,
        body: String,
    },
    /// A one-line result from work that ran off the redraw path.
    Notice(Result<String, String>),
    /// How an [`Action::Remote`] went on the machine labelled `label`.
    RemoteDone {
        label: String,
        id: String,
        action: crate::RemoteAction,
        result: Result<String, String>,
    },
    /// The state store's own health: `Err` while it cannot be read or written,
    /// `Ok` on the pass that recovers.
    ///
    /// Its own event, and its own field on the screen, because it is the only
    /// error nobody asked for. A failed attach or a missing `lazygit` answers a
    /// key press and belongs on the error line; this one appears on its own and
    /// has to leave on its own — and clearing the shared line would have taken
    /// whatever the person had just done with it.
    Health(Result<(), String>),
}

/// What the tower captured of a pane, and whose pane it was.
#[derive(Debug, Clone)]
pub struct Preview {
    pub pane_id: String,
    pub body: String,
}

/// Everything the tower needs that is not state: paths, ports, labels.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Short running-build label shown by the standalone popup.
    pub version: String,
    /// How often the pane preview is re-captured.
    pub preview_interval: Duration,
    /// Discord channel ID to human label.
    pub channel_labels: HashMap<String, String>,
    /// This binary, for starting the resident supervisor.
    pub exe: String,
    pub resume: ResumePaths,
    pub wiring: tower_wiring::Paths,
    pub tools: tower_usage::ToolSearch,
    /// The home directory the file readers work from. A parameter for the same
    /// reason `tower_usage` makes it one: a test must not read the developer's own
    /// `~/.claude`.
    pub home: Option<PathBuf>,
    /// Whether this is the dock's tower, which the supervisor starts again
    /// when it ends (`HERDR_TOWER_DOCK`, with `dock_keep` on). `q` does not
    /// quit it: quitting there only blinks the screen.
    pub kept: bool,
    /// The preview height the last tower was left at ([`App::preview_rows`]),
    /// read by the driver; `None` for the automatic split.
    pub preview_rows: Option<usize>,
}

impl Settings {
    pub fn from_env() -> Self {
        let cfg = tower_config::load();
        Settings {
            version: String::new(),
            preview_interval: cfg.preview_tick(),
            channel_labels: cfg.channel_labels.clone(),
            exe: std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "herdr-tower".into()),
            resume: ResumePaths::from_env(),
            wiring: tower_wiring::Paths::detect().unwrap_or_default(),
            tools: tower_usage::ToolSearch::from_env(),
            home: tower_usage::home_dir(),
            kept: std::env::var("HERDR_TOWER_DOCK").is_ok_and(|v| !v.is_empty())
                && cfg.dock_keep_enabled(),
            preview_rows: None,
        }
    }
}

/// The control tower.
pub struct App {
    store: Store,
    loops: LoopStore,
    schedules: ScheduleStore,
    /// The schedules as last read, for the badges and the header.
    schedule_list: Vec<Schedule>,
    /// `tui-hints.json`: the last `T` line and `/` filter, offered dim when
    /// those fields open. Read once here and written through on change — the
    /// tower is the only writer.
    hint_store: HintStore,
    hints: FieldHints,
    /// `parallax.json`: Parallax Browser's handoffs as the supervisor last
    /// read them. Read with the schedules on every records event, never from
    /// Parallax itself — see `tower_cli::parallax`.
    parallax_store: tower_state::ParallaxStore,
    parallax: Option<tower_state::ParallaxSnapshot>,
    /// The machine's offset from UTC. Not read every frame, for reading it
    /// can mean running `date` (see [`tower_state::local_offset`]) — but a
    /// docked tower runs for days, across a daylight-saving change, so not
    /// only once either: hourly, and whenever a schedule is made.
    utc_offset: time::UtcOffset,
    /// When `utc_offset` was read, by the records' clock.
    utc_offset_read: OffsetDateTime,
    tower: Arc<dyn Tower>,
    settings: Settings,

    agents: Vec<Agent>,
    /// Every live agent, before the filter — `agents` is what of it matches.
    all_agents: Vec<Agent>,
    /// The coordinator each worker works under, by the worker's agent id, as
    /// its row marks it (`pEJ`, or `w2:pEJ` from another workspace).
    coordinators: HashMap<String, String>,
    /// Last observed sidebar order; retained through a missing heartbeat.
    workspace_order: Vec<String>,
    workspace_display_labels: BTreeMap<String, String>,
    /// What `/` narrowed the list to; empty shows every agent.
    filter: String,
    cursor: usize,
    now: OffsetDateTime,
    /// The last known terminal size, so the preview can ask for exactly as many
    /// rows as will be drawn.
    size: (u16, u16),

    screen: Screen,
    pending: Option<Pending>,
    /// `Some` while a broadcast or idle-loop message is being edited.
    composer: Option<Composer>,
    /// A one-shot line of guidance. Cleared by the next key press.
    notice: String,
    /// A failure worth showing until something replaces it.
    err: Option<String>,

    /// Whether the resident supervisor currently reports a live Herdr link.
    watching: bool,
    daemon: Option<DaemonSnapshot>,
    inside: bool,
    /// Whether the tower lives in a pane of its own rather than a popup.
    ///
    /// herdr gives a pane `HERDR_PANE_ID` and a popup none, which is how the two
    /// are told apart. It decides whether a jump ends the tower: a popup covers
    /// the screen and has to get out of the way, while a pane — the dock
    /// workspace, reached with a mouse click — is somewhere to come back to, and
    /// quitting there would leave a bare shell for the next click to find.
    docked: bool,

    preview: Option<Preview>,
    local: LocalInfo,
    /// Worktree folder to branch, delivered with every store read. The `⎇`
    /// column is rendered from it.
    branches: HashMap<String, String>,
    /// Folder to open PR or branch-named issue number, looked up every
    /// minute. Drawn as `#52` beside the branch.
    prs: HashMap<String, u64>,
    loop_leases: HashMap<String, LoopLease>,
    /// Agents on saved machines. Drawn under the list; only remote mode (`R`)
    /// moves a cursor over them, and the local list's keys never reach one.
    remote: Vec<crate::RemoteMachine>,
    remote_mode: bool,
    remote_cursor: usize,
    /// The machine and agent an [`Action::Remote`] is out for. One at a time:
    /// a second press while the first is still on the wire would race it, and
    /// `L` twice would start and stop a loop in an order nobody chose.
    remote_busy: Option<(String, String)>,
    /// The state store's last complaint, kept apart from `err` so that clearing
    /// it cannot clear something a person was reading.
    health: Option<String>,
    /// What was already true when the screen opened — today, a previous
    /// release's data still sitting in its old folders.
    ///
    /// Its own field because nothing in here can resolve it: the folders stay
    /// where they are until somebody moves them, so nothing in here may clear
    /// the line either. On the storage-health channel it did not last — the
    /// first unreadable record replaced it, and the recovery that followed
    /// cleared it, on a condition that still held.
    startup: Option<String>,
    /// The agent the last click landed on, and when — the first half of a
    /// double click. Keyed by ID, so a list that re-sorts between the two
    /// clicks cannot turn them into a jump to a different agent.
    last_click: Option<(String, Instant)>,
    /// For each line of the last frame drawn, the ID of the agent on it.
    ///
    /// What a click is tested against. The model can move on between a draw and
    /// a click — a store re-read re-sorts the list — and a click means the row
    /// the person saw, not the one that has since taken its place.
    drawn: Vec<Option<String>>,
    /// For each screen row, the saved machine's agent drawn there, by machine
    /// label and agent id — so a click selects that one even if the list has
    /// moved since.
    drawn_remote: Vec<Option<(String, String)>>,
    /// For each screen row, the machine whose band is drawn there — what a
    /// click there folds or opens.
    drawn_bands: Vec<Option<String>>,
    /// The screen row the list started on when last drawn.
    list_top_drawn: Option<usize>,
    /// The band last clicked and the list row it was on, held there until
    /// the next key or click; see [`App::band_anchor`].
    band_anchor: Option<(String, usize)>,
    /// The machines whose rows are folded under their band, by label;
    /// [`LOCAL_MACHINE`] for this one. See [`App::toggle_fold`].
    folded: HashSet<String>,
    /// How many local agents the filter leaves, folded or not: the count a
    /// folded `@local` band shows.
    local_matched: usize,
    /// Where each key of the guide was in the last frame drawn — what a click
    /// on the footer is tested against, for the same reason as `drawn`.
    keys_drawn: Vec<crate::view::KeyHit>,
    /// The clickable `u usage` reminder in the last drawn header.
    header_usage_drawn: Option<(usize, std::ops::Range<usize>)>,
    /// The screen row the open field was drawn on in the last frame, for a
    /// click that focuses it.
    prompt_drawn: Option<usize>,
    /// The entry of the key guide the mouse is holding down, drawn reversed
    /// until the button comes back up.
    pressed_key: Option<usize>,
    /// Where the preview's text and session ID were in the last frame drawn,
    /// and that text — what a selection copies, so it copies what the person
    /// saw.
    preview_drawn: Option<crate::view::PreviewArea>,
    /// A drag over the preview in progress, drawn reversed.
    selection: Option<Selection>,
    /// The machine's load as last read, drawn in the header.
    host: HostLoad,
    /// The usage board, when the binary gave the tower one ([`App::set_usage`]).
    usage: Option<Arc<dyn crate::usage::Usage>>,
    /// The board as last drawn by it: the header's line, and the whole of it.
    usage_frame: Option<crate::usage::UsageFrame>,
    /// `u`: the board in the preview's place.
    usage_view: bool,
    /// The slider the keys move, an index into [`UsageFrame::sliders`].
    usage_edit: usize,
    /// When `r` last asked the board's sources again, so a held key does
    /// not turn into a flood of requests.
    usage_refreshed: Option<Instant>,
    /// Where the board was drawn in the last frame, for a click or a drag
    /// on a slider.
    usage_drawn: Option<crate::view::UsageArea>,
    /// The preview's height as the person chose it — by dragging its heading
    /// or with `[`/`]` — or `None` for the automatic split. See
    /// [`crate::view::split_sized`].
    preview_rows: Option<usize>,
    /// The divider between the list and the preview in the last frame drawn.
    split_drawn: Option<crate::view::SplitHandle>,
    /// A drag of that divider in progress: the divider as it was when the
    /// button went down, so the drag is measured from where it started.
    resizing: Option<crate::view::SplitHandle>,
    /// Whether that press has moved off its row. One that never did is a
    /// click, and folds or opens the preview.
    resize_moved: bool,
    /// The preview folded to its band, by a click on its heading or `[` at its
    /// smallest. Kept for this tower only, as a machine's fold is.
    preview_folded: bool,
}

/// The least time between two `r` refreshes of the usage board. Each asks
/// claude's endpoint and codex's app server; a key held down must not get
/// the account throttled.
pub const USAGE_REFRESH_GAP: Duration = Duration::from_secs(30);

/// A drag over the preview: where the button went down and where the pointer
/// is now, as screen `(row, column)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The columns selected on `row` of a screen `width` wide, the way a
    /// terminal selects: from the start column on the first row, whole rows
    /// between, up to and including the end column on the last.
    pub fn columns(&self, row: usize, width: usize) -> Option<std::ops::Range<usize>> {
        let ((r0, c0), (r1, c1)) = self.ordered();
        if row < r0 || row > r1 {
            return None;
        }
        let from = if row == r0 { c0 } else { 0 };
        let to = if row == r1 { c1 + 1 } else { width };
        (from < to.min(width)).then(|| from..to.min(width))
    }

    /// A press and release in one place: a click, not a selection.
    pub fn is_click(&self) -> bool {
        self.anchor == self.head
    }
}

impl App {
    pub fn new(store: Store, tower: Arc<dyn Tower>, settings: Settings) -> Self {
        let inside = tower.inside();
        let docked = tower.self_pane().is_some();
        let settings_preview_rows = settings.preview_rows;
        let loops = LoopStore::new(store.dir())
            .expect("the agent store exists, so its loop store must be creatable");
        let schedules = ScheduleStore::new(store.dir())
            .expect("the agent store exists, so its schedule store must be creatable");
        let parallax_store = tower_state::ParallaxStore::new(store.dir());
        let hint_store = HintStore::new(store.dir());
        let hints = hint_store.read();
        App {
            store,
            loops,
            schedules,
            schedule_list: Vec::new(),
            hint_store,
            hints,
            parallax_store,
            parallax: None,
            utc_offset: local_offset(),
            utc_offset_read: OffsetDateTime::now_utc(),
            tower,
            settings,
            agents: Vec::new(),
            all_agents: Vec::new(),
            coordinators: HashMap::new(),
            workspace_order: Vec::new(),
            workspace_display_labels: BTreeMap::new(),
            filter: String::new(),
            cursor: 0,
            now: OffsetDateTime::now_utc(),
            size: (80, 24),
            screen: Screen::List,
            pending: None,
            composer: None,
            notice: String::new(),
            err: None,
            watching: false,
            daemon: None,
            inside,
            docked,
            preview: None,
            local: LocalInfo::default(),
            branches: HashMap::new(),
            prs: HashMap::new(),
            loop_leases: HashMap::new(),
            remote: Vec::new(),
            remote_mode: false,
            remote_cursor: 0,
            remote_busy: None,
            health: None,
            startup: None,
            last_click: None,
            drawn: Vec::new(),
            drawn_remote: Vec::new(),
            drawn_bands: Vec::new(),
            list_top_drawn: None,
            band_anchor: None,
            folded: HashSet::new(),
            local_matched: 0,
            keys_drawn: Vec::new(),
            header_usage_drawn: None,
            prompt_drawn: None,
            pressed_key: None,
            preview_drawn: None,
            selection: None,
            host: HostLoad::default(),
            usage: None,
            usage_frame: None,
            usage_view: false,
            usage_edit: 0,
            usage_refreshed: None,
            usage_drawn: None,
            preview_rows: settings_preview_rows,
            split_drawn: None,
            resizing: None,
            resize_moved: false,
            preview_folded: false,
        }
    }

    // ── what the renderer reads ──────────────────────────────────────────

    /// Agents on saved machines, for the read-only section under the list.
    pub fn remote(&self) -> &[crate::RemoteMachine] {
        &self.remote
    }

    /// The saved machines as the list draws them, each with the agents the
    /// `/` filter leaves: the same fields the local rows are matched on, and
    /// every agent of a machine whose name matches, as herdr's navigator
    /// does. Every machine shows while nothing is filtered — one with no
    /// agents too, for its band's load; filtered, a machine shows only with
    /// something under it that matches.
    pub fn remote_shown(&self) -> Vec<(&crate::RemoteMachine, Vec<&crate::RemoteAgent>)> {
        let needle = self.filter.to_lowercase();
        let hit = |s: &str| s.to_lowercase().contains(&needle);
        self.remote
            .iter()
            .filter_map(|m| {
                if needle.is_empty() || hit(&m.label) {
                    return Some((m, m.agents.iter().collect()));
                }
                let agents: Vec<_> = m
                    .agents
                    .iter()
                    .filter(|a| {
                        [
                            &a.workspace,
                            &a.tab,
                            &a.title,
                            &a.agent,
                            &a.cwd,
                            &a.pane_id,
                            &a.status,
                        ]
                        .into_iter()
                        .any(|f| hit(f))
                    })
                    .collect();
                (!agents.is_empty() || !self.remote_unseen(m).is_empty()).then_some((m, agents))
            })
            .collect()
    }

    /// `m`'s panes that look like agents but are not listed as any, as the
    /// `/` filter leaves them: matched like its agents, and all of them for a
    /// machine whose name matches.
    pub fn remote_unseen<'a>(&self, m: &'a crate::RemoteMachine) -> Vec<&'a crate::UnseenPane> {
        let needle = self.filter.to_lowercase();
        let hit = |s: &str| s.to_lowercase().contains(&needle);
        let all = needle.is_empty() || hit(&m.label);
        m.unseen
            .iter()
            .filter(|p| {
                all || [&p.workspace, &p.tab, &p.title, &p.pane_id]
                    .into_iter()
                    .any(|f| hit(f))
            })
            .collect()
    }

    /// Every agent on a saved machine that is on screen, in the order the list
    /// draws them — what the remote cursor moves over.
    /// A folded machine's agents are not among them: its rows are not drawn,
    /// so the cursor must not land where nothing is.
    pub fn remote_agents(&self) -> Vec<(&crate::RemoteMachine, &crate::RemoteAgent)> {
        self.remote_shown()
            .into_iter()
            .filter(|(m, _)| !self.is_folded(&m.label))
            .flat_map(|(m, agents)| agents.into_iter().map(move |a| (m, a)))
            .collect()
    }

    /// Whether `label`'s rows are folded under its band. `@local` folds only
    /// while it has a band, which is only beside a saved machine: folded
    /// without one, the whole list would vanish with nothing to click to
    /// bring it back.
    pub fn is_folded(&self, label: &str) -> bool {
        self.folded.contains(label) && (label != LOCAL_MACHINE || !self.remote_shown().is_empty())
    }

    /// How many local agents a folded `@local` band stands for.
    pub fn local_matched(&self) -> usize {
        self.local_matched
    }

    /// Folds `label`'s rows under its band, or opens them again. The
    /// selection stays on the agent it was on while that one is still drawn;
    /// one folded away goes beside the band: the row after it, or the one
    /// before when nothing follows. Where the list is drawn is the click's
    /// business ([`App::band_anchor`]), not the selection's.
    pub fn toggle_fold(&mut self, label: &str) {
        let remote_was = self
            .selected_remote()
            .map(|(m, a)| (m.label.clone(), a.id.clone()));
        let local_was = !self.remote_mode && self.selected().is_some();
        if !self.folded.remove(label) {
            self.folded.insert(label.to_string());
        }
        self.apply_filter();
        let remote = self.remote_agents();
        if let Some((m, id)) = remote_was {
            if let Some(i) = remote
                .iter()
                .position(|(rm, a)| rm.label == m && a.id == id)
            {
                self.remote_cursor = i;
                return;
            }
        } else if local_was && !self.agents.is_empty() {
            return;
        }
        let n = remote.len();
        // The rows drawn before `label`'s band among the machines: where the
        // rows after it start now that its own are folded.
        let before: usize = if label == LOCAL_MACHINE {
            0
        } else {
            self.remote_shown()
                .iter()
                .take_while(|(m, _)| m.label != label)
                .filter(|(m, _)| !self.is_folded(&m.label))
                .map(|(_, agents)| agents.len())
                .sum()
        };
        if before < n {
            self.remote_mode = true;
            self.remote_cursor = before;
        } else if n > 0 {
            self.remote_mode = true;
            self.remote_cursor = n - 1;
        } else {
            self.remote_mode = false;
            self.cursor = self.agents.len().saturating_sub(1);
        }
    }

    /// The band last clicked, by label, and the row of the list it was
    /// clicked on. Until the next key or click the list is drawn with that
    /// band on that row rather than around the cursor, so folding or opening
    /// it moves neither the selection nor the band.
    pub fn band_anchor(&self) -> Option<(&str, usize)> {
        self.band_anchor.as_ref().map(|(l, at)| (l.as_str(), *at))
    }

    /// The agent of a saved machine under the cursor, while it is on one.
    pub fn selected_remote(&self) -> Option<(&crate::RemoteMachine, &crate::RemoteAgent)> {
        self.remote_mode
            .then(|| self.remote_agents().get(self.remote_cursor).copied())
            .flatten()
    }

    /// Whether the cursor is on the saved machines' rows.
    pub fn remote_mode(&self) -> bool {
        self.remote_mode
    }

    /// The remote agent under the remote cursor, as an index into
    /// [`Self::remote_agents`].
    pub fn remote_cursor(&self) -> usize {
        self.remote_cursor
    }

    /// The text field open for a remote message, if one is.
    pub fn remote_input_open(&self) -> bool {
        matches!(self.composer, Some(Composer::RemoteSend { .. }))
    }

    /// `R`: the keys go to the agents on saved machines until `R` or `esc`.
    /// The keys while the cursor is on an agent of a saved machine: moving,
    /// `L` loop on/off, `m` message, `x` interrupt. The keys that work on the
    /// whole list (`/`, `r`, `u`, `W`, `C`, `B`, `q`) go on to the list; the
    /// ones that act on a local agent (`enter`, `i`, `g`, …) only say so, so
    /// none of them can reach a pane on another machine by accident.
    fn on_remote_key(&mut self, key: KeyEvent) -> Option<Action> {
        let n = self.remote_agents().len();
        match key.code {
            KeyCode::Esc => self.leave_remote(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.remote_cursor = (self.remote_cursor + 1).min(n.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.remote_cursor > 0 {
                    self.remote_cursor -= 1;
                } else if !self.is_folded(LOCAL_MACHINE) {
                    // With `@local` folded there is no local row to go up into.
                    self.leave_remote();
                }
            }
            KeyCode::Char('L') => return Some(self.remote_toggle_loop()),
            KeyCode::Char('T') => return Some(self.remote_toggle_schedule()),
            KeyCode::Char('m') => {
                if let Some((m, a)) = self.remote_agents().get(self.remote_cursor).copied() {
                    self.composer = Some(Composer::RemoteSend {
                        input: TextInput::new(BROADCAST_LIMIT),
                        label: m.label.clone(),
                        id: a.id.clone(),
                    });
                }
            }
            KeyCode::Char('x') => {
                if let Some((m, a)) = self.remote_agents().get(self.remote_cursor).copied() {
                    self.pending = Some(Pending::RemoteInterrupt {
                        label: m.label.clone(),
                        id: a.id.clone(),
                    });
                }
            }
            KeyCode::Char('/' | 'r' | 'u' | 'W' | 'C' | 'B' | 'q') => return None,
            _ => {
                self.notice =
                    "on another machine: L loop · T schedule · m message · x interrupt".into();
            }
        }
        Some(Action::Nothing)
    }

    /// Back to the local rows, on the last one — where `j` left them.
    fn leave_remote(&mut self) {
        self.remote_mode = false;
        self.cursor = self.agents.len().saturating_sub(1);
    }

    /// Finds an agent on a saved machine by its label and id, cloned so the
    /// command can run while `self` is borrowed again.
    fn remote_find(
        &self,
        label: &str,
        id: &str,
    ) -> Option<(crate::RemoteMachine, crate::RemoteAgent)> {
        let m = self.remote.iter().find(|m| m.label == label)?;
        let a = m.agents.iter().find(|a| a.id == id)?;
        Some((m.clone(), a.clone()))
    }

    /// Checks that `action` can be asked of the agent `id` on `label`, and
    /// hands it to the driver to ask.
    fn remote_do(&mut self, label: &str, id: &str, action: crate::RemoteAction) -> Action {
        if self.refuse_while_busy() {
            return Action::Nothing;
        }
        let Some((m, a)) = self.remote_find(label, id) else {
            self.err = Some(format!("{label} · {id} is gone — wait for the next read"));
            return Action::Nothing;
        };
        if m.stale_since.is_some() {
            self.err = Some(format!(
                "{label} is not answering — what is shown is the last read"
            ));
            return Action::Nothing;
        }
        self.err = None;
        self.notice = format!("asking {label}…");
        self.remote_busy = Some((label.to_string(), id.to_string()));
        Action::Remote(Box::new(crate::RemoteRequest {
            machine: m,
            agent: a,
            action,
        }))
    }

    /// Whether an [`Action::Remote`] is still out, saying so on the error line
    /// when it is.
    fn refuse_while_busy(&mut self) -> bool {
        let Some((label, id)) = &self.remote_busy else {
            return false;
        };
        self.err = Some(format!(
            "still waiting for {label} · {id} — try again once it answers"
        ));
        true
    }

    /// [`Event::RemoteDone`]: the answer to the one [`Action::Remote`] out.
    fn remote_done(
        &mut self,
        label: &str,
        id: &str,
        action: &crate::RemoteAction,
        result: Result<String, String>,
    ) {
        self.remote_busy = None;
        let done = match result {
            Ok(done) => done,
            Err(e) => {
                self.err = Some(format!("{label} · {id}: {e}"));
                return;
            }
        };
        self.err = None;
        self.notice = done;
        // Show it now; the next read (10s) confirms it.
        let state = match action {
            crate::RemoteAction::LoopStart => Some(Some("starting…".to_string())),
            crate::RemoteAction::LoopStop => Some(None),
            _ => None,
        };
        // A schedule shows at once; the next read (10s) brings its id.
        let schedules = match action {
            crate::RemoteAction::Schedule { time, .. } => Some(vec![crate::RemoteSchedule {
                id: String::new(),
                next: time.clone(),
            }]),
            crate::RemoteAction::Unschedule(_) => Some(Vec::new()),
            _ => None,
        };
        let Some(agent) = self
            .remote
            .iter_mut()
            .find(|m| m.label == label)
            .and_then(|m| m.agents.iter_mut().find(|a| a.id == id))
        else {
            return;
        };
        if let Some(list) = schedules {
            agent.schedules = list;
        }
        if let Some(state) = state {
            agent.loop_state = state;
        }
    }

    /// `T` on a saved machine's row: a schedule for it, or — when it already
    /// has some — cancelling them, as `T` does on a local row.
    fn remote_toggle_schedule(&mut self) -> Action {
        let Some((m, a)) = self.remote_agents().get(self.remote_cursor).copied() else {
            return Action::Nothing;
        };
        let (label, id) = (m.label.clone(), a.id.clone());
        if !a.schedules.is_empty() {
            let ids = a.schedules.iter().map(|s| s.id.clone()).collect();
            return self.remote_do(&label, &id, crate::RemoteAction::Unschedule(ids));
        }
        if a.session_id.is_empty() {
            self.err =
                Some("a scheduled message needs the agent's session id; it has none yet".into());
            return Action::Nothing;
        }
        self.composer = Some(Composer::RemoteSchedule {
            input: TextInput::hinted(
                SCHEDULE_MESSAGE_LIMIT,
                or_example(&self.hints.schedule, SCHEDULE_EXAMPLE),
            ),
            label,
            id,
            repeat: 0,
        });
        Action::Nothing
    }

    /// The `T` line for a saved machine's agent: checked here with the same
    /// parser that machine uses, then handed to its herdr-tower. The time is
    /// read there, in that machine's local time.
    fn arm_remote_schedule(
        &mut self,
        line: String,
        label: String,
        id: String,
        repeat: usize,
    ) -> Action {
        let (time, message) = line
            .trim()
            .split_once(char::is_whitespace)
            .map_or((line.trim(), ""), |(t, m)| (t, m.trim()));
        let now = OffsetDateTime::now_utc().to_offset(self.utc_offset);
        let checked = parse_when(time, Repeat::ALL[repeat], now).and_then(|_| {
            if message.is_empty() {
                Err("after the time, the message to send".to_string())
            } else {
                Ok(())
            }
        });
        if let Err(e) = checked {
            self.err = Some(format!("⏰ {e}"));
            self.composer = Some(Composer::RemoteSchedule {
                input: TextInput::prefilled(SCHEDULE_MESSAGE_LIMIT, &line),
                label,
                id,
                repeat,
            });
            return Action::Nothing;
        }
        // Refused here rather than in `remote_do`, so the line stays typed.
        if self.refuse_while_busy() {
            self.composer = Some(Composer::RemoteSchedule {
                input: TextInput::prefilled(SCHEDULE_MESSAGE_LIMIT, &line),
                label,
                id,
                repeat,
            });
            return Action::Nothing;
        }
        self.remote_do(
            &label,
            &id,
            crate::RemoteAction::Schedule {
                time: time.to_string(),
                message: message.to_string(),
                repeat: Repeat::ALL[repeat],
            },
        )
    }

    fn remote_toggle_loop(&mut self) -> Action {
        let Some((m, a)) = self.remote_agents().get(self.remote_cursor).copied() else {
            return Action::Nothing;
        };
        let action = if a.loop_state.is_some() {
            crate::RemoteAction::LoopStop
        } else {
            crate::RemoteAction::LoopStart
        };
        let (label, id) = (m.label.clone(), a.id.clone());
        self.remote_do(&label, &id, action)
    }

    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    pub fn workspace_display_label(&self, id: &str) -> Option<&str> {
        self.workspace_display_labels.get(id).map(String::as_str)
    }

    /// The coordinator `a` works under, as its row marks it: `pEJ` in the
    /// same workspace, `w2:pEJ` from another. `None` for a pane nobody
    /// supervises.
    pub fn coordinator_of(&self, a: &Agent) -> Option<&str> {
        self.coordinators.get(&a.id).map(String::as_str)
    }

    /// Every live agent, whatever `/` has filtered out of [`App::agents`].
    pub fn all_agents(&self) -> &[Agent] {
        &self.all_agents
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selected(&self) -> Option<&Agent> {
        self.agents.get(self.cursor)
    }

    pub fn now(&self) -> OffsetDateTime {
        self.now
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    pub fn input(&self) -> Option<&TextInput> {
        self.composer.as_ref().map(Composer::input)
    }

    /// `Some` distinguishes the loop composer from the broadcast composer and
    /// carries the currently selected idle preset for the footer.
    pub fn loop_input_idle_for(&self) -> Option<Duration> {
        self.composer.as_ref().and_then(Composer::idle_for)
    }

    /// The repetition picked in the open schedule field, if one is open.
    pub fn schedule_input_repeat(&self) -> Option<Repeat> {
        self.composer.as_ref().and_then(Composer::repeat)
    }

    /// Every schedule, as last read.
    pub fn schedules(&self) -> &[Schedule] {
        &self.schedule_list
    }

    /// Parallax Browser's handoffs waiting on the person, most urgent first —
    /// none while the supervisor's snapshot is missing or stale.
    pub fn parallax_waiting(&self) -> Vec<&tower_state::ParallaxHandoff> {
        tower_state::parallax_waiting(self.parallax.as_ref(), self.now)
    }

    /// The machine's offset from UTC, for reading schedules in local time.
    /// Read again hourly and whenever a schedule is made.
    pub fn utc_offset(&self) -> time::UtcOffset {
        self.utc_offset
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    /// What was true before the screen opened, shown until something replaces it.
    ///
    /// Set once, by the driver, and never cleared: see [`App::startup`].
    pub fn set_startup_note(&mut self, text: String) {
        self.startup = Some(text);
    }

    /// The failure worth showing, most immediate first: what the last action
    /// reported, then the state store saying it cannot be read, then what was
    /// already wrong when the screen opened.
    pub fn err(&self) -> Option<&str> {
        self.err
            .as_deref()
            .or(self.health.as_deref())
            .or(self.startup.as_deref())
    }

    pub fn local(&self) -> &LocalInfo {
        &self.local
    }

    pub fn preview(&self) -> Option<&Preview> {
        self.preview.as_ref()
    }

    /// Never capture the screen that contains this preview: feeding it back
    /// into itself accumulates copies of the tower's footer on every refresh.
    pub(crate) fn preview_target(&self) -> Option<String> {
        if self.remote_mode {
            // A machine that stopped answering has nothing to show but what
            // the dim row already says.
            let (m, a) = self.remote_agents().get(self.remote_cursor).copied()?;
            return m
                .stale_since
                .is_none()
                .then(|| crate::remote::preview_key(&m.label, &a.pane_id));
        }
        let sel = self.selected()?;
        if sel.state == AgentState::Dead || sel.pane.pane_id.is_empty() {
            return None;
        }
        let target = tower_mux::target_key(&sel.pane.backend, &sel.pane.pane_id);
        (self.tower.self_pane().as_deref() != Some(target.as_str())).then_some(target)
    }

    /// The captured pane body, but only when it belongs to the row that is
    /// highlighted right now.
    ///
    /// The capture carries the pane it came from, and that is checked here.
    /// Without the check, after moving the cursor the panel would show the
    /// *previous* agent's screen under the newly selected agent's title until
    /// the next capture landed — a screenshot attributed to the wrong session,
    /// which is the one thing a preview must never do.
    pub fn live_preview(&self) -> Option<&str> {
        let target = self.preview_target()?;
        let p = self.preview.as_ref()?;
        (p.pane_id == target && !p.body.trim().is_empty()).then_some(p.body.as_str())
    }

    pub fn branch_of(&self, cwd: &str) -> Option<&str> {
        self.branches.get(cwd).map(String::as_str)
    }

    pub fn pr_of(&self, cwd: &str) -> Option<u64> {
        self.prs.get(cwd).copied()
    }

    pub fn loop_lease(&self, id: &str) -> Option<&LoopLease> {
        self.loop_leases.get(id)
    }

    pub fn ctx_of(&self, id: &str) -> Option<&CtxInfo> {
        self.local.ctx.get(id)
    }

    pub fn watching(&self) -> bool {
        self.watching
    }

    /// What this machine runs, for `@local`'s band: its system, and this
    /// tower's own version — not the supervisor's, which may lag behind it.
    pub fn local_system(&self) -> tower_os::host::System {
        tower_os::host::system(&self.settings.version)
    }

    pub fn version(&self) -> &str {
        self.daemon
            .as_ref()
            .filter(|snapshot| snapshot.fresh(self.now))
            .map(|snapshot| snapshot.version.as_str())
            .unwrap_or(&self.settings.version)
    }

    pub fn backend_count(&self) -> usize {
        self.daemon
            .as_ref()
            .filter(|snapshot| snapshot.fresh(self.now))
            .map_or(0, |snapshot| snapshot.backends.len())
    }

    pub fn set_size(&mut self, width: u16, height: u16) {
        self.size = (width, height);
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    // ── events ───────────────────────────────────────────────────────────

    pub fn on_event(&mut self, ev: Event) -> Action {
        match ev {
            Event::Key(k) => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m, Instant::now()),
            Event::Records {
                agents,
                branches,
                tab_roots: _,
                parents,
                daemon,
                now,
            } => {
                // The tower is an operational view of agents that exist now.
                // DEAD records are recovery data: the scanner keeps them for 24
                // hours and the explicit `resume` / `restore` commands consume
                // them. Drawing them here made a normal popup mostly history
                // and buried the live sessions the tower is for.
                let mut agents: Vec<_> = agents.into_iter().filter(Agent::is_live).collect();
                // Older long-running board views can still write display labels.
                // Every UI consumer uses the same semantic workspace name.
                for agent in &mut agents {
                    agent.pane.session =
                        herdrx::strip_workspace_prefix(&agent.pane.session).to_string();
                }
                if let Some(snapshot) = daemon.as_ref().filter(|s| s.fresh(now)) {
                    self.workspace_order = snapshot.workspace_order.clone();
                    self.workspace_display_labels = snapshot.workspace_display_labels.clone();
                }
                order_tower_agents(&mut agents, &self.workspace_order);
                self.coordinators = coordinators(&agents, &parents);
                self.all_agents = agents;
                self.branches = branches;
                self.apply_filter();
                self.loop_leases = self
                    .loops
                    .list()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| (l.agent_id.clone(), l))
                    .collect();
                self.schedule_list = self.schedules.list().unwrap_or_default();
                self.parallax = self.parallax_store.read();
                if now - self.utc_offset_read >= time::Duration::HOUR {
                    self.utc_offset = local_offset();
                    self.utc_offset_read = now;
                }
                self.watching = daemon
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.fresh(now) && snapshot.connected);
                self.daemon = daemon;
                self.now = now;
                Action::Nothing
            }
            Event::Local(l) => {
                self.local = l;
                Action::Nothing
            }
            Event::Host(load) => {
                self.host = load;
                Action::Nothing
            }
            Event::Usage => {
                self.redraw_usage();
                Action::Nothing
            }
            Event::Remote(v) => {
                self.remote = keep_stale(&self.remote, v, OffsetDateTime::now_utc());
                // Whether `@local` can be folded hangs on there being a
                // machine beside it.
                if self.folded.contains(LOCAL_MACHINE) {
                    self.apply_filter();
                }
                let n = self.remote_agents().len();
                if n == 0 {
                    self.remote_mode = false;
                }
                self.remote_cursor = self.remote_cursor.min(n.saturating_sub(1));
                Action::Nothing
            }
            Event::Prs(prs) => {
                self.prs = prs;
                self.apply_filter();
                Action::Nothing
            }
            Event::Preview { pane_id, body } => {
                self.preview = Some(Preview { pane_id, body });
                Action::Nothing
            }
            Event::Health(r) => {
                self.health = r.err();
                Action::Nothing
            }
            Event::RemoteDone {
                label,
                id,
                action,
                result,
            } => {
                self.remote_done(&label, &id, &action, result);
                Action::Nothing
            }
            Event::Notice(r) => {
                match r {
                    Ok(text) => {
                        self.err = None;
                        self.notice = text;
                    }
                    Err(e) => self.err = Some(e),
                }
                Action::Nothing
            }
        }
    }

    /// Keeps the highlight on the same agent across a re-read.
    ///
    /// The listing is sorted by urgency, so a row changing state re-sorts the
    /// list underneath a fixed index: keeping `cursor` as a number and only
    /// clamping it would make the highlight drift to a different agent every
    /// time anything anywhere changed state — twice a second under a watch. On a
    /// screen where `enter` moves the user's view, the highlight has to mean the
    /// agent they were looking at.
    ///
    /// When that agent is gone the index is clamped, which is the only thing
    /// left to do.
    /// What `/` narrowed the list to, empty when it shows every agent.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Whether the `/` field is open.
    pub fn filtering(&self) -> bool {
        matches!(self.composer, Some(Composer::Filter(_)))
    }

    /// Puts on the list the agents whose workspace, tab, title, task, kind,
    /// folder, branch or open PR (`#52`) contain the filter, ignoring case — the selection
    /// staying on its agent while that one still matches.
    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        let next: Vec<Agent> = self
            .all_agents
            .iter()
            .filter(|a| {
                let pr = self
                    .prs
                    .get(&a.cwd)
                    .map_or_else(String::new, |n| format!("#{n}"));
                needle.is_empty()
                    || [
                        a.pane.session.as_str(),
                        a.pane.tab.as_str(),
                        a.title.as_str(),
                        a.task.as_str(),
                        a.kind.as_str(),
                        a.cwd.as_str(),
                        self.branches.get(&a.cwd).map_or("", String::as_str),
                        pr.as_str(),
                    ]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&needle))
            })
            .cloned()
            .collect();
        self.local_matched = next.len();
        let next = if self.is_folded(LOCAL_MACHINE) {
            Vec::new()
        } else {
            next
        };
        self.reseat_cursor(&next);
        self.agents = next;
    }

    fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.apply_filter();
        let n = self.remote_agents().len();
        self.remote_cursor = self.remote_cursor.min(n.saturating_sub(1));
        if n == 0 {
            self.remote_mode = false;
        }
    }

    fn reseat_cursor(&mut self, next: &[Agent]) {
        if let Some(id) = self.selected().map(|a| a.id.clone()) {
            if let Some(i) = next.iter().position(|a| a.id == id) {
                self.cursor = i;
                return;
            }
        }
        self.cursor = self.cursor.min(next.len().saturating_sub(1));
    }

    // ── mouse ────────────────────────────────────────────────────────────

    /// Records which agent is on which line, for the frame just drawn. The
    /// driver calls this after every draw; see [`App::on_mouse`].
    ///
    /// Lays the frame out again to do it, so it is for a caller with no frame
    /// in hand — the tests. The driver has just laid one out and hands it to
    /// [`App::remember_frame`], which this calls too.
    pub fn remember_drawn(&mut self) {
        let (width, height) = self.size();
        let frame = crate::view::compose(self, width.into(), height.into());
        self.remember_frame(frame);
    }

    /// [`App::remember_drawn`], from the frame the driver just drew rather than
    /// laying it out a second time.
    pub fn remember_frame(&mut self, frame: crate::view::Composed) {
        self.drawn = frame
            .agents
            .into_iter()
            .map(|i| i.map(|i| self.agents[i].id.clone()))
            .collect();
        let remote = self.remote_agents();
        self.drawn_remote = frame
            .remotes
            .into_iter()
            .map(|i| {
                i.and_then(|i| remote.get(i))
                    .map(|(m, a)| (m.label.clone(), a.id.clone()))
            })
            .collect();
        self.drawn_bands = frame.bands;
        self.list_top_drawn = frame.list_top;
        self.keys_drawn = frame.keys;
        self.header_usage_drawn = frame.header_usage;
        self.prompt_drawn = frame.prompt_row;
        self.preview_drawn = frame.preview_area;
        self.usage_drawn = frame.usage_area;
        self.split_drawn = frame.split;
    }

    // ── preview size ─────────────────────────────────────────────────────

    /// The preview's height as the person chose it, `None` for automatic.
    pub fn preview_rows(&self) -> Option<usize> {
        self.preview_rows
    }

    /// Whether the preview is folded to its band.
    pub fn preview_folded(&self) -> bool {
        self.preview_folded
    }

    /// A click on the preview's heading or its band: an open preview folds,
    /// a folded one opens to what it was — its chosen height, or the
    /// automatic split. A preview that only the list's length kept closed has
    /// nothing to go back to, so it opens at half the body.
    fn toggle_preview(&mut self, split: crate::view::SplitHandle) -> Action {
        self.notice.clear();
        if split.preview_h >= crate::view::PREVIEW_MIN {
            self.preview_folded = true;
            return Action::Nothing;
        }
        let was_folded = std::mem::replace(&mut self.preview_folded, false);
        if self.preview_rows.is_some() || (was_folded && split.auto_opens) {
            return Action::Nothing;
        }
        self.set_preview_rows(Some(split.sized(split.body_h / 2)))
    }

    /// `[` and `]`: the preview a few rows shorter or taller, starting from
    /// the height it is drawn at now. At its smallest, `[` folds it to its
    /// band; from there `]` opens it to what it was, or — kept closed only by
    /// the list's length — at the smallest useful height, which is what the
    /// list can spare first.
    fn nudge_preview(&mut self, grow: bool) -> Action {
        const STEP: usize = 3;
        let Some(split) = self.split_drawn else {
            // No divider drawn — a list that fits but leaves too little for
            // the automatic preview. `]` still asks for the smallest one.
            if grow && self.preview_rows.is_none() && self.preview_target().is_some() {
                self.preview_folded = false;
                return self.set_preview_rows(Some(crate::view::PREVIEW_MIN));
            }
            return Action::Nothing;
        };
        // Folded by the person, `]` first opens it to what it was.
        if grow
            && std::mem::replace(&mut self.preview_folded, false)
            && (self.preview_rows.is_some() || split.auto_opens)
        {
            return Action::Nothing;
        }
        let now = split.preview_h;
        let next = if grow {
            now + STEP
        } else if now <= crate::view::PREVIEW_MIN {
            self.preview_folded = now == crate::view::PREVIEW_MIN || self.preview_folded;
            return Action::Nothing;
        } else {
            now.saturating_sub(STEP)
        };
        // Measured as a drag would be, so the keys and the mouse keep the
        // stored height inside the same bounds.
        let rows = split.dragged_to((split.row + now).saturating_sub(next));
        self.set_preview_rows(Some(rows))
    }

    /// Changes the chosen height, and asks the driver to remember it when it
    /// changed, so the next tower opens at the same split.
    fn set_preview_rows(&mut self, rows: Option<usize>) -> Action {
        if self.preview_rows == rows {
            return Action::Nothing;
        }
        self.preview_rows = rows;
        Action::SavePreviewRows(rows)
    }

    // ── usage ────────────────────────────────────────────────────────────

    /// Gives the tower the usage board: a line of it under the header, and
    /// all of it on `u`.
    pub fn set_usage(&mut self, usage: Arc<dyn crate::usage::Usage>) {
        self.usage = Some(usage);
        self.redraw_usage();
    }

    /// The machine's load as last read.
    pub fn host_load(&self) -> HostLoad {
        self.host
    }

    /// The board as last drawn, if there is one.
    pub fn usage_frame(&self) -> Option<&crate::usage::UsageFrame> {
        self.usage_frame.as_ref()
    }

    /// Whether the board is in the preview's place.
    /// Whether `q` quits: not in the dock the supervisor keeps.
    pub fn quits(&self) -> bool {
        !self.settings.kept
    }

    pub fn usage_view(&self) -> bool {
        self.usage_view && self.usage_frame.is_some()
    }

    /// Draws the board again from what it last read, with the chosen slider
    /// marked — after a refresh, a choice or a change.
    fn redraw_usage(&mut self) {
        let Some(u) = &self.usage else {
            return;
        };
        let frame = u.render(Some(self.usage_edit));
        if !frame.sliders.is_empty() && self.usage_edit >= frame.sliders.len() {
            self.usage_edit = frame.sliders.len() - 1;
            self.usage_frame = Some(u.render(Some(self.usage_edit)));
        } else {
            self.usage_frame = Some(frame);
        }
    }

    fn adjust_usage(&mut self, slider: usize, how: crate::usage::Adjust) {
        let Some(u) = self.usage.clone() else {
            return;
        };
        if let Err(e) = u.adjust(slider, how) {
            self.err = Some(e);
        }
        self.redraw_usage();
    }

    /// The board's own keys, while it is showing: `↑↓` choose a slider,
    /// `←→` move it, `x` turns it off, `r` reads the figures again, `u`, `q`
    /// or esc go back to the preview. Anything else is the list's — and a
    /// session chosen there (`j`/`k`, a click on its row, the wheel) brings its
    /// preview back too: choosing one is asking to see it.
    fn on_usage_key(&mut self, key: KeyEvent) -> Option<Action> {
        use crate::usage::Adjust;
        let n = self.usage_frame.as_ref().map_or(0, |f| f.sliders.len());
        match key.code {
            // `q` too, though the guide does not offer it: pressed out of
            // habit on the board, it goes back to the list rather than out
            // of the tower.
            KeyCode::Char('u') | KeyCode::Char('q') | KeyCode::Esc => {
                self.usage_view = false;
                Some(Action::Nothing)
            }
            KeyCode::Up if n > 0 => {
                self.usage_edit = (self.usage_edit + n - 1) % n;
                self.redraw_usage();
                Some(Action::Nothing)
            }
            KeyCode::Down if n > 0 => {
                self.usage_edit = (self.usage_edit + 1) % n;
                self.redraw_usage();
                Some(Action::Nothing)
            }
            KeyCode::Left | KeyCode::Char('-') if n > 0 => {
                self.adjust_usage(self.usage_edit, Adjust::Less);
                Some(Action::Nothing)
            }
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') if n > 0 => {
                self.adjust_usage(self.usage_edit, Adjust::More);
                Some(Action::Nothing)
            }
            KeyCode::Char('x') if n > 0 => {
                self.adjust_usage(self.usage_edit, Adjust::Toggle);
                Some(Action::Nothing)
            }
            KeyCode::Char('r') => Some(self.refresh_usage(Instant::now())),
            _ => None,
        }
    }

    /// `r` on the board: its sources asked again, at most once per
    /// [`USAGE_REFRESH_GAP`].
    fn refresh_usage(&mut self, now: Instant) -> Action {
        if let Some(last) = self.usage_refreshed {
            let since = now.duration_since(last);
            if since < USAGE_REFRESH_GAP {
                let wait = (USAGE_REFRESH_GAP - since).as_secs().max(1);
                self.notice = format!("usage refreshed a moment ago — again in {wait}s");
                return Action::Nothing;
            }
        }
        self.usage_refreshed = Some(now);
        self.notice = "reading usage again…".into();
        Action::RefreshUsage
    }

    /// A press or a drag on the board: on a slider, it chooses that slider
    /// and sets the floor where it landed.
    fn on_usage_mouse(&mut self, row: usize, col: usize) -> bool {
        let Some(area) = &self.usage_drawn else {
            return false;
        };
        if !area.rows.contains(&row) {
            return false;
        }
        let line = area.first_line + (row - area.rows.start);
        let Some(i) = self
            .usage_frame
            .as_ref()
            .and_then(|f| f.sliders.iter().position(|s| *s == line))
        else {
            return true;
        };
        self.usage_edit = i;
        let at = u16::try_from(col + 1).unwrap_or(u16::MAX);
        self.adjust_usage(i, crate::usage::Adjust::At(at));
        true
    }

    /// The drag over the preview in progress, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Copies what `sel` covers of the last frame drawn, trailing spaces off
    /// each line, and says so.
    fn copy_selection(&mut self, sel: Selection) {
        if sel.is_click() {
            return;
        }
        let Some(area) = &self.preview_drawn else {
            return;
        };
        let width = usize::from(self.size().0);
        let ((r0, _), (r1, _)) = sel.ordered();
        let text: Vec<String> = (r0..=r1)
            .filter_map(|row| {
                let cols = sel.columns(row, width)?;
                let line = area.text.get(row.checked_sub(area.rows.start)?)?;
                Some(crate::text::columns(line, cols).trim_end().to_string())
            })
            .collect();
        let text = text.join("\n");
        if text.trim().is_empty() {
            return;
        }
        let lines = text.lines().count();
        let what = if lines == 1 {
            format!("{} characters", text.chars().count())
        } else {
            format!("{lines} lines")
        };
        self.copy_text(&text, format!("copied {what} from the preview"));
    }

    fn copy_text(&mut self, text: &str, notice: String) {
        match self.tower.copy(text) {
            Ok(()) => {
                self.err = None;
                self.notice = notice;
            }
            Err(e) => self.err = Some(format!("could not copy: {e}")),
        }
    }

    /// `(row, column)` pulled inside the preview's text, so a drag that runs
    /// past its edge selects up to the edge.
    fn inside_preview(&self, row: u16, column: u16) -> Option<(usize, usize)> {
        let area = self.preview_drawn.as_ref()?;
        if area.rows.is_empty() {
            return None;
        }
        let row = usize::from(row).clamp(area.rows.start, area.rows.end - 1);
        let width = usize::from(self.size().0).max(1);
        Some((row, usize::from(column).min(width - 1)))
    }

    /// The entry of the key guide being held down with the mouse, if any.
    pub fn pressed_key(&self) -> Option<usize> {
        self.pressed_key
    }

    /// A click selects the row under it; a double click jumps there, exactly as
    /// `enter` does. The wheel moves the selection. `now` is when the event
    /// arrived, a parameter so the double-click window can be tested.
    ///
    /// A single click never jumps, because it is also how the preview is chosen
    /// — a tower that left on every click could not be looked around in with the
    /// mouse. Nor does a slow second click on the selected row: that is a person
    /// clicking again, not asking to leave.
    ///
    /// A click on the key guide at the bottom presses that key, on any screen,
    /// and the entry stays reversed while the button is held so the press can
    /// be seen; letting go draws it plainly again.
    ///
    /// Ignored while something is waiting on the keyboard (the broadcast field,
    /// a confirmation): a stray click must not answer a question it cannot see,
    /// nor type the letter it landed on into a message.
    pub fn on_mouse(&mut self, m: MouseEvent, now: Instant) -> Action {
        // Letting go of the button lets go of the key it was holding down, and
        // ends a drag over the preview by copying what it covered.
        if let MouseEventKind::Up(_) = m.kind {
            self.pressed_key = None;
            if let Some(split) = self.resizing.take() {
                if !std::mem::take(&mut self.resize_moved) {
                    return self.toggle_preview(split);
                }
                return match self.preview_rows {
                    Some(rows) => Action::SavePreviewRows(Some(rows)),
                    None => Action::Nothing,
                };
            }
            if let Some(sel) = self.selection.take() {
                self.copy_selection(sel);
            }
            return Action::Nothing;
        }
        if let MouseEventKind::Drag(MouseButton::Left) = m.kind {
            // Dragging the divider: the preview follows the pointer. Saved
            // once, when the button comes up, not on every row it passes.
            if let Some(split) = self.resizing {
                let row = usize::from(m.row);
                // Along its own row is still a click.
                if !self.resize_moved && row == split.row {
                    return Action::Nothing;
                }
                // Off it, a drag — but below a band is not a height at all.
                self.resize_moved = true;
                if split.preview_h < crate::view::PREVIEW_MIN && row > split.row {
                    return Action::Nothing;
                }
                self.preview_folded = false;
                self.preview_rows = Some(split.dragged_to(row));
                return Action::Nothing;
            }
            if self.usage_view()
                && self.selection.is_none()
                && self.on_usage_mouse(usize::from(m.row), usize::from(m.column))
            {
                return Action::Nothing;
            }
            if self.selection.is_some() {
                if let Some(head) = self.inside_preview(m.row, m.column) {
                    if let Some(sel) = &mut self.selection {
                        sel.head = head;
                    }
                }
            }
            return Action::Nothing;
        }
        // Any press or wheel turn disarms a pending first click, so only two
        // clicks back to back on one row make a double click — a click on the
        // header in between does not. The release that follows every press is
        // not input in that sense, and neither is motion.
        let armed = match m.kind {
            MouseEventKind::Down(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => self.last_click.take(),
            _ => return Action::Nothing,
        };
        // A release the terminal never sent must not leave a key held for good.
        self.pressed_key = None;
        if let Some(composer) = &mut self.composer {
            // A click on the field itself is focusing it, as in any other
            // editor: the dim hint goes, and the field is empty to type into.
            // It types nothing and answers nothing, so it is the one click a
            // field lets through.
            if m.kind == MouseEventKind::Down(MouseButton::Left)
                && self.prompt_drawn == Some(usize::from(m.row))
            {
                composer.input_mut().dismiss_hint();
            }
            return Action::Nothing;
        }
        if self.pending.is_some() {
            return Action::Nothing;
        }
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            let (row, col) = (usize::from(m.row), usize::from(m.column));
            if self.screen == Screen::List
                && self
                    .header_usage_drawn
                    .as_ref()
                    .is_some_and(|(hit_row, cols)| *hit_row == row && cols.contains(&col))
            {
                return self.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
            }
            if let Some(hit) = self
                .keys_drawn
                .iter()
                .find(|k| k.row == row && (k.from..k.to).contains(&col))
                .copied()
            {
                let screen = std::mem::discriminant(&self.screen);
                let action = self.on_key(KeyEvent::new(hit.key, KeyModifiers::NONE));
                // Only on the guide it was pressed on: a key that changed the
                // screen is followed by a different guide, where the same entry
                // is a different key.
                if std::mem::discriminant(&self.screen) == screen {
                    self.pressed_key = Some(hit.entry);
                }
                return action;
            }
        }
        if self.screen != Screen::List {
            return Action::Nothing;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // The preview is text to read and take, not rows to choose: a
                // click on its heading's session ID copies the whole ID, and a
                // press inside it starts a selection.
                let (row, col) = (usize::from(m.row), usize::from(m.column));
                if self.usage_view() && self.on_usage_mouse(row, col) {
                    return Action::Nothing;
                }
                if let Some(area) = &self.preview_drawn {
                    if let Some(sid) = area
                        .sid
                        .as_ref()
                        .filter(|s| s.row == row && s.cols.contains(&col))
                    {
                        let id = sid.id.clone();
                        self.copy_text(&id, format!("copied session id {id}"));
                        return Action::Nothing;
                    }
                }
                // Anywhere else on the divider takes hold of it: dragged, it
                // moves; let go where it was pressed, it folds or opens.
                if let Some(split) = self.split_drawn.filter(|s| s.row == row) {
                    self.notice.clear();
                    self.resizing = Some(split);
                    self.resize_moved = false;
                    return Action::Nothing;
                }
                if let Some(area) = &self.preview_drawn {
                    if area.rows.contains(&row) {
                        self.notice.clear();
                        self.selection = Some(Selection {
                            anchor: (row, col),
                            head: (row, col),
                        });
                        return Action::Nothing;
                    }
                }
                // A machine's band folds its rows under it, or opens them,
                // and the list is held with the band where it was clicked.
                self.band_anchor = None;
                if let Some(label) = self.drawn_bands.get(row).cloned().flatten() {
                    self.notice.clear();
                    self.toggle_fold(&label);
                    let at = row.saturating_sub(self.list_top_drawn.unwrap_or(0));
                    self.band_anchor = Some((label, at));
                    return Action::Nothing;
                }
                // A saved machine's agent under the pointer: the cursor goes
                // there, as `j` would take it. No double click — `enter`
                // jumps to a local pane only.
                if let Some((label, id)) = self.drawn_remote.get(row).cloned().flatten() {
                    if let Some(i) = self
                        .remote_agents()
                        .iter()
                        .position(|(m, a)| m.label == label && a.id == id)
                    {
                        self.notice.clear();
                        self.remote_mode = true;
                        self.remote_cursor = i;
                        self.usage_view = false;
                    }
                    return Action::Nothing;
                }
                // The agent that was drawn under the pointer, wherever it sits
                // in the list now — and nothing, if it has gone.
                let Some(i) = self
                    .drawn
                    .get(usize::from(m.row))
                    .cloned()
                    .flatten()
                    .and_then(|id| self.agents.iter().position(|a| a.id == id))
                else {
                    return Action::Nothing;
                };
                self.notice.clear();
                self.cursor = i;
                self.remote_mode = false;
                self.usage_view = false;
                let id = self.agents[i].id.clone();
                let double = armed
                    .is_some_and(|(prev, at)| prev == id && now.duration_since(at) <= DOUBLE_CLICK);
                if double {
                    return self.enter();
                }
                self.last_click = Some((id, now));
                Action::Nothing
            }
            // The wheel moves as `j` and `k` do, into the saved machines and
            // back — through the list, never the usage board's choice, which
            // the arrow keys would make while the board is open.
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                self.usage_view = false;
                let code = if m.kind == MouseEventKind::ScrollDown {
                    KeyCode::Char('j')
                } else {
                    KeyCode::Char('k')
                };
                self.on_key(KeyEvent::new(code, KeyModifiers::NONE))
            }
            _ => Action::Nothing,
        }
    }

    // ── keys ─────────────────────────────────────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // Repeat and Release arrive too on terminals that speak the kitty
        // keyboard protocol, and on Windows consoles. Acting on all three kinds
        // makes every keystroke happen twice, which on `enter` means jumping and
        // then jumping again from a tower that has already quit.
        if key.kind != KeyEventKind::Press {
            return Action::Nothing;
        }
        // A key between two clicks means they were not a double click.
        self.last_click = None;
        // And it lets go of a clicked band: the list follows the cursor again.
        self.band_anchor = None;

        // ctrl+c ends the program from anywhere, including either message field.
        // Left to the field, it would be swallowed — the text input ignores it —
        // and the only way out of the input mode would be to know that esc
        // worked.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }

        if self.composer.is_some() {
            return self.on_input_key(key);
        }

        // A pending confirmation outranks the detail card's esc. Checking the
        // card first would let esc close the card and leave the confirmation
        // armed: the next `y` would broadcast to every session with nothing on
        // screen having asked. (See also `view::footer_lines`, which draws the
        // prompt on every screen.)
        if let Some(p) = self.pending.take() {
            return self.confirm(p, key);
        }

        if matches!(self.screen, Screen::Info(_)) && key.code == KeyCode::Esc {
            // esc means "back" in every other corner of this screen — the card,
            // the input field, a confirmation — so it closes the card rather
            // than quitting the tower.
            self.screen = Screen::List;
            return Action::Nothing;
        }

        // The notice is one-shot: whatever the next key does, the guidance for
        // the last thing is stale. Branches that want to say something set it
        // again below.
        self.notice.clear();

        if self.usage_view() && self.screen == Screen::List {
            if let Some(action) = self.on_usage_key(key) {
                return action;
            }
        }

        if self.remote_mode && self.screen == Screen::List {
            if let Some(action) = self.on_remote_key(key) {
                return action;
            }
        }

        match key.code {
            KeyCode::Char('/') => {
                // A filter in force is the person's own text, reopened to be
                // edited — not a suggestion, so not a hint. Only an unfiltered
                // list gets the dim last filter, or the example.
                let input = if self.filter.is_empty() {
                    TextInput::hinted(FILTER_LIMIT, or_example(&self.hints.filter, FILTER_EXAMPLE))
                } else {
                    let mut input = TextInput::new(FILTER_LIMIT);
                    for c in self.filter.chars() {
                        input.insert(c);
                    }
                    input
                };
                self.composer = Some(Composer::Filter(input));
                Action::Nothing
            }
            // A filtered list is one esc from the whole list, before esc
            // means leaving.
            KeyCode::Esc if !self.filter.is_empty() => {
                self.set_filter(String::new());
                Action::Nothing
            }
            // The dock's tower comes straight back when it ends, so there is
            // nothing to quit to — only a blink of a bare shell.
            KeyCode::Char('q') | KeyCode::Esc if self.settings.kept => {
                self.notice = "the dock keeps the tower — switch tabs to leave it".into();
                Action::Nothing
            }
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                if self.cursor + 1 < self.agents.len() {
                    self.cursor += 1;
                } else if !self.remote_agents().is_empty() {
                    // Past the last local row the cursor goes on into the
                    // saved machines, as herdr's navigator runs on.
                    self.remote_mode = true;
                    self.remote_cursor = 0;
                }
                self.usage_view = false;
                Action::Nothing
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                self.usage_view = false;
                Action::Nothing
            }
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('W') => {
                self.pending = Some(Pending::Wake);
                Action::Nothing
            }
            KeyCode::Char('C') => {
                self.pending = Some(Pending::Close);
                Action::Nothing
            }
            KeyCode::Char('B') => {
                self.composer = Some(Composer::Broadcast(TextInput::new(BROADCAST_LIMIT)));
                Action::Nothing
            }
            KeyCode::Char('L') => self.toggle_loop(),
            KeyCode::Char('T') => self.toggle_schedule(),
            KeyCode::Char('i') => self.toggle_info(),
            KeyCode::Char('g') => self.open_lazygit(),
            KeyCode::Char('o') => self.mark_read_only(),
            KeyCode::Char(']') => self.nudge_preview(true),
            KeyCode::Char('[') => self.nudge_preview(false),
            KeyCode::Char('=') => {
                self.preview_folded = false;
                self.set_preview_rows(None)
            }
            KeyCode::Char('u') if self.usage_frame.is_some() && self.screen == Screen::List => {
                self.usage_view = true;
                Action::Nothing
            }
            KeyCode::Enter => self.enter(),
            _ => Action::Nothing,
        }
    }

    /// `T`: a scheduled message for the selected session — or, if it already
    /// has some, cancelling them, the way `L` turns a loop off.
    ///
    /// The schedule names the session by its conversation, so it finds the
    /// agent wherever its pane has gone by the time it is due — and a pane
    /// whose CLI has not reported one yet cannot have one, as it cannot have a
    /// loop. Schedules for a workspace or for every handoff folder are the
    /// command line's (`herdr-tower schedule`), and `T` leaves them alone.
    fn toggle_schedule(&mut self) -> Action {
        let Some(agent) = self.selected().cloned() else {
            return Action::Nothing;
        };
        let (sid, id) = (agent.session_id.trim().to_string(), agent.id.clone());
        let who = if agent.pane.session.is_empty() {
            agent.id.clone()
        } else {
            agent.pane.session.clone()
        };
        if self.schedule_list.iter().any(|s| s.names(&sid, &id)) {
            match self.schedules.remove_where(|s| s.names(&sid, &id)) {
                Ok(gone) => {
                    self.schedule_list.retain(|s| !s.names(&sid, &id));
                    self.notice = format!(
                        "⏰ cancelled {} for {who}",
                        if gone.len() == 1 {
                            "the scheduled message".to_string()
                        } else {
                            format!("{} scheduled messages", gone.len())
                        }
                    );
                }
                Err(e) => self.err = Some(format!("could not cancel the schedule: {e}")),
            }
            return Action::Nothing;
        }
        // Pane coordinates are reusable: named by its record, the schedule
        // would pass to whatever process herdr next put in that pane.
        if sid.is_empty() {
            self.err = Some(
                "a scheduled message needs a native agent session id; refresh after Herdr detects it"
                    .into(),
            );
            return Action::Nothing;
        }
        let target = ScheduleTarget::Session {
            id: sid,
            origin: id,
        };
        self.composer = Some(Composer::Schedule {
            input: TextInput::hinted(
                SCHEDULE_MESSAGE_LIMIT,
                or_example(&self.hints.schedule, SCHEDULE_EXAMPLE),
            ),
            target,
            who,
            repeat: 0,
        });
        Action::Nothing
    }

    /// Reads `19:00 the message` or `+30m the message` from the field and
    /// keeps the schedule. A line that does not start with a time goes back
    /// into the field, with what is wrong on the error line.
    fn arm_schedule(
        &mut self,
        line: String,
        target: ScheduleTarget,
        who: String,
        repeat: usize,
    ) -> Action {
        // Read afresh: a docked tower runs for days, across a clock change.
        self.utc_offset = local_offset();
        self.utc_offset_read = self.now;
        let now = OffsetDateTime::now_utc().to_offset(self.utc_offset);
        let (time, message) = line
            .trim()
            .split_once(char::is_whitespace)
            .map_or((line.trim(), ""), |(t, m)| (t, m.trim()));
        let parsed = parse_when(time, Repeat::ALL[repeat], now).and_then(|when| {
            if message.is_empty() {
                Err("after the time, the message to send".to_string())
            } else {
                Ok(when)
            }
        });
        let when = match parsed {
            Ok(when) => when,
            Err(e) => {
                self.err = Some(format!("⏰ {e}"));
                self.composer = Some(Composer::Schedule {
                    input: TextInput::prefilled(SCHEDULE_MESSAGE_LIMIT, &line),
                    target,
                    who,
                    repeat,
                });
                return Action::Nothing;
            }
        };
        let message = message.to_string();
        let typed = line.trim().to_string();
        let kept = self
            .schedules
            .add(|taken| Schedule::new(new_schedule_id(now, taken), message, target, when, now));
        // The resident supervisor is what sends it, tower open or not.
        let kept = kept.map_err(|e| e.to_string()).and_then(|s| {
            match self.tower.start_loop_supervisor(&self.settings.exe) {
                Ok(()) => Ok(s),
                Err(e) => {
                    let _ = self.schedules.remove_where(|kept| kept.id == s.id);
                    Err(format!("the resident supervisor did not start: {e}"))
                }
            }
        });
        match kept {
            Ok(s) => {
                // Only the field's own complaint: whatever else is on the
                // error line is not this schedule's to clear.
                if self.err.as_deref().is_some_and(|e| e.starts_with('⏰')) {
                    self.err = None;
                }
                self.notice = format!(
                    "⏰ {} → {who}: {:?} (waits for it to be idle)",
                    s.when_label(self.utc_offset),
                    s.message
                );
                self.schedule_list.push(s);
                // Kept only once it is a schedule: a line that failed is not
                // one worth offering again.
                self.hints.schedule = typed;
                self.save_hints();
            }
            Err(e) => self.err = Some(format!("could not schedule the message: {e}")),
        }
        Action::Nothing
    }

    fn toggle_loop(&mut self) -> Action {
        let Some(agent) = self.selected().cloned() else {
            return Action::Nothing;
        };
        if let Some(old) = self.loop_leases.remove(&agent.id) {
            match self.loops.disarm(&agent.id) {
                Ok(_) => {
                    self.tower
                        .clear_loop_display(&tower_mux::target_key(&old.backend, &old.pane_id));
                    self.notice = format!("idle loop off: {}", agent.pane.session);
                }
                Err(e) => self.err = Some(format!("could not stop idle loop: {e}")),
            }
            return Action::Nothing;
        }
        // Pane coordinates are reusable. Without the agent's native session
        // identity a lease could silently attach itself to a different process
        // that later inherited the same coordinate.
        if agent.session_id.is_empty() {
            self.err = Some(
                "idle loop needs a native agent session id; refresh after Herdr detects it".into(),
            );
            return Action::Nothing;
        }

        let message = match self
            .loops
            .remembered_message(&agent.pane.backend, &agent.session_id)
        {
            Ok(Some(message)) => message,
            Ok(None) => DEFAULT_LOOP_MESSAGE.into(),
            Err(error) => {
                self.err = Some(format!(
                    "could not restore the previous loop message: {error}"
                ));
                DEFAULT_LOOP_MESSAGE.into()
            }
        };
        // The message is a hint, not text: `→` takes it, typing replaces it,
        // and `enter` over it alone arms nothing. Pre-filled as real text, a
        // different message meant deleting the whole default first.
        self.composer = Some(Composer::Loop {
            input: TextInput::hinted(LOOP_MESSAGE_LIMIT, &message),
            agent_id: agent.id,
            agent_session_id: agent.session_id,
            idle_index: DEFAULT_LOOP_IDLE_INDEX,
        });
        Action::Nothing
    }

    fn arm_loop(
        &mut self,
        agent_id: &str,
        agent_session_id: &str,
        idle_for: Duration,
        message: String,
    ) -> Action {
        let Some(agent) = self.agents.iter().find(|a| a.id == agent_id).cloned() else {
            self.err = Some("could not start idle loop: the selected session disappeared".into());
            return Action::Nothing;
        };
        if agent.session_id.is_empty() || agent.session_id != agent_session_id {
            self.err =
                Some("could not start idle loop: the selected session identity changed".into());
            return Action::Nothing;
        }

        let mut lease = LoopLease::new(
            &agent,
            OffsetDateTime::now_utc(),
            idle_for,
            DEFAULT_LOOP_TTL,
            None,
            String::new(),
        );
        lease.message = if message.trim().is_empty() {
            DEFAULT_LOOP_MESSAGE.into()
        } else {
            message.trim().into()
        };
        // The pane's `loop_control` token is cleared *before* the lease is
        // armed, as `loop start` does (#129). It has no TTL and outlives the
        // session that set it, and the supervisor stops a lease the moment it
        // reads `done`: armed first, a `done` left over from earlier stopped
        // the new loop on the next pass as "agent reported completion".
        self.tower.prepare_loop(&tower_mux::target_key(
            &agent.pane.backend,
            &agent.pane.pane_id,
        ));
        match self.loops.arm(lease.clone()) {
            Ok(()) => {
                if let Err(e) = self.tower.start_loop_supervisor(&self.settings.exe) {
                    let _ = self.loops.disarm(&agent.id);
                    self.err = Some(format!("could not start idle loop: {e}"));
                } else {
                    self.loop_leases.insert(agent.id.clone(), lease);
                    self.notice = format!(
                        "idle loop on: {} · {} idle · 168h · unlimited",
                        agent.pane.session,
                        idle_label(idle_for)
                    );
                }
            }
            Err(e) => self.err = Some(format!("could not arm idle loop: {e}")),
        }
        Action::Nothing
    }

    fn on_input_key(&mut self, key: KeyEvent) -> Action {
        let Some(mut composer) = self.composer.take() else {
            return Action::Nothing;
        };
        let filtering = matches!(composer, Composer::Filter(_));
        // One-shot here too: the "→ to take the one shown" an empty `enter`
        // left goes with the next key.
        self.notice.clear();
        match key.code {
            // Esc in the filter field takes the filter away with it — but it
            // was used, so it is the one offered next time.
            KeyCode::Esc if filtering => {
                self.remember_filter(&composer.input().value());
                self.set_filter(String::new());
                return Action::Nothing;
            }
            KeyCode::Esc => return Action::Nothing,
            // An empty field showing only its hint: nothing to arm. The field
            // stays open and says how to take the hint.
            KeyCode::Enter
                if matches!(
                    composer,
                    Composer::Loop { .. }
                        | Composer::Schedule { .. }
                        | Composer::RemoteSchedule { .. }
                ) && composer.input().value().trim().is_empty() =>
            {
                self.notice = if matches!(composer, Composer::Loop { .. }) {
                    LOOP_EMPTY
                } else {
                    SCHEDULE_EMPTY
                }
                .into();
            }
            KeyCode::Enter => {
                return match composer {
                    // The field closes and the list stays filtered.
                    Composer::Filter(input) => {
                        self.remember_filter(&input.value());
                        Action::Nothing
                    }
                    Composer::RemoteSend { input, label, id } => {
                        // The message stays typed while another action is out.
                        if self.refuse_while_busy() {
                            self.composer = Some(Composer::RemoteSend { input, label, id });
                            return Action::Nothing;
                        }
                        self.remote_do(&label, &id, crate::RemoteAction::Send(input.value()))
                    }
                    Composer::Broadcast(input) => {
                        let text = input.value().trim().to_string();
                        if !text.is_empty() {
                            self.pending = Some(Pending::Broadcast(text));
                        }
                        Action::Nothing
                    }
                    Composer::Loop {
                        input,
                        agent_id,
                        agent_session_id,
                        idle_index,
                    } => self.arm_loop(
                        &agent_id,
                        &agent_session_id,
                        LOOP_IDLE_PRESETS[idle_index],
                        input.value(),
                    ),
                    Composer::Schedule {
                        input,
                        target,
                        who,
                        repeat,
                    } => self.arm_schedule(input.value(), target, who, repeat),
                    Composer::RemoteSchedule {
                        input,
                        label,
                        id,
                        repeat,
                    } => self.arm_remote_schedule(input.value(), label, id, repeat),
                };
            }
            KeyCode::BackTab => composer.cycle(true),
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                composer.cycle(true);
            }
            KeyCode::Tab => composer.cycle(false),
            KeyCode::Backspace => composer.input_mut().backspace(),
            KeyCode::Delete => composer.input_mut().delete(),
            KeyCode::Left => composer.input_mut().left(),
            KeyCode::Right => composer.input_mut().right(),
            KeyCode::Home => composer.input_mut().home(),
            KeyCode::End => composer.input_mut().end(),
            KeyCode::Char(c) => composer.input_mut().insert(c),
            _ => {}
        }
        if filtering {
            self.set_filter(composer.input().value());
        }
        self.composer = Some(composer);
        Action::Nothing
    }

    /// Keeps `filter` as the one `/` offers next, if there is one.
    fn remember_filter(&mut self, filter: &str) {
        let filter = filter.trim();
        if !filter.is_empty() && filter != self.hints.filter {
            self.hints.filter = filter.to_string();
            self.save_hints();
        }
    }

    /// Writes the hints through. A failure is dropped: what it costs is an
    /// example where the last value would have been, not worth an error line.
    fn save_hints(&self) {
        let _ = self.hint_store.write(&self.hints);
    }

    /// `y` goes ahead; anything else cancels.
    ///
    /// Cancel-on-anything-else is the safety property, and it is why the
    /// confirmation is worth having at all. `Y` counts as consent: treating a
    /// shift slip as a cancellation only teaches people to hit the key twice,
    /// which is the habit the prompt exists to prevent. `ㅛ` counts too: it is
    /// the `y` key under a Korean input method, which is still on right after
    /// a broadcast typed in Korean — read as "anything else", it cancelled the
    /// message the user had just said yes to.
    ///
    /// Either answer leaves a receipt that says whether anything was sent: a
    /// bare "cancelled" did not say what, and was easy to miss.
    fn confirm(&mut self, p: Pending, key: KeyEvent) -> Action {
        let yes = matches!(key.code, KeyCode::Char('y' | 'Y' | 'ㅛ'));
        if !yes {
            self.notice = match p {
                Pending::Broadcast(text) => {
                    format!("✖ not sent — broadcast of {} cancelled", shown(&text))
                }
                Pending::Wake => "✖ not sent — wake cancelled".into(),
                Pending::Close => "✖ not sent — close cancelled".into(),
                Pending::RemoteInterrupt { .. } => "✖ cancelled — Escape not pressed".into(),
            };
            return Action::Nothing;
        }
        match p {
            Pending::Wake => self.send_all(WAKE_MESSAGE, true, true),
            Pending::Close => {
                let old: Vec<_> = self.loop_leases.values().cloned().collect();
                match self.loops.replace(&[]) {
                    Ok(()) => {
                        self.loop_leases.clear();
                        for lease in old {
                            self.tower.clear_loop_display(&tower_mux::target_key(
                                &lease.backend,
                                &lease.pane_id,
                            ));
                        }
                        self.send_all(CLOSE_MESSAGE, true, true)
                    }
                    Err(e) => {
                        self.err = Some(format!(
                            "close cancelled because idle loops could not be stopped: {e}"
                        ));
                        Action::Nothing
                    }
                }
            }
            Pending::Broadcast(text) => self.send_all(&text, false, false),
            Pending::RemoteInterrupt { label, id } => {
                self.remote_do(&label, &id, crate::RemoteAction::Interrupt)
            }
        }
    }

    /// Writes `message` into every target pane.
    ///
    /// `handoff_only` restricts it to folders that have the session-handoff
    /// discipline — wake and close are instructions about that discipline, and
    /// an arbitrary broadcast is not.
    ///
    /// The targets come from the list on screen rather than from a fresh store
    /// read. That way `2/2` means "the two rows you are looking at", not two
    /// records the user was never shown. The whole list, not what `/` left of
    /// it: the confirmation says "every session", and a filter typed a minute
    /// ago must not quietly make that a few.
    fn send_all(&mut self, message: &str, handoff_only: bool, watchable: bool) -> Action {
        let self_pane = self.tower.self_pane();
        // The record id rides along so a dispatch can clear the read
        // acknowledgement — see `tower_state::Agent::done_acked`.
        let mut chosen: Vec<(String, String, String)> =
            targets(&self.all_agents, self_pane.as_deref())
                .into_iter()
                .filter(|a| !handoff_only || has_session_handoff(&a.cwd))
                .map(|a| {
                    (
                        tower_mux::target_key(&a.pane.backend, &a.pane.pane_id),
                        a.pane.session.clone(),
                        a.id.clone(),
                    )
                })
                .collect();
        chosen.dedup_by(|a, b| a.0 == b.0);

        let total = chosen.len();
        let mut sent = 0usize;
        let mut failed: Vec<String> = Vec::new();
        let now = OffsetDateTime::now_utc();
        for (pane, session, id) in &chosen {
            match self.tower.send(pane, message) {
                Ok(()) => {
                    // New work, so a completion the user already read is no
                    // longer the current one.
                    self.store.note_dispatch(id, now);
                    sent += 1;
                }
                Err(_) => failed.push(session.clone()),
            }
        }
        self.err = None;
        // The mark is the answer at a glance: all of them, some, or none.
        let mark = if sent == 0 {
            "✖"
        } else if sent < total {
            "⚠"
        } else {
            "✔"
        };
        self.notice = if total == 0 {
            format!("✖ not sent — no session to send {} to", shown(message))
        } else if watchable && sent > 0 {
            format!(
                "{mark} {} sent to {sent}/{total} sessions — their state changes show up here live",
                shown(message)
            )
        } else {
            format!("{mark} {} sent to {sent}/{total} sessions", shown(message))
        };
        if !failed.is_empty() {
            // The names are kept, not just counted: a broadcast that reached
            // four of five sessions has to say which one it missed — and the
            // one it missed is usually the one sitting at an approval dialog,
            // which herdr refuses to write to on purpose.
            self.err = Some(format!("delivery failed: {}", failed.join(", ")));
        }
        Action::Refresh
    }

    fn enter(&mut self) -> Action {
        let Some(a) = self.selected().cloned() else {
            return Action::Nothing;
        };
        // Read *after* the navigation succeeds, not before. Marking it first
        // meant that a failed `attach_argv` or `focus` still cleared the unread
        // marker: the user never reached the pane, never saw the result, and lost
        // both the marker and any future notification for that completion.
        let seen = |app: &mut Self| {
            let _ = app.store.mark_read(&a.id, OffsetDateTime::now_utc());
        };
        let target = tower_mux::target_key(&a.pane.backend, &a.pane.pane_id);
        if !self.inside {
            return match self.tower.attach_argv(&target) {
                // Carried, not done here: `attach_argv` only *builds* the command,
                // and the driver is what learns whether it ran.
                Ok(argv) => Action::Run {
                    argv,
                    focus: Some(target),
                    seen: Some(a.id.clone()),
                },
                Err(e) => {
                    self.err = Some(e);
                    Action::Refresh
                }
            };
        }
        match self.tower.focus(&target) {
            Ok(()) if self.docked => {
                seen(self);
                Action::Refresh
            }
            Ok(()) => {
                seen(self);
                Action::Quit
            }
            Err(e) => {
                self.err = Some(e);
                Action::Refresh
            }
        }
    }

    /// Everything that waits until an attach child is actually running.
    ///
    /// The driver calls this from [`Action::Run`]'s `started` callback, and
    /// nowhere else: every other read and every other focus is a key press. Both
    /// halves are held back for the same reason — a command that cannot start
    /// never reached the pane — and this is the order they go in.
    ///
    /// The selection first, because it is the part that can fail and the part
    /// that moves *other people's* screens: focus belongs to the session, so
    /// every client attached to it follows. Sending it while the command was
    /// still being built moved a desk's view for an attach that might never
    /// happen. herdr has no client-scoped selection to do better than that —
    /// `pane.focus` takes a pane id alone — so what is left is not doing it for
    /// nothing.
    ///
    /// Then the completion, and only if the selection landed: the unread marker
    /// is the only thing that would bring somebody back to a result they never
    /// saw.
    ///
    /// A returned message is the whole sentence the person reads, because what it
    /// describes is the attach being over: the driver ends a child that cannot
    /// reach what it was started for, rather than leaving somebody attached to a
    /// pane they did not ask for with the reason stuck behind it.
    pub fn attached(&mut self, focus: Option<&str>, seen: Option<&str>) -> Option<String> {
        if let Some(pane) = focus {
            if let Err(e) = self.tower.focus(pane) {
                return Some(format!(
                    "could not reach {pane}, so the attach was cancelled: {e}"
                ));
            }
        }
        if let Some(id) = seen {
            let _ = self.store.mark_read(id, OffsetDateTime::now_utc());
        }
        None
    }

    fn mark_read_only(&mut self) -> Action {
        let Some(a) = self.selected() else {
            return Action::Nothing;
        };
        let _ = self.store.mark_read(&a.id, OffsetDateTime::now_utc());
        Action::Refresh
    }

    fn toggle_info(&mut self) -> Action {
        if matches!(self.screen, Screen::Info(_)) {
            self.screen = Screen::List;
            return Action::Nothing;
        }
        let Some(a) = self.selected() else {
            return Action::Nothing;
        };
        let wiring = tower_wiring::collect(
            &self.settings.wiring,
            &a.cwd,
            &a.pane.session,
            &self.settings.channel_labels,
        );
        let card = render_info(
            &InfoData {
                agent: a,
                wiring: &wiring,
                ctx: self.local.ctx.get(&a.id),
                branch: self.branches.get(&a.cwd).map(String::as_str),
                labels: &self.settings.channel_labels,
                unit: crate::info::unit_word(),
            },
            self.now,
        );
        self.screen = Screen::Info(card);
        Action::Nothing
    }

    /// Hands the folder to `lazygit`, which owns every git gesture the tower
    /// would otherwise have to reimplement badly.
    fn open_lazygit(&mut self) -> Action {
        let Some(a) = self.selected() else {
            return Action::Nothing;
        };
        if a.cwd.is_empty() {
            return Action::Nothing;
        }
        let (cwd, session) = (a.cwd.clone(), a.pane.session.clone());
        let Some(bin) = self.settings.tools.lookup("lazygit") else {
            self.err = Some("lazygit is not installed (brew install lazygit)".into());
            return Action::Nothing;
        };
        if !self.tower.is_git_repo(&cwd) {
            self.err = Some(format!(
                "{session} is not a git repository — g works in worktree tasks and repo folders only"
            ));
            return Action::Nothing;
        }
        self.err = None;
        Action::Run {
            argv: vec![bin.to_string_lossy().into_owned(), "-p".into(), cwd],
            focus: None,
            seen: None,
        }
    }
}

/// The coordinator each worker works under, by the worker's id, written as
/// the worker's row marks it. Its direct coordinator, not the top of the
/// chain: that is the pane it reports to. A coordinator that is not a live
/// agent here (a map a pass behind) marks nothing.
fn coordinators(agents: &[Agent], parents: &BTreeMap<String, String>) -> HashMap<String, String> {
    let by_key: HashMap<String, &Agent> = agents
        .iter()
        .map(|a| (tower_mux::target_key(&a.pane.backend, &a.pane.pane_id), a))
        .collect();
    agents
        .iter()
        .filter_map(|a| {
            let up = parents
                .get(&tower_mux::target_key(&a.pane.backend, &a.pane.pane_id))
                .and_then(|k| by_key.get(k))
                .filter(|up| up.id != a.id)?;
            let pane = up.pane.pane_id.as_str();
            let mark = match pane.split_once(':') {
                Some((_, p)) if tower_state::group_key(up) == tower_state::group_key(a) => p,
                _ => pane,
            };
            Some((a.id.clone(), mark.to_string()))
        })
        .collect()
}

fn idle_label(duration: Duration) -> String {
    format!("{}m", duration.as_secs() / 60)
}

/// Orders the list as herdr's sidebar does: workspace, tab, then pane in
/// herdr's own count ([`herdrx::pane_order`]). Workspaces follow Herdr's
/// sidebar order and tabs their number in it.
///
/// Nothing else has a say. The list used to bring the tab with the most
/// urgent agent up, order a tab's agents by state, put a tab's layout root
/// first and draw a worker in its coordinator's tab, so a row moved whenever
/// an agent started or finished a turn or a link changed, and a pane was in
/// a different place than in the sidebar. What needs answering is found from
/// the header's counts and the state badges, and who works under whom from
/// the worker's `↳` mark ([`App::coordinator_of`]).
///
/// The workspace ID is only a tie-breaker for two distinct workspaces carrying
/// the same label.
fn order_tower_agents(agents: &mut [Agent], workspace_order: &[String]) {
    let workspace_rank: HashMap<_, _> = workspace_order
        .iter()
        .enumerate()
        .map(|(rank, id)| (id.as_str(), rank))
        .collect();
    let rank = |a: &Agent| {
        workspace_rank
            .get(a.pane.workspace_id.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    };
    agents.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.pane.session.cmp(&b.pane.session))
            .then_with(|| a.pane.workspace_id.cmp(&b.pane.workspace_id))
            .then_with(|| a.pane.window.cmp(&b.pane.window))
            .then_with(|| {
                herdrx::pane_order(&a.pane.pane_id).cmp(&herdrx::pane_order(&b.pane.pane_id))
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// The machines just read, with each one that stopped answering keeping the
/// agents it had last time, marked stale since the first read that failed —
/// herdr's navigator keeps a lost endpoint's snapshot the same way. A machine
/// that answers again is live again.
fn keep_stale(
    old: &[crate::RemoteMachine],
    new: Vec<crate::RemoteMachine>,
    now: OffsetDateTime,
) -> Vec<crate::RemoteMachine> {
    new.into_iter()
        .map(|mut m| {
            if m.error.is_some() {
                if let Some(before) = old
                    .iter()
                    .find(|o| o.label == m.label && !(o.agents.is_empty() && o.unseen.is_empty()))
                {
                    m.agents = before.agents.clone();
                    m.unseen = before.unseen.clone();
                    m.stale_since = before.stale_since.or(Some(now));
                    m.system = m.system.take().or_else(|| before.system.clone());
                }
            }
            m
        })
        .collect()
}
