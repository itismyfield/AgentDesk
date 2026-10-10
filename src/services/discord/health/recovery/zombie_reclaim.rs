use super::*;

/// #5176 — release a zombie foreground anchor identified by tmux session name.
///
/// `POST /api/sessions/{session_key}/reconcile-stale-turn` owns the Postgres
/// session row, but the row was never the thing blocking the channel: the
/// in-memory mailbox anchor was. Flipping the row to `idle` while the mailbox
/// still owns the foreground slot would leave the operator with a "reconciled"
/// response and a channel that is still unusable — the same false success this
/// issue is about. Session keys carry a tmux name rather than a channel id, so
/// the runtime is resolved by tmux name here.
///
/// The release itself goes through the single guarded authority, so this cannot
/// take a live turn's anchor even if the caller's own guard was wrong.
pub(crate) async fn release_zombie_foreground_turn_by_tmux_name(
    registry: Option<&HealthRegistry>,
    tmux_name: &str,
    stop_source: &'static str,
) -> discord::zombie_foreground_release::ZombieForegroundReleaseOutcome {
    let Some(registry) = registry else {
        return discord::zombie_foreground_release::ZombieForegroundReleaseOutcome::default();
    };
    let Some(runtime) = find_runtime_channel_match(registry, None, None, Some(tmux_name)).await
    else {
        return discord::zombie_foreground_release::ZombieForegroundReleaseOutcome::default();
    };
    let release = discord::zombie_foreground_release::release_zombie_foreground_turn(
        &runtime.shared,
        &runtime.provider,
        runtime.channel_id,
        stop_source,
    )
    .await;
    if matches!(
        release.verdict,
        Some(
            discord::zombie_foreground_release::ZombieForegroundVerdict::HoldInflightPresent
                | discord::zombie_foreground_release::ZombieForegroundVerdict::HoldTuiNotIdle
        )
    ) {
        discord::queue_io::arm_slow_idle_queue_backstop_if_queue_nonempty(
            &runtime.shared,
            &runtime.provider,
            runtime.channel_id,
            stop_source,
        )
        .await;
    }
    release
}
