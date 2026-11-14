//! The Discord surface of a routine agent turn. A run claimed as headless shares its agent's
//! primary channel as an isolated process turn; every other run keeps its thread or DM.
use super::*;

/// Launch metadata key the headless turn start reads to isolate the run from its channel.
const RUN_MODE_KEY: &str = "routine_run_mode";

/// Whether `run_id` was claimed as headless. The claim snapshot decides, not the live routine,
/// and an unreadable snapshot fails the start instead of taking another surface.
pub(super) async fn headless(store: &RoutineStore, run_id: &str) -> Result<bool> {
    Ok(store.run_execution_mode(run_id).await?.as_deref() == Some("headless"))
}

/// A headless run always runs as a process turn on the primary channel, never in a DM.
pub(super) fn headless_backend(
    provider: &ProviderKind,
    dm_user_id: Option<&str>,
) -> Result<execution::AgentExecutionMode> {
    if dm_user_id.is_some() {
        return Err(anyhow!("a headless routine run cannot target a DM"));
    }
    let process = execution::AgentExecutionMode::Process;
    process.ensure_provider(provider)?;
    Ok(process)
}

/// Marks the launch so its turn is isolated from the channel session and posts no body.
pub(super) fn mark_headless(metadata: &mut Value) {
    metadata[RUN_MODE_KEY] = json!("headless");
    metadata["silent"] = json!(true);
}
