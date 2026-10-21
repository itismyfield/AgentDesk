//! A forced cancel of a Herdr execution. It is chosen from the same session read the cancel made,
//! retired by the P11 terminate service, and its effects run under that service's exclusion;
//! every other cancel keeps its existing path.

use std::sync::Arc;

use futures::FutureExt;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::cancel_effects::{HERDR_TERMINATE, HerdrForceEffects, herdr_force_effects};
use crate::db::dispatched_sessions::hosted_execution::http_force::herdr_execution;
use crate::services::cluster::channel_home::current_command;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::herdr_terminate::http_force::terminate_for_http;
use crate::services::discord::herdr_terminate::{OperatorTerminate, terminate_enabled};
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::provider::ProviderKind;
use crate::services::service_error::{ServiceError, ServiceResult};
use crate::services::session_forwarding::{CancelTurnSessionInfo, load_cancel_turn_hosted};

const REASON: &str = "queue-api cancel_turn (force)";

/// The forced cancel's answer when `session_key`, the cancel's own pick, is a Herdr execution of
/// this channel; `None` leaves the cancel to its existing path. The switch is read before any IO.
pub(super) async fn herdr_force(
    force: bool,
    health_registry: Option<&Arc<HealthRegistry>>,
    pool: Option<&PgPool>,
    channel_id: &str,
    session_key: Option<&str>,
) -> Option<ServiceResult<Value>> {
    if !force || !terminate_enabled() {
        return None;
    }
    let (registry, pool) = (health_registry?, pool?);
    let (info, row) = load_cancel_turn_hosted(pool, channel_id).await.ok()??;
    let record = herdr_execution(row.hosted.as_ref(), &row.identity)?;
    let provider = ProviderKind::from_str(&record.owner.provider);
    let chosen = Some(info.session_key.as_str()) == session_key
        && info.match_rank == 0
        && record.owner.channel_id == channel_id
        && matches!(provider, Some(ProviderKind::Claude | ProviderKind::Codex));
    if !chosen {
        return None;
    }
    let request = OperatorTerminate {
        session_key: info.session_key.clone(),
        execution_nonce: record.execution_nonce,
    };
    let (effects_pool, key) = (pool.clone(), info.session_key.clone());
    let permit = current_command(channel_id, "cancel_turn");
    let bound = (request, info.dispatch_id.clone());
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
    if let Some(step) = effects.failed {
        let message = format!("herdr terminate retired the execution; {step}");
        return Err(refused(ServiceError::internal(message)));
    }
    let loss = effects.loss.unwrap_or_default();
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
        "queued_remaining": loss.queue_depth_after.or(effects.purge.and_then(|p| p.queue_len_after)),
        "queue_purged": effects.purge.map(|p| p.drained),
        "queue_dead_lettered_message_ids": loss.dead_lettered_message_ids,
        "queue_unpreserved_message_ids": loss.unpreserved_message_ids,
        "queue_loss_recorded": loss_recorded,
        "dispatch_cancelled": info.dispatch_id,
        "turn_status": effects.cancelled.status,
        "turn_completed_at": effects.cancelled.completed_at.to_rfc3339(),
        "termination": termination,
    }))
}
