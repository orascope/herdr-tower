// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Workspace names and only the first tab carry `[wX]` automatically.
//! The HERDR-TOWER dock workspace and its tab keep their plain names.
//! The resident supervisor checks each server every ten seconds and writes
//! only differences, so user renames and tab moves converge without idle
//! rename traffic. Pane IDs remain controlled by `id_labels`.
//! Herdr's typed reader keeps semantic names separate from raw display names,
//! so dock/usage keepers and creation requests do not fight these prefixes.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use herdrx::{Herdr, Pane, Tab, Workspace};

/// How often each server's names are reconciled. Names drift when a person
/// renames something or a pane moves, which is rare; a pass every two
/// seconds asking for every tab would be the cost of nothing.
pub const INTERVAL: Duration = Duration::from_secs(10);

/// One name to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rename {
    Workspace {
        workspace_id: String,
        label: String,
    },
    Tab {
        tab_id: String,
        label: String,
    },
    /// `None` takes the pane's name away: an exempt pane whose name was only
    /// an ID prefix.
    Pane {
        pane_id: String,
        label: Option<String>,
    },
}

/// Tabs whose panes keep plain names: dock, usage, and a dock tab being made.
/// Known by their semantic names under a legacy ID prefix, or by
/// the dock's `dock-active`
/// claim and the usage board's token on a pane in them — the same marks the
/// dock and the usage tab are found by everywhere else.
pub fn exempt_tabs<'a>(tabs: &'a [Tab], panes: &'a [Pane]) -> HashSet<&'a str> {
    let by_name = tabs
        .iter()
        .filter(|tab| {
            herdrx::strip_id_prefix(&tab.label) == crate::dockcmd::TAB_LABEL
                || crate::usagetab::is_usage_label(&tab.label)
                // A dock tab being made is found again by its semantic request
                // label after a lost reply. Its pane is exempt as well.
                || crate::dockcmd::tab_request_id(&tab.label).is_some()
        })
        .map(|tab| tab.tab_id.as_str());
    let by_mark = panes
        .iter()
        .filter(|pane| {
            pane.tokens.contains_key(crate::dockcmd::ACTIVE_TOKEN)
                || crate::usagetab::is_usage_pane(pane)
        })
        .map(|pane| pane.tab_id.as_str());
    by_name
        .chain(by_mark)
        .filter(|tab_id| !tab_id.is_empty())
        .collect()
}

/// Desired display names, computed without I/O. Herdr's tab.list is in visual
/// order; number is a stable identity, so the first listed tab gets the prefix.
pub fn plan(workspaces: &[Workspace], tabs: &[Tab], panes: &[Pane], pane_ids: bool) -> Vec<Rename> {
    let mut out = Vec::new();
    out.extend(workspaces.iter().filter_map(|ws| {
        if !herdrx::is_workspace_id(&ws.workspace_id) {
            return None;
        }
        let plain = herdrx::plain_display_label(&ws.display_label);
        let want = if plain == crate::dockcmd::LABEL {
            plain.to_string()
        } else {
            herdrx::workspace_id_label(&ws.workspace_id, &ws.display_label)
        };
        (want != ws.display_label).then(|| Rename::Workspace {
            workspace_id: ws.workspace_id.clone(),
            label: want,
        })
    }));
    let mut first: HashMap<&str, usize> = HashMap::new();
    for (i, tab) in tabs.iter().enumerate() {
        if !herdrx::is_herdr_id(&tab.tab_id) || !herdrx::is_workspace_id(&tab.workspace_id) {
            continue;
        }
        first.entry(&tab.workspace_id).or_insert(i);
    }
    out.extend(tabs.iter().enumerate().filter_map(|(i, tab)| {
        let first_index = first.get(tab.workspace_id.as_str())?;
        if !herdrx::is_herdr_id(&tab.tab_id) {
            return None;
        }
        let plain = herdrx::plain_display_label(&tab.display_label);
        let want = if plain == crate::dockcmd::TAB_LABEL || i != *first_index {
            plain.to_string()
        } else {
            herdrx::workspace_id_label(&tab.workspace_id, &tab.display_label)
        };
        (want != tab.display_label).then(|| Rename::Tab {
            tab_id: tab.tab_id.clone(),
            label: want,
        })
    }));
    if !pane_ids {
        return out;
    }
    let exempt = exempt_tabs(tabs, panes);
    let (own, prefixed): (Vec<&Pane>, Vec<&Pane>) = panes
        .iter()
        .partition(|pane| exempt.contains(pane.tab_id.as_str()));
    out.extend(own.into_iter().filter_map(|pane| {
        let current = pane.label.as_deref().unwrap_or_default();
        let plain = herdrx::strip_id_prefix(current);
        (plain != current).then(|| Rename::Pane {
            pane_id: pane.pane_id.clone(),
            label: (!plain.is_empty()).then(|| plain.to_string()),
        })
    }));
    out.extend(
        herdrx::id_label_renames(prefixed.into_iter().map(|pane| {
            (
                pane.pane_id.as_str(),
                pane.label.as_deref().unwrap_or_default(),
            )
        }))
        .into_iter()
        .map(|(pane_id, label)| Rename::Pane {
            pane_id,
            label: Some(label),
        }),
    );
    out
}

