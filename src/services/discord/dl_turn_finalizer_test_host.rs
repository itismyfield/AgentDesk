//! DL `turn_finalizer`·`queue_io` 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "queue_io/decision_drain_tests.rs"]
mod decision_drain_tests;

#[path = "turn_finalizer/finalize/tests/residue_tests.rs"]
mod residue_tests;

#[cfg(unix)]
#[path = "queue_io/input_owner/claude_handoff/tests/actual_close_tests.rs"]
mod claude_handoff_tests;
