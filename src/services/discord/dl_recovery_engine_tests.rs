//! DL `recovery_engine`·`inflight`·`outbound`·placeholder 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "recovery_engine/manual_rebind/adoption_notice_pg_tests.rs"]
mod adoption_notice_pg_tests;

#[path = "inflight/save_store/create_monotonic_host_tests.rs"]
mod create_monotonic_host_tests;

#[path = "outbound/handle_send_contract_tests.rs"]
mod handle_send_contract_tests;

#[cfg(unix)]
#[path = "recovery_engine/herdr_row_end_rig_tests.rs"]
mod herdr_row_end_rig_tests;

#[path = "outbound/manual_delivery/api_contract_tests.rs"]
mod manual_delivery_api_contract_tests;

#[path = "placeholder_sweeper/retirement_tests.rs"]
mod placeholder_retirement_tests;

#[path = "placeholder_sweeper/tick_tests.rs"]
mod placeholder_sweeper_tick_tests;

#[cfg(unix)]
#[path = "recovery_engine/producer_resume/entry_tests.rs"]
mod producer_resume_entry_tests;

#[path = "outbound/turn_output_controller/raw_request_tests.rs"]
mod raw_request_tests;

#[path = "inflight/rebind_reap/boot_reconcile_tests.rs"]
mod rebind_reap_boot_reconcile_tests;

#[path = "outbound/send_gate_host_tests.rs"]
mod send_gate_host_tests;

#[path = "inflight/stall_recovery_tests/synthetic_carry_forward_reclaim_tests.rs"]
mod stall_recovery_synthetic_carry_forward_reclaim_tests;

#[path = "inflight/synthetic_carry_forward_tests.rs"]
mod stall_recovery_synthetic_carry_forward_tests;

#[path = "outbound/confirmation/terminal_notice_tests.rs"]
mod terminal_notice_tests;

#[path = "recovery_engine/restore_inflight/ready_without_output_rehydration_tests.rs"]
mod ready_without_output_rehydration_tests;
