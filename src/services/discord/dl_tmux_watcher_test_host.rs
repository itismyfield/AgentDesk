//! DL `tmux_watcher`·`watchers` 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[cfg(unix)]
#[path = "tmux_watcher/adopted_run_first_output_tests.rs"]
mod adopted_run_first_output_tests;

#[cfg(unix)]
#[cfg(test)]
#[path = "tmux_watcher/judged_stop_harness_tests.rs"]
mod judged_stop_harness_tests;

#[cfg(unix)]
#[path = "tmux_watcher/o_seed_install_tests.rs"]
mod o_seed_install_tests;

#[cfg(unix)]
#[path = "tmux_watcher/o_capsule_origin_tests.rs"]
mod o_capsule_origin_tests;

#[cfg(unix)]
#[path = "tmux_watcher/read_source_switch_tests.rs"]
mod read_source_switch_tests;

#[cfg(unix)]
#[path = "tmux_watcher/receipt_watcher_harness_tests.rs"]
mod receipt_watcher_harness_tests;

#[cfg(unix)]
#[path = "tmux_watcher/terminal_commit_epilogue/synthetic_mailbox_release_tests.rs"]
mod synthetic_mailbox_release_tests;

#[cfg(unix)]
#[path = "tmux_watcher/streaming_status_tick/native_collector_tests.rs"]
mod native_collector_tests;

#[cfg(unix)]
#[path = "watchers/lifecycle/watcher_install_fixture_tests.rs"]
pub(in crate::services::discord) mod watcher_install_fixture;
