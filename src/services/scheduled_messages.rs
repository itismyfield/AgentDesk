//! Fire executor + scheduler loop for the scheduled-message reservation pool.
//!
//! Design: docs/design/scheduled-messages.md.
//!
//! Ownership boundaries (deliberately narrow):
//!   * push fires hand off to `message_outbox` and finish immediately —
//!     retry/final delivery state belongs to `message_outbox_loop`, never
//!     re-polled here.
//!   * agent fires start a headless turn in the target channel (the relayed
//!     assistant reply IS the delivered message) and stay `running` until
//!     transcript evidence, terminal turn error, or timeout — the same
//!     completion-evidence model as `RoutineAgentExecutor`.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;

mod evidence;

#[cfg(test)]
use evidence::transcript_delivery_evidence;
use evidence::{
    TurnEvidence, find_turn_delivery_evidence, find_turn_delivery_evidence_on_connection,
    poll_running_agent_deliveries,
};

use crate::db::scheduled_messages as db;
use crate::db::scheduled_messages::{ClaimedFire, RunningAgentDelivery, ScheduledMessageRow};
use crate::services::discord::health::{
    HealthRegistry, reserve_headless_agent_turn,
    start_reserved_headless_agent_turn_with_owner_channel,
};
use crate::services::message_outbox::{
    OutboxEnqueueError, OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx,
};

const CLAIM_BATCH: i64 = 10;
const AGENT_POLL_BATCH: i64 = 20;
pub(crate) const LEASE_SECS: i64 = 120;
/// A fire slot is retried this many times after interruptions before the
/// definition is failed outright (claim-time cap; slot retry_count counts
/// re-arms of the same fire slot).
const MAX_FIRE_RETRIES: i32 = 3;
const FIRE_RETRY_BACKOFF_SECS: [i64; 3] = [60, 300, 900];
/// Agent turns without terminal evidence after this window fail closed. Raw
/// fallback is reserved for definitive NO_REPLY/empty-response outcomes so a
/// late live turn cannot race a second user-visible delivery.
const AGENT_COMPLETION_TIMEOUT_SECS: i64 = 1800;
const RUNTIME_DEFER_SECS: i64 = 15;
const OUTBOX_SOURCE: &str = "scheduled_message";

// ── Scheduler loop ──────────────────────────────────────────────────────────

pub async fn scheduled_message_loop(
    pg_pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
) {
    use std::time::Duration;

    // Give Discord runtime bootstrap a brief head start (message_outbox_loop
    // pattern) — agent fires need a live runtime to start turns.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let claim_owner = format!(
        "scheduled-messages:{}:{}:{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    tracing::info!("[smsg] scheduled message worker started (adaptive backoff 500ms-5s)");

    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);
    loop {
        tokio::time::sleep(poll_interval).await;
        let did_work = tick_once(&pg_pool, health_registry.as_deref(), &claim_owner).await;
        if did_work {
            poll_interval = Duration::from_millis(500);
        } else {
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
        }
    }
}

/// One scheduler pass: lease recovery, due-claim + fire, agent-turn polling.
/// Returns true when any row moved (drives the adaptive backoff).
async fn tick_once(
    pool: &PgPool,
    health_registry: Option<&HealthRegistry>,
    claim_owner: &str,
) -> bool {
    let mut did_work = false;

    match db::recover_expired_leases_pg(pool).await {
        Ok(recovered) if recovered > 0 => {
            tracing::warn!(recovered, "[smsg] rewound expired delivery leases");
            did_work = true;
        }
        Ok(_) => {}
        Err(error) => tracing::warn!("[smsg] lease recovery failed: {error}"),
    }

    let claim_now = Utc::now();
    match db::claim_due_fires_pg(
        pool,
        claim_owner,
        health_registry.is_some(),
        CLAIM_BATCH,
        LEASE_SECS,
        claim_now,
    )
    .await
    {
        Ok(claimed) => {
            for fire in claimed {
                did_work = true;
                fire_claimed(pool, health_registry, fire, Utc::now()).await;
            }
        }
        Err(error) => tracing::warn!("[smsg] due claim failed: {error}"),
    }

    // A process without Discord runtime also has no message_outbox worker.
    // Leave durable agent turns untouched for a runtime-capable leader to
    // adopt; resolving NO_REPLY here could otherwise finalize a reservation
    // after enqueueing a push_raw fallback that nobody can deliver.
    if health_registry.is_some()
        && poll_running_agent_deliveries(pool, claim_owner, LEASE_SECS, AGENT_POLL_BATCH).await
    {
        did_work = true;
    }

    did_work
}

// ── Fire execution ──────────────────────────────────────────────────────────

