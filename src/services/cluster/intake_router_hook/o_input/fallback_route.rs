//! Enabled pilot/derived channels pick home or peer from shared PG; off never enters this path.
use crate::services::cluster::intake_router_hook::owner_record::{
    OwnerIdentity, adoption, cleanup, custody, fallback::Target, plan, snapshot,
};
use crate::services::cluster::intake_router_hook::*;

#[cfg(test)]
pub(super) static ENTERED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// `None` keeps the existing placement path: fallback off, same target, or an unadopted session.
pub(super) async fn route(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
) -> Option<IntakeRouterDecision> {
    let (fallback, _) = crate::services::cluster::input_fallback_config::current();
    let pair = fallback
        .pair_for(ctx.channel_id, ctx.policy_channel_id)
        .cloned();
    #[cfg(test)]
    let pair = pair.or_else(|| {
        owner_record::cleanup::tests::mutant("fallback-off-gate")
            .then(|| ["mac-a".to_string(), "mac-b".to_string()])
    });
    let pair = pair?;
    #[cfg(test)]
    ENTERED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(ctx.channel_id.to_owned());
    let timeout = i32::try_from(
        effective_intake_routing_config()
            .forward_pre_claim_timeout_secs
            .max(1),
    )
    .unwrap_or(i32::MAX);
    match switch(pool, ctx, pair, timeout).await {
        Ok(decision) => decision,
        Err(error) => Some(required_block(format!("input fallback: {error}"))),
    }
}

enum Adoption {
    Settled,
    Unadopted,
}

/// A live session without owner history is adopted only by its own Mac, from its own binding.
async fn adopt_local(
    pool: &PgPool,
    id: &OwnerIdentity,
    local: &str,
) -> Result<Adoption, sqlx::Error> {
    use crate::services::tui_prompt_dedupe::binding_context::{
        SpawnNonceMarker, observe_spawn_nonce_marker,
    };
    let mut tx = pool.begin().await?;
    custody::lock(&mut tx, id).await?;
    if owner_record::read_latest_owner_in_tx(&mut tx, id)
        .await?
        .is_some()
    {
        return Ok(Adoption::Settled);
    }
    let live: Vec<(String, String)> = sqlx::query_as("SELECT session_key,instance_id FROM sessions WHERE LOWER(BTRIM(provider))=$1 AND channel_id=$2 AND COALESCE(LOWER(BTRIM(status)),'') NOT IN ('disconnected','aborted')")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_all(&mut *tx).await?;
    let [(locator, instance)] = live.as_slice() else {
        return Ok(if live.is_empty() {
            Adoption::Settled
        } else {
            Adoption::Unadopted
        });
    };
    let Some((_, owner)) = locator.rsplit_once(':').filter(|_| instance == local) else {
        return Ok(Adoption::Unadopted);
    };
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(owner) else {
        return Ok(Adoption::Unadopted);
    };
    let binding = serde_json::json!({"host":"tmux","endpoint":local,"owner":owner,
        "root":crate::services::tmux_common::current_tmux_owner_marker(),"execution_nonce":nonce});
    match adoption::adopt_on_tx(&mut tx, id, local, locator, &binding).await {
        Ok(true) => {
            tx.commit().await?;
            Ok(Adoption::Settled)
        }
        Ok(false) | Err(adoption::Error::Native(_)) => Ok(Adoption::Unadopted),
        Err(adoption::Error::Storage(error)) => Err(error),
    }
}

async fn switch(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
    pair: [String; 2],
    timeout: i32,
) -> Result<Option<IntakeRouterDecision>, sqlx::Error> {
    let id = OwnerIdentity::new(ctx.provider, ctx.channel_id);
    if let Adoption::Unadopted = adopt_local(pool, &id, ctx.leader_instance_id).await? {
        return Ok(None);
    }
    let (agent, labels) = agent_id_and_preferred_labels(pool, ctx.policy_channel_id)
        .await?
        .map(|(agent, _, labels)| (agent, labels))
        .unwrap_or_default();
    let context = snapshot::Context {
        policy_channel: ctx.policy_channel_id.into(),
        pair,
        gateway: Some(ctx.leader_instance_id.into()),
        auth_profile: crate::services::cluster::readiness::expected_auth_profile(
            ctx.provider,
            ctx.channel_id,
            &agent,
        ),
        lease_seconds: i64::try_from(worker_heartbeat_lease_secs()).unwrap_or(i64::MAX),
    };
    let mut tx = pool.begin().await?;
    let shared = snapshot::read_on_tx(&mut tx, &id, &context).await?;
    let queued: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_state='held')")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    let target = match shared.target() {
        Target::Selected(target) => target.to_owned(),
        Target::Held(reason) => {
            return Ok(Some(required_block(format!(
                "input fallback held: {reason:?}"
            ))));
        }
    };
    let active = shared.owner.as_ref().filter(|o| o.status == "active");
    let same = active.map_or(shared.home.as_deref() == Some(target.as_str()), |o| {
        o.owner_instance_id == target
    });
    if same && !queued {
        return Ok(None);
    }
    let payload = build_payload_for_insert(ctx, &target, &labels, &agent);
    let pending = custody::stage(
        pool,
        custody::StageRequest {
            payload: &payload,
            home_revision: shared.raw_revision,
            wait_generation: shared.owner.as_ref().map(|o| o.generation),
            grace_seconds: 0,
        },
    )
    .await?;
    let mut tx = pool.begin().await?;
    plan::save_on_tx(&mut tx, &id, pending, &shared).await?;
    tx.commit().await?;
    match active {
        Some(old) if old.owner_instance_id != target => {
            cleanup::request(
                pool,
                &id,
                (&old.owner_instance_id, old.generation),
                pending,
                &target,
                timeout,
            )
            .await?;
        }
        _ => {
            let expected = shared
                .owner
                .as_ref()
                .map(|o| (o.owner_instance_id.as_str(), o.generation));
            custody::promote(
                pool,
                pending,
                &target,
                expected,
                custody::SwitchReadiness::Wait,
            )
            .await?;
        }
    }
    Ok(Some(IntakeRouterDecision::Forwarded {
        target_instance_id: target,
        outbox_id: pending,
        basis: IntakeRoutingBasis::NodeOverride,
    }))
}

#[cfg(test)]
#[path = "fallback_route_tests.rs"]
mod tests;
