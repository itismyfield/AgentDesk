//! DL `recovery_engine`·`inflight`·`outbound`·placeholder 시험 중 `all(test, unix)` 사슬인 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "recovery_engine/restore_inflight/replay_hold_tests.rs"]
mod replay_hold_tests;

#[path = "recovery_engine/o_aged_settlement_tests.rs"]
mod o_aged_settlement_tests;

#[path = "inflight/save_store/identity_gate/runtime_stamp/claude_terminal_tests/o_stream_regression_tests.rs"]
mod o_stream_regression_tests;

#[path = "outbound/notification_policy_tests.rs"]
mod notification_policy_tests;
