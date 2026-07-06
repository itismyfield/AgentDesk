use super::*;

const ROTATION_CHECK_EVERY: u32 = 120; // ~30s at 250ms base cadence

#[allow(clippy::too_many_arguments)]
pub(super) async fn rotate_watcher_jsonl_if_due(
    rotation_tick: u32,
    output_path: &str,
    tmux_session_name: &str,
    mut current_offset: u64,
    mut last_relayed_offset: Option<u64>,
    mut last_observed_generation_mtime_ns: Option<i64>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> (u64, Option<u64>, Option<i64>) {
    // Periodic size-cap rotation for the session jsonl. Running this off
    // the watcher loop keeps the wrapper child process simple while
    // still enforcing a 20 MB soft cap (see issue #892).
    if rotation_tick % ROTATION_CHECK_EVERY == 0 {
        let path = output_path.to_string();
        let session = tmux_session_name.to_string();
        let prev_offset = current_offset;
        let rotation = tokio::task::spawn_blocking(move || {
            crate::services::tmux_common::truncate_jsonl_head_safe(
                &path,
                crate::services::tmux_common::JSONL_SIZE_CAP_BYTES,
                crate::services::tmux_common::JSONL_TARGET_KEEP_BYTES,
            )
            .map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("join error: {e}")));
        match rotation {
            Ok(Some(new_size)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✂ rotated jsonl for {} — new size {} bytes (was beyond cap)",
                    session,
                    new_size
                );
                // File was rewritten from the head: reset reader offset
                // so the watcher doesn't seek past the new EOF. Also
                // reset the duplicate-relay guard.
                if prev_offset > new_size {
                    current_offset = new_size;
                    last_relayed_offset = Some(new_size);
                    // #1270 codex P2: snapshot the current `.generation`
                    // mtime alongside the local offset so a later regression
                    // check has a real baseline. Without this, the local
                    // mtime would still be `None` after a normal relay path
                    // and any subsequent regression would misclassify
                    // same-wrapper rotation as fresh-respawn and clear the
                    // local offset to None — re-relaying surviving content.
                    last_observed_generation_mtime_ns =
                        Some(read_generation_file_mtime_ns(tmux_session_name));
                    reset_stale_relay_watermark_if_output_regressed(
                        shared,
                        channel_id,
                        tmux_session_name,
                        new_size,
                        "jsonl_rotation",
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!("  [{ts}] ⚠ jsonl rotation failed for {}: {}", session, e);
            }
        }
    }

    (
        current_offset,
        last_relayed_offset,
        last_observed_generation_mtime_ns,
    )
}
