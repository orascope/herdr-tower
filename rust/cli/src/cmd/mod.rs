// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Command implementations, split from the monolithic `main.rs`.
//!
//! Each submodule is one command or a group of related commands, plus
//! `common` for the shared wiring functions (store, backend, notifications)
//! and `error` for the single error type.

pub mod card;
pub mod common;
pub mod config;
pub mod dock;
pub mod error;
pub mod hook;
pub mod init;
pub mod mcp;
pub mod rest;
pub mod status;
pub mod tui;
pub mod usage;

pub use card::run_card;
pub use config::run_config;
pub use dock::run_dock;
pub use error::{CliError, Result};
pub use hook::{run_hook, run_notify};
pub use init::run_init;
pub use mcp::run_mcp;
pub use rest::{
    run_all, run_board, run_close, run_daemon, run_doctor, run_host, run_info, run_interrupt,
    run_loop, run_open, run_report, run_restore, run_resume, run_schedule, run_tree,
    run_workspaces, run_wt,
};
pub use status::run_status;
pub use tui::run_tui;
pub use usage::run_usage;
