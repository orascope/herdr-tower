// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Which pane, if any, a hook is allowed to record against.
//!
//! This is the load-bearing part of the crate. Everything downstream trusts the
//! coordinate it returns, and a wrong coordinate does not fail loudly — it
//! writes a live agent's record with another process's state.

/// The pane a hook may write to, or `None` when it must record nothing.
pub fn hook_pane<F>(env: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    herdr_hook_pane(&env)
}

/// The pane ID herdr injected (`"w2:p4S"`). Its backend namespace is recorded
/// separately by [`hook_backend`], so named servers no longer need a separate
/// state store merely to avoid coordinate collisions.
fn herdr_hook_pane<F>(env: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let pane = var(env, "HERDR_PANE_ID")?;
    if var(env, "HERDR_ENV").as_deref() != Some("1") {
        return None;
    }
    Some(pane)
}

/// Stable namespace of the Herdr socket that emitted this hook.
pub fn hook_backend<F>(env: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let home = var(&env, "HOME").unwrap_or_default();
    herdrx::backend_from_env(&env, &home)
}

/// An unset variable and one set to the empty string mean the same thing here,
/// whatever the injected lookup chooses to do with them.
fn var<F>(env: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    env(key).filter(|v| !v.is_empty())
}
