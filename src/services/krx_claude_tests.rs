//! KRX (claude/codex) test files that call KE items, declared here so they leave the KRX crate.
//! Each file stays in place; every `mod` keeps the cfg chain of its original declaration.

#[cfg(unix)]
#[path = "claude/first_input/tests.rs"]
mod claude_first_input;
#[cfg(unix)]
#[path = "claude/herdr_turn/first_input/tests.rs"]
pub(crate) mod claude_herdr_first_input;
#[cfg(unix)]
#[path = "codex/herdr_turn/first_input/pg_tests.rs"]
mod codex_herdr_first_input_pg;
#[cfg(unix)]
#[path = "codex/tui_session_launch/first_input/tests.rs"]
mod codex_tui_session_launch_first_input;
