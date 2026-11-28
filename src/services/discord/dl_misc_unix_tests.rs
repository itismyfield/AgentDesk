//! DL 나머지 영역 시험 중 `all(test, unix)` 사슬인 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "session_idle_cleanup_tests.rs"]
mod session_idle_cleanup_tests;

#[path = "host_defer_gate_tests.rs"]
mod host_defer_readiness_tests;
