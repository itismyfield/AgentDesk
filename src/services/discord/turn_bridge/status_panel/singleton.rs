use super::super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CompletedBindingDisposition {
    NotApplicable,
    CommittedCurrent,
    Superseded,
    DurabilityFailure,
}

/// Classify the singleton binding after the panel's completion footer became
/// Discord-visible. A durability failure keeps the completed panel and its
/// recovery record; a superseded panel remains eligible for orphan retirement.
pub(super) fn commit_completed_binding(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_message_id: Option<MessageId>,
) -> CompletedBindingDisposition {
    if !shared.ui.two_message_panel_enabled {
        return CompletedBindingDisposition::NotApplicable;
    }
    let Some(panel_message_id) = normalize_status_panel_message_id(panel_message_id) else {
        return CompletedBindingDisposition::NotApplicable;
    };
    match crate::services::discord::status_panel_singleton_store::commit_if_owned_or_current(
        provider,
        &shared.token_hash,
        channel_id.get(),
        panel_message_id.get(),
    ) {
        crate::services::discord::status_panel_singleton_store::CompletedBindingCommitOutcome::CommittedCurrent(_) => {
            CompletedBindingDisposition::CommittedCurrent
        }
        crate::services::discord::status_panel_singleton_store::CompletedBindingCommitOutcome::Superseded => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                panel_message_id = panel_message_id.get(),
                "completed two-message singleton panel was superseded; retaining orphan retirement intent (#4891)"
            );
            CompletedBindingDisposition::Superseded
        }
        crate::services::discord::status_panel_singleton_store::CompletedBindingCommitOutcome::DurabilityFailure(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                panel_message_id = panel_message_id.get(),
                error = %error,
                "failed to durably commit completed two-message singleton panel; \
                 keeping the live completed panel and recovery intent (#4891)"
            );
            CompletedBindingDisposition::DurabilityFailure
        }
    }
}

pub(in crate::services::discord) fn completion_commit_allows_pending_bind_purge(
    disposition: CompletedBindingDisposition,
) -> bool {
    matches!(
        disposition,
        CompletedBindingDisposition::NotApplicable
            | CompletedBindingDisposition::CommittedCurrent
            | CompletedBindingDisposition::Superseded
    )
}

pub(in crate::services::discord) fn completion_commit_allows_orphan_removal(
    disposition: CompletedBindingDisposition,
) -> bool {
    matches!(
        disposition,
        CompletedBindingDisposition::NotApplicable | CompletedBindingDisposition::CommittedCurrent
    )
}
