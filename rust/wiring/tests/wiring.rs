// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What the collection must and must not conclude from the files on disk.
//!
//! Every fixture here is a directory tree, not a mock: the rules being tested
//! are about the exact text real installers write into plists, systemd units
//! and their sidecars, so anything less than the real bytes would prove the
//! test's own assumptions instead.
// The bot registrations these read are launchd and systemd files;
// on Windows only the parts that do not need them run.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use tower_wiring::{collect, session_agents};
use tower_wiring::{Paths, Registration};

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn launch_agents_dir(root: &TempDir) -> PathBuf {
    let dir = root.path().join("LaunchAgents");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A macOS-shaped machine: a folder-bot registration and a plist that starts the bot.
fn fixture() -> (TempDir, Paths, String) {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("collab").display().to_string();

    let bots_json = root.path().join("config").join("bots.json");
    write(
        &bots_json,
        &format!(
            r#"{{
              "collab": {{"engine": "claude", "folder": "{folder}", "session": "collab-bot"}},
              "other":  {{"engine": "codex",  "folder": "/x/other",  "session": "other-bot"}}
            }}"#
        ),
    );

    let la = launch_agents_dir(&root);
    write(
        &la.join("com.folder-bot.collab.plist"),
        &format!("<plist>... -s collab-bot ... cd {folder} ...</plist>"),
    );
    write(
        &la.join("com.unrelated.plist"),
        "<plist>some other bot</plist>",
    );

    let paths = Paths {
        bots_json: Some(bots_json),
        launch_agents_dir: Some(la),
        ..Paths::default()
    };
    (root, paths, folder)
}

/// A Linux-shaped machine. The session name appears only in the unit body
/// (`ExecStop`) and the folder only in the `.up.sh` sidecar — the folder is
/// not visible without concatenating the sidecar, which is the whole point.
fn systemd_fixture() -> (TempDir, Paths, String) {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("collab").display().to_string();
    let sd = root.path().join("systemd-user");
    fs::create_dir_all(&sd).unwrap();
    let sd_str = sd.display().to_string();

    write(
        &sd.join("com.folder-bot.collab.service"),
        &format!(
            "[Unit]\n\
             Description=folder-bot collab\n\n\
             [Service]\n\
             Type=oneshot\n\
             RemainAfterExit=yes\n\
             ExecStart=/bin/bash {sd_str}/collab-bot.up.sh\n\
             ExecStop=/usr/local/bin/herdr workspace close collab-bot\n"
        ),
    );
    write(
        &sd.join("collab-bot.up.sh"),
        &format!(
            "#!/bin/bash\n\
             exec /usr/local/bin/herdr workspace create --label collab-bot \
             --cwd {folder} --no-focus\n"
        ),
    );

    // A sub-folder bot: mentions the folder as a prefix, has no `.up.sh` on
    // disk, and must be filtered out by both the path boundary and the
    // "starts a session" rule.
    write(
        &sd.join("com.folder-bot.sub.service"),
        &format!(
            "[Service]\n\
             Environment=BOT_DIR={folder}/child\n\
             ExecStart=/bin/bash {sd_str}/sub-bot.up.sh\n\
             ExecStop=/usr/local/bin/herdr workspace close sub-bot\n"
        ),
    );
    write(&sd.join("backup.timer"), "[Timer]\n");

    (
        root,
        Paths {
            systemd_user_dir: Some(sd),
            ..Paths::default()
        },
        folder,
    )
}

#[cfg(unix)]
#[test]
fn collects_every_source_of_one_agents_wiring() {
    let (_root, paths, folder) = fixture();
    let info = collect(&paths, &folder, "collab-bot");

    assert_eq!(
        info.bot,
        Some(Registration {
            name: "collab".into(),
            session: "collab-bot".into(),
            engine: "claude".into()
        })
    );
    assert_eq!(
        info.launch_units,
        ["com.folder-bot.collab"],
        "the unrelated plist stays out"
    );
}

/// Collection runs on machines where none of this is installed. A missing
/// source empties one field; it never takes the rest of the card down with it.
#[test]
fn missing_sources_collect_to_nothing() {
    let info = collect(
        &Paths {
            bots_json: Some("/nonexistent/bots.json".into()),
            launch_agents_dir: Some("/nonexistent".into()),
            ..Paths::default()
        },
        "/nonexistent/folder",
        "x",
    );
    assert!(info.bot.is_none());
    assert!(info.launch_units.is_empty());
}

/// An agent in a worktree runs from a different directory than the one that was
/// registered, so the session name has to be able to find the registration on
/// its own.
#[cfg(unix)]
#[test]
fn a_worktree_still_finds_its_registration_by_session_name() {
    let (_root, paths, _folder) = fixture();
    let info = collect(&paths, "/some/other/path", "collab-bot");
    assert_eq!(info.bot.map(|b| b.name).as_deref(), Some("collab"));
}

