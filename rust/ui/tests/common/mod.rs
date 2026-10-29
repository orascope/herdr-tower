// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Fixtures: a store in a temp directory, a backend that does nothing but
//! remember what it was asked to do, and a tower built on both.
//!
//! Nothing here touches a terminal, a socket, or the developer's home
//! directory. That is the whole point of [`tower_ui::Tower`] being a trait — see its
//! doc for why `tower_mux` reached the opposite conclusion about its own backend.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use time::{Duration, OffsetDateTime};
use tower_state::{Agent, AgentState, PaneRef, Store};
use tower_ui::{App, Event, ResumePaths, Settings, Tower};

/// A fixed clock, well clear of the epoch — `last_hook` uses the epoch itself to
/// mean "never", so a fixture starting there reads as a record nothing ever
/// touched.
pub fn at(mins: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::minutes(mins)
}

pub fn t0() -> OffsetDateTime {
    at(0)
}

/// A record with the fields a row actually draws.
pub fn agent(id: &str, kind: &str, session: &str, state: AgentState) -> Agent {
    let mut a = Agent::new(id, kind, t0());
    a.state = state;
    a.state_since = t0();
    a.updated_at = t0();
    a.pane = PaneRef {
        backend: String::new(),
        session: session.into(),
        window: 0,
        pane_id: format!("p-{id}"),
        terminal_id: String::new(),
        tab: String::new(),
        // Left empty on purpose: these tests exercise the label fallback, which
        // is what every record written before the id existed still uses.
        workspace_id: String::new(),
    };
    a
}

/// A backend that records instead of doing.
pub struct StubTower {
    pub inside: bool,
    pub self_pane: Option<String>,
    pub focus_fails: bool,
    pub git: bool,
    pub capture: Option<String>,
    pub prs: HashMap<String, u64>,
    pub tab_roots: Option<HashSet<String>>,
    /// Panes that refuse a write — herdr does this for an agent sitting at an
    /// approval dialog.
    pub send_fails: Vec<String>,
    /// A clipboard that refuses every copy — no tool installed and no
    /// terminal to take the escape.
    pub copy_fails: bool,

    pub sent: Mutex<Vec<(String, String)>>,
    pub focused: Mutex<Vec<String>>,
    pub loop_starts: Mutex<Vec<String>>,
    /// Where the loop leases live, when a test wants `prepare_loop` to look.
    pub loop_dir: Mutex<Option<std::path::PathBuf>>,
    /// For each `prepare_loop`: whether a lease was already armed then.
    pub prepared_armed: Mutex<Vec<bool>>,
    pub copied: Mutex<Vec<String>>,
    /// Every copy asked for, refused or not — what a refused copy leaves no
    /// other trace of.
    pub copy_tries: Mutex<Vec<String>>,
    /// Every action done to an agent on a saved machine: `label`, agent id,
    /// what.
    pub remote_acts: Mutex<Vec<(String, String, tower_ui::RemoteAction)>>,
    /// A refusal for every remote action, when a test wants one.
    pub remote_fails: Option<String>,
}

impl Default for StubTower {
    fn default() -> Self {
        StubTower {
            inside: true,
            self_pane: None,
            focus_fails: false,
            git: true,
            capture: None,
            prs: HashMap::new(),
            tab_roots: None,
            send_fails: Vec::new(),
            copy_fails: false,
            sent: Mutex::new(Vec::new()),
            focused: Mutex::new(Vec::new()),
            loop_starts: Mutex::new(Vec::new()),
            loop_dir: Mutex::new(None),
            prepared_armed: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
            copy_tries: Mutex::new(Vec::new()),
            remote_acts: Mutex::new(Vec::new()),
            remote_fails: None,
        }
    }
}

impl StubTower {
    pub fn sent(&self) -> Vec<(String, String)> {
        self.sent.lock().unwrap().clone()
    }

    pub fn focused(&self) -> Vec<String> {
        self.focused.lock().unwrap().clone()
    }
}

impl Tower for StubTower {
    fn inside(&self) -> bool {
        self.inside
    }

    fn remote_act(
        &self,
        machine: &tower_ui::RemoteMachine,
        agent: &tower_ui::RemoteAgent,
        action: &tower_ui::RemoteAction,
    ) -> Result<String, String> {
        self.remote_acts.lock().unwrap().push((
            machine.label.clone(),
            agent.id.clone(),
            action.clone(),
        ));
        match &self.remote_fails {
            Some(e) => Err(e.clone()),
            None => Ok(format!("did {action:?}")),
        }
    }

    fn self_pane(&self) -> Option<String> {
        self.self_pane.clone()
    }

