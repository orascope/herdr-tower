// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower mcp`: a remote MCP server, so the tower can be read from a
//! phone's Claude app (or any MCP client that speaks Streamable HTTP and OAuth).
//!
//! Its tools sit behind OAuth with a pairing code, on a loopback listener that
//! a Tailscale Funnel publishes; `docs/remote-mcp.md` describes it for users.
//!
//! - [`grants`] — the file of clients, codes and tokens (hashed)
//! - [`oauth`] — the authorization server, pure over that file
//! - [`tools`] — what the server offers, over a [`tools::Source`]
//! - [`prompts`] — the usual tasks, step by step over the tools
//! - [`rpc`] — MCP's JSON-RPC lifecycle
//! - [`http`] — routing and the listener

pub mod audit;
pub mod grants;
pub mod http;
pub mod limit;
pub mod oauth;
pub mod prompts;
pub mod rpc;
pub mod run;
pub mod tools;

use std::path::{Path, PathBuf};

/// The LaunchAgent that keeps `mcp serve` running.
pub const LABEL: &str = "dev.herdr.tower.mcp";

/// The server's log, next to the supervisor's.
pub const LOG: &str = "mcp.log";

pub fn service_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// The LaunchAgent definition. The state directory and the config path travel
/// with it for the same reason as the supervisor's (`daemoncmd`): launchd starts
/// it with none of the caller's environment.
pub fn plist(home: &Path, exe: &Path, state_dir: &Path, config: Option<&Path>) -> String {
    let xml = |p: &Path| {
        p.to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    };
    let config_entry = config.map_or_else(String::new, |path| {
        format!(
            "\n    <key>HERDR_TOWER_CONFIG</key><string>{}</string>",
            xml(path)
        )
    });
    let (home, exe, state) = (xml(home), xml(exe), xml(state_dir));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>mcp</string><string>serve</string></array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>HERDR_TOWER_STATE_DIR</key><string>{state}</string>{config_entry}
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{state}/{LOG}</string>
  <key>StandardErrorPath</key><string>{state}/{LOG}</string>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_runs_mcp_serve_with_explicit_paths() {
        let body = plist(
            Path::new("/Users/a&b"),
            Path::new("/opt/herdr tower"),
            Path::new("/state"),
            Some(Path::new("/cfg.json")),
        );
        assert!(body.contains(
            "<string>/opt/herdr tower</string><string>mcp</string><string>serve</string>"
        ));
        assert!(body.contains("<key>HOME</key><string>/Users/a&amp;b</string>"));
        assert!(body.contains("<key>HERDR_TOWER_STATE_DIR</key><string>/state</string>"));
        assert!(body.contains("<key>HERDR_TOWER_CONFIG</key><string>/cfg.json</string>"));
        assert!(body.contains("<string>/state/mcp.log</string>"));
        assert!(
            !plist(Path::new("/h"), Path::new("/x"), Path::new("/s"), None)
                .contains("HERDR_TOWER_CONFIG")
        );
    }
}
