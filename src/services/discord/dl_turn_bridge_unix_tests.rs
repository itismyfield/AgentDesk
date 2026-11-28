//! DL `turn_bridge` 시험 중 `all(test, unix)` 사슬인 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "turn_bridge/tmux_runtime/process_force_kill_tests.rs"]
mod process_force_kill_tests;

#[path = "turn_bridge/tmux_runtime/session_stop_tests.rs"]
mod session_stop_tests;

#[path = "turn_bridge/resume_pin_tests.rs"]
mod resume_pin_tests;