/// Found in the field: the agent sitting in `~/W` was reported as started by a
/// unit because the plist of a bot in `~/W/bot` contains `~/W` as a prefix. A folder needle has to end at a path boundary.
#[test]
fn a_parent_folder_does_not_match_its_childs_unit() {
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    write(
        &la.join("com.jordan.claude-bot.plist"),
        "<plist>cd /Users/x/VSCodeWorkspace/bot && run</plist>",
    );
    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };

    let parent = collect(&paths, "/Users/x/VSCodeWorkspace", "");
    assert!(
        parent.launch_units.is_empty(),
        "the child's plist is not the parent's"
    );

    let exact = collect(&paths, "/Users/x/VSCodeWorkspace/bot", "");
    assert_eq!(
        exact.launch_units,
        ["com.jordan.claude-bot"],
        "the owner still matches"
    );
}

/// A two-letter session name matches inside `ai.openclaw.gateway` even with
/// word boundaries, because `.` is one. Names that short are not evidence.
#[test]
fn a_very_short_session_name_matches_no_unit() {
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    write(
        &la.join("ai.openclaw.gateway.plist"),
        "<plist>Label ai.openclaw.gateway</plist>",
    );
    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };

    let info = collect(&paths, "/somewhere", "ai");
    assert!(info.launch_units.is_empty());
}

/// restore asks this to find sessions launchd will revive on its own. A card or
/// monitor unit that merely mentions the session is not the launcher — treat it
/// as one and restore creates the session first, the real unit's start fails as
/// a duplicate, and the bot never comes up.
#[test]
fn session_agents_reports_only_the_unit_that_starts_the_session() {
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    write(
        &la.join("com.folder-bot.collab.plist"),
        "<string>/usr/local/bin/herdr</string><string>workspace</string><string>create</string>\
         <string>--label</string><string>collab-bot</string>",
    );
    write(
        &la.join("com.example.card.plist"),
        "<string>herdr-tower card</string> collab-bot only mentioned",
    );
    write(
        &la.join("com.other.plist"),
        "<string>herdr</string><string>tab</string><string>create</string>\
         <string>--label</string><string>other-bot</string>",
    );
    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };

    assert_eq!(
        session_agents(&paths, "collab-bot"),
        ["com.folder-bot.collab"]
    );
    assert!(
        session_agents(&paths, "ai").is_empty(),
        "too short to match anything"
    );
}

/// The session name lives in the unit body, the folder only in the sidecar.
/// Both have to be found by the same rules that judge a plist.
#[test]
fn a_systemd_unit_matches_by_session_name() {
    let (_root, paths, _folder) = systemd_fixture();
    let info = collect(&paths, "/some/other/path", "collab-bot");
    assert_eq!(info.launch_units, ["com.folder-bot.collab"]);
}

#[test]
fn a_systemd_unit_matches_by_the_folder_in_its_sidecar() {
    let (_root, paths, folder) = systemd_fixture();
    // The session name deliberately does not match: only the `.up.sh` sidecar
    // carries the folder, and the sub-folder bot must stay out.
    let info = collect(&paths, &folder, "x");
    assert_eq!(info.launch_units, ["com.folder-bot.collab"]);
}

#[test]
fn session_agents_reads_the_systemd_sidecars() {
    let (_root, paths, _folder) = systemd_fixture();
    assert_eq!(
        session_agents(&paths, "collab-bot"),
        ["com.folder-bot.collab"]
    );
    assert!(
        session_agents(&paths, "sub-bot").is_empty(),
        "without its .up.sh nothing in the text creates a place"
    );
}

/// A unit that creates a place is a launcher; one that only names the binary
/// is not.
#[test]
fn a_herdr_unit_that_creates_a_place_is_a_launcher() {
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    write(
        &la.join("com.folder-bot.collab.plist"),
        "<string>/usr/local/bin/herdr</string><string>tab create</string>\
         <string>--label</string><string>collab-bot</string>",
    );
    write(
        &la.join("com.example.card.plist"),
        "<string>herdr-tower card</string> herdr collab-bot only mentioned",
    );
    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };

    assert_eq!(
        session_agents(&paths, "collab-bot"),
        ["com.folder-bot.collab"]
    );
}

