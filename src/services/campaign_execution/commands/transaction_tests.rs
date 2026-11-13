use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sqlx::PgPool;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::server_campaigns::{
    self as store, CommandOutcome, ReceiptResponse, StoreError, StoreResult,
    command_transaction::StoreRequest,
};
use crate::domain::campaign::model::{Command, CommandEnvelope};
use crate::services::campaign_execution::control::tests as domain;

fn render(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    Ok(ReceiptResponse {
        status: match outcome {
            CommandOutcome::Decided(Ok(_)) => 200,
            CommandOutcome::Decided(Err(_)) => 409,
            CommandOutcome::NotFound => 404,
        },
        body: serde_json::to_value(outcome)?,
    })
}

fn edit(key: &str, revision: u64, title: &str) -> CommandEnvelope {
    let mut definition = domain::input();
    definition.title = title.into();
    CommandEnvelope {
        idempotency_key: key.into(),
        observed_at: domain::now(),
        command: Command::EditDefinition {
            expected_revision: revision,
            definition,
            change: None,
        },
    }
}

struct Harness {
    fixture: TestPostgresDb,
    pool: PgPool,
}

impl Harness {
    async fn new() -> Self {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        let reply = super::create(
            &pool,
            "campaign",
            &domain::input(),
            "create",
            domain::now(),
            render,
        )
        .await
        .unwrap();
        assert!(matches!(reply.outcome, CommandOutcome::Decided(Ok(_))));
        Self { fixture, pool }
    }

    async fn close(self) {
        self.pool.close().await;
        self.fixture.drop().await;
    }
}

async fn wait_for_blocker(pool: &PgPool, waiter: i32, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT $1 = ANY(pg_blocking_pids($2))")
                .bind(blocker)
                .bind(waiter)
                .fetch_one(pool)
                .await
                .unwrap();
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the second command must reach the held campaign lock");
}

#[tokio::test]
async fn facts_are_read_after_campaign_lock_wait_pg() {
    let h = Harness::new().await;
    let mut first_tx = h.pool.begin().await.unwrap();
    let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *first_tx)
        .await
        .unwrap();
    let first = edit("first", 1, "First committed edit");
    let (first_tx, first_reply) = super::apply_in(
        first_tx,
        "campaign",
        &first.idempotency_key,
        first.observed_at,
        StoreRequest::Command(first.command),
        render,
    )
    .await
    .unwrap();
    assert!(matches!(
        first_reply.outcome,
        CommandOutcome::Decided(Ok(ref result)) if result.definition_revision == 2
    ));

    let mut second_tx = h.pool.begin().await.unwrap();
    let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *second_tx)
        .await
        .unwrap();
    let second = edit("second", 2, "Read only after the first commit");
    let pending = tokio::spawn(async move {
        super::apply_in(
            second_tx,
            "campaign",
            &second.idempotency_key,
            second.observed_at,
            StoreRequest::Command(second.command),
            render,
        )
        .await
    });
    wait_for_blocker(&h.pool, second_pid, first_pid).await;
    first_tx.commit().await.unwrap();
    let (second_tx, second_reply) = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    second_tx.commit().await.unwrap();
    assert!(matches!(
        second_reply.outcome,
        CommandOutcome::Decided(Ok(ref result)) if result.definition_revision == 3
    ));
    let state = store::get(&h.pool, "campaign").await.unwrap().unwrap();
    assert_eq!(state.definition_revision, 3);
    assert_eq!(
        state.definition.input().title,
        "Read only after the first commit"
    );
    assert_eq!(
        store::events(&h.pool, "campaign", None, 0, 100)
            .await
            .unwrap()
            .len(),
        3
    );
    h.close().await;
}

static RENDER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn counted_render(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    RENDER_CALLS.fetch_add(1, Ordering::SeqCst);
    render(outcome)
}

#[tokio::test]
async fn persist_failure_precedes_renderer_and_receipt_pg() {
    let h = Harness::new().await;
    let before = store::get(&h.pool, "campaign").await.unwrap().unwrap();
    sqlx::query(
        "CREATE FUNCTION reject_definition_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'injected definition write failure'; END $$",
    )
    .execute(&h.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_definition_write BEFORE UPDATE ON server_campaign_definitions \
         FOR EACH ROW EXECUTE FUNCTION reject_definition_write()",
    )
    .execute(&h.pool)
    .await
    .unwrap();
    RENDER_CALLS.store(0, Ordering::SeqCst);
    let result = super::execute(
        &h.pool,
        "campaign",
        &edit("persist-failure", 1, "Must roll back"),
        counted_render,
    )
    .await;
    assert!(matches!(result, Err(StoreError::Sql(_))));
    assert_eq!(RENDER_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(store::get(&h.pool, "campaign").await.unwrap(), Some(before));
    assert_eq!(
        store::events(&h.pool, "campaign", None, 0, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    let receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM server_campaign_receipts WHERE campaign_id = 'campaign'",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(receipts, 1);
    h.close().await;
}
