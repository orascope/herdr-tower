// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! What each CLI has pinned as its default model, and how it reads on a row.

use std::fs;
use std::path::Path;

use tower_usage::*;

fn write(home: &Path, rel: &str, body: &str) {
    let path = home.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// Claude Code resolves the local file first; so must we, or the tower reports
/// a model the CLI is not going to use.
#[test]
fn claude_local_settings_outrank_the_shared_ones() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(claude_default_model(d.path()), "", "nothing pinned yet");

    write(
        d.path(),
        ".claude/settings.json",
        r#"{"model":"claude-fable-5","theme":"dark"}"#,
    );
    assert_eq!(claude_default_model(d.path()), "claude-fable-5");

    write(
        d.path(),
        ".claude/settings.local.json",
        r#"{"model":"claude-opus-5"}"#,
    );
    assert_eq!(claude_default_model(d.path()), "claude-opus-5");

    // A local file that pins no model must fall through, or adding any
    // unrelated local setting would blank the pinned model.
    write(
        d.path(),
        ".claude/settings.local.json",
        r#"{"theme":"dark"}"#,
    );
    assert_eq!(claude_default_model(d.path()), "claude-fable-5");
}

/// A model inside `[profiles.*]` is that profile's choice. Reading past the
/// first section header would report a profile nobody selected.
#[test]
fn codex_reads_only_the_top_level_of_config_toml() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(codex_default_model(d.path()), Default::default());

    write(
        d.path(),
        ".codex/config.toml",
        "model = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"\n\n[profiles.lightweight]\nmodel = \"gpt-5.4-mini\"\n",
    );
    let cx = codex_default_model(d.path());
    assert_eq!(cx.model, "gpt-5.6-sol");
    assert_eq!(cx.effort, "high");
    assert_eq!(cx.label(), "gpt-5.6-sol high");
}

#[test]
fn codex_label_omits_an_unpinned_effort() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), ".codex/config.toml", "model = \"gpt-5.6-sol\"\n");
    assert_eq!(codex_default_model(d.path()).label(), "gpt-5.6-sol");
}

/// The settings schema changed shape between versions. Accepting only one of
/// them would blank the field on whichever half of the fleet is not on it.
#[test]
fn gemini_accepts_both_settings_schemas() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(gemini_default_model(d.path()), "");

    write(
        d.path(),
        ".gemini/settings.json",
        r#"{"model":"gemini-2.5-pro"}"#,
    );
    assert_eq!(gemini_default_model(d.path()), "gemini-2.5-pro");

    write(
        d.path(),
        ".gemini/settings.json",
        r#"{"model":{"name":"gemini-3-pro"}}"#,
    );
    assert_eq!(gemini_default_model(d.path()), "gemini-3-pro");
}

/// Where both are installed, agy is the CLI actually being run.
#[test]
fn gemini_prefers_the_antigravity_settings() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        ".gemini/settings.json",
        r#"{"model":"gemini-2.5-pro"}"#,
    );
    write(
        d.path(),
        ".gemini/antigravity-cli/settings.json",
        r#"{"model":"Gemini 3.6 Flash (Medium)"}"#,
    );
    assert_eq!(gemini_default_model(d.path()), "Gemini 3.6 Flash (Medium)");
}

/// Empty means "not pinned, the CLI will choose" — a real answer, not a gap.
#[test]
fn default_models_collects_all_three() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        ".claude/settings.json",
        r#"{"model":"claude-opus-5"}"#,
    );
    write(d.path(), ".codex/config.toml", "model = \"gpt-5.6-sol\"\n");

    let got = DefaultModels::read(d.path());
    assert_eq!(got.for_kind("claude"), "claude-opus-5");
    assert_eq!(got.for_kind("codex"), "gpt-5.6-sol");
    assert_eq!(got.for_kind("gemini"), "");
    assert_eq!(got.for_kind("something-else"), "");
}

/// An unrecognized model has to show up as itself. Anything else and a model
/// released tomorrow disappears from the tower.
#[test]
fn pretty_model_passes_through_what_it_does_not_know() {
    assert_eq!(pretty_model("gpt-5.6-sol"), "gpt-5.6-sol");
    assert_eq!(
        pretty_model("Gemini 3.6 Flash (Medium)"),
        "Gemini 3.6 Flash (Medium)"
    );
    assert_eq!(
        pretty_model("claude-opus-5-not-a-date"),
        "claude-opus-5-not-a-date"
    );
    assert_eq!(pretty_model(""), "");
}

#[test]
fn pretty_model_shortens_the_ids_it_knows() {
    for (raw, want) in [
        ("claude-fable-5", "Fable 5"),
        ("claude-fable-5[1m]", "Fable 5 (1M)"),
        ("claude-opus-5", "Opus 5"),
        ("claude-sonnet-5", "Sonnet 5"),
        ("claude-haiku-4-5-20251001", "Haiku 4.5"),
        ("default", "auto"),
        ("opusplan", "OpusPlan"),
    ] {
        assert_eq!(pretty_model(raw), want, "pretty_model({raw:?})");
    }
}

/// Pinned on purpose: the version group is greedy, so on an ID whose version
/// has no minor part the release date is swallowed as one. The quirk stays
/// until it is fixed deliberately, and this test makes that a visible change.
#[test]
fn pretty_model_keeps_the_date_quirk() {
    assert_eq!(
        pretty_model("claude-sonnet-4-20250514"),
        "Sonnet 4.20250514"
    );
}

/// Fable is the top tier, so an accidental default burns tokens fast — the
/// tower colours it as a warning and has to recognize it in every form a row
/// might be carrying.
#[test]
fn fable_is_recognized_as_an_id_or_a_display_name() {
    for s in [
        "claude-fable-5",
        "Fable 5",
        "Fable 5 (1M context)",
        "claude-fable-5[1m]",
    ] {
        assert!(is_fable(s), "is_fable({s:?})");
    }
    for s in ["Opus 5 (1M context)", "claude-sonnet-5", "", "gpt-5.6-sol"] {
        assert!(!is_fable(s), "is_fable({s:?})");
    }
}
