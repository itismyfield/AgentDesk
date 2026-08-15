//! Intake-outbox handoff stamp at the worker bridge boundary.

use super::*;
/// Stamps the worker's intake row immediately before the bridge is registered.
///
/// `dispatched` 도장은 유일 호출 지점(intake_turn.rs의 spawn_turn_bridge 직전)에서만 찍힌다.
/// 도장 SQL 실패 시 spawned인 채 bridge가 뜨는 창, 도장 성공 후 spawn 등록 실패로 bridge 없는
/// dispatched가 남는 창이 각각 존재하며, intake-delivery sweep은 worker가 닫지 않은
/// spawned 잔류와 dispatched 잔류를 회수한다.
///
/// `wait_for_completion=false`에서는 도장 SQL이 실패한 뒤 worker가 spawned→done을 먼저
/// 끝내고 terminal delivery 커밋 전에 bridge가 죽을 수 있다. 그 미배달 done 행은 sweep 대상이
/// 아니며 pre-T2-W에도 있던 기존 노출이다. receipt settlement의 2-상태 CAS는 도장 실패 뒤
/// spawned를 직접 종결할 수 있다.
///
/// `wait_for_completion=true`인 Forwarded 경로에서는 bridge 커밋이 worker의 done 도장보다
/// 먼저 올 수 있다. `wait_for_completion=false`도 스케줄링상 worker-first를 보장하지 않는다.
/// receipt settlement의 2-상태 CAS는 이 두 순서를 모두 받아들인다.
pub(super) async fn stamp_before_bridge_handoff(
    shared: &Arc<SharedData>,
    intake_outbox_id: Option<i64>,
) {
    let Some(outbox_id) = intake_outbox_id else {
        return;
    };
    // This pre-await sample is the snapshot contract declared on
    // `SettlementCapabilities`; record it for the adjacent bridge spawn after
    // the stamp attempt finishes.
    let capabilities = shared.intake_delivery_capabilities.current();
    if !capabilities.stamp_dispatched {
        shared
            .intake_delivery_capabilities
            .record_bridge_turn_snapshot(outbox_id, capabilities);
        return;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        tracing::debug!(
            intake_outbox_id = outbox_id,
            "intake bridge handoff has no PostgreSQL pool for dispatched stamping"
        );
        shared
            .intake_delivery_capabilities
            .record_bridge_turn_snapshot(outbox_id, capabilities);
        return;
    };
    match crate::db::intake_outbox_dispatch_stamp::mark_dispatched(pool, outbox_id).await {
        Ok(true) => tracing::debug!(
            intake_outbox_id = outbox_id,
            "intake bridge handoff stamped dispatched"
        ),
        // This site does not own terminal-state classification. The worker's
        // mark_done miss path performs the read-only status classification.
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
    shared
        .intake_delivery_capabilities
        .record_bridge_turn_snapshot(outbox_id, capabilities);
}

#[cfg(test)]
#[path = "dispatch_stamp/tests.rs"]
mod postgres_tests;
