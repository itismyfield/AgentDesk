use super::super::*;

pub(super) static STARTUP_THREAD_MAP_VALIDATION_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn spawn_startup_thread_map_validation(pg_pool: Option<sqlx::PgPool>, token: String) {
    tokio::spawn(async move {
        let (checked, cleared) =
            crate::services::dispatches::discord_delivery::validate_channel_thread_maps_on_startup_with_backends(
                pg_pool.as_ref(),
                &token,
            )
            .await;
        if checked > 0 || cleared > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 THREAD-MAP: validated {checked} mapping(s), cleared {cleared} stale binding(s)"
            );
        }
    });
}

/// Remove the retired durable handoff tree without parsing or executing it.
/// Current recovery paths no longer write these JSON records, so any files here
/// are legacy residue and must not affect boot behavior.
pub(super) fn purge_legacy_durable_handoffs() {
    let Some(root) = crate::services::discord::runtime_store::legacy_discord_handoff_root() else {
        return;
    };
    if !root.exists() {
        return;
    }
    match std::fs::remove_dir_all(&root) {
        Ok(()) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Removed retired durable handoff directory: {}",
                root.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to remove retired durable handoff directory {}: {error}",
                root.display()
            );
        }
    }
}

pub(super) async fn recover_orphan_pending_dispatches(shared: &Arc<SharedData>) {
    crate::supervisor::orphan_recovery::recover_on_boot(shared.pg_pool.as_ref()).await;
}

#[cfg(test)]
use crate::supervisor::orphan_recovery::recover_once as recover_orphan_pending_dispatches_once;

#[cfg(test)]
#[path = "orphan_recovery_fence_tests.rs"]
mod tests;

pub(super) fn should_skip_agent_runtime_launch(token: &str) -> Option<String> {
    let bot = agentdesk_config::find_discord_bot_by_token(token)?;
    let agent_bot_names = agentdesk_config::collect_agent_bot_names();
    if !agent_bot_names.is_empty() && !agent_bot_names.contains(&bot.name) {
        return Some(bot.name);
    }
    None
}
