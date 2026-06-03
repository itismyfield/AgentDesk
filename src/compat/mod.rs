//! # `compat/` — centralised home for compatibility / legacy / fallback shims
//!
//! #1076 (905-7). The goal of this module is to make "what is still here only
//! for backward compatibility?" a single `rg -n 'REMOVE_WHEN'` query. Any
//! symbol that exists *only* to keep an older path working during a migration
//! belongs in this module (or is re-exported from here with a `REMOVE_WHEN`
//! comment pointing at the authoritative copy).
//!
//! ## Rules
//!
//! 1. Every public item in this module carries a `// REMOVE_WHEN: <condition>`
//!    comment. The condition must be a **grep-able, verifiable** statement —
//!    e.g. "no callers under `services/` reference `legacy_tmp_session_path`"
//!    — so the tagged test below can mechanically flag removable items.
//! 2. New compat paths go here from day one. Old code that already ships a
//!    legacy fallback can either migrate here or gain a `// #1076 compat note`
//!    pointer so we can move it later without churn.
//! 3. This module must never accrue real business logic. It is a holding pen
//!    for things on their way out.
//!
//! ## Current inventory
//!
//! | Shim | Purpose | Removal condition |
//! | --- | --- | --- |
//! | [`legacy_tmp_session_path`] | Re-export of `services::tmux_common::legacy_tmp_session_path`. Older wrappers (pre-#?? migration) hold open file descriptors under `/tmp/` — this helper lets read-side code discover those files. | #1076: all tmux wrappers have been respawned post-migration (no caller is dated before the `agentdesk_temp_dir()` switchover). Verified when `rg 'legacy_tmp_session_path'` returns only the `src/compat/` reference + this module. |
//!
//! ## Adding a shim
//!
//! 1. Create `src/compat/<name>.rs` with the re-export / wrapper.
//! 2. Add a `// REMOVE_WHEN:` comment at the top of each public item.
//! 3. Extend the [`tests::compat_shim_removable_when_condition_met`] test
//!    with a new row describing the removal condition.
//! 4. Update the inventory table above.

pub mod legacy_db_paths;
pub mod legacy_tmp_paths;

// Re-exports so callers can `use crate::compat::legacy_tmp_session_path`
// directly — mirrors the original symbol names so migration is search/replace.
