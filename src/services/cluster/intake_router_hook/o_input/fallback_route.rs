//! Enabled pilot/derived channels pick home or peer from shared PG; off never enters this path.
use crate::services::cluster::intake_router_hook::owner_record::{
    OwnerIdentity, adoption, cleanup, custody, fallback::Target, plan, snapshot,
};
use crate::services::cluster::intake_router_hook::*;

/// Lets a test change shared state between the first observation and planning.
#[cfg(test)]
pub(super) static BEFORE_PLAN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(super) static ENTERED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// `None` keeps the existing placement path: fallback off, out of scope, or an unadopted session.
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
    let timeout = super::super::switch_timeout();
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
    tx.commit().await?;
    let target = match shared.target() {
        Target::Selected(target) => target.to_owned(),
        Target::Held(reason) => {
            return Ok(Some(required_block(format!(
                "input fallback held: {reason:?}"
            ))));
        }
    };
    // A managed channel keeps its PG owner even for the same target; placement never re-picks it.
    #[cfg(test)]
    if owner_record::cleanup::tests::mutant("same-target-shortcut") {
        let active = shared.owner.as_ref().filter(|o| o.status == "active");
        if active.map_or(shared.home.as_deref() == Some(target.as_str()), |o| {
            o.owner_instance_id == target
        }) {
            return Ok(None);
        }
    }
    // A local file on its own owner (or unowned home) runs here as before, unless custody queues first.
    if ctx.has_nonportable_uploads
        && target == ctx.leader_instance_id
        && shared
            .owner
            .as_ref()
            .map_or(shared.home.as_deref() == Some(target.as_str()), |o| {
                o.status == "active" && o.owner_instance_id == target
            })
        && !held_any(pool, &id).await?
    {
        #[cfg(test)]
        let keep = !owner_record::cleanup::tests::mutant("same-target-upload-reject");
        #[cfg(not(test))]
        let keep = true;
        if keep {
            return Ok(Some(IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideIsLeader,
            }));
        }
    }
    // A local path cannot cross Macs; the queue's explicit rejection notice applies instead.
    #[cfg(test)]
    let local_upload =
        ctx.has_nonportable_uploads && !owner_record::cleanup::tests::mutant("fallback-local");
    #[cfg(not(test))]
    let local_upload = ctx.has_nonportable_uploads;
    if local_upload {
        return Ok(Some(IntakeRouterDecision::Blocked {
            reason: IntakeBlockedReason::NonPortableAttachmentRoutedTarget {
                target_instance_id: target,
            },
        }));
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
    #[cfg(test)]
    let hook = BEFORE_PLAN.lock().unwrap_or_else(|e| e.into_inner()).take();
    #[cfg(test)]
    if let Some(sql) = hook {
        sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await?;
    }
    // Every held row gets a plan; only the head may move, otherwise it waits for advance_pending.
    let Some((head, planned)) = plan::replan_held(pool, &id, &context).await? else {
        return Ok(Some(forwarded(target, pending)));
    };
    cleanup::resume_head(pool, &id, head, &planned, timeout).await?;
    let target = if head == pending {
        planned.target
    } else {
        target
    };
    Ok(Some(forwarded(target, pending)))
}

async fn held_any(pool: &PgPool, id: &OwnerIdentity) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_state='held')")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_one(pool).await
}

fn forwarded(target: String, pending: i64) -> IntakeRouterDecision {
    IntakeRouterDecision::Forwarded {
        target_instance_id: target,
        outbox_id: pending,
        basis: IntakeRoutingBasis::NodeOverride,
    }
}

#[cfg(test)]
#[path = "fallback_route_tests.rs"]
mod tests;