    fn focus(&self, pane_id: &str) -> Result<(), String> {
        self.focused.lock().unwrap().push(pane_id.to_string());
        if self.focus_fails {
            return Err("focus refused".into());
        }
        Ok(())
    }

    fn attach_argv(&self, _target: &str) -> Result<Vec<String>, String> {
        Ok(vec!["herdr".into(), "attach".into()])
    }

    fn capture_pane(&self, _pane_id: &str, _lines: u16) -> Option<String> {
        self.capture.clone()
    }

    fn send(&self, pane_id: &str, text: &str) -> Result<(), String> {
        if self.send_fails.iter().any(|p| p == pane_id) {
            return Err("agent_blocked".into());
        }
        self.sent
            .lock()
            .unwrap()
            .push((pane_id.to_string(), text.to_string()));
        Ok(())
    }

    fn prepare_loop(&self, _pane_id: &str) {
        if let Some(dir) = self.loop_dir.lock().unwrap().as_ref() {
            let armed = !tower_state::LoopStore::new(dir)
                .unwrap()
                .list()
                .unwrap()
                .is_empty();
            self.prepared_armed.lock().unwrap().push(armed);
        }
    }

    fn clear_loop_display(&self, _pane_id: &str) {}

    fn start_loop_supervisor(&self, exe: &str) -> Result<(), String> {
        self.loop_starts.lock().unwrap().push(exe.into());
        Ok(())
    }

    fn copy(&self, text: &str) -> Result<(), String> {
        self.copy_tries.lock().unwrap().push(text.into());
        if self.copy_fails {
            return Err("no clipboard".into());
        }
        self.copied.lock().unwrap().push(text.into());
        Ok(())
    }

    fn is_git_repo(&self, _dir: &str) -> bool {
        self.git
    }

    fn open_prs(&self, _folders: &[String]) -> HashMap<String, u64> {
        self.prs.clone()
    }

    fn checked_out_branches(&self, _folders: &[String]) -> HashMap<String, String> {
        HashMap::new()
    }

    fn hold_dock(&self) {}
}

/// A tower with its store and its temp directories kept alive alongside it.
pub struct Fixture {
    pub app: App,
    pub tower: Arc<StubTower>,
    pub store: Store,
    _dirs: Vec<tempfile::TempDir>,
}

impl Fixture {
    /// The fixture's home, where [`Settings::tools`] finds `~/.local/bin`.
    pub fn home(&self) -> &std::path::Path {
        self._dirs[1].path()
    }

    /// Puts an executable `name` in the home's `~/.local/bin`, where the
    /// fixture's tool search finds it.
    pub fn install_tool(&self, name: &str) {
        let bin = self.home().join(".local").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // On Windows a tool is found by its extension, not a mode bit.
        let file = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        let path = bin.join(file);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        tower_os::set_mode(&path, 0o755).unwrap();
    }

    /// Presses a key by name: a single character, or `enter`/`esc`/`left` and
    /// friends. Sentences read better than `KeyEvent::new(KeyCode::Char('j'),
    /// KeyModifiers::NONE)` fourteen times over.
    pub fn press(&mut self, key: &str) -> tower_ui::Action {
        self.app.on_event(Event::Key(key_event(key)))
    }

    /// Presses a key whose outcome this test does not examine.
    ///
    /// A remote action is carried out at once, the way the driver's thread
    /// would, and its answer delivered — so a test reads the screen after the
    /// machine answered.
    pub fn tap(&mut self, key: &str) {
        if let tower_ui::Action::Remote(request) = self.press(key) {
            let done = request.run(self.tower.as_ref());
            self.feed(done);
        }
    }

    /// Delivers an event whose outcome this test does not examine.
    pub fn feed(&mut self, ev: Event) {
        let _ = self.app.on_event(ev);
    }

    /// Replaces the record list the way a store re-read would.
    pub fn records(&mut self, agents: Vec<Agent>) {
        self.records_with_roots(agents, HashSet::new());
    }

    pub fn records_with_roots(&mut self, agents: Vec<Agent>, tab_roots: HashSet<String>) {
        self.records_with_tree(agents, tab_roots, Default::default());
    }

    /// Records with the supervisor's worker → coordinator map as well.
    pub fn records_with_tree(
        &mut self,
        agents: Vec<Agent>,
        tab_roots: HashSet<String>,
        parents: std::collections::BTreeMap<String, String>,
    ) {
        for a in &agents {
            self.store.save(a).unwrap();
        }
        let _ = self.app.on_event(Event::Records {
            agents,
            branches: HashMap::new(),
            tab_roots,
            parents,
            daemon: None,
            now: t0(),
        });
    }
}

