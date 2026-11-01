// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower config`: read and change `config.json` from the command line —
//! and so, through the MCP `tower` tool, from a phone.
//!
//! The file is edited as JSON, not round-tripped through [`Config`]: a key this
//! version does not know (written by a newer one, say) survives an edit. Only
//! keys in [`tower_config::keys::KEYS`] can be set, each parsed by its type,
//! and nothing is written unless the result still reads as a `Config`. That
//! last check matters more than it looks: `tower_config::load` answers a file
//! it cannot read with the defaults, so one badly typed value would otherwise
//! quietly drop the webhooks and every other setting with it.

use std::io::{self, Write};
use std::path::Path;

use serde_json::{Map, Value};
use tower_config::keys::{self, Key};
use tower_config::Config;

pub const USAGE: &str = "usage: herdr-tower config [get [key] [--reveal] | set <key> <value> | unset <key> | keys | path]";

/// What a secret shows as, unless `--reveal`.
pub const HIDDEN: &str = "<hidden>";

/// The value at a dotted key, if set.
pub fn lookup<'a>(doc: &'a Value, name: &str) -> Option<&'a Value> {
    name.split('.').try_fold(doc, |v, part| v.get(part))
}

fn known(name: &str) -> Result<&'static Key, String> {
    keys::find(name)
        .ok_or_else(|| format!("no setting {name:?} — `herdr-tower config keys` lists them"))
}

/// A copy with every secret replaced by [`HIDDEN`].
pub fn masked(doc: &Value) -> Value {
    let mut out = doc.clone();
    for k in keys::KEYS.iter().filter(|k| k.secret) {
        let mut parts: Vec<&str> = k.name.split('.').collect();
        let last = parts.pop().expect("a key has a name");
        let parent = parts.into_iter().try_fold(&mut out, |v, p| v.get_mut(p));
        if let Some(Value::Object(m)) = parent {
            if let Some(v) = m.get_mut(last) {
                if !v.is_null() {
                    *v = Value::String(HIDDEN.into());
                }
            }
        }
    }
    out
}

/// `get`: the whole file, or one value. A string prints bare, anything else
/// as JSON; an unset key says so and names its default.
pub fn get(doc: &Value, name: Option<&str>, reveal: bool) -> Result<String, String> {
    let Some(name) = name else {
        let shown = if reveal { doc.clone() } else { masked(doc) };
        return Ok(serde_json::to_string_pretty(&shown).unwrap_or_default());
    };
    let key = known(name)?;
    match lookup(doc, name) {
        None | Some(Value::Null) => Ok(format!("(not set — {})", key.about)),
        Some(_) if key.secret && !reveal => Ok(format!("{HIDDEN} (--reveal shows it)")),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v) => Ok(serde_json::to_string(v).unwrap_or_default()),
    }
}

/// Refuses a document `tower_config::load` would throw away.
fn still_loads(doc: &Value) -> Result<(), String> {
    serde_json::from_value::<Config>(doc.clone())
        .map(|_| ())
        .map_err(|e| format!("the result would not load ({e}), so nothing was written"))
}

/// `set`: the value, parsed by the key's type, at the dotted key.
pub fn set(doc: &mut Value, name: &str, raw: &str) -> Result<&'static Key, String> {
    let key = known(name)?;
    let value = keys::parse(key.kind, raw).map_err(|e| format!("{name}: {e}"))?;
    if !doc.is_object() {
        return Err("config.json does not hold a JSON object".into());
    }
    let mut next = doc.clone();
    let mut parts: Vec<&str> = name.split('.').collect();
    let last = parts.pop().expect("a key has a name");
    let mut at = &mut next;
    for p in parts {
        let obj = at
            .as_object_mut()
            .expect("checked above, and every step inserts an object");
        let child = obj.entry(p).or_insert_with(|| Value::Object(Map::new()));
        if !child.is_object() {
            return Err(format!("{p} in config.json is not an object"));
        }
        at = child;
    }
    at.as_object_mut()
        .expect("an object")
        .insert(last.to_string(), value);
    still_loads(&next)?;
    *doc = next;
    Ok(key)
}

/// `unset`: back to the default. An emptied section goes too.
pub fn unset(doc: &mut Value, name: &str) -> Result<bool, String> {
    known(name)?;
    let mut next = doc.clone();
    let parts: Vec<&str> = name.split('.').collect();
    let removed = match parts.as_slice() {
        [one] => next.as_object_mut().and_then(|m| m.remove(*one)).is_some(),
        [section, one] => {
            let removed = next
                .get_mut(*section)
                .and_then(Value::as_object_mut)
                .and_then(|m| m.remove(*one))
                .is_some();
            let empty = next
                .get(*section)
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty);
            if empty {
                next.as_object_mut().map(|m| m.remove(*section));
            }
            removed
        }
        _ => false,
    };
    still_loads(&next)?;
    *doc = next;
    Ok(removed)
}

