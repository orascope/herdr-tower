// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Which account each AI's CLI is signed in to, for the usage board.
//!
//! Read from the files each CLI keeps its login in, on this machine, every
//! time the board is put together — no request is made. Only the account's
//! name is taken (an email address, or "API key"); no token leaves the file.
//!
//! - claude: `oauthAccount.emailAddress` in `.claude.json`, under
//!   `$CLAUDE_CONFIG_DIR` when set, else the home folder.
//! - codex: `auth.json` under `$CODEX_HOME` (else `~/.codex`) — the `email`
//!   claim of its ChatGPT `id_token`, or "API key" for an API-key login.
//! - gemini: `active` in `~/.gemini/google_accounts.json`.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;

/// The longest account name shown; anything longer is cut.
const MOST: usize = 64;

/// The account `vendor`'s CLI is signed in to, when its login file says.
pub fn account(vendor: &str, home: &Path) -> Option<String> {
    account_with(vendor, home, |k| std::env::var_os(k).map(PathBuf::from))
}

fn account_with(
    vendor: &str,
    home: &Path,
    env: impl Fn(&str) -> Option<PathBuf>,
) -> Option<String> {
    let found = match vendor {
        "claude" => {
            let dir = env("CLAUDE_CONFIG_DIR")
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| home.to_path_buf());
            claude_account(&read_json(&dir.join(".claude.json"))?)
        }
        "codex" => {
            let dir = env("CODEX_HOME")
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| home.join(".codex"));
            codex_account(&read_json(&dir.join("auth.json"))?)
        }
        "gemini" => gemini_account(&read_json(
            &home.join(".gemini").join("google_accounts.json"),
        )?),
        _ => None,
    }?;
    shown(&found)
}

/// A login file, read only when it is small: these are a few kilobytes.
fn read_json(path: &Path) -> Option<Value> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 4 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn claude_account(config: &Value) -> Option<String> {
    text(config.pointer("/oauthAccount/emailAddress"))
}

pub fn codex_account(auth: &Value) -> Option<String> {
    let id_token = auth.pointer("/tokens/id_token").and_then(Value::as_str);
    if let Some(email) = id_token
        .and_then(jwt_claims)
        .and_then(|c| text(c.get("email")))
    {
        return Some(email);
    }
    let api_key = auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|k| !k.is_empty());
    let mode = auth.get("auth_mode").and_then(Value::as_str);
    (api_key || mode.is_some_and(|m| m.eq_ignore_ascii_case("apikey"))).then(|| "API key".into())
}

pub fn gemini_account(accounts: &Value) -> Option<String> {
    text(accounts.get("active"))
}

/// The claims of a JWT, read without checking its signature: only to name
/// the account on this machine's own screen.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Another program's text, made safe and short for the screen.
fn shown(s: &str) -> Option<String> {
    let clean = tower_state::clean_text(s).trim().to_string();
    (!clean.is_empty()).then(|| tower_text::truncate_cell(&clean, MOST))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.c2ln",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn claude_names_its_oauth_account() {
        let c =
            json!({ "oauthAccount": { "emailAddress": "a@example.com", "organizationName": "o" } });
        assert_eq!(claude_account(&c).as_deref(), Some("a@example.com"));
        assert_eq!(claude_account(&json!({})), None, "not signed in");
        assert_eq!(
            claude_account(&json!({ "oauthAccount": { "emailAddress": " " } })),
            None
        );
    }

    #[test]
    fn codex_names_its_chatgpt_login_or_says_api_key() {
        let auth = json!({ "tokens": { "id_token": jwt(json!({ "email": "c@example.com" })) } });
        assert_eq!(codex_account(&auth).as_deref(), Some("c@example.com"));
        assert_eq!(
            codex_account(&json!({ "OPENAI_API_KEY": "sk-x", "tokens": null })).as_deref(),
            Some("API key")
        );
        assert_eq!(
            codex_account(&json!({ "auth_mode": "apikey" })).as_deref(),
            Some("API key")
        );
        assert_eq!(codex_account(&json!({ "OPENAI_API_KEY": null })), None);
        assert_eq!(
            codex_account(&json!({ "tokens": { "id_token": "not-a-jwt" } })),
            None,
            "a token that does not decode names nobody"
        );
        // Padding some writers leave on is accepted.
        let padded = format!("{}=", jwt(json!({ "email": "p@example.com" })));
        let padded = padded
            .rsplit_once('.')
            .map(|(a, _)| format!("{a}=.sig"))
            .unwrap();
        assert_eq!(
            codex_account(&json!({ "tokens": { "id_token": padded } })).as_deref(),
            Some("p@example.com")
        );
    }

    #[test]
    fn gemini_names_its_active_google_account() {
        let g = json!({ "active": "g@example.com", "old": ["x@example.com"] });
        assert_eq!(gemini_account(&g).as_deref(), Some("g@example.com"));
        assert_eq!(gemini_account(&json!({ "active": null })), None);
    }

    #[test]
    fn the_files_are_found_where_each_cli_keeps_them() {
        let dir = std::env::temp_dir().join(format!("tower-acct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            json!({ "oauthAccount": { "emailAddress": "home@example.com" } }).to_string(),
        )
        .unwrap();
        std::fs::write(
            home.join(".codex/auth.json"),
            json!({ "tokens": { "id_token": jwt(json!({ "email": "cx@example.com" })) } })
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            home.join(".gemini/google_accounts.json"),
            json!({ "active": "gm@example.com" }).to_string(),
        )
        .unwrap();
        let none = |_: &str| None;
        assert_eq!(
            account_with("claude", &home, none).as_deref(),
            Some("home@example.com")
        );
        assert_eq!(
            account_with("codex", &home, none).as_deref(),
            Some("cx@example.com")
        );
        assert_eq!(
            account_with("gemini", &home, none).as_deref(),
            Some("gm@example.com")
        );
        assert_eq!(account_with("kiro", &home, none), None);

        // A config folder set for the CLI wins over the home folder.
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join(".claude.json"),
            json!({ "oauthAccount": { "emailAddress": "work@example.com" } }).to_string(),
        )
        .unwrap();
        let env = |k: &str| (k == "CLAUDE_CONFIG_DIR").then(|| other.clone());
        assert_eq!(
            account_with("claude", &home, env).as_deref(),
            Some("work@example.com")
        );
        let empty = |k: &str| (k == "CODEX_HOME").then(PathBuf::new);
        assert_eq!(
            account_with("codex", &home, empty).as_deref(),
            Some("cx@example.com"),
            "an empty CODEX_HOME is no folder"
        );
        // No login file: nothing to say.
        assert_eq!(account_with("claude", &dir.join("nobody"), none), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_is_shown_is_clean_and_short() {
        assert_eq!(
            shown("a\x1b]0;x\x07b@example.com").as_deref(),
            Some("a]0;xb@example.com")
        );
        let long = format!("{}@example.com", "x".repeat(200));
        assert!(tower_text::width(&shown(&long).unwrap()) <= MOST);
        assert_eq!(shown("\x1b\x07"), None);
    }
}