/// Execute one armed fire slot. Every branch leaves the delivery and parent in
/// a consistent state; errors degrade to `interrupted` so the bounded re-arm
/// path (claim-time retry cap) owns the retry policy.
pub async fn fire_claimed(
    pool: &PgPool,
    health_registry: Option<&HealthRegistry>,
    fire: ClaimedFire,
    now: DateTime<Utc>,
) {
    let message = &fire.message;

    // Compare against the claim time, not the fire slot: a worker that wakes
    // up late must not deliver a message whose expiry has already passed. This
    // check intentionally precedes retry exhaustion: `push_raw` is still a
    // delivery and must never bypass the definition's expiry boundary.
    if let Some(expires_at) = message.expires_at {
        if expires_at <= now {
            if let Err(error) =
                db::mark_expired_pg(pool, &message.id, &fire.delivery_id, &fire.claim_token).await
            {
                tracing::warn!(id = message.id, "[smsg] expire transition failed: {error}");
            }
            return;
        }
    }

    if fire.retry_count > MAX_FIRE_RETRIES {
        let error = format!("fire retry budget exhausted after {MAX_FIRE_RETRIES} re-arms");
        if message.delivery_kind == db::KIND_AGENT && message.on_agent_failure == "push_raw" {
            finish_exhausted_agent_with_raw_fallback(pool, &fire, &error).await;
        } else {
            finish_terminal_failure(pool, &fire, &error).await;
        }
        return;
    }

    match message.delivery_kind.as_str() {
        db::KIND_PUSH => fire_push(pool, &fire, now).await,
        db::KIND_AGENT => fire_agent(pool, health_registry, &fire).await,
        other => {
            let error = format!("unknown delivery_kind '{other}'");
            finish_terminal_failure(pool, &fire, &error).await;
        }
    }
}

async fn fire_push(pool: &PgPool, fire: &ClaimedFire, now: DateTime<Utc>) {
    let message = &fire.message;
    let Some(channel_id) = message.target_channel_id.as_deref() else {
        // chk_smsg_push_target_required makes this unreachable; degrade safely.
        finish_terminal_failure(pool, fire, "push delivery has no target channel").await;
        return;
    };
    let target = format!("channel:{channel_id}");
    // reason_code carries the fire-slot identity so the outbox dedupe key is
    // per-slot: a crashed node re-firing the same slot is suppressed, while
    // the next recurrence (different slot) passes.
    let reason_code = format!(
        "scheduled_message:v1:{}:{}",
        message.id,
        fire.fire_scheduled_at.timestamp_micros()
    );
    match commit_push_handoff(
        pool,
        fire,
        OutboxMessage {
            target: &target,
            content: &message.content,
            bot: &message.bot,
            source: OUTBOX_SOURCE,
            reason_code: Some(&reason_code),
            session_key: None,
        },
        now,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => tracing::info!(
            id = message.id,
            delivery_id = fire.delivery_id,
            "[smsg] push handoff skipped after claim cancellation"
        ),
        Err(error) => {
            let error = format!("outbox enqueue failed: {error}");
            tracing::warn!(id = message.id, "[smsg] {error}");
            interrupt_for_retry(pool, fire, &error).await;
        }
    }
}