/// Verified against reality (2026-09-11): this body is lifted from the script
/// that actually created a herdr workspace and tab and brought a claude bot up
/// with `herdr agent start`. If the recognised verbs drift from the CLI's real
/// surface, this test fails before restore starts skipping live bots.
#[test]
fn session_agents_herdr_real_script() {
    const REAL_SCRIPT: &str = r#"#!/bin/sh
set -e
LABEL="al-bot-test"
WS=$(herdr workspace list | ...)
if [ -z "$WS" ]; then
  WS=$(herdr workspace create --label "$LABEL" --cwd "$DIR" --no-focus | ...)
fi
PANE=$(herdr tab create --workspace "$WS" --label "$LABEL" --cwd "$DIR" --no-focus | ...)
herdr agent start albot --kind claude --pane "$PANE"
"#;
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    write(&la.join("com.folder-bot.albot.plist"), REAL_SCRIPT);

    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };
    assert_eq!(
        session_agents(&paths, "al-bot-test"),
        ["com.folder-bot.albot"]
    );
}

/// A real LaunchAgent wraps every argument in its own element, so the
/// `herdr tab create` that is one line in a shell script never occurs as that
/// substring in the plist. Matching on the joined text misses a live launcher —
/// and a missed launcher is a bot that restore starts twice.
#[test]
fn session_agents_sees_herdr_arguments_split_across_plist_elements() {
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    write(
        &la.join("com.folder-bot.albot.plist"),
        "<key>ProgramArguments</key><array>\
         <string>/usr/local/bin/herdr</string>\
         <string>tab</string>\
         <string>create</string>\
         <string>--label</string>\
         <string>al-bot-test</string>\
         </array>",
    );

    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };
    assert_eq!(
        session_agents(&paths, "al-bot-test"),
        ["com.folder-bot.albot"]
    );
}

/// A known limit, kept as a test so nobody re-investigates it: a unit that only
/// points at a script path carries no evidence in its own text. systemd escapes
/// this through its sidecars; a plist has to spell the command out.
#[test]
fn a_unit_that_only_names_a_script_is_not_recognised() {
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    write(
        &la.join("com.folder-bot.albot.plist"),
        "<string>/usr/local/bin/albot-up.sh</string><string>al-bot-test</string>",
    );

    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };
    assert!(session_agents(&paths, "al-bot-test").is_empty());
}

/// A launcher that has been commented out is not this session's launcher, in a
/// plist and in a systemd unit alike — and the live one beside it still is.
///
/// This is the whole of #55 seen from outside: `restore` asks `session_agents`
/// which units bring the session up by themselves and leaves those agents to
/// launchd or systemd. Naming a unit that starts nothing means the dead agent is
/// left alone forever, with nothing else to bring it back, so a retired block
/// that a person disabled must read as disabled here.
#[test]
fn a_commented_out_launcher_is_not_the_launcher() {
    let root = tempfile::tempdir().unwrap();
    let la = launch_agents_dir(&root);
    // The retired unit: the launcher is inside an XML comment, and what is live
    // in it only mentions the session.
    write(
        &la.join("com.folder-bot.retired.plist"),
        "<!--<string>herdr</string><string>workspace</string><string>create</string>\
         <string>--label</string>-->\
         <string>herdr-tower</string><string>card</string><string>collab-bot</string>",
    );
    write(
        &la.join("com.folder-bot.collab.plist"),
        "<string>herdr</string><string>tab</string><string>create</string>\
         <string>--label</string><string>collab-bot</string>",
    );
    let paths = Paths {
        launch_agents_dir: Some(la),
        ..Paths::default()
    };
    assert_eq!(
        session_agents(&paths, "collab-bot"),
        ["com.folder-bot.collab"]
    );

    // The same thing under systemd, where the launcher lives in the sidecar the
    // commented-out `ExecStart` named: the sidecar is never read at all.
    let sd = root.path().join("systemd-user");
    write(
        &sd.join("com.folder-bot.retired.service"),
        "[Service]\n\
         # ExecStart=/bin/bash /x/collab-bot.up.sh\n\
         ExecStart=/usr/local/bin/herdr-tower card collab-bot\n",
    );
    write(
        &sd.join("collab-bot.up.sh"),
        "#!/bin/bash\nexec /usr/local/bin/herdr workspace create --label collab-bot\n",
    );
    write(
        &sd.join("com.folder-bot.live.service"),
        "[Service]\nExecStart=/bin/bash /x/live-bot.up.sh\n",
    );
    write(
        &sd.join("live-bot.up.sh"),
        "#!/bin/bash\n\
         # exec /usr/local/bin/herdr agent start old --pane w1:p1\n\
         exec /usr/local/bin/herdr tab create --label collab-bot\n",
    );
    let paths = Paths {
        systemd_user_dir: Some(sd),
        ..Paths::default()
    };
    assert_eq!(
        session_agents(&paths, "collab-bot"),
        ["com.folder-bot.live"]
    );
}
