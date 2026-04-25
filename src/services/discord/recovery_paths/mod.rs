//! Recovery engine module split scaffold (issue #1074, 905-5).
//!
//! `recovery_engine.rs` is ~4600 lines and hosts three logically independent
//! recovery paths:
//!
//! 1. **restart recovery** — entry: `recovery_engine::restore_inflight_turns`.
//!    Fires once during dcserver startup. Scans persisted inflight state and
//!    resumes or handoffs each turn.
//! 2. **runtime recovery** — entry: `recovery_engine::reregister_active_turn_from_inflight`.
//!    Fires mid-execution when the mailbox/runtime rediscovers an inflight
//!    state it is not currently tracking (e.g. after a hot-swap or watcher
//!    reattach triggered by a live turn).
//! 3. **manual rebind** — entry: `recovery_engine::rebind_inflight_for_channel`.
//!    Fires when an operator calls `POST /api/inflight/rebind` (or equivalent
//!    Discord slash command) to force re-association of an inflight state to
//!    a new tmux pane / watcher.
//!
//! This directory is the **landing zone** for eventually splitting those
//! three paths into `restart.rs`, `runtime.rs`, and `manual_rebind.rs`. The
//! full mechanical split is a multi-week refactor and is intentionally
//! deferred — see `docs/recovery-paths.md` for the contract each module must
//! honor once the split lands.
//!
//! For now this module only exposes [`shared`], which hosts helpers that are
//! already SSoT and will be shared across the three future modules. New
//! recovery code should add helpers here if they are cross-path, and in the
//! path-specific module otherwise.

pub(super) mod shared;