/// The supervisor's side: when each server was last reconciled, and which
/// kinds of failure have been logged already.
#[derive(Default)]
pub struct Reconciler {
    last: HashMap<String, Instant>,
    logged: HashSet<String>,
}

impl Reconciler {
    /// Reconciles one server's names if [`INTERVAL`] has passed since the
    /// last time, from the pane list the pass already read. Returns the lines
    /// worth logging: a failure of a kind not logged before, once per
    /// supervisor — a rename that keeps failing is retried every interval,
    /// and saying so every interval would bury the log.
    pub fn tick(
        &mut self,
        h: &Herdr,
        backend: &str,
        panes: &[Pane],
        enabled: bool,
        now: Instant,
    ) -> Vec<String> {
        if self
            .last
            .get(backend)
            .is_some_and(|at| now.duration_since(*at) < INTERVAL)
        {
            return Vec::new();
        }
        self.last.insert(backend.to_string(), now);
        let tabs = match h.tabs() {
            Ok(tabs) => tabs,
            Err(e) => return self.once("tab.list", &e).into_iter().collect(),
        };
        let workspaces = match h.workspaces() {
            Ok(workspaces) => workspaces,
            Err(e) => return self.once("workspace.list", &e).into_iter().collect(),
        };
        let mut lines = Vec::new();
        for rename in plan(&workspaces, &tabs, panes, enabled) {
            let (what, result) = match &rename {
                Rename::Workspace {
                    workspace_id,
                    label,
                } => ("workspace.rename", h.rename_workspace(workspace_id, label)),
                Rename::Tab { tab_id, label } => ("tab.rename", h.rename_tab(tab_id, label)),
                Rename::Pane {
                    pane_id,
                    label: Some(label),
                } => ("pane.rename", h.rename_pane(pane_id, label)),
                Rename::Pane {
                    pane_id,
                    label: None,
                } => ("pane.rename", h.clear_pane_name(pane_id)),
            };
            if let Err(e) = result {
                lines.extend(self.once(what, &e));
            }
        }
        lines
    }

