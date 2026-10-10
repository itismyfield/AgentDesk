//! The legacy long replace and, after a failed edit, its fallback post under one claimed send.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::discord::SharedData;
use crate::services::discord::formatting::{
    DeferredReplaceLongMessageOutcome, ReplaceLongMessageOutcome,
    replace_long_message_raw_deferred_returning_receipt,
};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send_held};

/// What the legacy long replace delivered, or that a failed edit's range was committed already.
pub(super) enum WatcherDeferredReplaceOutcome {
    Replace(ReplaceLongMessageOutcome),
    AlreadyCommittedAfterEditFailure { edit_error: String },
}

/// What a failed edit re-reads from the durable frontier before any fallback post.
pub(super) struct EditFailureRecheck<'a> {
    pub(super) provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a str,
    pub(super) expected: Option<
        &'a crate::services::discord::outbound::delivery_record::EditFailureTranscriptIdentity,
    >,
    pub(super) range_end: u64,
}

/// The legacy long replace and, after a failed edit of a range not yet committed, its fallback
/// post; the claimed send stays counted across both. `None` when nothing was sent.
pub(super) async fn replace_or_post_after_edit_failure(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    (channel_id, msg_id): (ChannelId, MessageId),
    relay_text: &str,
    recheck: EditFailureRecheck<'_>,
    body_claim: Option<BodyClaim<'_>>,
    (last_chunk_anchor, edit_anchor_receipt): (
        &mut Option<crate::services::discord::formatting::ReplaceLastChunkAnchor>,
        &mut Option<crate::services::discord::outbound::DiscordTransportReceipt>,
    ),
) -> Option<Result<WatcherDeferredReplaceOutcome, Box<dyn std::error::Error + Send + Sync>>> {
    let (anchor, receipt) = (&mut *last_chunk_anchor, &mut *edit_anchor_receipt);
    let replace = move || {
        // Moved in whole, so the send may keep them for as long as it runs.
        let (anchor, receipt) = (anchor, receipt);
        replace_long_message_raw_deferred_returning_receipt(
            http, channel_id, msg_id, relay_text, shared, anchor, receipt,
        )
    };
    let Ok((BodySend::Sent(replace_outcome), legacy_send)) =
        claim_then_send_held(body_claim, replace).await
    else {
        return None;
    };
    let replace_outcome = match replace_outcome {
        Ok(DeferredReplaceLongMessageOutcome::Edited(outcome)) => {
            Ok(WatcherDeferredReplaceOutcome::Replace(outcome))
        }
        Ok(DeferredReplaceLongMessageOutcome::EditFailed { edit_error }) => {
            if crate::services::discord::outbound::delivery_record::range_committed_after_edit_failure(
                shared,
                recheck.provider,
                channel_id,
                recheck.tmux_session_name,
                recheck.expected,
                recheck.range_end,
            ) {
                Ok(WatcherDeferredReplaceOutcome::AlreadyCommittedAfterEditFailure { edit_error })
            } else {
                crate::services::discord::formatting::send_long_message_raw_with_rollback_returning_receipts(
                    http,
                    channel_id,
                    msg_id,
                    relay_text,
                    shared,
                )
                .await
                .and_then(|receipts| {
                    let message_ids =
                        crate::services::discord::formatting::message_ids_from_receipts(
                            receipts.clone(),
                        )?;
                    *edit_anchor_receipt = receipts.first().cloned();
                    Ok(WatcherDeferredReplaceOutcome::Replace(
                        ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                            edit_error,
                            replacement_anchor: message_ids.first().copied(),
                        },
                    ))
                })
            }
        }
        Err(error) => Err(error),
    };
    drop(legacy_send);
    Some(replace_outcome)
}
