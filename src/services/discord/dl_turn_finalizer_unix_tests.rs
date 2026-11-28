//! DL `turn_finalizer`·`queue_io` 시험 중 `all(test, unix)` 사슬인 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "queue_io/cancel_backstop_tests.rs"]
mod cancel_backstop_tests;

#[path = "queue_io/owned_backstop_tests.rs"]
mod owned_backstop_tests;

#[path = "queue_io/input_owner/ingress_tests.rs"]
mod ingress_tests;

#[path = "queue_io/queued_head_inject_tests.rs"]
mod queued_head_inject_tests;

#[path = "turn_finalizer/native_input_tests.rs"]
mod native_input_tests;