async fn commit_push_handoff(
    pool: &PgPool,
    fire: &ClaimedFire,
    message: OutboxMessage<'_>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;
    if !db::lock_active_delivery_tx(
        &mut tx,
        &fire.message.id,
        &fire.delivery_id,
        &fire.claim_token,
    )
    .await?
    {
        return Ok(false);
    }
    let outbox_id =
        enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(&mut tx, message).await?;
    let (next, forced_terminal) = compute_resume(
        fire.message.schedule.as_deref(),
        &fire.message.timezone,
        fire.message.scheduled_at,
        fire.message.expires_at,
        now,
    );
    let terminal_status = forced_terminal.unwrap_or(db::STATUS_SENT);
    let next = forced_terminal.is_none().then_some(next).flatten();
    let transitioned = db::finish_locked_delivery_and_finalize_parent_tx(
        &mut tx,
        &fire.delivery_id,
        &fire.claim_token,
        db::DELIVERY_SENT,
        None,
        Some(outbox_id),
        None,
        &fire.message.id,
        true,
        terminal_status,
        next,
    )
    .await?;
    if !transitioned {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

async fn fire_agent(pool: &PgPool, health_registry: Option<&HealthRegistry>, fire: &ClaimedFire) {
    let message = &fire.message;
    let Some(health_registry) = health_registry else {
        let reason = "discord runtime health registry unavailable";
        let retry_not_before = runtime_defer_until(Utc::now());
        if let Err(error) = db::defer_delivery_without_retry_pg(
            pool,
            &fire.delivery_id,
            &fire.claim_token,
            &message.id,
            message.scheduled_at,
            retry_not_before,
            reason,
        )
        .await
        {
            tracing::warn!(
                id = message.id,
                "[smsg] runtime-unavailable defer failed: {error}"
            );
        }
        return;
    };
    match start_agent_turn(pool, health_registry, fire).await {
        Ok(AgentTurnStartDisposition::Started) => {
            // Delivery stays running; poll_agent_delivery owns completion.
        }
        Ok(AgentTurnStartDisposition::Consumed(turn_id)) => {
            finish_terminal_failure(
                pool,
                fire,
                &format!("scheduled message turn {turn_id} was consumed without a provider start"),
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(id = message.id, "[smsg] agent turn start failed: {error}");
            let reason = format!("agent turn start failed: {error}");
            if agent_start_error_is_runtime_unavailable(&error) {
                let retry_not_before = runtime_defer_until(Utc::now());
                if let Err(defer_error) = db::defer_delivery_without_retry_pg(
                    pool,
                    &fire.delivery_id,
                    &fire.claim_token,
                    &message.id,
                    message.scheduled_at,
                    retry_not_before,
                    &reason,
                )
                .await
                {
                    tracing::warn!(
                        id = message.id,
                        "[smsg] booting-runtime defer failed: {defer_error}"
                    );
                }
            } else {
                interrupt_for_retry(pool, fire, &reason).await;
            }
        }
    }
}

enum AgentTurnStartDisposition {
    Started,
    /// A lifecycle command was consumed before provider/bridge spawn. Repeating
    /// it could repeat the lifecycle side effect, so terminalize instead.
    Consumed(String),
}

fn runtime_defer_until(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::seconds(RUNTIME_DEFER_SECS)
}

fn agent_start_error_is_runtime_unavailable(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    [
        "provider runtime not registered",
        "provider runtime is not ready",
        "matched runtime is not ready",
        "provider token unavailable",
    ]
    .into_iter()
    .any(|needle| message.contains(needle))
}

/// Start a headless agent turn whose relayed reply delivers the message.
/// Mirrors `RoutineAgentExecutor::start_turn` minus routine-thread routing:
/// the turn runs directly in the target channel (or the agent's primary
/// channel when no target was pinned).
async fn start_agent_turn(
    pool: &PgPool,
    health_registry: &HealthRegistry,
    fire: &ClaimedFire,
) -> anyhow::Result<AgentTurnStartDisposition> {
    use anyhow::anyhow;

    let message = &fire.message;

    let agent_id = message
        .agent_id
        .as_deref()
        .ok_or_else(|| anyhow!("agent delivery requires agent_id"))?;
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pool, agent_id)
        .await
        .map_err(|error| anyhow!("load agent bindings for {agent_id}: {error}"))?
        .ok_or_else(|| anyhow!("agent {agent_id} not found"))?;
    let provider = bindings
        .resolved_primary_provider_kind()
        .ok_or_else(|| anyhow!("agent {agent_id} primary provider is not configured"))?;
    let primary_channel = bindings
        .primary_channel()
        .ok_or_else(|| anyhow!("agent {agent_id} primary channel is not configured"))?;
    let resolve_channel = |value: &str| {
        crate::services::dispatches::outbox_route::resolve_channel_alias_pub(value)
            .or_else(|| value.parse::<u64>().ok())
    };
    let owner_channel_num = resolve_channel(&primary_channel)
        .ok_or_else(|| anyhow!("agent {agent_id} primary channel is invalid: {primary_channel}"))?;
    let turn_channel_num = match message.target_channel_id.as_deref() {
        Some(target) => {
            resolve_channel(target).ok_or_else(|| anyhow!("target channel is invalid: {target}"))?
        }
        None => owner_channel_num,
    };
    let owner_channel = poise::serenity_prelude::ChannelId::new(owner_channel_num);
    let turn_channel = poise::serenity_prelude::ChannelId::new(turn_channel_num);

    let prompt = build_agent_prompt(message);
    let reservation = reserve_headless_agent_turn(turn_channel);
    let turn_id = reservation.turn_id().to_string();
    let recorded = db::record_delivery_agent_turn_intent_pg(
        pool,
        &message.id,
        &fire.delivery_id,
        &fire.claim_token,
        &turn_id,
    )
    .await
    .map_err(|error| anyhow!("record scheduled message turn intent {turn_id}: {error}"))?;
    if !recorded {
        return Err(anyhow!(
            "scheduled message claim was lost before turn {turn_id} could start"
        ));
    }
    let metadata = Some(serde_json::json!({
        "agent_id": agent_id,
        "scheduled_message_id": message.id,
        "turn_id": turn_id,
        "target_channel_id": message.target_channel_id,
        "parent_channel_id": owner_channel_num.to_string(),
    }));

    // Final cancellation/claim fence. Once this at-most-once barrier commits,
    // recovery must treat the turn as possibly launched even if this process
    // dies before persisting the later runtime acknowledgement.
    let launch_committed = db::commit_delivery_agent_launch_pg(
        pool,
        &message.id,
        &fire.delivery_id,
        &fire.claim_token,
        &turn_id,
        LEASE_SECS,
    )
    .await
    .map_err(|error| anyhow!("commit scheduled message turn launch {turn_id}: {error}"))?;
    if !launch_committed {
        return Err(anyhow!(
            "scheduled message claim was canceled before turn {turn_id} launch"
        ));
    }

    let outcome = start_reserved_headless_agent_turn_with_owner_channel(
        health_registry,
        owner_channel,
        turn_channel,
        provider,
        prompt,
        Some(OUTBOX_SOURCE.to_string()),
        metadata,
        Some(primary_channel.clone()),
        None,
        reservation,
    )
    .await
    .map_err(|error| anyhow!("start scheduled message turn for {agent_id}: {error}"))?;

    if outcome.status.as_str() != "started" {
        return Ok(AgentTurnStartDisposition::Consumed(turn_id));
    }

    // `HeadlessTurnStartError` is a pre-spawn contract. After `Started`, never
    // bubble a database/mismatch problem into the retry path: the committed
    // launch is ambiguous and a replacement could duplicate a late relay.
    if outcome.turn_id != turn_id {
        tracing::error!(
            expected_turn_id = turn_id,
            actual_turn_id = outcome.turn_id,
            "[smsg] started turn id mismatched its reservation; leaving launch committed and fail-closed"
        );
    } else {
        match db::mark_delivery_agent_turn_started_pg(
            pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            &turn_id,
            LEASE_SECS,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                delivery_id = fire.delivery_id,
                turn_id,
                "[smsg] launched turn could not record its runtime acknowledgement; launch remains fail-closed"
            ),
            Err(error) => tracing::warn!(
                delivery_id = fire.delivery_id,
                turn_id,
                "[smsg] failed to record runtime acknowledgement; launch remains fail-closed: {error}"
            ),
        }
    }
    match db::release_agent_delivery_to_poller_pg(pool, &fire.delivery_id, &fire.claim_token).await
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            delivery_id = fire.delivery_id,
            turn_id,
            "[smsg] started turn could not be released to poller; lease adoption will recover it"
        ),
        Err(error) => tracing::warn!(
            delivery_id = fire.delivery_id,
            turn_id,
            "[smsg] failed to release started turn to poller; lease adoption will recover it: {error}"
        ),
    }
    Ok(AgentTurnStartDisposition::Started)
}

