//! Deprecated-API-alias logging helper.
//!
//! Historical `/api/send`, `/api/senddm`, `/api/send_to_agent` endpoints
//! were renamed under `/api/discord/` in #1067. We kept the old paths as
//! aliases so in-flight integrations (scripts, plugins, dashboards) would
//! not break mid-flight. Every alias call is logged via this helper so we
//! can see who is still hitting the legacy URLs before deleting them.
//!
//! This shim is a re-export wrapper around the same-named function in
//! `server::routes::log_deprecated_alias`. Having it in `compat/` means a
//! single grep surfaces every legacy alias call-site at removal time.

/// Emit a `tracing::warn!` for a deprecated URL alias hit.
///
// REMOVE_WHEN: no route under `src/server/routes/domains/` still forwards
// a deprecated alias. Grep target: `rg 'log_deprecated_alias' src/server/`
// returns zero non-compat hits — at which point delete this shim and the
// mirror helper in `server::routes::mod.rs`.
pub fn log_deprecated_alias(old_path: &'static str, canonical_path: &'static str) {
    // Delegated to the mirror helper in `server::routes` so the compat shim
    // does not duplicate formatting. When the server-side copy is deleted,
    // inline the `tracing::warn!` here until this shim itself is removed.
    tracing::warn!(
        old_path,
        canonical_path,
        "deprecated API alias called; use canonical path"
    );
}
