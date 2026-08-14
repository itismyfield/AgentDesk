//! Intake-outbox handoff stamp at the worker bridge boundary.

use super::*;
use crate::services::discord::runtime_bootstrap::intake_delivery_capability::{
    SettlementCapabilities, resolve_capabilities,
};

/// Stamps the worker's intake row immediately before the bridge is registered.
///
/// `dispatched` 도장은 유일 호출 지점(intake_turn.rs의 spawn_turn_bridge 직전)에서만 찍힌다.
/// 도장 SQL 실패 시 spawned인 채 bridge가 뜨는 창, 도장 성공 후 spawn 등록 실패로 bridge 없는
/// dispatched가 남는 창이 각각 존재하며, 전자는 spawned sweep(A), 후자는 dispatched sweep +
/// E-회수자가 정산한다.
///
/// `wait_for_completion=true`인 Forwarded 경로에서는 bridge 커밋이 worker의 done 도장보다
/// 먼저 올 수 있다. `wait_for_completion=false`도 스케줄링상 worker-first를 보장하지 않는다.
/// 후속 정산 슬라이스의 2-상태 CAS는 이 두 순서를 모두 받아들여야 한다.
pub(super) async fn stamp_before_bridge_handoff(
    shared: &Arc<SharedData>,
    intake_outbox_id: Option<i64>,
) {
    let Some(outbox_id) = intake_outbox_id else {
        return;
    };
    let capabilities = resolve_capabilities(shared.pg_pool.as_ref()).await;
    stamp_with_capabilities(shared.pg_pool.as_ref(), outbox_id, capabilities).await;
}

async fn stamp_with_capabilities(
    pool: Option<&sqlx::PgPool>,
    outbox_id: i64,
    capabilities: SettlementCapabilities,
) {
    if !capabilities.stamp_dispatched {
        tracing::debug!(
            intake_outbox_id = outbox_id,
            "intake bridge handoff observed with dispatched stamping disabled"
        );
        return;
    }
    let Some(pool) = pool else {
        tracing::debug!(
            intake_outbox_id = outbox_id,
            "intake bridge handoff has no PostgreSQL pool for dispatched stamping"
        );
        return;
    };
    match crate::db::intake_outbox_dispatch_stamp::mark_dispatched(pool, outbox_id).await {
        Ok(true) => tracing::debug!(
            intake_outbox_id = outbox_id,
            "intake bridge handoff stamped dispatched"
        ),
        // A terminal row (including `done`) means another actor won a normal
        // lifecycle race. S-W5 adds read-only status classification; until
        // then a false CAS is deliberately observation-only and never warns.
        Ok(false) => tracing::debug!(
            intake_outbox_id = outbox_id,
            "intake bridge handoff dispatched CAS was a no-op"
        ),
        // The bridge still launches. A later spawned/dispatched sweep owns the
        // open row; turning this into a turn error risks duplicate delivery.
        Err(error) => tracing::error!(
            intake_outbox_id = outbox_id,
            %error,
            "failed to stamp intake bridge handoff as dispatched"
        ),
    }
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::intake_outbox_status::IntakeOutboxStatus;

    #[tokio::test]
    async fn stamp_is_skipped_when_capability_not_ready_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO public.intake_outbox (
                target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                status, claim_owner, spawned_at
             ) VALUES (
                'worker', 'leader', 'stamp-off', 'stamp-off', 'user', 'hello',
                'standard', 'agent', 'spawned', 'dispatch-worker', NOW()
             ) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("seed spawned intake row");

        stamp_with_capabilities(Some(&pool), id, SettlementCapabilities::default()).await;
        let status: IntakeOutboxStatus =
            sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("read unstamped row");
        assert_eq!(status, IntakeOutboxStatus::Spawned);

        pool.close().await;
        database.drop().await;
    }
}