pub(crate) fn build_agent_prompt(message: &ScheduledMessageRow) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "예약 메시지 전달 요청입니다. 아래 예약 메시지를 이 채널의 독자에게 전달하세요. \
         당신의 답변이 그대로 채널에 게시됩니다.\n\n",
    );
    if let Some(instruction) = message
        .agent_instruction
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        prompt.push_str("[전달 지침]\n");
        prompt.push_str(instruction.trim());
        prompt.push_str("\n\n");
    }
    if let Some(title) = message.title.as_deref().filter(|v| !v.trim().is_empty()) {
        prompt.push_str("[제목]\n");
        prompt.push_str(title.trim());
        prompt.push_str("\n\n");
    }
    prompt.push_str("[예약 메시지 원문]\n");
    prompt.push_str(&message.content);
    prompt
}

// ── Agent-turn completion polling ───────────────────────────────────────────

/// Check one running agent delivery for completion evidence. Returns true
/// when the delivery transitioned.
async fn poll_agent_delivery(pool: &PgPool, delivery: RunningAgentDelivery) -> bool {
    let Some(turn_id) = delivery.turn_id.as_deref() else {
        // Unreachable: the listing query only returns rows with a recorded
        // turn_id. Never interrupt here — a missing turn id means the start
        // call is still in flight (trigger-now fires outside the scheduler
        // tick), and rewinding would race it into a duplicate delivery.
        // Pre-turn crashes are owned by lease expiry recovery.
        return false;
    };

    match find_turn_delivery_evidence(pool, turn_id, delivery.launch_committed_at).await {
        Ok(Some(_)) => resolve_agent_delivery(pool, &delivery, false).await,
        Ok(None) => {
            let deadline =
                delivery.started_at + chrono::Duration::seconds(AGENT_COMPLETION_TIMEOUT_SECS);
            if Utc::now() >= deadline {
                return resolve_agent_delivery(pool, &delivery, true).await;
            }
            false
        }
        Err(error) => {
            tracing::warn!(
                delivery_id = delivery.delivery_id,
                "[smsg] completion evidence lookup failed: {error}"
            );
            false
        }
    }
}

