//! DL `tmux_watcher`·`watchers` 시험 중 `all(test, unix)` 사슬인 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[cfg(unix)]
#[path = "tmux_watcher/codex_replacement_tests.rs"]
mod codex_replacement_tests;

#[cfg(unix)]
#[path = "watchers/lifecycle/n3bc_restore_tests.rs"]
mod n3bc_restore_tests;

#[cfg(unix)]
#[path = "watchers/lifecycle/restore_legacy_tests.rs"]
mod restore_legacy_tests;

#[cfg(unix)]
#[path = "watchers/lifecycle/resume_companion_restore_tests.rs"]
mod resume_companion_restore_tests;

#[cfg(unix)]
#[path = "tmux_watcher/redrive_reclaim_e2e_tests.rs"]
mod redrive_reclaim_e2e_tests;
