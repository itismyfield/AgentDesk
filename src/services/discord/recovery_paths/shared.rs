//! Cross-path recovery helpers (issue #1074).
//!
//! This module collects helpers that the three recovery paths (restart /
//! runtime / manual rebind) all need. It intentionally starts very small —
//! the goal of issue #1074's first landing is to create the SSoT surface and
//! migration target, not to relocate every helper at once.
//!
//! Helpers that live here must be:
//!   - pure or nearly pure (no lifecycle state mutation),
//!   - used by at least two of the three paths, or
//!   - explicitly documented as the canonical owner (e.g. inflight cleanup).
//!
//! See `docs/recovery-paths.md` for the path contract.

use crate::services::provider::ProviderKind;

/// Canonical inflight cleanup entry point for all recovery paths.
///
/// Thin re-export around [`crate::services::discord::clear_inflight_by_tmux_name`]
/// (which in turn delegates to [`crate::services::discord::inflight::clear_inflight_by_tmux_name`]).
///
/// Recovery paths that need to drop an inflight file because a tmux session
/// is gone or being replaced MUST call this wrapper rather than reaching into
/// `std::fs::remove_file` or reimplementing the directory scan. Past
/// duplication of this helper (two copies existed before #1074) is the
/// reason we now centralize.
#[allow(dead_code)] // consumed by future recovery::{restart,runtime,manual_rebind} modules
pub(in crate::services::discord) fn clear_inflight_by_tmux_name(
    provider: &ProviderKind,
    tmux_name: &str,
) -> bool {
    super::super::clear_inflight_by_tmux_name(provider, tmux_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Delegation smoke test: the `recovery_paths::shared` wrapper must reach
    /// the inflight SSoT without panicking, and must return `false` for a tmux
    /// name that does not exist in any inflight directory. This exercises the
    /// full delegation chain:
    ///   `recovery_paths::shared` → `discord::clear_inflight_by_tmux_name`
    ///   → `discord::inflight::clear_inflight_by_tmux_name`
    /// and pins the SSoT invariant (no caller reimplements the directory scan).
    #[test]
    fn clear_inflight_nonexistent_tmux_returns_false_via_delegation() {
        // Tmux name deliberately embeds a unique marker so it cannot collide
        // with any real inflight file in a dev environment.
        let result = clear_inflight_by_tmux_name(
            &ProviderKind::Codex,
            "AgentDesk-codex-ssot-probe-1074-nonexistent-cdx",
        );
        assert!(
            !result,
            "nonexistent tmux name must return false via the full delegation chain"
        );
    }
}
