//! Between two episodes of one front: a Claude Pending execution that never started, on which the
//! front's wait consumed a due, is closed before the front's next turn is published.
#![cfg(unix)]
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::drive::FrontClaim;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
};
use crate::services::discord::SharedData;
use crate::services::discord::herdr_terminate::{
    NeverStartedTerminate, TerminationResult, terminate_enabled, terminate_never_started,
};
use crate::services::provider::ProviderKind;

/// Ends the never-started execution `claim`'s front waited on, then drops that wait. The previous
/// handoff returned and no turn of this one is published yet; a turn still holding the channel,
/// a logged start or the switch off closes nothing and leaves the front to the turn that follows.
pub(crate) async fn recover(shared: &Arc<SharedData>, channel: ChannelId, claim: &FrontClaim) {
    let Some(request) = qualified(shared, channel, claim).await else {
        return;
    };
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let (front, nonce) = (request.front.clone(), request.execution_nonce.clone());
    let result = terminate_never_started(request, shared.clone(), pool).await;
    let (outcome, detail) = result.outcome();
    tracing::info!(
        channel_id = channel.get(),
        execution_nonce = %nonce,
        outcome,
        %detail,
        "never-started pending recovery finished"
    );
    if result != TerminationResult::Retired {
        return;
    }
    let ended = shared.mailbox(channel).v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("V2 not adopted")?;
        owner.end_never_started_wait(&front, &nonce)
    });
    if let Err(error) = ended.await {
        tracing::warn!(channel_id = channel.get(), %error, "retry wait kept after its close");
    }
}

/// The request the current claim may make: the owner's wait is read first, so a front with no
/// such wait, or a node with the switch off, reads no row at all.
async fn qualified(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    claim: &FrontClaim,
) -> Option<NeverStartedTerminate> {
    if shared.provider != ProviderKind::Claude {
        return None;
    }
    let (front, number) = (claim.front().clone(), claim.number());
    let asked = front.clone();
    let named = shared.mailbox_peek(channel)?.v2_request(move |slot| {
        Ok(slot
            .as_ref()
            .and_then(|owner| owner.never_started_close_claimed((&asked, number))))
    });
    let nonce = named.await.ok()??;
    if !terminate_enabled() {
        return None;
    }
    let session_key = pending_key(shared.pg_pool.as_ref()?, &shared.token_hash, channel).await?;
    let lookup = HostedLookupKey::SessionKey(&session_key);
    let HostedLookup::Found(row) = load_hosted_execution_pg(shared.pg_pool.as_ref()?, lookup).await
    else {
        return None;
    };
    let HostedRecord::Known(record) = row.record else {
        return None;
    };
    let pending = record.state == HostedState::Pending && record.expected.is_some();
    if record.execution_nonce != nonce || !pending {
        return None;
    }
    let context =
        crate::services::tui_prompt_dedupe::binding_context::execution_context("claude", &nonce);
    Some(NeverStartedTerminate {
        session_key,
        session_id: context.ok()?.expected_native_session_id?,
        execution_nonce: nonce,
        front,
        claim_number: number,
    })
}

/// The session key of the channel's canonical Claude row.
async fn pending_key(pool: &PgPool, token_hash: &str, channel: ChannelId) -> Option<String> {
    let channel = channel.get().to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: token_hash,
        channel_id: &channel,
    };
    let resolved = resolve_session_row_pg(pool, None, Some("claude"), Some(identity)).await;
    resolved.ok()?.map(|(_, key)| key)
}
