//! Footer and panel writes are retired; legacy readers remain available for recovery.

// The rich block remains a budget input; new placeholder writes use only this anchor.
pub(in crate::services::discord) fn visible_anchor(_legacy_status_block: &str) -> &'static str {
    "⠋ 계속 처리 중"
}

pub(in crate::services::discord) fn enqueue_completed_singletons(
    provider: &crate::services::provider::ProviderKind,
    token_hash: &str,
) -> usize {
    use super::{inflight, runtime_store, status_panel_orphan_store, status_panel_singleton_store};
    let (Some(singletons), Some(inflight_root)) = (
        runtime_store::discord_status_panel_singletons_root(),
        runtime_store::discord_inflight_root(),
    ) else {
        return 0;
    };
    let directory = singletons.join(provider.as_str()).join(token_hash);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => {
            tracing::warn!(%error, "could not inspect retired status-panel singleton bindings");
            return 0;
        }
    };
    let mut queued = 0;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(channel) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|channel| *channel != 0)
        else {
            continue;
        };
        let state_path = inflight::inflight_state_path(&inflight_root, provider, channel);
        let Ok(_guard) = inflight::try_lock_inflight_state_path(&state_path) else {
            continue;
        };
        // Only a definitively absent row proves this boot is not touching an open turn.
        if !matches!(std::fs::symlink_metadata(&state_path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            continue;
        }
        let Some(binding) = status_panel_singleton_store::load(provider, token_hash, channel)
        else {
            continue;
        };
        if super::is_synthetic_headless_message_id_raw(binding.panel_message_id) {
            continue;
        }
        status_panel_orphan_store::enqueue(provider, token_hash, channel, binding.panel_message_id);
        if status_panel_orphan_store::is_queued(
            provider,
            token_hash,
            channel,
            binding.panel_message_id,
        ) {
            queued += 1;
        } else {
            tracing::warn!(
                channel,
                "could not queue retired completed status-panel singleton"
            );
        }
    }
    queued
}

#[cfg(test)]
#[path = "footer_retirement_boot_tests.rs"]
mod boot_tests;
