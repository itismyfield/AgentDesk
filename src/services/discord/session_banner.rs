//! #3983 item4: one-shot top session banner emit with dual-path de-dup.
//!
//! Track A (#3992) retired the compact host block from the every-tick status
//! footer; item4 finishes the move by lifting the residual SESSION line
//! (`🆕 새 세션 시작 · provider session … · tmux …` + `(최근 대화 N개…)`) out of the
//! per-tick footer render entirely and posting it ONCE, at the top, per session
//! start/boundary.
//!
//! The session-panel snapshot is refreshed from BOTH the sink
//! (`turn_bridge::panel_lifecycle::refresh_session_panel_line_from_lifecycle`)
//! and the tmux watcher
//! (`tmux_watcher::orphan_status_panel_cleanup::refresh_watcher_session_panel_from_lifecycle`),
//! either of which can run first for a given session. To keep the banner
//! EXACTLY ONCE per session, the de-dup token lives in the per-channel
//! `StatusPanelState` (guarded by its mutex): `claim_session_banner_line`
//! performs a single atomic compare-and-record of the session identity, so the
//! first refresh path to reach it wins the banner and the second (plus every
//! later status tick) observes the recorded key and skips. This module owns the
//! emit orchestration (claim → resolve HTTP → post); the EXTREME hot files
//! (`turn_bridge/mod.rs`, `tmux_watcher.rs`) carry only the thin call-site
//! `provider` argument that threads through to their existing refresh helpers.

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::provider::ProviderKind;

use super::SharedData;

/// Post the one-shot top session banner for `channel_id` iff the current session
/// has not been bannered yet. Idempotent: safe to call on every refresh tick
/// from either the sink or the watcher path — the atomic claim in
/// `claim_session_banner_line` guarantees at most one post per session identity
/// and no omission when the two paths race.
///
/// Silently no-ops when status-panel v2 is disabled, when the channel has no
/// unclaimed session banner, or when no Discord HTTP handle is available. A
/// send failure is logged (the banner is already claimed, so it is not retried;
/// a genuinely new session boundary re-arms the claim).
pub(in crate::services::discord) async fn emit_session_banner_if_new(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
) {
    if !shared.ui.status_panel_v2_enabled {
        return;
    }
    let Some(line) = shared
        .ui
        .placeholder_live_events
        .claim_session_banner_line(channel_id, provider)
    else {
        return;
    };
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::debug!(
            channel_id = channel_id.get(),
            "#3983 item4: skipped one-shot session banner — no Discord HTTP handle available"
        );
        return;
    };
    if let Err(error) = super::http::send_channel_message(&http, channel_id, &line).await {
        tracing::debug!(
            channel_id = channel_id.get(),
            "#3983 item4: failed to post one-shot session banner: {error}"
        );
    }
}