/// `keys`: every setting, its type, when it applies, and what it is.
pub fn keys_text() -> String {
    let mut out = String::new();
    for k in keys::KEYS {
        out.push_str(&format!(
            "{:<22} {:<13} {}{}\n{:<36} applies {}\n",
            k.name,
            k.kind.label(),
            k.about,
            if k.secret { " [secret]" } else { "" },
            "",
            k.applies
        ));
    }
    out
}

/// Reads the file: missing is an empty object; unreadable or not JSON is an
/// error, never an empty object — writing over it would lose what is there.
pub fn read(path: &Path) -> Result<Value, String> {
    match std::fs::read(path) {
        Ok(body) => serde_json::from_slice(&body).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}) — fix it by hand; nothing was changed",
                path.display()
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Writes the file atomically, keeping its mode, `0600` for a new one — it
/// holds webhook URLs.
///
/// The temp file is this write's own (`.config.json.tmp-<pid>-<n>`, created
/// with `create_new`): with one fixed name, two writers truncated and renamed
/// each other's file (#123), as the state store once did (`write_atomic`).
pub fn write(path: &Path, doc: &Value) -> io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777)
    };
    let name = path
        .file_name()
        .map_or("config.json".into(), |n| n.to_string_lossy());
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
    let mut body = serde_json::to_vec_pretty(doc)?;
    body.push(b'\n');
    let written = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, mode);
        let mut f = opts.open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        // The umask may have narrowed the mode at creation; set it exactly.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Runs one read → change → write with `<file>.lock` held, so two `config
