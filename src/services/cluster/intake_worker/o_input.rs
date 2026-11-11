//! Claimed input keeps its SQL identity while the writer observes fresh eligibility.

#[cfg(test)]
use super::test_executor::execute_intake_turn_core;
use super::*;
#[cfg(not(test))]
use crate::services::discord::execute_intake_turn_core;
use crate::services::tui_o::cutover::intake_route::IntakeRoute;

pub(super) async fn hold_off_o_gateway(
    pool: &PgPool,
    row: &IntakeOutboxRow,
    claim_owner: &str,
    provider: &str,
    channel_id: ChannelId,
    target: &str,
    phase: &'static str,
) -> Result<bool, sqlx::Error> {
    let selected = intake_route::selected_codex_input(provider, channel_id.get());
    #[cfg(test)]
    if selected && phase == "final" {
        test_executor::checkpoint(test_executor::Checkpoint::PreAccept).await;
    }
    let exact = if selected {
        let current = sqlx::query_as::<_, (String, String, String, String, Option<String>)>(
            "SELECT provider, channel_id, target_instance_id, status, claim_owner
         FROM intake_outbox WHERE id=$1",
        )
        .bind(row.id)
        .fetch_optional(pool)
        .await?;
        row.provider == provider
            && row.channel_id == channel_id.get().to_string()
            && row.target_instance_id == target
            && row.status == IntakeOutboxStatus::Claimed
            && row.claim_owner.as_deref() == Some(claim_owner)
            && current.is_some_and(|(kind, channel, node, status, owner)| {
                kind == row.provider
                    && channel == row.channel_id
                    && node == row.target_instance_id
                    && status == "claimed"
                    && owner.as_deref() == Some(claim_owner)
            })
    } else {
        true
    };
    let detail = if !exact {
        Some("claimed input identity changed".to_owned())
    } else {
        let route = if selected {
            intake_route::for_claimed_input(provider, channel_id.get(), row.id, phase).await
        } else {
            intake_route::route(provider, channel_id.get())
        };
        match route {
            IntakeRoute::Hold(detail) => Some(detail),
            IntakeRoute::Unselected | IntakeRoute::Gateway => None,
        }
    };
    let Some(detail) = detail else {
        return Ok(false);
    };
    let released = return_claimed_to_pending(pool, row.id, claim_owner).await?;
    tracing::warn!(
        row_id = row.id,
        channel_id = row.channel_id,
        released,
        "[intake_worker] O channel held before accept: {detail}"
    );
    Ok(true)
}

#[cfg(unix)]
pub(super) mod scope_key {
    pub(crate) struct WorkerScopeKey(pub(super) ());
}

/// The first entry consumes the slot even when the request does not match.
#[cfg(unix)]
pub(crate) fn take_before_effects(request: &IntakeRequest) -> Option<BeforeEffects> {
    crate::intake_consent_authority::issuer::take(scope_key::WorkerScopeKey(())).filter(|proof| {
        intake_request_from_row(proof.row()).is_ok_and(|original| original == *request)
    })
}

#[cfg(unix)]
async fn scope_before_effects<T>(
    row: &IntakeOutboxRow,
    execution: impl std::future::Future<Output = T>,
) -> T {
    crate::intake_consent_authority::issuer::scope(scope_key::WorkerScopeKey(()), row, execution)
        .await
}

pub(super) async fn execute_before_effects(
    row: &IntakeOutboxRow,
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    request: IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> Result<IntakeExecution, Box<dyn std::error::Error + Send + Sync>> {
    let execution = Box::pin(execute_intake_turn_core(http, shared, token, request, uploads));
    #[cfg(unix)]
    {
        scope_before_effects(row, execution).await
    }
    #[cfg(not(unix))]
    {
        let _ = row;
        execution.await
    }
}

#[cfg(all(test, unix))]
#[path = "consent_scope_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[tokio::test]
async fn effects_cannot_acquire_late_before_effects_authority_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (row, root, store, consent, intent) =
        crate::db::intake_outbox::consent_handoff::tests::setup(&pool, "891214").await;
    let request = intake_request_from_row(&row).unwrap();
    let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, row.id).await;
    scope_before_effects(&row, async {
        let proof = take_before_effects(&request).unwrap();
        drop(proof);
        sqlx::query("CREATE TABLE consent_boundary_effects (row_id BIGINT PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO consent_boundary_effects VALUES ($1)")
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
        let rearmed = match take_before_effects(&request) {
            Some(late) => {
                crate::db::intake_outbox::consent_handoff::return_before_record(&pool, late)
                    .await
                    .unwrap()
            }
            None => false,
        };
        assert!(!rearmed, "effects cannot obtain late rollback authority");
    })
    .await;
    assert!(take_before_effects(&request).is_none());
    assert_eq!(
        crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, row.id).await,
        before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM consent_boundary_effects")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    drop((root, store, consent, intent));
    pool.close().await;
    db.drop().await;
}
#[cfg(all(test, unix))]
#[tokio::test]
async fn scoped_reclaim_preserves_once_marker_across_process_tokens_pg() {
    use crate::db::intake_outbox::{
        claim_pending_for_target, consent_handoff, mark_accepted, mark_spawned,
        owner_fence_pg_tests::snapshot,
    };

    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (channel, fresh_incarnation) in [("891290", false), ("891291", true)] {
        let fixture = consent_handoff::tests::setup(&pool, channel).await;
        let original = &fixture.0;
        let request = intake_request_from_row(original).unwrap();
        let original_token = original.claim_owner.as_deref().unwrap();
        let next_incarnation = format!("{original_token}-fresh-incarnation");
        let token = if fresh_incarnation {
            next_incarnation.as_str()
        } else {
            original_token
        };
        assert_eq!(token == original_token, !fresh_incarnation);
        assert!(take_before_effects(&request).is_none());
        assert!(
            scope_before_effects(original, async {
                let proof = take_before_effects(&request).unwrap();
                assert!(take_before_effects(&request).is_none());
                consent_handoff::return_before_record(&pool, proof)
                    .await
                    .unwrap()
            })
            .await
        );
        let rearmed = snapshot(&pool, original.id).await;
        assert_eq!(rearmed["status"], "pending");
        assert!(rearmed["claim_owner"].is_null());
        let claimed = claim_pending_for_target(&pool, "fence-worker", "claude", token)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, original.id);
        assert_eq!(claimed.claim_owner.as_deref(), Some(token));
        assert!(mark_accepted(&pool, original.id, token).await.unwrap());
        assert!(mark_spawned(&pool, original.id, token).await.unwrap());
        let before = snapshot(&pool, original.id).await;
        assert_eq!(before["status"], "spawned");
        assert_eq!(before["last_error"], rearmed["last_error"]);
        let reclaimed_request = intake_request_from_row(&claimed).unwrap();
        assert!(take_before_effects(&reclaimed_request).is_none());
        assert!(
            !scope_before_effects(&claimed, async {
                let proof = take_before_effects(&reclaimed_request).unwrap();
                assert!(take_before_effects(&reclaimed_request).is_none());
                consent_handoff::return_before_record(&pool, proof)
                    .await
                    .unwrap()
            })
            .await
        );
        assert!(take_before_effects(&reclaimed_request).is_none());
        assert_eq!(snapshot(&pool, original.id).await, before);
        drop(fixture);
    }
    pool.close().await;
    db.drop().await;
}
