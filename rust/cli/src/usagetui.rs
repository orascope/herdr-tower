// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! The usage board as the tower shows it ([`tower_ui::Usage`]): a line under
//! its header, and the whole board, sliders and all, on `u`.
//!
//! The figures are read on the tower's usage thread ([`TowerUsage::refresh`])
//! and kept here; drawing and changing a floor work from what was last read,
//! so neither waits on a network call. The guard's floors are read from the
//! config at each draw, so a change — here or by `config set` — shows at once.

use std::path::PathBuf;
use std::sync::Mutex;

use time::UtcOffset;
use tower_ui::{Adjust, UsageFrame};

use crate::usagecmd::{self, Board, Key, SliderRow};

pub struct TowerUsage {
    home: PathBuf,
    state_dir: PathBuf,
    offset: UtcOffset,
    board: Mutex<Option<Board>>,
}

impl TowerUsage {
    pub fn new(home: PathBuf, state_dir: PathBuf, offset: UtcOffset) -> TowerUsage {
        TowerUsage {
            home,
            state_dir,
            offset,
            board: Mutex::new(None),
        }
    }

    /// The board as last read, with the floors as the config has them now.
    fn current(&self) -> Option<Board> {
        let mut b = self
            .board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()?;
        b.guard = crate::usageguard::Guard::from_config(&tower_config::load());
        Some(b)
    }
}

/// The frame for a board: the summary line, the board's lines, and the
/// lines (0-based) its sliders are on.
pub fn frame(board: &Board, offset: UtcOffset, editing: Option<usize>) -> UsageFrame {
    let drawn = usagecmd::render_editing(board, offset, true, editing);
    UsageFrame {
        summary: usagecmd::summary_line(board, offset, true),
        board: drawn.text.lines().map(String::from).collect(),
        sliders: drawn
            .sliders
            .iter()
            .map(|s| usize::from(s.row.saturating_sub(1)))
            .collect(),
    }
}

/// A floor after a change: the arrow keys' 1% steps, `x`, or where a click
/// landed on the slider.
pub fn next_floor(now: Option<u8>, how: Adjust) -> Option<u8> {
    let key = match how {
        Adjust::Less => Key::Left,
        Adjust::More => Key::Right,
        Adjust::Toggle => Key::Toggle,
        Adjust::At(col) => return usagecmd::floor_at_column(col, usagecmd::BAR_WIDTH),
    };
    usagecmd::step_floor(now, key, tower_config::DEFAULT_USAGE_GUARD)
}

impl tower_ui::Usage for TowerUsage {
    fn refresh(&self, force: bool) {
        let b = if force {
            crate::usagefetch::board_refreshed(&self.home, &self.state_dir)
        } else {
            crate::usagefetch::board(&self.home, &self.state_dir)
        };
        *self.board.lock().unwrap_or_else(|p| p.into_inner()) = Some(b);
    }

    fn render(&self, editing: Option<usize>) -> UsageFrame {
        match self.current() {
            Some(b) => frame(&b, self.offset, editing),
            None => UsageFrame::default(),
        }
    }

    fn adjust(&self, slider: usize, how: Adjust) -> Result<(), String> {
        let b = self.current().ok_or("the usage figures are not read yet")?;
        let drawn = usagecmd::render_editing(&b, self.offset, false, Some(slider));
        let s: &SliderRow = drawn
            .sliders
            .get(slider)
            .ok_or("that slider is not on the board any more")?;
        let now = b.guard.floors(s.vendor).get(s.window);
        let next = next_floor(now, how);
        if next == now {
            return Ok(());
        }
        usagecmd::save_floor(s, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_steps_the_floor_or_sets_it_where_the_click_landed() {
        assert_eq!(next_floor(Some(30), Adjust::Less), Some(29));
        assert_eq!(next_floor(Some(30), Adjust::More), Some(31));
        assert_eq!(next_floor(Some(1), Adjust::Less), None);
        assert_eq!(next_floor(None, Adjust::More), Some(1));
        assert_eq!(next_floor(Some(30), Adjust::Toggle), None);
        assert_eq!(
            next_floor(None, Adjust::Toggle),
            Some(tower_config::DEFAULT_USAGE_GUARD)
        );
        // The bar's first cell is column 7: all the way left spends nothing.
        assert_eq!(next_floor(None, Adjust::At(usagecmd::BAR_COL)), Some(100));
        assert_eq!(
            next_floor(Some(1), Adjust::At(usagecmd::BAR_COL + 20)),
            None
        );
        assert_eq!(
            next_floor(None, Adjust::At(usagecmd::BAR_COL + 19)),
            Some(5)
        );
    }
}
