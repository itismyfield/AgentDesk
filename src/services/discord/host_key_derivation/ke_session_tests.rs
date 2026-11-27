//! KE agent-channel and session-activity tests that call discord fixtures, declared here so they leave the KE
//! crate. Each `#[path]` file stays in place and keeps the cfg chain of its original declaration.

#[cfg(unix)]
#[path = "../../agent_channels_reload_pg_tests.rs"]
mod agent_channels_reload_pg;
#[cfg(unix)]
#[path = "../../session_activity_heartbeat_tests.rs"]
mod session_activity_heartbeat;
