//! Intake-outbox handoff stamp at the worker bridge boundary.

use super::*;
/// Stamps the worker's intake row immediately before the bridge is registered.
///
/// `dispatched` 도장은 유일 호출 지점(intake_turn.rs의 spawn_turn_bridge 직전)에서만 찍힌다.
/// 도장 SQL 실패 시 spawned인 채 bridge가 뜨는 창, 도장 성공 후 spawn 등록 실패로 bridge 없는
/// dispatched가 남는 창이 각각 존재하며, 후속 S-W3 intake-delivery sweep은 worker가 닫지 않은
/// spawned 잔류와 dispatched 잔류를 회수한다.
///
/// `wait_for_completion=false`에서는 도장 SQL이 실패한 뒤 worker가 spawned→done을 먼저
/// 끝내고 terminal delivery 커밋 전에 bridge가 죽을 수 있다. 그 미배달 done 행은 sweep 대상이
/// 아니며 pre-T2-W에도 있던 기존 노출이다. 후속 S-W2 receipt settlement 설계가 이 순서를
/// 다룬다.
///
/// `wait_for_completion=true`인 Forwarded 경로에서는 bridge 커밋이 worker의 done 도장보다
/// 먼저 올 수 있다. `wait_for_completion=false`도 스케줄링상 worker-first를 보장하지 않는다.
/// 후속 정산 슬라이스의 2-상태 CAS는 이 두 순서를 모두 받아들여야 한다.
///
/// `changed` 완료 후 설정 버전은 `borrow_and_update`에서 소비되고 probe를 await한다. 그 사이 Off가
/// 도착하면 stale stage 결과가 먼저 replace되고, 다음 반복에서야 Off 결과가 replace된다. 이
/// 빌드에서는 dispatched-stamping 클램프 때문에 그 창에 이전 능력으로 도장할 수 없다. 클램프가
/// 해제되는 S-W3 이후에는 두 replace 사이에서 시작된 턴이 stale 능력으로 도장할 수 있으므로,
/// probe 후 스테이지 재확인도 S-W3에서 함께 다룬다.
pub(super) async fn stamp_before_bridge_handoff(
    shared: &Arc<SharedData>,
    intake_outbox_id: Option<i64>,
) {
    let Some(outbox_id) = intake_outbox_id else {
        return;
    };
    let capabilities = shared.intake_delivery_capabilities.current();
    if !capabilities.stamp_dispatched {
        return;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
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
        // lifecycle race. S-W4 adds read-only status classification; until
        // then a false CAS is deliberately observation-only and never warns.
        Ok(false) => tracing::debug!(
            intake_outbox_id = outbox_id,
            "intake bridge handoff dispatched CAS was a no-op"
        ),
        // The bridge still launches. S-W3 can reclaim an open spawned row, but
        // not the existing worker-first done window described above. Turning
        // the SQL failure into a turn error risks duplicate delivery.
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
        sqlx::query(
            "ALTER TABLE public.intake_outbox
             DROP CONSTRAINT intake_outbox_dispatched_requires_clock",
        )
        .execute(&pool)
        .await
        .expect("make the migrated schema fail the capability probe");
        let capabilities =
            crate::services::discord::runtime_bootstrap::intake_delivery_capability::bootstrap_for_test(
                Some(pool.clone()),
                crate::config::IntakeDeliverySettlementStage::Enforce,
            )
            .await;
        assert!(!capabilities.current().stamp_dispatched);
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool.clone()),
            capabilities,
        );
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

        stamp_before_bridge_handoff(&shared, Some(id)).await;
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

    #[tokio::test]
    async fn stamp_is_written_when_ready_capability_is_injected_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let capabilities = crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilityCache::for_test(
            crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilities {
                stamp_dispatched: true,
                settle_and_sweep: true,
            },
        );
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool.clone()),
            capabilities,
        );
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO public.intake_outbox (
                target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                status, claim_owner, spawned_at
             ) VALUES (
                'worker', 'leader', 'stamp-on', 'stamp-on', 'user', 'hello',
                'standard', 'agent', 'spawned', 'dispatch-worker', NOW()
             ) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("seed spawned intake row");

        stamp_before_bridge_handoff(&shared, Some(id)).await;
        let status: IntakeOutboxStatus =
            sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("read stamped row");
        assert_eq!(status, IntakeOutboxStatus::Dispatched);

        pool.close().await;
        database.drop().await;
    }
}