pub fn key_event(key: &str) -> crossterm::event::KeyEvent {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (code, mods) = match key {
        "enter" => (KeyCode::Enter, KeyModifiers::NONE),
        "esc" => (KeyCode::Esc, KeyModifiers::NONE),
        "backspace" => (KeyCode::Backspace, KeyModifiers::NONE),
        "delete" => (KeyCode::Delete, KeyModifiers::NONE),
        "left" => (KeyCode::Left, KeyModifiers::NONE),
        "right" => (KeyCode::Right, KeyModifiers::NONE),
        "home" => (KeyCode::Home, KeyModifiers::NONE),
        "end" => (KeyCode::End, KeyModifiers::NONE),
        "up" => (KeyCode::Up, KeyModifiers::NONE),
        "down" => (KeyCode::Down, KeyModifiers::NONE),
        "tab" => (KeyCode::Tab, KeyModifiers::NONE),
        "backtab" => (KeyCode::BackTab, KeyModifiers::SHIFT),
        "ctrl+c" => (KeyCode::Char('c'), KeyModifiers::CONTROL),
        other => {
            let mut chars = other.chars();
            let c = chars.next().expect("a key name");
            assert!(chars.next().is_none(), "one character, or a named key");
            let m = if c.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            (KeyCode::Char(c), m)
        }
    };
    KeyEvent::new(code, mods)
}

/// Settings pointed at throwaway directories.
pub fn settings(home: &std::path::Path) -> Settings {
    Settings {
        version: "v-test".into(),
        preview_interval: std::time::Duration::from_secs(1),
        channel_labels: HashMap::new(),
        exe: "herdr-tower".into(),
        resume: ResumePaths::under(home),
        // Empty: no `bots.json`, no unit directories, so nothing is ever found
        // wired. Tests that care set the map on the app instead.
        wiring: tower_wiring::Paths::default(),
        // Nothing found unless a test puts it in the temp home: a `lazygit` in
        // this machine's /opt/homebrew/bin must not change what a key does.
        tools: tower_usage::ToolSearch::new(Some(home.to_path_buf()), "").without_system_dirs(),
        home: Some(home.to_path_buf()),
        kept: false,
        preview_rows: None,
    }
}

/// Two agents: one waiting on approval, one with an unread completion.
pub fn fixture() -> Fixture {
    let store_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::new(store_dir.path()).unwrap();

    let mut waiting = agent("claude-7", "claude", "collab-bot", AgentState::Waiting);
    waiting.task = "awaiting approval".into();
    let mut done = agent("claude-3", "claude", "z-ai", AgentState::DoneUnread);
    done.task = "check the handoff doc".into();

    let tower = Arc::new(StubTower::default());
    let app = App::new(store.clone(), tower.clone(), settings(home.path()));
    let mut f = Fixture {
        app,
        tower,
        store,
        _dirs: vec![store_dir, home],
    };
    f.app.set_size(120, 40);
    f.records(vec![waiting, done]);
    f
}

/// [`fixture`], as the dock's tower: the one the supervisor starts again.
pub fn kept_fixture() -> Fixture {
    let mut f = fixture();
    let home = tempfile::tempdir().unwrap();
    let mut s = settings(home.path());
    s.kept = true;
    f.app = App::new(f.store.clone(), f.tower.clone(), s);
    f.app.set_size(120, 40);
    f._dirs.push(home);
    f
}

/// A fixture whose backend is outside the tower's own terminal.
pub fn fixture_with(tower: StubTower) -> Fixture {
    let store_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::new(store_dir.path()).unwrap();
    let tower = Arc::new(tower);
    let app = App::new(store.clone(), tower.clone(), settings(home.path()));
    let mut f = Fixture {
        app,
        tower,
        store,
        _dirs: vec![store_dir, home],
    };
    f.app.set_size(120, 40);
    f
}

/// A folder that carries the session-handoff discipline.
pub fn handoff_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("SESSION.md"), b"# handoff").unwrap();
    d
}

/// Writes `parallax.json` the way the supervisor would, taken at `t0()`, so
/// the next records event shows these handoffs.
pub fn parallax_snapshot(store: &Store, handoffs: Vec<tower_state::ParallaxHandoff>) {
    tower_state::ParallaxStore::new(store.dir())
        .write(&tower_state::ParallaxSnapshot {
            updated_at: t0(),
            reachable: true,
            workspaces: 1,
            handoffs,
        })
        .unwrap();
}

/// A Parallax handoff with the fields the tower draws.
pub fn parallax_handoff(
    id: &str,
    workspace: &str,
    status: &str,
    attention: tower_state::Attention,
    title: &str,
) -> tower_state::ParallaxHandoff {
    tower_state::ParallaxHandoff {
        id: id.into(),
        workspace: workspace.into(),
        status: status.into(),
        attention,
        title: title.into(),
        created_at: None,
        updated_at: None,
    }
}