fn raw_fallback_target(target_channel_id: Option<&str>, agent_id: Option<&str>) -> Option<String> {
    match (target_channel_id, agent_id) {
        (Some(channel_id), _) => Some(format!("channel:{channel_id}")),
        (None, Some(agent_id)) => Some(format!("agent:{agent_id}")),
        (None, None) => None,
    }
}

async fn enqueue_raw_fallback_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    scheduled_message_id: &str,
    fire_scheduled_at: DateTime<Utc>,
    target_channel_id: Option<&str>,
    agent_id: Option<&str>,
    content: &str,
    bot: &str,
) -> Result<Option<i64>, OutboxEnqueueError> {
    let Some(target) = raw_fallback_target(target_channel_id, agent_id) else {
        return Ok(None);
    };
    let reason_code = format!(
        "scheduled_message:v1:{scheduled_message_id}:fallback:{}",
        fire_scheduled_at.timestamp_micros()
    );
    enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
        tx,
        OutboxMessage {
            target: &target,
            content,
            bot,
            source: OUTBOX_SOURCE,
            reason_code: Some(&reason_code),
            session_key: None,
        },
    )
    .await
    .map(Some)
}

/// Re-check completion evidence while holding the active parent + delivery
/// locks, then commit the terminal state and any raw fallback together.
///
/// A timeout without terminal evidence deliberately fails closed without a
/// fallback: the headless turn may still relay a late answer, so staging raw
/// content at that boundary could deliver both messages to the user.
async fn resolve_agent_delivery(
    pool: &PgPool,
    delivery: &RunningAgentDelivery,
    allow_timeout: bool,
) -> bool {
    match resolve_agent_delivery_inner(pool, delivery, allow_timeout).await {
        Ok(transitioned) => transitioned,
        Err(error) => {
            tracing::warn!(
                delivery_id = delivery.delivery_id,
                "[smsg] atomic agent delivery resolution failed: {error}"
            );
            false
        }
    }
}

