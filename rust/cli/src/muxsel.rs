// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Getting hold of the backend, from the command line's point of view.

use herdrx::Herdr;

/// The backend, resolved from this process's environment.
///
/// Resolution goes through `tower_mux` rather than `Herdr::default()` so the
/// socket comes out of the same environment lookup as everything else that
/// consults it. Two independent lookups that can disagree is exactly the bug
/// `tower_mux::socket_from` was written to close.
pub fn default_mux() -> Herdr {
    tower_mux::select(env)
}

/// Stable namespace selected by this process's Herdr environment.
pub fn default_backend_id() -> String {
    let home = env("HOME").unwrap_or_default();
    herdrx::backend_from_env(env, &home)
}

/// The process environment, with the empty string read as "unset".
///
/// `HOME=""` is not a home directory. Without the filter the socket resolves
/// to `/.config/herdr/herdr.sock` and the failure names a socket instead of
/// the missing home.
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment reader is the whole of this module's own logic, and an
    /// empty variable must read as absent rather than as a value.
    #[test]
    fn an_empty_variable_reads_as_unset() {
        // A name no test or shell sets, so this cannot collide with a
        // neighbouring test in the same process.
        let key = "ALCLI_MUXSEL_PROBE";
        std::env::set_var(key, "");
        assert_eq!(env(key), None);
        std::env::set_var(key, "x");
        assert_eq!(env(key).as_deref(), Some("x"));
        std::env::remove_var(key);
        assert_eq!(env(key), None);
    }
}
