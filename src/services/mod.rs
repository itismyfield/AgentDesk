pub mod agent_protocol;
pub mod claude;
pub mod codex;
#[cfg(unix)]
pub mod codex_tmux_wrapper;
pub mod discord;
pub mod gemini;
pub mod memory;
pub mod platform;
pub mod process;
pub mod provider;
pub mod provider_exec;
pub mod qwen;
#[cfg(unix)]
pub mod qwen_tmux_wrapper;
pub mod remote_stub;
pub mod session_backend;
pub mod termination_audit;
pub mod tmux_common;
pub mod tmux_diagnostics;
#[cfg(unix)]
pub mod tmux_wrapper;

// Compatibility alias: code referencing services::remote::* uses the stub
pub use remote_stub as remote;
