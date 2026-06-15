//! #3479 Phase-1 rank-1: tests for the supervisor relay-FORWARD half. PURE MOVE
//! from `tmux_watcher.rs`'s `#[cfg(test)] mod tests` (zero logic change). Kept in
//! a sibling `*_tests.rs` so the production module stays within the
//! `src/services/discord/tmux_watcher/**` namespace LoC cap (test files are
//! excluded from the cap by the audit's `production_rust_files()` filter).

use super::*;

#[test]
fn terminal_event_consumed_offset_excludes_buffered_tail() {
    assert_eq!(terminal_event_consumed_offset(128, "next-turn\n"), 118);
    assert_eq!(terminal_event_consumed_offset(8, "longer-than-offset"), 0);
}
