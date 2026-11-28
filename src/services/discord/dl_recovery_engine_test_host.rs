//! DL `recovery_engine`·`inflight`·`outbound`·placeholder 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "recovery_engine/manual_rebind/adoption_notice_pg_tests.rs"]
mod adoption_notice_pg_tests;

#[cfg(unix)]
#[path = "recovery_engine/herdr_row_end_rig_tests.rs"]
mod herdr_row_end_rig_tests;

#[path = "placeholder_sweeper/retirement_tests.rs"]
mod placeholder_retirement_tests;

#[cfg(unix)]
#[path = "recovery_engine/producer_resume/entry_tests.rs"]
mod producer_resume_entry_tests;

#[path = "outbound/turn_output_controller/raw_request_tests.rs"]
mod raw_request_tests;

#[path = "outbound/confirmation/terminal_notice_tests.rs"]
mod terminal_notice_tests;
