// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The dock: a workspace that holds only the tower, pinned to the top of herdr's
//! sidebar so it is always the first thing a click can reach.
//!
//! herdr has no notion of a pinned workspace, and the order changes whenever
//! someone drags a workspace or a tool moves one. So the tower running in the
//! dock puts it back, on a short clock — see [`crate::Tower::hold_dock`].

use std::time::Duration;

use herdrx::Workspace;

/// The label the dock workspace carries.
///
/// 🗼, a control tower, on both sides of the name. Every mark before it was
/// learned by looking at herdr's sidebar on a dark theme:
///
/// - 🎼 is drawn black-on-transparent by the emoji font, and nearly vanished
///   against the dark sidebar. Its width was never the problem.
/// - ❖ is not in the usual programming fonts, so WezTerm borrowed it from
///   Menlo and stretched a borrowed square glyph over the next cell only when
///   a space followed it — the left diamond drew larger than the right.
/// - 🗼 is a colour emoji, red on any background, and wide in every width
///   table, so the name sits the same in the sidebar and in the tab.
///
/// Marked so it cannot be mistaken for a workspace named after a checkout of
/// this repository, which is what a bare `herdr-tower` would collide with —
/// and the dock types commands into shells it finds there. Upper case, so the
/// name reads as a heading over the workspaces below it.
///
/// A dock still wearing an earlier spelling is renamed rather than rebuilt only
/// while its tower is alive: that makes it a mislabeled dock, which the
/// supervisor takes back (`dockkeep::dock_to_reclaim`). One whose tower is gone
/// is not recognised at all — the old spelling is not kept as an alias, by the
/// same no-compatibility rule as the rename to herdr-tower — so `dock` builds a
/// new one and the old workspace is left to close by hand.
pub const DOCK_LABEL: &str = "🗼 HERDR-TOWER 🗼";

/// How long a moved dock may stay out of first place.
pub const DOCK_PIN_INTERVAL: Duration = Duration::from_secs(1);

/// Whether `workspace_id` is the dock and is not at the top.
///
/// Keyed on the ID *and* the label, because the tower asks about the workspace
/// its own pane is in: a tower started in an ordinary workspace must not drag
/// that workspace to the top.
pub fn needs_pin(workspaces: &[Workspace], workspace_id: &str) -> bool {
    workspaces
        .iter()
        .position(|w| w.workspace_id == workspace_id && w.label == DOCK_LABEL)
        .is_some_and(|i| i > 0)
}

/// Whether `workspace_id` is the dock at all, wherever it currently sits.
///
/// The same ID-and-label pair as [`needs_pin`], for the decisions that hold
/// whether or not the dock has been dragged out of first place.
pub fn in_dock(workspaces: &[Workspace], workspace_id: &str) -> bool {
    workspaces
        .iter()
        .any(|w| w.workspace_id == workspace_id && w.label == DOCK_LABEL)
}

/// Whether the dock's tab is wearing a name other than the dock's.
///
/// An unnamed herdr tab is drawn from its number or from the command running in
/// it, so the dock's tab reads "1", or the tower's own name, until something
/// renames it. The tower puts [`DOCK_LABEL`] back on the same short clock that
/// puts the workspace back at the top.
pub fn needs_tab_label(label: &str) -> bool {
    label != DOCK_LABEL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: &str, label: &str) -> Workspace {
        serde_json::from_value(serde_json::json!({ "workspace_id": id, "label": label })).unwrap()
    }

    /// The same mark on both sides, and the same width in every locale. An
    /// ambiguous-width mark draws as one cell in some terminals and two in
    /// others, which shears the sidebar; 🗼 is wide in both tables.
    #[test]
    fn the_dock_is_framed_by_one_mark_of_one_width() {
        let first = DOCK_LABEL.chars().next().unwrap();
        let last = DOCK_LABEL.chars().last().unwrap();
        assert_eq!(first, last, "the same mark on both sides");
        assert_eq!(unicode_width::UnicodeWidthChar::width(first), Some(2));
        assert_eq!(unicode_width::UnicodeWidthChar::width_cjk(first), Some(2));
    }

    /// 🚨 Not a mark that was already tried and seen to fail in the sidebar —
    /// see [`DOCK_LABEL`] for what each one did.
    #[test]
    fn the_dock_mark_is_none_that_failed_before() {
        let mark = DOCK_LABEL.chars().next().unwrap();
        for failed in ['🎼', '❖'] {
            assert_ne!(mark, failed);
        }
    }

    #[test]
    fn a_dock_below_the_top_is_moved() {
        let list = [ws("w2", "00.dbstore"), ws("w9", DOCK_LABEL)];
        assert!(needs_pin(&list, "w9"));
    }

    #[test]
    fn a_dock_at_the_top_is_left_alone() {
        let list = [ws("w9", DOCK_LABEL), ws("w2", "00.dbstore")];
        assert!(!needs_pin(&list, "w9"));
    }

    #[test]
    fn a_tower_outside_the_dock_moves_nothing() {
        let list = [ws("w2", "00.dbstore"), ws("w8", "20.herdr-tower")];
        assert!(!needs_pin(&list, "w8"));
    }

    #[test]
    fn a_workspace_that_is_gone_moves_nothing() {
        let list = [ws("w2", "00.dbstore")];
        assert!(!needs_pin(&list, "w9"));
    }

    #[test]
    fn the_dock_is_recognized_wherever_it_sits() {
        let list = [ws("w2", "00.dbstore"), ws("w9", DOCK_LABEL)];
        assert!(in_dock(&list, "w9"));
        assert!(!in_dock(&list, "w2"));
        assert!(!in_dock(&list, "w8"));
    }

    /// herdr's own fallbacks — the tab number, or the command in it — are
    /// exactly the names the dock has to replace.
    #[test]
    fn any_name_but_the_docks_is_replaced() {
        assert!(needs_tab_label("1"));
        assert!(needs_tab_label("herdr-tower"));
        assert!(needs_tab_label("tower"));
        assert!(!needs_tab_label(DOCK_LABEL));
        // The dock is exempt from `id_labels`: a prefix is taken off again.
        assert!(needs_tab_label("[w3B:t1] 🗼 HERDR-TOWER 🗼"));
    }
}
