//! A forced cancel of a Herdr execution, picked by the cancel's own selection read: the P11
//! terminate service retires only that execution and its effects run under the service's exclusion.

use std::sync::Arc;

use futures::FutureExt;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::cancel_effects::herdr::{HERDR_TERMINATE, HerdrForceEffects, herdr_force_effects};
use crate::db::dispatched_sessions::hosted_execution::http_force::herdr_execution;
use crate::services::cluster::channel_home::current_command;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::herdr_terminate::http_force::{Expected, terminate_for_http};
use crate::services::discord::herdr_terminate::{OperatorTerminate, terminate_enabled};
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::provider::ProviderKind;
use crate::services::service_error::{ServiceError, ServiceResult};
use crate::services::session_forwarding::{
    CancelTurnSessionInfo, load_cancel_turn_hosted, load_cancel_turn_session,
};

const REASON: &str = "queue-api cancel_turn (force)";

/// The cancel's chosen session and, from the same read, its row's identity and hosted record.
pub(super) struct Selected {
    info: CancelTurnSessionInfo,
    identity: [Option<String>; 4],
    hosted: Option<Value>,
}

/// The cancel's one selection read. Only a forced cancel with terminate on, decided before any IO,
/// also keeps the chosen row's hosted columns; every other cancel reads as it always did.
pub(super) async fn select(
    pool: &PgPool,
    channel_id: &str,
    force: bool,
) -> ServiceResult<(Option<CancelTurnSessionInfo>, Option<Selected>)> {
    if !force || !terminate_enabled() {
        return Ok((load_cancel_turn_session(pool, channel_id).await?, None));
    }
    let Some((info, row)) = load_cancel_turn_hosted(pool, channel_id).await? else {
        return Ok((None, None));
    };
    let selected = row.map(|row| Selected {
        info: info.clone(),
        identity: row.identity,
        hosted: row.hosted,
    });
    Ok((Some(info), selected))
}

/// The forced cancel's answer when `selected` is a Herdr execution of this channel, retired only
/// as that read found it; `None`, decided from that read alone, keeps the rest.
pub(super) async fn herdr_force(
    health_registry: Option<&Arc<HealthRegistry>>,
    pool: &PgPool,
    channel_id: &str,
    selected: Option<Selected>,
) -> Option<ServiceResult<Value>> {
    let Selected {
        info,
        identity,
        hosted,
    } = selected?;
    let registry = health_registry?;
    #[cfg(test)]
    crate::services::discord::herdr_terminate::run_before_selection_probe().await;
    let record = herdr_execution(hosted.as_ref(), &identity)?;
    let provider = ProviderKind::from_str(&record.owner.provider);
    let chosen = info.match_rank == 0
        && record.owner.channel_id == channel_id
        && matches!(provider, Some(ProviderKind::Claude | ProviderKind::Codex));
    if !chosen {
        return None;
    }
    let request = OperatorTerminate {
        session_key: info.session_key.clone(),
        execution_nonce: record.execution_nonce,
    };
    let expected = Expected {
        owner: record.owner,
        dispatch: info.dispatch_id.clone(),
    };
    let (effects_pool, key) = (pool.clone(), info.session_key.clone());
    let permit = current_command(channel_id, "cancel_turn");
    let bound = (request, expected);
    let (result, effects) = terminate_for_http(registry, pool.clone(), bound, permit, REASON, {
        move |exclusion| herdr_force_effects(exclusion, effects_pool, key).boxed()
    })
    .await;
    Some(answer(channel_id, &info, result.outcome(), effects))
}

/// Only a retire whose effects all applied is success; a run or effect that cannot say what it
/// left is a server error, and every other refusal changed nothing.
fn answer(
    channel_id: &str,
    info: &CancelTurnSessionInfo,
    (name, detail): (&'static str, String),
    effects: Option<HerdrForceEffects>,
) -> ServiceResult<Value> {
    let termination = json!({"result": name, "detail": detail});
    let refused = |error: ServiceError| {
        error
            .with_context("channel_id", channel_id)
            .with_context("session_key", &info.session_key)
            .with_context("termination", &termination)
    };
    let Some(effects) = effects else {
        let message = format!("herdr terminate {name}: {detail}");
        return Err(refused(if name == "indeterminate" {
            ServiceError::internal(message)
        } else {
            ServiceError::conflict(message)
        }));
    };
    let loss = effects.loss.clone().unwrap_or_default();
    let remaining = loss
        .queue_depth_after
        .or(effects.purge.and_then(|p| p.queue_len_after));
    if let Some(step) = effects.failed {
        let message = format!("herdr terminate retired the execution; {step}");
        let error = refused(ServiceError::internal(message));
        // Past the disconnect, the reply still names what applied and what the purge removed.
        return Err(match effects.disconnected {
            false => error,
            true => (error.with_context("dispatch_cancelled", &info.dispatch_id))
                .with_context("session_disconnected", true)
                .with_context("queue_purged", effects.purge.map(|p| p.removed))
                .with_context(
                    "queue_dead_lettered_message_ids",
                    &loss.dead_lettered_message_ids,
                )
                .with_context(
                    "queue_unpreserved_message_ids",
                    &loss.unpreserved_message_ids,
                )
                .with_context("queued_remaining", remaining)
                .with_context("queue_loss_recorded", loss.loss_recorded()),
        });
    }
    let loss_recorded = match effects.purge {
        Some(_) => json!(loss.loss_recorded()),
        None => json!(false),
    };
    Ok(json!({
        "ok": true,
        "channel_id": channel_id,
        "agent_id": info.agent_id,
        "requested_provider": info.requested_provider,
        "exact_channel_match": true,
        "session_key": info.session_key,
        "tmux_session": tmux_name_from_session_key(&info.session_key),
        "tmux_killed": false,
        "lifecycle_path": HERDR_TERMINATE,
        "queued_remaining": remaining,
        "queue_purged": effects.purge.map(|p| p.removed),
        "queue_dead_lettered_message_ids": loss.dead_lettered_message_ids,
        "queue_unpreserved_message_ids": loss.unpreserved_message_ids,
        "queue_loss_recorded": loss_recorded,
        "dispatch_cancelled": info.dispatch_id,
        "turn_status": effects.cancelled.status,
        "turn_completed_at": effects.cancelled.completed_at.to_rfc3339(),
        "termination": termination,
    }))
}
