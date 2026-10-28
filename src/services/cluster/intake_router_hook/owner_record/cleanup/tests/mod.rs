use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::intake_router_hook::owner_record::{self, output};
use crate::services::tui_o::ownership::execution;

pub(crate) struct Fixture {
    _db: TestPostgresDb,
    pub(crate) pool: PgPool,
    id: OwnerIdentity,
    generation: i64,
    pending: i64,
}
impl Fixture {
    async fn new(channel: u64) -> Self {
        Self::with_native(channel, None).await
    }
    pub(crate) async fn with_native(channel: u64, native: Option<Value>) -> Self {
        Self::with_native_provider(channel, "claude", native).await
    }
    pub(crate) async fn with_native_provider(
        channel: u64,
        provider: &str,
        native: Option<Value>,
    ) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let id = OwnerIdentity::new(provider, &channel.to_string());
        let old = execution::tests::admission(&pool, provider, channel, "mac-a").await;
        if let Some(binding) = native {
            use crate::db::intake_outbox;
            use crate::services::cluster::intake_router_hook::owner_record::input_start;
            let claimed = intake_outbox::claim_pending_for_target_except(
                &pool,
                "mac-a",
                provider,
                "cleanup-fixture",
                &[],
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(claimed.id, old);
            assert!(
                intake_outbox::mark_accepted(&pool, old, "cleanup-fixture")
                    .await
                    .unwrap()
            );
            assert!(
                intake_outbox::mark_spawned(&pool, old, "cleanup-fixture")
                    .await
                    .unwrap()
            );
            let permit = input_start::prepare(
                &pool,
                input_start::StartRequest {
                    delivery_id: old,
                    executor: "mac-a",
                    claim_owner: "cleanup-fixture",
                    incarnation: "cleanup-runtime",
                    frame: "old frame",
                    native_binding: &binding,
                },
            )
            .await
            .unwrap();
            permit
                .submit("old frame", &binding, |_| async {})
                .await
                .unwrap();
        }
        sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        let mut payload = output::tests::payload(&channel.to_string(), "90200", "gateway");
        payload.provider = provider.into();
        let pending = custody::stage(
            &pool,
            custody::StageRequest {
                payload: &payload,
                home_revision: 0,
                wait_generation: Some(0),
                grace_seconds: 0,
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE intake_outbox SET target_instance_id='mac-b' WHERE id=$1")
            .bind(pending)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            request(&pool, &id, ("mac-a", 0), pending, "mac-b", 12)
                .await
                .unwrap()
        );
        Self {
            _db: db,
            pool,
            id,
            generation: 0,
            pending,
        }
    }
    pub(crate) async fn row(&self) -> Record {
        sqlx::query_as("SELECT * FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3").bind(self.id.provider()).bind(self.id.raw_channel_id()).bind(self.generation).fetch_one(&self.pool).await.unwrap()
    }
    pub(crate) async fn wake(&self) {
        sqlx::query("UPDATE intake_session_owners SET routing_cleanup_next=NOW()-INTERVAL '1 second' WHERE provider=$1 AND raw_channel_id=$2").bind(self.id.provider()).bind(self.id.raw_channel_id()).execute(&self.pool).await.unwrap();
    }
}

#[tokio::test]
async fn owner_cleanup_fixed_deadline_busy_and_unavailable_release_only_new_source_pg() {
    let fx = Fixture::new(73369201).await;
    let first = fx.row().await;
    assert!(
        !request(&fx.pool, &fx.id, ("mac-a", 0), fx.pending, "mac-b", 100)
            .await
            .unwrap()
    );
    let repeat = fx.row().await;
    assert_eq!(repeat.routing_cleanup.0, first.routing_cleanup.0);
    assert_eq!(
        repeat.routing_cleanup_deadline,
        first.routing_cleanup_deadline
    );
    let claimed = claim(&fx.pool, &first).await.unwrap().unwrap();
    assert!(respond(&fx.pool, &claimed, Response::Busy).await.unwrap());
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 hour' WHERE id=$1").bind(first.id).execute(&fx.pool).await.unwrap();
    assert!(
        !advance(&fx.pool, first.id).await.unwrap(),
        "responsive Busy has no turn time limit"
    );
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_seen=NOW()-INTERVAL '13 seconds' WHERE id=$1").bind(first.id).execute(&fx.pool).await.unwrap();
    assert!(
        advance(&fx.pool, first.id).await.unwrap(),
        "missing control ACK cannot hold healthy target forever"
    );
    let owner = owner_record::read_latest_owner_in_tx(&mut fx.pool.begin().await.unwrap(), &fx.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (owner.owner_instance_id.as_str(), owner.generation),
        ("mac-b", 1)
    );
    let state = fx.row().await;
    assert_ne!(
        state.routing_cleanup_state, "ended",
        "unavailable never fabricates normal end"
    );
    let deferred: bool = sqlx::query_scalar(
        "SELECT routing_cleanup_deferred FROM intake_session_owners WHERE id=$1",
    )
    .bind(state.id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(deferred);
    fx.pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_retry_budget_restart_and_late_callback_cas_pg() {
    let fx = Fixture::new(73369202).await;
    let mut stale = None;
    for attempt in 1..=3 {
        fx.wake().await;
        let row = fx.row().await;
        let row = claim(&fx.pool, &row).await.unwrap().unwrap();
        if let Some(stale) = stale.as_ref() {
            assert!(
                !respond(&fx.pool, stale, Response::Ended).await.unwrap(),
                "late lease cannot overwrite another attempt"
            );
        }
        assert!(begin_end(&fx.pool, &row).await.unwrap());
        assert!(
            !begin_end(&fx.pool, &row).await.unwrap(),
            "one lease cannot spend its attempt twice"
        );
        assert!(respond(&fx.pool, &row, Response::Retry).await.unwrap());
        let restored = fx.row().await;
        assert_eq!(restored.routing_cleanup_attempts, attempt);
        assert_eq!(
            restored.routing_cleanup.operation,
            row.routing_cleanup.operation
        );
        stale = Some(row);
    }
    fx.wake().await;
    let row = fx.row().await;
    assert_eq!(row.routing_cleanup_state, "deferred");
    assert!(claim(&fx.pool, &row).await.unwrap().is_none());
    assert!(
        advance(&fx.pool, row.id).await.unwrap(),
        "restart between response and admission still resumes the stored input"
    );
    assert!(
        due(&fx.pool, "claude", "mac-b").await.unwrap().is_empty(),
        "only original node sees physical cleanup"
    );
    fx.pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_normal_end_and_permanent_refusal_are_distinct_pg() {
    for (channel, response) in [
        (73369203, Response::Ended),
        (73369204, Response::NeedsOperator),
        (73369205, Response::IdentityChanged),
    ] {
        let fx = Fixture::new(channel).await;
        let row = claim(&fx.pool, &fx.row().await).await.unwrap().unwrap();
        assert!(respond(&fx.pool, &row, response).await.unwrap());
        assert!(advance(&fx.pool, row.id).await.unwrap());
        let restored = fx.row().await;
        assert_eq!(restored.routing_cleanup_state, response.state());
        assert!(due(&fx.pool, "claude", "mac-a").await.unwrap().is_empty());
        fx.pool.close().await;
    }
}

pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_PC_C_MUTANT").as_deref() == Ok(name)
}

#[tokio::test]
async fn owner_cleanup_promotion_requires_durable_control_not_caller_readiness_pg() {
    let fx = Fixture::new(73369206).await;
    let row = fx.row().await;
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup=NULL,routing_cleanup_state=NULL,routing_cleanup_deadline=NULL,routing_cleanup_seen=NULL,routing_cleanup_next=NULL,routing_cleanup_attempts=0,routing_cleanup_claim=NULL WHERE id=$1").bind(row.id).execute(&fx.pool).await.unwrap();
    assert!(
        !custody::promote(
            &fx.pool,
            fx.pending,
            "mac-b",
            Some(("mac-a", 0)),
            custody::SwitchReadiness::Unavailable
        )
        .await
        .unwrap(),
        "a caller's enum alone is not control evidence"
    );
    assert!(
        request(&fx.pool, &fx.id, ("mac-a", 0), fx.pending, "mac-b", 12)
            .await
            .unwrap()
    );
    assert!(
        !custody::promote(
            &fx.pool,
            fx.pending,
            "mac-a",
            Some(("mac-a", 0)),
            custody::SwitchReadiness::Wait
        )
        .await
        .unwrap(),
        "a closing generation is frozen even when it currently has no open delivery"
    );
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE id=$1").bind(row.id).execute(&fx.pool).await.unwrap();
    assert!(
        advance(&fx.pool, row.id).await.unwrap(),
        "same input advances after a stored bounded control request"
    );
    fx.pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_protected_pending_preserves_body_and_reuses_deadline_for_next_input_pg() {
    use crate::db::{intake_outbox, replay_disposition::write};
    use crate::services::cluster::intake_router_hook::owner_record::input_start;
    let fx = Fixture::new(73369207).await;
    let original = fx.row().await;
    let receipt: i64 =
        sqlx::query_scalar("SELECT routing_receipt_id FROM intake_outbox WHERE id=$1")
            .bind(fx.pending)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    let mut tx = fx.pool.begin().await.unwrap();
    custody::lock(&mut tx, &fx.id).await.unwrap();
    write::withhold_owner_loss_on_tx(&mut tx, receipt)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let payload = output::tests::payload(fx.id.raw_channel_id(), "90300", "mac-b");
    let next = custody::stage(
        &fx.pool,
        custody::StageRequest {
            payload: &payload,
            home_revision: 0,
            wait_generation: Some(0),
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE id=$1").bind(original.id).execute(&fx.pool).await.unwrap();
    let deadline = fx.row().await.routing_cleanup_deadline;
    advance_pending(&fx.pool, "claude").await.unwrap();
    let restored = fx.row().await;
    assert_eq!(
        restored.routing_cleanup.operation,
        original.routing_cleanup.operation
    );
    assert_eq!(restored.routing_cleanup_deadline, deadline);
    let old: (String, String) =
        sqlx::query_as("SELECT routing_state,user_text FROM intake_outbox WHERE id=$1")
            .bind(fx.pending)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(old.0, "superseded");
    assert!(!old.1.is_empty());
    let row = intake_outbox::claim_pending_for_target_except(
        &fx.pool,
        "mac-b",
        "claude",
        "next-input",
        &[],
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(row.id, next);
    intake_outbox::mark_accepted(&fx.pool, next, "next-input")
        .await
        .unwrap();
    intake_outbox::mark_spawned(&fx.pool, next, "next-input")
        .await
        .unwrap();
    let binding = serde_json::json!({"native":"new-input"});
    let frame = payload.user_text.clone();
    let permit = input_start::prepare(
        &fx.pool,
        input_start::StartRequest {
            delivery_id: next,
            executor: "mac-b",
            claim_owner: "next-input",
            incarnation: "next-runtime",
            frame: &frame,
            native_binding: &binding,
        },
    )
    .await
    .unwrap();
    let count = std::sync::atomic::AtomicUsize::new(0);
    permit
        .submit(&frame, &binding, |_| async {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .await
        .unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    fx.pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_closing_generation_blocks_wait_and_prepared_first_effect_pg() {
    use super::super::input_start::{self, StartRequest};
    use crate::db::intake_outbox;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369208;
    let id = OwnerIdentity::new("claude", &channel.to_string());
    let old = execution::tests::admission(&pool, "claude", channel, "mac-a").await;
    let claim = "closing-fixture";
    let claimed =
        intake_outbox::claim_pending_for_target_except(&pool, "mac-a", "claude", claim, &[])
            .await
            .unwrap()
            .unwrap();
    assert_eq!(claimed.id, old);
    assert!(
        intake_outbox::mark_accepted(&pool, old, claim)
            .await
            .unwrap()
    );
    assert!(
        intake_outbox::mark_spawned(&pool, old, claim)
            .await
            .unwrap()
    );
    let binding = serde_json::json!({"native":"original-first-effect"});
    let prepared = input_start::prepare(
        &pool,
        StartRequest {
            delivery_id: old,
            executor: "mac-a",
            claim_owner: claim,
            incarnation: "closing-runtime",
            frame: "old",
            native_binding: &binding,
        },
    )
    .await
    .unwrap();
    let payload = output::tests::payload(&channel.to_string(), "90200", "gateway");
    let pending = custody::stage(
        &pool,
        custody::StageRequest {
            payload: &payload,
            home_revision: 0,
            wait_generation: Some(0),
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE intake_outbox SET target_instance_id='mac-b' WHERE id=$1")
        .bind(pending)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        request(&pool, &id, ("mac-a", 0), pending, "mac-b", 12)
            .await
            .unwrap()
    );
    assert!(
        !custody::promote(
            &pool,
            pending,
            "mac-a",
            Some(("mac-a", 0)),
            custody::SwitchReadiness::Wait
        )
        .await
        .unwrap(),
        "same target cannot join a generation already closing"
    );
    let count = AtomicUsize::new(0);
    assert!(
        prepared
            .submit("old", &binding, |_| async {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .is_err(),
        "a permit prepared before close cannot begin afterwards"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(
        input_start::prepare(
            &pool,
            StartRequest {
                delivery_id: old,
                executor: "mac-a",
                claim_owner: claim,
                incarnation: "closing-runtime",
                frame: "old",
                native_binding: &binding
            }
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE provider='claude' AND raw_channel_id=$1").bind(channel.to_string()).execute(&pool).await.unwrap();
    let row: Record = sqlx::query_as(
        "SELECT * FROM intake_session_owners WHERE provider='claude' AND raw_channel_id=$1",
    )
    .bind(channel.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(advance(&pool, row.id).await.unwrap());
    let fresh =
        intake_outbox::claim_pending_for_target_except(&pool, "mac-b", "claude", claim, &[])
            .await
            .unwrap()
            .unwrap();
    assert!(
        intake_outbox::mark_accepted(&pool, fresh.id, claim)
            .await
            .unwrap()
    );
    assert!(
        intake_outbox::mark_spawned(&pool, fresh.id, claim)
            .await
            .unwrap()
    );
    let permit = input_start::prepare(
        &pool,
        StartRequest {
            delivery_id: fresh.id,
            executor: "mac-b",
            claim_owner: claim,
            incarnation: "fresh-runtime",
            frame: "new",
            native_binding: &binding,
        },
    )
    .await
    .unwrap();
    permit
        .submit("new", &binding, |_| async {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "the actual healthy executor still runs once"
    );
    pool.close().await;
}
