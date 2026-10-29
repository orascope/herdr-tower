// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! `herdr-tower config` — see `tower_cli::configcmd`.

use super::Result;

pub fn run_config(args: &[String]) -> Result<()> {
    let path = tower_config::path()
        .ok_or("the config file has no path — neither HERDR_TOWER_CONFIG nor HOME is set")?;
    tower_cli::configcmd::run(&mut std::io::stdout().lock(), &path, args)?;
    Ok(())
}