/// set`s at once (two MCP `tower` calls, say) cannot drop each other's edit:
/// the second reads what the first wrote (#123).
fn locked<T>(path: &Path, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = path
        .file_name()
        .map_or("config.json".into(), |n| n.to_string_lossy());
    let lock_path = dir.join(format!(".{name}.lock"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let lock = opts
        .open(&lock_path)
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    let out = f();
    drop(lock);
    out
}

/// The command, against the file at `path`.
pub fn run(w: &mut dyn Write, path: &Path, args: &[String]) -> Result<(), String> {
    let io = |e: io::Error| e.to_string();
    let reveal = args.iter().any(|a| a == "--reveal");
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--reveal")
        .collect();
    match words.as_slice() {
        [] | ["get"] => {
            let doc = read(path)?;
            writeln!(w, "{}", get(&doc, None, reveal)?).map_err(io)
        }
        ["get", name] => {
            let doc = read(path)?;
            writeln!(w, "{}", get(&doc, Some(name), reveal)?).map_err(io)
        }
        ["set", name, value] => {
            let (doc, key) = locked(path, || {
                let mut doc = read(path)?;
                let key = set(&mut doc, name, value)?;
                write(path, &doc).map_err(io)?;
                Ok((doc, key))
            })?;
            let shown = if key.secret {
                HIDDEN.to_string()
            } else {
                get(&doc, Some(name), true)?
            };
            writeln!(w, "{name} = {shown}  (applies {})", key.applies).map_err(io)
        }
        ["unset", name] => {
            let removed = locked(path, || {
                let mut doc = read(path)?;
                let removed = unset(&mut doc, name)?;
                if removed {
                    write(path, &doc).map_err(io)?;
                }
                Ok(removed)
            })?;
            if removed {
                let key = known(name)?;
                writeln!(
                    w,
                    "{name} unset — back to its default  (applies {})",
                    key.applies
                )
                .map_err(io)
            } else {
                writeln!(w, "{name} was not set").map_err(io)
            }
        }
        ["keys"] => write!(w, "{}", keys_text()).map_err(io),
        ["path"] => writeln!(w, "{}", path.display()).map_err(io),
        ["set", ..] => Err(
            "usage: herdr-tower config set <key> <value> — one value; quote it if it has spaces"
                .into(),
        ),
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn run_in(path: &Path, a: &[&str]) -> Result<String, String> {
        let mut out = Vec::new();
        run(&mut out, path, &args(a))?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn set_writes_a_nested_key_and_keeps_everything_else() {
        let mut doc = json!({"discord_webhook_url":"https://d/x","future_key":{"a":1}});
        set(&mut doc, "mcp.public_url", "https://mac.t.ts.net").unwrap();
        set(&mut doc, "wait_renotify", "off").unwrap();
        set(&mut doc, "notify_discord", "on").unwrap();
        assert_eq!(
            doc,
            json!({
                "discord_webhook_url":"https://d/x",
                "future_key":{"a":1},
                "mcp":{"public_url":"https://mac.t.ts.net"},
                "wait_renotify":"off",
                "notify_discord":true
            })
        );
    }

    /// `config set parallax false` is how a person with Parallax installed
    /// keeps its handoffs off the tower; it has to land as a bool the
    /// supervisor reads as off, and `unset` has to give the default back.
    #[test]
    fn parallax_can_be_turned_off_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        run_in(&path, &["set", "parallax", "false"]).unwrap();
        let doc = read(&path).unwrap();
        assert_eq!(doc, json!({"parallax": false}));
        let cfg: Config = serde_json::from_value(doc).unwrap();
        assert!(!cfg.parallax_enabled());
        run_in(&path, &["unset", "parallax"]).unwrap();
        let cfg: Config = serde_json::from_value(read(&path).unwrap()).unwrap();
        assert!(cfg.parallax_enabled(), "on by default");
        assert!(keys_text().contains("parallax"));
    }

    /// Round trip: `config set update_check false` lands as a bool the
    /// release check reads as off, `true` back on, and `unset` gives the
    /// default (on) back — the opt-out the README promises.
    #[test]
    fn update_check_can_be_turned_off_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let enabled = |path: &std::path::Path| {
            serde_json::from_value::<Config>(read(path).unwrap())
                .unwrap()
                .update_check_enabled()
        };
        run_in(&path, &["set", "update_check", "false"]).unwrap();
        assert_eq!(read(&path).unwrap(), json!({"update_check": false}));
        assert!(!enabled(&path));
        run_in(&path, &["set", "update_check", "true"]).unwrap();
        assert!(enabled(&path));
        run_in(&path, &["unset", "update_check"]).unwrap();
        assert_eq!(read(&path).unwrap(), json!({}));
        assert!(enabled(&path), "on by default");
        assert!(run_in(&path, &["set", "update_check", "maybe"]).is_err());
        assert!(keys_text().contains("update_check"));
    }

    #[test]
    fn unknown_keys_and_bad_values_change_nothing() {
        let mut doc = json!({"notify_macos":false});
        let before = doc.clone();
        let e = set(&mut doc, "wait_renotfy", "5m").unwrap_err();
        assert!(e.contains("config keys"), "{e}");
        assert!(set(&mut doc, "usage_guard_5h", "lots").is_err());
        assert!(set(&mut doc, "preview_interval", "soon").is_err());
        assert!(
            set(&mut doc, "mcp", "{}").is_err(),
            "a section is not a setting"
        );
        assert_eq!(doc, before);
    }

    #[test]
    fn nothing_is_written_that_would_not_load() {
        // A hand-edited `mcp` that is not an object: setting under it must not
        // produce a file `load()` would replace with the defaults.
        let mut doc = json!({"mcp": "oops", "discord_webhook_url":"https://d/x"});
        assert!(set(&mut doc, "mcp.act", "false").is_err());
        let mut doc = json!({"notify_macos": "yes please"});
        let e = set(&mut doc, "notify_discord", "true").unwrap_err();
        assert!(e.contains("would not load"), "{e}");
        assert_eq!(doc, json!({"notify_macos": "yes please"}));
    }

    #[test]
    fn secrets_stay_hidden_unless_revealed() {
        let doc = json!({"discord_webhook_url":"https://d/secret","notify_macos":true});
        let all = get(&doc, None, false).unwrap();
        assert!(!all.contains("secret") && all.contains(HIDDEN), "{all}");
        assert!(get(&doc, None, true).unwrap().contains("https://d/secret"));
        assert!(get(&doc, Some("discord_webhook_url"), false)
            .unwrap()
            .starts_with(HIDDEN));
        assert_eq!(
            get(&doc, Some("discord_webhook_url"), true).unwrap(),
            "https://d/secret"
        );
        assert_eq!(get(&doc, Some("notify_macos"), false).unwrap(), "true");
        assert!(get(&doc, Some("wait_renotify"), false)
            .unwrap()
            .starts_with("(not set"));
    }

    #[test]
    fn unset_returns_a_key_to_its_default_and_drops_an_empty_section() {
        let mut doc = json!({"mcp":{"act":false},"dock_keep":false});
        assert!(unset(&mut doc, "mcp.act").unwrap());
        assert!(unset(&mut doc, "dock_keep").unwrap());
        assert_eq!(doc, json!({}));
        assert!(!unset(&mut doc, "dock_keep").unwrap());
        assert!(unset(&mut doc, "nonsense").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_round_trip_keeps_its_mode_and_starts_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.json");
        let out = run_in(&path, &["set", "mcp.public_url", "https://mac.t.ts.net"]).unwrap();
        assert_eq!(
            out,
            "mcp.public_url = https://mac.t.ts.net  (applies when the supervisor restarts (herdr-tower mcp on, or daemon install))\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            run_in(&path, &["set", "notify_macos", "false"]).unwrap();
            assert_eq!(mode(&path), 0o640, "an existing file keeps its mode");
        }
        let loaded: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            loaded.mcp.public_url(),
            Some("https://mac.t.ts.net".to_string())
        );
        assert_eq!(loaded.notify_macos, Some(false));
        assert_eq!(run_in(&path, &["get", "notify_macos"]).unwrap(), "false\n");
        assert_eq!(
            run_in(&path, &["path"]).unwrap(),
            format!("{}\n", path.display())
        );
    }

    #[test]
    fn a_secret_set_is_not_echoed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let out = run_in(&path, &["set", "notify_webhook_url", "https://d/secret"]).unwrap();
        assert!(!out.contains("secret") && out.contains(HIDDEN), "{out}");
    }

    #[test]
    fn usage_guard_zero_is_saved_as_off_and_positive_values_enable_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        for key in keys::KEYS
            .iter()
            .filter(|k| k.name.starts_with("usage_guard_"))
        {
            for (raw, stored, expected) in [
                ("0", "off", None),
                ("1", "1", Some(1)),
                ("3%", "3", Some(3)),
                ("100", "100", Some(100)),
                ("0%", "off", None),
            ] {
                run_in(&path, &["set", key.name, raw]).unwrap();
                assert_eq!(run_in(&path, &["get", key.name]).unwrap().trim(), stored);
                let c: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let tail = key.name.strip_prefix("usage_guard_").unwrap();
                let (vendor, window) = tail.split_once('_').unwrap_or(("claude", tail));
                assert_eq!(
                    c.usage_guard_for(vendor, window),
                    expected,
                    "{} {raw}",
                    key.name
                );
            }
        }
    }

    #[test]
    fn a_file_that_is_not_json_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not json").unwrap();
        let e = run_in(&path, &["set", "notify_macos", "false"]).unwrap_err();
        assert!(e.contains("not valid JSON"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn keys_lists_every_setting() {
        let text = keys_text();
        for k in keys::KEYS {
            assert!(text.contains(k.name), "{}", k.name);
        }
        assert!(text.contains("[secret]"));
    }

    #[test]
    fn a_stray_word_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert!(run_in(&path, &["set", "notify_macos"]).is_err());
        assert!(run_in(&path, &["set", "preview_interval", "1s", "2s"]).is_err());
        assert!(run_in(&path, &["frobnicate"]).is_err());
        assert!(!path.exists());
    }

    /// A valid value for each kind, as a person would type it.
    fn sample(kind: keys::Kind) -> &'static str {
        match kind {
            keys::Kind::Text => "https://mac.tail.ts.net",
            keys::Kind::Bool => "on",
            keys::Kind::Duration => "2s",
            keys::Kind::DurationOrOff => "off",
            keys::Kind::PercentOrOff => "85%",
            keys::Kind::TextList => "https://a/cb, https://b/cb",
            keys::Kind::TextMap => r#"{"123":"general"}"#,
        }
    }

    /// Round trip for every setting: what `get` prints can be given back to
    /// `set`, and the file comes out the same.
    #[test]
    fn what_get_prints_sets_the_same_value_again() {
        for k in keys::KEYS {
            let mut first = json!({});
            set(&mut first, k.name, sample(k.kind)).unwrap_or_else(|e| panic!("{}: {e}", k.name));
            let printed = get(&first, Some(k.name), true).unwrap();
            let mut second = json!({});
            set(&mut second, k.name, &printed)
                .unwrap_or_else(|e| panic!("{}: {printed:?}: {e}", k.name));
            assert_eq!(first, second, "{}", k.name);
            // And the result is a Config the tower loads.
            serde_json::from_value::<Config>(first).unwrap();
        }
    }

    /// set then unset leaves the document as it was, for every setting.
    #[test]
    fn set_then_unset_is_a_no_op_for_every_setting() {
        let original = json!({"future_key": [1, 2], "notify_macos": false});
        for k in keys::KEYS.iter().filter(|k| k.name != "notify_macos") {
            let mut doc = original.clone();
            set(&mut doc, k.name, sample(k.kind)).unwrap();
            assert_ne!(doc, original, "{}", k.name);
            assert!(unset(&mut doc, k.name).unwrap(), "{}", k.name);
            assert_eq!(doc, original, "{}", k.name);
        }
    }

    /// Setting the same value twice is the same as once.
    #[test]
    fn set_is_idempotent() {
        for k in keys::KEYS {
            let mut once = json!({});
            set(&mut once, k.name, sample(k.kind)).unwrap();
            let mut twice = once.clone();
            set(&mut twice, k.name, sample(k.kind)).unwrap();
            assert_eq!(once, twice, "{}", k.name);
        }
    }
}