    /// A line for this failure, unless one of its kind — the call and the
    /// server's error code — was logged already.
    fn once(&mut self, what: &str, error: &herdrx::Error) -> Option<String> {
        let kind = match error {
            herdrx::Error::Api(api) => format!("{what}: {}", api.code),
            _ => format!("{what}: unreachable"),
        };
        self.logged
            .insert(kind)
            .then(|| format!("id labels: {what} failed: {error} (retried; said once)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(id: &str, label: &str) -> Workspace {
        serde_json::from_value(json!({ "workspace_id": id, "label": label })).unwrap()
    }
    fn tab(id: &str, number: u32, label: &str) -> Tab {
        serde_json::from_value(json!({ "tab_id": id,
            "workspace_id": id.split(':').next().unwrap(), "number": number, "label": label
        }))
        .unwrap()
    }
    fn pane(id: &str, tab_id: &str, label: Option<&str>) -> Pane {
        serde_json::from_value(json!({ "pane_id": id, "tab_id": tab_id,
            "workspace_id": id.split(':').next().unwrap(), "label": label
        }))
        .unwrap()
    }
    fn applied(
        ws: &[Workspace],
        tabs: &[Tab],
        panes: &[Pane],
        renames: &[Rename],
    ) -> (Vec<Workspace>, Vec<Tab>, Vec<Pane>) {
        let (mut ws, mut tabs, mut panes) = (ws.to_vec(), tabs.to_vec(), panes.to_vec());
        for rename in renames {
            match rename {
                Rename::Workspace {
                    workspace_id,
                    label,
                } => {
                    let w = ws
                        .iter_mut()
                        .find(|w| &w.workspace_id == workspace_id)
                        .unwrap();
                    *w = workspace(workspace_id, label);
                }
                Rename::Tab { tab_id, label } => {
                    let t = tabs.iter_mut().find(|t| &t.tab_id == tab_id).unwrap();
                    *t = tab(tab_id, t.number, label);
                }
                Rename::Pane { pane_id, label } => {
                    panes
                        .iter_mut()
                        .find(|p| &p.pane_id == pane_id)
                        .unwrap()
                        .label = label.clone();
                }
            }
        }
        (ws, tabs, panes)
    }

    #[test]
    fn workspace_and_first_tab_ids_are_always_on_and_idempotent() {
        for pane_ids in [false, true] {
            let ws = [workspace("w2", "project"), workspace("w3B", "")];
            // API list order is visual order; stable numbers may differ.
            let tabs = [
                tab("w2:t9", 9, "first"),
                tab("w2:t1", 1, "[w2:t1] third"),
                tab("w2:t2", 2, "[w2] second"),
                tab("w3B:t1", 1, ""),
            ];
            let renames = plan(&ws, &tabs, &[], pane_ids);
            let (ws, tabs, panes) = applied(&ws, &tabs, &[], &renames);
            assert_eq!(ws[0].display_label, "[w2]project");
            assert_eq!(ws[1].display_label, "[w3B]");
            assert_eq!(
                tabs.iter()
                    .map(|t| t.display_label.as_str())
                    .collect::<Vec<_>>(),
                ["[w2]first", "third", "second", "[w3B]"]
            );
            assert!(
                plan(&ws, &tabs, &panes, pane_ids).is_empty(),
                "no idle writes"
            );
        }
    }

    #[test]
    fn dock_sidebar_and_tab_keep_plain_names_without_idle_writes() {
        let dock = crate::dockcmd::LABEL;
        for pane_ids in [false, true] {
            for raw in [
                dock.to_string(),
                format!("[w9] {dock}"),
                format!("[w9]{dock}"),
                format!("[w2] [w9:t1] {dock}"),
                format!("[w9:t1] {dock}"),
            ] {
                for dock_first in [false, true] {
                    let ws = [workspace("w9", &raw), workspace("w2", "herdr-tower")];
                    let dock_tab = tab("w9:t1", 1, &raw);
                    let other_tab = tab("w9:t2", 2, "other");
                    let tabs = if dock_first {
                        [dock_tab, other_tab]
                    } else {
                        [other_tab, dock_tab]
                    };
                    let got = plan(&ws, &tabs, &[], pane_ids);
                    let (ws, tabs, _) = applied(&ws, &tabs, &[], &got);
                    assert_eq!(ws[0].display_label, dock);
                    assert_eq!(
                        ws[1].display_label, "[w2]herdr-tower",
                        "ordinary repository workspace"
                    );
                    let dock_index = if dock_first { 0 } else { 1 };
                    assert_eq!(tabs[dock_index].display_label, dock);
                    assert_eq!(
                        tabs[1 - dock_index].display_label,
                        if dock_first { "other" } else { "[w9]other" }
                    );
                    assert_eq!(
                        ws[0].label, dock,
                        "wire round trip preserves keeper identity"
                    );
                    assert_eq!(tabs[dock_index].label, dock);
                    assert!(
                        plan(&ws, &tabs, &[], pane_ids).is_empty(),
                        "idempotent: no idle writes"
                    );
                }
            }
        }
    }

    #[test]
    fn moving_a_newer_tab_left_keeps_stable_numbers_and_moves_the_prefix() {
        // tab.list is in visual order; moving a tab does not change its number.
        let tabs = [tab("w2:t9", 9, "newer"), tab("w2:t1", 1, "[w2] older")];
        let got = plan(&[], &tabs, &[], false);
        assert_eq!(
            got,
            [
                Rename::Tab {
                    tab_id: "w2:t9".into(),
                    label: "[w2]newer".into()
                },
                Rename::Tab {
                    tab_id: "w2:t1".into(),
                    label: "older".into()
                },
            ]
        );
        let (_, tabs, _) = applied(&[], &tabs, &[], &got);
        assert_eq!(tabs[0].number, 9);
        assert_eq!(tabs[1].number, 1);
        assert!(plan(&[], &tabs, &[], false).is_empty());
    }

    #[test]
    fn manual_rename_reorder_and_first_tab_close_converge() {
        let ws = [workspace("w2", "changed by user")];
        let tabs = [
            tab("w2:t2", 2, "new first"),
            tab("w2:t1", 1, "[w2] old first"),
        ];
        let got = plan(&ws, &tabs, &[], false);
        let (ws, tabs, _) = applied(&ws, &tabs, &[], &got);
        assert_eq!(ws[0].display_label, "[w2]changed by user");
        assert_eq!(tabs[0].display_label, "[w2]new first");
        assert_eq!(tabs[1].display_label, "old first");
        let got = plan(&ws, &tabs[1..], &[], false);
        assert_eq!(
            got,
            [Rename::Tab {
                tab_id: "w2:t1".into(),
                label: "[w2]old first".into()
            }]
        );
    }

    #[test]
    fn dock_and_creation_request_names_still_match_after_wire_round_trip() {
        let request = crate::dockcmd::tab_request_label("100000000000-a");
        for name in [
            crate::dockcmd::TAB_LABEL,
            crate::usagetab::LABEL,
            request.as_str(),
        ] {
            let ws = [workspace("w9", name)];
            let tabs = [tab("w9:t2", 1, name)];
            let panes = [pane("w9:p2", "w9:t2", Some("[w9:p2]"))];
            let (ws, tabs, panes) = applied(&ws, &tabs, &panes, &plan(&ws, &tabs, &panes, true));
            assert_eq!(ws[0].label, name);
            assert_eq!(tabs[0].label, name);
            assert_eq!(panes[0].label, None, "own panes stay plain");
            assert!(exempt_tabs(&tabs, &panes).contains("w9:t2"));
            assert!(plan(&ws, &tabs, &panes, true).is_empty());
        }
    }

    #[test]
    fn own_panes_are_exempt_by_tokens_even_after_a_manual_tab_rename() {
        let tabs = [
            tab("w2:t1", 1, "renamed dock"),
            tab("w2:t2", 2, "renamed usage"),
        ];
        let mut panes = [
            pane("w2:p1", "w2:t1", Some("[w2:p1] dock")),
            pane("w2:p2", "w2:t2", Some("[w2:p2]")),
        ];
        panes[0]
            .tokens
            .insert(crate::dockcmd::ACTIVE_TOKEN.into(), "x".into());
        panes[1]
            .tokens
            .insert(crate::usagetab::TOKEN.into(), "x".into());
        let (_, tabs, panes) = applied(&[], &tabs, &panes, &plan(&[], &tabs, &panes, true));
        assert_eq!(panes[0].label.as_deref(), Some("dock"));
        assert_eq!(panes[1].label, None);
        assert!(plan(&[], &tabs, &panes, true).is_empty());
    }

    #[test]
    fn pane_switch_only_controls_panes_and_invalid_ids_stay_untouched() {
        let ws = [workspace("remote/w2", "name")];
        let tabs = [tab("remote/w2:t1", 1, "[w2:t1] x")];
        let panes = [pane("remote/w2:p1", "remote/w2:t1", Some("[w2:p1] x"))];
        assert!(plan(&ws, &tabs, &panes, true).is_empty());
        let panes = [pane("w2:p1", "w2:t1", Some("agent"))];
        assert!(plan(&[], &[], &panes, false).is_empty());
        assert_eq!(
            plan(&[], &[], &panes, true),
            [Rename::Pane {
                pane_id: "w2:p1".into(),
                label: Some("[w2:p1] agent".into())
            }]
        );
    }

    #[test]
    fn tab_list_order_oracle_for_every_permutation_and_missing_numbers() {
        // The server lists left to right; the literal first entry is the independent oracle.
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let all = [
                tab("wF:t9", 9, "nine"),
                tab("wF:t8", 8, "eight"),
                tab("wF:t7", 7, "seven"),
            ];
            let tabs: Vec<_> = order.iter().map(|i| all[*i].clone()).collect();
            let (_, tabs, _) = applied(&[], &tabs, &[], &plan(&[], &tabs, &[], false));
            for t in &tabs {
                assert_eq!(
                    t.display_label.starts_with("[wF]"),
                    t.tab_id == all[order[0]].tab_id
                );
            }
            assert!(plan(&[], &tabs, &[], false).is_empty());
        }
        let tabs = [tab("wF:t8", 0, "a"), tab("wF:t7", 0, "b")];
        assert_eq!(
            plan(&[], &tabs, &[], false),
            [Rename::Tab {
                tab_id: "wF:t8".into(),
                label: "[wF]a".into()
            }]
        );
    }

    #[test]
    fn nothing_is_asked_before_the_next_interval_even_with_pane_ids_off() {
        let h = Herdr::at(std::path::Path::new("/nonexistent/herdr.sock"));
        let mut r = Reconciler::default();
        let t0 = Instant::now();
        assert_eq!(r.tick(&h, "b", &[], false, t0).len(), 1);
        assert!(r.tick(&h, "b", &[], true, t0 + INTERVAL / 2).is_empty());
        assert!(
            r.tick(&h, "b", &[], true, t0 + INTERVAL).is_empty(),
            "failure logged once"
        );
    }
    #[cfg(unix)]
    #[test]
    fn socket_reconcile_reads_only_twice_and_idle_pass_writes_nothing() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("idl{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("h.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let mut calls = Vec::new();
            let (mut ws_label, mut tab_label) = ("project".to_string(), "work".to_string());
            // First pass: two reads + two writes. Second pass: two reads only.
            for conn in listener.incoming() {
                let mut conn = conn.unwrap();
                conn.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut line = String::new();
                BufReader::new(conn.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                let method = req["method"].as_str().unwrap();
                if method == "test.stop" {
                    break;
                }
                let body = match method {
                    "tab.list" => json!({"tabs": [{"tab_id":"wF:t9", "workspace_id":"wF",
                        "number":1, "label": tab_label}]}),
                    "workspace.list" => {
                        json!({"workspaces":[{"workspace_id":"wF", "label":ws_label}]})
                    }
                    "workspace.rename" => {
                        assert_eq!(req["params"]["workspace_id"], "wF");
                        ws_label = req["params"]["label"].as_str().unwrap().to_string();
                        json!({})
                    }
                    "tab.rename" => {
                        assert_eq!(req["params"]["tab_id"], "wF:t9");
                        tab_label = req["params"]["label"].as_str().unwrap().to_string();
                        json!({})
                    }
                    _ => panic!("unexpected call {method}"),
                };
                calls.push(method.to_string());
                writeln!(conn, "{}", json!({"id":req["id"],"result":body})).unwrap();
            }
            assert_eq!(ws_label, "[wF]project");
            assert_eq!(tab_label, "[wF]work");
            calls
        });
        let h = Herdr::at(&socket);
        let mut r = Reconciler::default();
        let t0 = Instant::now();
        assert!(r.tick(&h, "b", &[], false, t0).is_empty());
        assert!(r.tick(&h, "b", &[], false, t0 + INTERVAL / 2).is_empty());
        assert!(r.tick(&h, "b", &[], false, t0 + INTERVAL).is_empty());
        let mut stop = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        writeln!(stop, "{}", json!({"method": "test.stop"})).unwrap();
        assert_eq!(
            server.join().unwrap(),
            [
                "tab.list",
                "workspace.list",
                "workspace.rename",
                "tab.rename",
                "tab.list",
                "workspace.list"
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
