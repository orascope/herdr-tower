// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What each CLI has configured as its default model.
//!
//! "Default" here means the setting the CLI persists — Claude Code's `/model`,
//! codex's `config.toml`, gemini's `settings.json`. An empty answer is not a
//! failure: it means nothing is pinned and the CLI will pick for itself.
//!
//! Every reader takes the home directory as an argument, so a test never has to
//! borrow the developer's own configuration.

use std::path::Path;

use serde::Deserialize;

/// Claude Code's configured default model.
///
/// `settings.local.json` outranks `settings.json`, the same way Claude Code
/// itself resolves them. A local file that parses but pins no model falls
/// through rather than winning with an empty string — otherwise adding an
/// unrelated local setting would silently blank the pinned model.
pub fn claude_default_model(home: &Path) -> String {
    claude_default_model_in(&home.join(".claude"))
}

/// [`claude_default_model`] for a caller that already holds `~/.claude`.
pub(crate) fn claude_default_model_in(claude_dir: &Path) -> String {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default)]
        model: String,
    }
    for name in ["settings.local.json", "settings.json"] {
        let Ok(body) = std::fs::read(claude_dir.join(name)) else {
            continue;
        };
        if let Ok(s) = serde_json::from_slice::<Settings>(&body) {
            if !s.model.is_empty() {
                return s.model;
            }
        }
    }
    String::new()
}

/// codex's top-level model settings.
///
/// Named fields rather than a pair of strings: the two are the same type, and
/// swapping them at a call site would produce `"high gpt-5.6-sol"` with nothing
/// to catch it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexDefault {
    pub model: String,
    pub effort: String,
}

impl CodexDefault {
    /// How the pair reads on a row: `"gpt-5.6-sol high"`, or just the model
    /// when no effort is pinned.
    pub fn label(&self) -> String {
        if self.model.is_empty() || self.effort.is_empty() {
            return self.model.clone();
        }
        format!("{} {}", self.model, self.effort)
    }
}

/// Reads `model` and `model_reasoning_effort` from `~/.codex/config.toml`.
///
/// Stops at the first section header. A `model` inside `[profiles.lightweight]`
/// is that profile's choice, not the default, and reading on would report a
/// profile nobody selected as the thing codex is about to run.
pub fn codex_default_model(home: &Path) -> CodexDefault {
    let Ok(body) = std::fs::read_to_string(home.join(".codex").join("config.toml")) else {
        return CodexDefault::default();
    };
    let mut out = CodexDefault::default();
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            break;
        }
        if let Some(v) = toml_string_assignment(t, "model_reasoning_effort") {
            out.effort = v;
        } else if let Some(v) = toml_string_assignment(t, "model") {
            out.model = v;
        }
    }
    out
}

/// `key = "value"` at the start of a line, with TOML's own whitespace rules.
fn toml_string_assignment(line: &str, key: &str) -> Option<String> {
    fn skip_spaces(s: &str) -> &str {
        s.trim_start_matches([' ', '\t', '\n', '\x0c', '\r'])
    }
    let rest = skip_spaces(line.strip_prefix(key)?).strip_prefix('=')?;
    let rest = skip_spaces(rest).strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The Gemini-family default model.
///
/// Antigravity's `~/.gemini/antigravity-cli/settings.json` first, because where
/// both exist agy is the CLI actually being run — see
/// [`crate::gemini_command`]. Both the old schema (a plain string) and the new
/// one (`{"name": …}`) are accepted; a version bump that changed the shape
/// should not blank the field.
pub fn gemini_default_model(home: &Path) -> String {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default)]
        model: Option<ModelField>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ModelField {
        Name(String),
        Named { name: String },
    }

    for path in [
        home.join(".gemini")
            .join("antigravity-cli")
            .join("settings.json"),
        home.join(".gemini").join("settings.json"),
    ] {
        let Ok(body) = std::fs::read(&path) else {
            continue;
        };
        let Ok(s) = serde_json::from_slice::<Settings>(&body) else {
            continue;
        };
        let name = match s.model {
            Some(ModelField::Name(s)) => s,
            Some(ModelField::Named { name }) => name,
            None => continue,
        };
        if !name.is_empty() {
            return name;
        }
    }
    String::new()
}

/// All three CLIs' pinned defaults. An empty field means "not pinned".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DefaultModels {
    pub claude: String,
    pub codex: String,
    pub gemini: String,
}

impl DefaultModels {
    pub fn read(home: &Path) -> Self {
        DefaultModels {
            claude: claude_default_model(home),
            codex: codex_default_model(home).label(),
            gemini: gemini_default_model(home),
        }
    }

    /// The default for an agent of this kind, keyed the way records are.
    pub fn for_kind(&self, kind: &str) -> &str {
        match kind {
            "claude" => &self.claude,
            "codex" => &self.codex,
            "gemini" => &self.gemini,
            _ => "",
        }
    }
}

/// Shortens a model ID or alias for display. Anything unrecognized is returned
/// unchanged — a new model must show up as itself rather than disappear.
///
/// Follows the pattern `^claude-([a-z]+)-(\d+(?:-\d+)?)(?:-\d{8})?$` exactly,
/// including its greediness: on `claude-sonnet-4-20250514` the optional minor
/// version claims the date, so the result is `Sonnet 4.20250514`. That is a
/// known wart, kept on purpose and pinned by a test, so that fixing it is a
/// deliberate change to what rows show rather than a silent one.
pub fn pretty_model(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let one_m = raw.ends_with("[1m]");
    let id = raw.strip_suffix("[1m]").unwrap_or(raw);
    match id {
        "default" => return "auto".into(),
        "opusplan" => return "OpusPlan".into(),
        _ => {}
    }

    let Some(rest) = id.strip_prefix("claude-") else {
        return raw.to_string();
    };
    let parts: Vec<&str> = rest.split('-').collect();
    let [name, version @ ..] = parts.as_slice() else {
        return raw.to_string();
    };
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase()) {
        return raw.to_string();
    }
    if version
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return raw.to_string();
    }
    let version = match version {
        [major] => major.to_string(),
        // Two components: the optional minor takes the second, whatever it is.
        [major, minor] => format!("{major}.{minor}"),
        // Three: the last is only allowed to be a release date.
        [major, minor, date] if date.len() == 8 => format!("{major}.{minor}"),
        _ => return raw.to_string(),
    };

    let mut out = String::new();
    let mut chars = name.chars();
    if let Some(c) = chars.next() {
        out.extend(c.to_uppercase());
    }
    out.push_str(chars.as_str());
    out.push(' ');
    out.push_str(&version);
    if one_m {
        out.push_str(" (1M)");
    }
    out
}

/// Whether a model is Fable, given either a display name or an ID.
///
/// Fable is the top tier, so leaving it pinned as a default burns tokens fast.
/// The tower colors it as a warning, which means this has to recognize it in
/// whatever form the row happens to be carrying.
pub fn is_fable(model: &str) -> bool {
    model.to_lowercase().contains("fable")
}
