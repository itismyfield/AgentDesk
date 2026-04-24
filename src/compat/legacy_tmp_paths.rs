//! Legacy `/tmp/`-based tmux session file paths.
//!
//! Before the runtime-temp-dir migration (switch from `std::env::temp_dir()`
//! to `~/.adk/release/runtime/sessions/`), every watcher / wrapper wrote
//! its JSONL / FIFO / owner files under `/tmp/agentdesk-*`. Long-running
//! wrappers spawned before the migration still hold open file descriptors
//! to those paths, so read-side code (e.g. `session_usable` probes) must be
//! able to look them up until every pre-migration wrapper has exited.
//!
//! This shim forwards to `services::tmux_common::legacy_tmp_session_path`
//! so new code that *has* to read legacy paths does it through `compat::`
//! and shows up in the #1076 removal sweep.

/// Build a path to the legacy `/tmp/`-based location for a session temp file.
///
// REMOVE_WHEN: every tmux wrapper has been respawned post-runtime-dir
// migration; a `rg 'legacy_tmp_session_path' src/` returns only this shim +
// the canonical implementation in `tmux_common.rs` + the
// `resolve_session_temp_path` fallback chain. At that point, inline the
// resolve-fallback into `session_temp_path` and delete both.
pub fn legacy_tmp_session_path(session_name: &str, extension: &str) -> String {
    crate::services::tmux_common::legacy_tmp_session_path(session_name, extension)
}
