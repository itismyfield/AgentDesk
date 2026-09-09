use super::super::*;

/// #4891: record the completed panel as the channel's durable two-message
/// singleton. LEDGER bookkeeping only — it returns nothing, because the caller's
/// boolean means "the Discord completion surface was committed" and the watcher
/// tail reads a completion `false` as "recover this panel", handing it to
/// `status_panel_orphan_store::drain()` to delete. Conflating the two deleted
/// live panels on 2026-07-24 (`panel_message_id=1530266420234031306` /
/// `…449355210913`). Genuinely superseded panels short-circuit to `true`
/// upstream, so the paths reaching here are real completions; the residual cost
/// of a failed commit is a stale binding, not a deleted panel.
pub(super) fn commit_completed_binding(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_message_id: Option<MessageId>,
) {
    if !shared.ui.two_message_panel_enabled {
        return;
    }
    let Some(panel_message_id) = normalize_status_panel_message_id(panel_message_id) else {
        return;
    };
    if let Err(error) =
        crate::services::discord::status_panel_singleton_store::commit_if_owned_or_current(
            provider,
            &shared.token_hash,
            channel_id.get(),
            panel_message_id.get(),
        )
    {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            panel_message_id = panel_message_id.get(),
            error = %error,
            "failed to durably commit completed two-message singleton panel"
        );
    }
}
