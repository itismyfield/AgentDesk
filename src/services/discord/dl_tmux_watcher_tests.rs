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
#[path = "tmux_watcher/loop_poll_prologue/post_terminal_disposal_root_tests.rs"]
mod post_terminal_disposal_tests;

#[cfg(unix)]
#[path = "tmux_watcher/session_bound_ack_root_tests.rs"]
mod session_bound_ack_tests;

#[cfg(unix)]
#[path = "tmux_watcher/task_response_authority_root_tests.rs"]
mod task_response_authority_tests;

#[cfg(unix)]
#[path = "tmux_watcher/terminal_direct_fallback_root_tests.rs"]
mod terminal_direct_fallback_tests;

#[cfg(unix)]
#[path = "tmux_watcher/terminal_commit_epilogue/continuation_marker_root_tests.rs"]
mod continuation_marker_tests;

#[cfg(unix)]
#[path = "tmux_watcher/terminal_commit_epilogue/synthetic_mailbox_release_tests.rs"]
mod synthetic_mailbox_release_tests;

#[cfg(unix)]
#[path = "tmux_watcher/streaming_status_tick/native_collector_tests.rs"]
mod native_collector_tests;

#[cfg(unix)]
#[path = "tmux_watcher/o_evidence_ack_tests.rs"]
mod o_evidence_ack_tests;

#[cfg(unix)]
#[path = "tmux_watcher/idle_sink_frontier_tests.rs"]
mod idle_sink_frontier_tests;

#[cfg(unix)]
#[path = "tmux_watcher/turn_stream_collector/chunk_forward_tests.rs"]
mod chunk_forward_tests;

#[cfg(unix)]
#[path = "watchers/lifecycle/watcher_install_fixture_tests.rs"]
pub(in crate::services::discord) mod watcher_install_fixture;
