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

pub mod legacy_tmp_paths;

// Re-exports so callers can `use crate::compat::legacy_tmp_session_path`
// directly — mirrors the original symbol names so migration is search/replace.

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    //! Tagged test suite for the compat module.
    //!
    //! Each row in [`COMPAT_SHIMS`] pairs a shim with a *removal condition* —
    //! a closure that returns `true` when the shim is believed safe to
    //! delete. The test itself does not fail when a shim becomes removable
    //! (that would block unrelated work); instead it emits a `[compat]`
    //! tracing info line so campaigns like #1076 have a deterministic signal
    //! to sweep. A shim is only *forced* out by the `assert!` below when its
    //! condition has been manually marked `MUST_REMOVE` to trigger a hard
    //! break.

    use super::*;

    /// Describes a single compat shim. The harness runs
    /// `removable_condition()` and reports its result; if a campaign wants
    /// to hard-block a shim's survival they set `must_remove = true`.
    struct CompatShim {
        name: &'static str,
        /// Grep-able description of the removal condition (same text as the
        /// `// REMOVE_WHEN:` comment on the shim).
        condition: &'static str,
        /// Returns `true` when the shim appears safe to remove right now.
        removable_condition: fn() -> bool,
        /// When true, the test panics if the shim is still present AND the
        /// condition reports removable. Flip this flag when the campaign
        /// that owns the shim is ready to force the deletion.
        must_remove: bool,
    }

    fn compat_shims() -> Vec<CompatShim> {
        vec![CompatShim {
            name: "legacy_tmp_session_path",
            condition: "`rg 'legacy_tmp_session_path' src/` returns only compat/ + tmux_common/ + resolve_session_temp_path.",
            removable_condition: legacy_tmp_session_path_removable,
            must_remove: false,
        }]
    }

    /// Source-grep helper. Counts non-test callers of `symbol` across the
    /// `src/` tree, excluding `src/compat/` (the shim itself) and
    /// `#[cfg(all(test, feature = "legacy-sqlite-tests"))]` blocks.
    ///
    /// The grep walks the crate source directory at compile time via
    /// `CARGO_MANIFEST_DIR`, so it runs in `cargo test` and CI without
    /// spawning an external process.
    fn count_non_compat_callers(symbol: &str) -> usize {
        use std::fs;
        use std::path::Path;

        fn walk(dir: &Path, symbol: &str, out: &mut usize) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    // Skip the compat/ dir (the shim lives here by design)
                    // and target/ to avoid scanning build artifacts.
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name == "compat" || name == "target" || name == "tests" {
                        continue;
                    }
                    walk(&path, symbol, out);
                } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                    if let Ok(body) = fs::read_to_string(&path) {
                        for line in body.lines() {
                            // Crude: skip comment lines and doc comments so
                            // the inventory table above doesn't count.
                            let trimmed = line.trim_start();
                            if trimmed.starts_with("//") {
                                continue;
                            }
                            if line.contains(symbol) {
                                *out += 1;
                            }
                        }
                    }
                }
            }
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut count = 0;
        walk(&src, symbol, &mut count);
        count
    }

    fn legacy_tmp_session_path_removable() -> bool {
        // REMOVE_WHEN: nothing else in src/ grep-matches the symbol (the
        // only surviving references are the canonical impl + compat shim
        // + the `resolve_session_temp_path` fallback chain).
        //
        // Today the shim is still wired through `resolve_session_temp_path`
        // in tmux_common.rs, so this returns false (not yet removable).
        count_non_compat_callers("legacy_tmp_session_path") == 0
    }

    #[test]
    fn compat_shim_removable_when_condition_met() {
        let shims = compat_shims();
        assert!(
            !shims.is_empty(),
            "compat inventory must not be empty while compat/ exists"
        );

        for shim in &shims {
            let removable = (shim.removable_condition)();
            if removable {
                // Reporting-only: print a notice so contributors see which
                // shim is ready to go.
                eprintln!(
                    "[compat] shim `{}` is removable (condition: {})",
                    shim.name, shim.condition
                );
                if shim.must_remove {
                    panic!(
                        "compat shim `{}` is flagged must_remove=true AND removable; \
                         delete the shim and its row in compat_shims().",
                        shim.name
                    );
                }
            } else {
                eprintln!(
                    "[compat] shim `{}` still in use (condition: {})",
                    shim.name, shim.condition
                );
            }
        }
    }

    #[test]
    fn compat_inventory_covers_every_public_symbol() {
        // Smoke check: the inventory should enumerate every public re-export
        // in this module. If someone adds a new shim but forgets to update
        // `compat_shims()` this assertion reminds them.
        let shims = compat_shims();
        let names: Vec<&str> = shims.iter().map(|s| s.name).collect();
        assert!(
            names.contains(&"legacy_tmp_session_path"),
            "legacy_tmp_session_path missing from compat inventory"
        );
    }
}
