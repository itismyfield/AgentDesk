//! DL `turn_bridge` 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[cfg(unix)]
#[path = "turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/rowless_receipt_tests/background_child_ordering_tests.rs"]
mod background_child_ordering_tests;

#[cfg(unix)]
#[path = "turn_bridge/before_start_stop_tests.rs"]
pub(super) mod before_start_stop_tests;

#[path = "turn_bridge/entry_abort_mailbox_tests.rs"]
mod entry_abort_mailbox_tests;

#[path = "turn_bridge/headless_delivery/production_seam_tests.rs"]
mod production_seam_tests;

#[path = "turn_bridge/tmux_runtime/stop_host_tests.rs"]
mod stop_host_tests;

#[cfg(unix)]
#[path = "turn_bridge/runtime_handoff_loop/codex_replacement_tests.rs"]
mod codex_replacement_tests;
