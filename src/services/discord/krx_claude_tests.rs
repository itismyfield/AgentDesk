//! KRX (claude/codex) tests that call discord-layer items, declared here so they leave the KRX crate.
//! Each `#[path]` file stays in place and keeps the cfg chain of its original declaration.

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
#[path = "../claude_tui/host_input/busy/herdr_tests.rs"]
mod claude_tui_busy_herdr_observation;
#[cfg(unix)]
#[path = "../codex/herdr_turn/restart_resume_tests.rs"]
mod codex_herdr_turn_restart_resume;
#[cfg(unix)]
mod herdr_first_input_tests;
#[cfg(unix)]
mod herdr_handoff_tests;
#[cfg(unix)]
mod pinned_tmux_tests;
