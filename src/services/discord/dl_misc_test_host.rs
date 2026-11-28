//! DL 나머지 영역(`host_defer_gate`·`agentdesk_config`·`queue_dispatch`·`tmux`·`gateway`·`session_*`·`tui_o`·`claude::herdr_turn` 등) 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[cfg(unix)]
#[path = "queue_dispatch/canary_workspace_tests.rs"]
mod canary_workspace_tests;

#[path = "mailbox_finish/closed_actor_tests.rs"]
mod mailbox_closed_actor_tests;

#[cfg(unix)]
#[path = "../claude/herdr_turn/first_input/clear_held_tests.rs"]
mod claude_herdr_first_input_clear_held;

#[cfg(unix)]
#[path = "../claude/herdr_turn/first_input/hold_tests.rs"]
mod claude_herdr_first_input_hold;

#[cfg(unix)]
#[path = "../claude/herdr_turn/first_input/unproven_tests.rs"]
mod claude_herdr_first_input_unproven;

#[cfg(unix)]
#[path = "krx_claude_tests/claude_readiness_tests.rs"]
mod claude_readiness_tests;

#[cfg(unix)]
#[path = "krx_claude_tests/herdr_first_input_tests.rs"]
mod herdr_first_input_tests;

#[path = "../tui_o/exact_pg_tests.rs"]
mod tui_o_exact_pg_tests;

#[cfg(unix)]
#[path = "tmux_restart_handoff_mailbox_tests.rs"]
mod tmux_restart_handoff_mailbox_tests;

#[path = "tui_direct_abort_marker/retirement_census_tests.rs"]
mod abort_marker_retirement_census_tests;
