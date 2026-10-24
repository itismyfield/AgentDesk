//! E2E smoke test suite for AgentDesk HTTP API.
//!
//! Spawns a real AgentDesk server on a random port, runs the full API lifecycle,
//! and verifies responses. No Discord integration required.

mod smoke_test;

#[path = "../../src/config/runtime_root_guard/test_process.rs"]
mod test_runtime_process;