async fn resolve_agent_delivery_inner(
    pool: &PgPool,
    delivery: &RunningAgentDelivery,
    allow_timeout: bool,
) -> anyhow::Result<bool> {
    let Some(turn_id) = delivery.turn_id.as_deref() else {
        return Ok(false);
    };
    let mut tx = pool.begin().await?;
    if !db::lock_active_delivery_tx(
        &mut tx,
        &delivery.scheduled_message_id,
        &delivery.delivery_id,
        &delivery.claim_token,
    )
    .await?
    {
        return Ok(false);
    }

    let evidence =
        find_turn_delivery_evidence_on_connection(&mut tx, turn_id, delivery.launch_committed_at)
            .await?;
    let now = Utc::now();
    let deadline = delivery.started_at + chrono::Duration::seconds(AGENT_COMPLETION_TIMEOUT_SECS);
    let timed_out = evidence.is_none() && allow_timeout && now >= deadline;
    let terminal_failure = matches!(&evidence, Some(TurnEvidence::TerminalFailure(_)));
    if (terminal_failure || timed_out)
        && delivery
            .expires_at
            .is_some_and(|expires_at| expires_at <= now)
    {
        let error = "definition expired while agent turn awaited terminal evidence";
        let transitioned = db::finish_locked_delivery_and_finalize_parent_tx(
            &mut tx,
            &delivery.delivery_id,
            &delivery.claim_token,
            db::DELIVERY_INTERRUPTED,
            Some(error),
            None,
            None,
            &delivery.scheduled_message_id,
            false,
            db::STATUS_EXPIRED,
            None,
        )
        .await?;
        if !transitioned {
            return Ok(false);
        }
        tx.commit().await?;
        return Ok(true);
    }
    let (delivery_status, error, fallback_outbox_id, fired) = match evidence {
        Some(TurnEvidence::Delivered) => (db::DELIVERY_SENT, None, None, true),
        Some(TurnEvidence::TerminalFailure(reason)) => {
            if delivery.on_agent_failure == "push_raw" {
                match enqueue_raw_fallback_on_tx(
                    &mut tx,
                    &delivery.scheduled_message_id,
                    delivery.fire_scheduled_at,
                    delivery.target_channel_id.as_deref(),
                    delivery.agent_id.as_deref(),
                    &delivery.content,
                    &delivery.bot,
                )
                .await?
                {
                    Some(outbox_id) => (
                        db::DELIVERY_SENT,
                        Some(format!("{reason}; fell back to raw push")),
                        Some(outbox_id),
                        true,
                    ),
                    None => (
                        db::DELIVERY_FAILED,
                        Some(format!("{reason}; no fallback target")),
                        None,
                        false,
                    ),
                }
            } else {
                (db::DELIVERY_FAILED, Some(reason), None, false)
            }
        }
        None => {
            if !timed_out {
                return Ok(false);
            }
            let mut reason = format!(
                "agent turn produced no terminal evidence within {AGENT_COMPLETION_TIMEOUT_SECS}s"
            );
            if delivery.on_agent_failure == "push_raw" {
                reason.push_str(
                    "; raw fallback suppressed because the live turn may still relay a late answer",
                );
            }
            (db::DELIVERY_FAILED, Some(reason), None, false)
        }
    };

    let (next, forced_terminal) = compute_resume(
        delivery.schedule.as_deref(),
        &delivery.timezone,
        delivery.scheduled_at,
        delivery.expires_at,
        now,
    );
    let terminal_status = forced_terminal.unwrap_or(if delivery_status == db::DELIVERY_SENT {
        db::STATUS_SENT
    } else {
        db::STATUS_FAILED
    });
    let next = forced_terminal.is_none().then_some(next).flatten();
    let transitioned = db::finish_locked_delivery_and_finalize_parent_tx(
        &mut tx,
        &delivery.delivery_id,
        &delivery.claim_token,
        delivery_status,
        error.as_deref(),
        None,
        fallback_outbox_id,
        &delivery.scheduled_message_id,
        fired,
        terminal_status,
        next,
    )
    .await?;
    if !transitioned {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

async fn finish_exhausted_agent_with_raw_fallback(pool: &PgPool, fire: &ClaimedFire, reason: &str) {
    let message = &fire.message;
    if raw_fallback_target(
        message.target_channel_id.as_deref(),
        message.agent_id.as_deref(),
    )
    .is_none()
    {
        finish_terminal_failure(pool, fire, &format!("{reason}; no fallback target")).await;
        return;
    }
    match commit_exhausted_fallback(pool, fire, reason).await {
        Ok(true) => {}
        Ok(false) => tracing::info!(
            id = message.id,
            delivery_id = fire.delivery_id,
            "[smsg] exhausted fallback skipped after claim cancellation"
        ),
        Err(error) => {
            let error = format!("{reason}; push_raw fallback enqueue failed: {error}");
            finish_terminal_failure(pool, fire, &error).await;
        }
    }
}

async fn commit_exhausted_fallback(
    pool: &PgPool,
    fire: &ClaimedFire,
    reason: &str,
) -> anyhow::Result<bool> {
    let message = &fire.message;
    let mut tx = pool.begin().await?;
    if !db::lock_active_delivery_tx(&mut tx, &message.id, &fire.delivery_id, &fire.claim_token)
        .await?
    {
        return Ok(false);
    }
    let Some(fallback_outbox_id) = enqueue_raw_fallback_on_tx(
        &mut tx,
        &message.id,
        fire.fire_scheduled_at,
        message.target_channel_id.as_deref(),
        message.agent_id.as_deref(),
        &message.content,
        &message.bot,
    )
    .await?
    else {
        return Ok(false);
    };
    let error = format!("{reason}; fell back to raw push");
    let transitioned = db::finish_locked_delivery_and_finalize_parent_tx(
        &mut tx,
        &fire.delivery_id,
        &fire.claim_token,
        db::DELIVERY_SENT,
        Some(&error),
        None,
        Some(fallback_outbox_id),
        &message.id,
        true,
        db::STATUS_FAILED,
        None,
    )
    .await?;
    if !transitioned {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

// ── Shared transitions ──────────────────────────────────────────────────────

async fn finish_terminal_failure(pool: &PgPool, fire: &ClaimedFire, error: &str) {
    finish_terminal(
        pool,
        fire,
        db::DELIVERY_FAILED,
        Some(error),
        None,
        None,
        false,
    )
    .await;
}

/// Exhausting a fire slot is terminal for the definition, even when the
/// agent failure policy successfully hands this final attempt to raw push.
/// The delivery can record that fallback as sent, but the recurring parent
/// stays failed and requires an operator decision instead of silently moving
/// on to its next occurrence.
#[allow(clippy::too_many_arguments)]
async fn finish_terminal(
    pool: &PgPool,
    fire: &ClaimedFire,
    delivery_status: &str,
    error: Option<&str>,
    outbox_id: Option<i64>,
    fallback_outbox_id: Option<i64>,
    fired: bool,
) {
    if let Err(db_error) = db::finish_delivery_and_finalize_parent_pg(
        pool,
        &fire.delivery_id,
        &fire.claim_token,
        delivery_status,
        error,
        outbox_id,
        fallback_outbox_id,
        &fire.message.id,
        fired,
        db::STATUS_FAILED,
        None,
    )
    .await
    {
        tracing::warn!(
            id = fire.message.id,
            "[smsg] terminal delivery failure transition failed: {db_error}"
        );
    }
}

/// Recurrence: a live future slot (manual trigger-now case) resumes as-is;
/// otherwise the next occurrence comes from the routine schedule grammar.
/// Recurrences past expires_at end the definition as `expired`.
fn compute_resume(
    message_schedule: Option<&str>,
    timezone: &str,
    current_scheduled_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, Option<&'static str>) {
    let Some(schedule) = message_schedule.filter(|value| !value.trim().is_empty()) else {
        return (None, None);
    };
    let next = if current_scheduled_at > now {
        current_scheduled_at
    } else {
        match crate::services::routines::next_due_after_anchor(
            schedule,
            timezone,
            current_scheduled_at,
            now,
        ) {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!("[smsg] recurrence computation failed: {error}");
                return (None, Some(db::STATUS_FAILED));
            }
        }
    };
    if let Some(expires_at) = expires_at {
        if next >= expires_at {
            return (None, Some(db::STATUS_EXPIRED));
        }
    }
    (Some(next), None)
}

/// Transient failure: mark the delivery interrupted and rewind the parent to
/// its fire slot so the due scan re-arms it (bounded by MAX_FIRE_RETRIES).
async fn interrupt_for_retry(pool: &PgPool, fire: &ClaimedFire, error: &str) {
    let next_attempt_at = fire_retry_next_at(fire.retry_count, Utc::now());
    interrupt_delivery(
        pool,
        &fire.delivery_id,
        &fire.claim_token,
        &fire.message.id,
        fire.fire_scheduled_at,
        next_attempt_at,
        error,
    )
    .await;
}

fn fire_retry_next_at(
    retry_count_before_increment: i32,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    usize::try_from(retry_count_before_increment)
        .ok()
        .and_then(|index| FIRE_RETRY_BACKOFF_SECS.get(index))
        .map(|delay_secs| now + chrono::Duration::seconds(*delay_secs))
}

async fn interrupt_delivery(
    pool: &PgPool,
    delivery_id: &str,
    claim_token: &str,
    message_id: &str,
    fire_scheduled_at: DateTime<Utc>,
    next_attempt_at: Option<DateTime<Utc>>,
    error: &str,
) {
    if let Err(db_error) = db::interrupt_delivery_and_rewind_pg(
        pool,
        delivery_id,
        claim_token,
        message_id,
        fire_scheduled_at,
        next_attempt_at,
        error,
    )
    .await
    {
        tracing::warn!(
            id = message_id,
            "[smsg] interrupt transition failed: {db_error}"
        );
    }
}

#[cfg(test)]
mod postgres_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn message_with(
        schedule: Option<&str>,
        scheduled_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> (Option<String>, String, DateTime<Utc>, Option<DateTime<Utc>>) {
        (
            schedule.map(str::to_string),
            "Asia/Seoul".to_string(),
            scheduled_at,
            expires_at,
        )
    }

    #[test]
    fn one_shot_has_no_resume() {
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let (schedule, tz, at, exp) = message_with(None, now, None);
        let (next, terminal) = compute_resume(schedule.as_deref(), &tz, at, exp, now);
        assert_eq!(next, None);
        assert_eq!(terminal, None);
    }

    #[test]
    fn recurring_advances_past_now() {
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let (schedule, tz, at, exp) = message_with(Some("@every 10m"), now, None);
        let (next, terminal) = compute_resume(schedule.as_deref(), &tz, at, exp, now);
        assert_eq!(terminal, None);
        assert_eq!(
            next,
            Some(Utc.with_ymd_and_hms(2026, 7, 8, 0, 10, 0).unwrap())
        );
    }

    #[test]
    fn recurring_interval_stays_anchored_when_completion_is_late() {
        let slot = Utc.with_ymd_and_hms(2026, 7, 8, 9, 0, 0).unwrap();
        let completed = Utc.with_ymd_and_hms(2026, 7, 8, 9, 5, 0).unwrap();
        let (schedule, tz, at, exp) = message_with(Some("@every 24h"), slot, None);
        let (next, terminal) = compute_resume(schedule.as_deref(), &tz, at, exp, completed);
        assert_eq!(terminal, None);
        assert_eq!(
            next,
            Some(Utc.with_ymd_and_hms(2026, 7, 9, 9, 0, 0).unwrap())
        );
    }

    #[test]
    fn recurring_interval_skips_missed_slots_to_first_future_anchor() {
        let slot = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let completed = Utc.with_ymd_and_hms(2026, 7, 8, 0, 27, 0).unwrap();
        let (schedule, tz, at, exp) = message_with(Some("@every 10m"), slot, None);
        let (next, terminal) = compute_resume(schedule.as_deref(), &tz, at, exp, completed);
        assert_eq!(terminal, None);
        assert_eq!(
            next,
            Some(Utc.with_ymd_and_hms(2026, 7, 8, 0, 30, 0).unwrap())
        );
    }

    #[test]
    fn fire_retry_backoff_is_exponential_and_capped() {
        let now = Utc.with_ymd_and_hms(2026, 7, 11, 7, 0, 0).unwrap();
        assert_eq!(
            fire_retry_next_at(0, now),
            Some(now + chrono::Duration::seconds(60))
        );
        assert_eq!(
            fire_retry_next_at(1, now),
            Some(now + chrono::Duration::seconds(300))
        );
        assert_eq!(
            fire_retry_next_at(2, now),
            Some(now + chrono::Duration::seconds(900))
        );
        assert_eq!(fire_retry_next_at(3, now), None);
        assert_eq!(fire_retry_next_at(-1, now), None);
    }

    #[test]
    fn no_reply_transcripts_are_terminal_failures() {
        for message in ["NO_REPLY", " no_reply ", "No_RePlY\n"] {
            assert_eq!(
                transcript_delivery_evidence(message),
                TurnEvidence::TerminalFailure("agent turn returned NO_REPLY".to_string())
            );
        }
        assert_eq!(
            transcript_delivery_evidence("예약 내용을 전달했습니다."),
            TurnEvidence::Delivered
        );
    }

    #[test]
    fn manual_fire_resumes_original_future_slot() {
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let original = Utc.with_ymd_and_hms(2026, 7, 9, 9, 0, 0).unwrap();
        let (schedule, tz, at, exp) = message_with(Some("@every 24h"), original, None);
        let (next, terminal) = compute_resume(schedule.as_deref(), &tz, at, exp, now);
        assert_eq!(terminal, None);
        assert_eq!(next, Some(original));
    }

    #[test]
    fn recurrence_past_expiry_ends_expired() {
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let expires = Utc.with_ymd_and_hms(2026, 7, 8, 0, 5, 0).unwrap();
        let (schedule, tz, at, exp) = message_with(Some("@every 10m"), now, Some(expires));
        let (next, terminal) = compute_resume(schedule.as_deref(), &tz, at, exp, now);
        assert_eq!(next, None);
        assert_eq!(terminal, Some(db::STATUS_EXPIRED));
    }

    #[test]
    fn invalid_schedule_fails_terminal() {
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let (schedule, tz, at, exp) = message_with(Some("not a schedule"), now, None);
        let (next, terminal) = compute_resume(schedule.as_deref(), &tz, at, exp, now);
        assert_eq!(next, None);
        assert_eq!(terminal, Some(db::STATUS_FAILED));
    }

    #[test]
    fn agent_prompt_includes_instruction_and_content() {
        let message = ScheduledMessageRow {
            id: "smsg_test".to_string(),
            content: "내일 배포 예정".to_string(),
            title: Some("배포 공지".to_string()),
            target_channel_id: Some("123".to_string()),
            bot: "announce".to_string(),
            delivery_kind: "agent".to_string(),
            agent_id: Some("coder".to_string()),
            agent_instruction: Some("3줄로 요약".to_string()),
            on_agent_failure: "fail".to_string(),
            scheduled_at: Utc::now(),
            schedule: None,
            timezone: "Asia/Seoul".to_string(),
            expires_at: None,
            status: "firing".to_string(),
            in_flight_delivery_id: None,
            fire_count: 0,
            last_fired_at: None,
            last_error: None,
            source: "api".to_string(),
            created_by: None,
            dedupe_key: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let prompt = build_agent_prompt(&message);
        assert!(prompt.contains("3줄로 요약"));
        assert!(prompt.contains("내일 배포 예정"));
        assert!(prompt.contains("배포 공지"));
    }

    #[test]
    fn booting_discord_runtime_errors_defer_without_consuming_retry() {
        for message in [
            "provider runtime not registered: codex",
            "provider runtime is not ready for channel 123",
            "matched runtime is not ready for provider codex on channel 123",
            "provider token unavailable for channel 123",
        ] {
            assert!(agent_start_error_is_runtime_unavailable(&anyhow::anyhow!(
                "start scheduled message turn: {message}"
            )));
        }
        assert!(!agent_start_error_is_runtime_unavailable(&anyhow::anyhow!(
            "agent mailbox is busy for channel 123"
        )));
    }
}
