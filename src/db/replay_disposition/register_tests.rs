use super::write::{self, CanonicalInput, WriteError};
use crate::db::auto_queue::test_support::TestPostgresDb;
use sqlx::PgPool;

fn input(key: &str) -> CanonicalInput {
    CanonicalInput {
        request_key: key.into(),
        provider: "claude".into(),
        channel: "chan".into(),
        sources: vec!["s1".into(), "s2".into()],
        original_text: format!("original request {key}"),
        owner_id: "user".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([{ "id": "attachment-1" }]),
        reply_context: Some("reply context".into()),
        provenance: "new_input",
    }
}

async fn rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap()
}

// The key names one whole request: any other scope, source set, owner, agent, side input or
// text under it conflicts, while a provider spelling or provenance difference reuses it.
#[tokio::test(flavor = "current_thread")]
async fn register_compares_the_whole_request_identity_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    let base = input("whole");
    let id = write::register_or_reuse(&pool, &base, "node-a")
        .await
        .unwrap()
        .id;
    let variants: [(&str, fn(&mut CanonicalInput)); 8] = [
        ("provider", |i| i.provider = "codex".into()),
        ("channel", |i| i.channel = "other".into()),
        ("sources", |i| i.sources = vec!["s1".into()]),
        ("owner", |i| i.owner_id = "someone".into()),
        ("agent", |i| i.agent_id = "other-agent".into()),
        ("attachments", |i| i.attachments = serde_json::json!([])),
        ("reply", |i| i.reply_context = None),
        ("text", |i| i.original_text = "another request".into()),
    ];
    for (field, change) in variants {
        let mut other = input("whole");
        change(&mut other);
        let outcome = write::register_or_reuse(&pool, &other, "node-a").await;
        assert!(
            matches!(outcome, Err(WriteError::IdentityConflict(conflict)) if conflict == id),
            "{field}: {outcome:?}"
        );
    }
    let mut same = input("whole");
    same.provider = " Claude ".into();
    same.provenance = "reentry";
    let reused = write::register_or_reuse(&pool, &same, "node-b")
        .await
        .unwrap();
    assert_eq!(reused.id, id);
    assert_eq!(rows(&pool).await, 1);
    pool.close().await;
    fixture.drop().await;
}

// Registration is one transaction: a lost commit acknowledgement is unknown, never a receipt,
// and a write refused before commit leaves no row behind.
#[tokio::test(flavor = "current_thread")]
async fn register_reports_a_lost_commit_as_unknown_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    sqlx::raw_sql(
        "CREATE FUNCTION lose_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'commit acknowledgement lost'; END $$;
         CREATE CONSTRAINT TRIGGER lose_register_commit AFTER INSERT ON intake_outbox
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
             WHEN (NEW.replay_request_key = 'lost')
             EXECUTE FUNCTION lose_commit();
         CREATE TRIGGER refuse_register BEFORE INSERT ON intake_outbox FOR EACH ROW
             WHEN (NEW.replay_request_key = 'refused')
             EXECUTE FUNCTION lose_commit();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let outcome = write::register_or_reuse(&pool, &input("lost"), "node-a").await;
    assert!(
        matches!(&outcome, Err(WriteError::AckUnknown(error))
            if error.to_string().contains("commit acknowledgement lost")),
        "{outcome:?}"
    );
    let outcome = write::register_or_reuse(&pool, &input("refused"), "node-a").await;
    assert!(
        matches!(outcome, Err(WriteError::Storage(_))),
        "{outcome:?}"
    );
    assert_eq!(
        rows(&pool).await,
        0,
        "nothing survives an unacknowledged register"
    );
    pool.close().await;
    fixture.drop().await;
}

// Concurrent registrations of one key all reuse the first receipt; none is fenced or duplicated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_key_registrations_serialise_and_reuse_one_receipt_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(16).await;
    for round in 0..8 {
        let key = format!("race-{round}");
        let racers = (0..8).map(|n| {
            let (pool, request) = (pool.clone(), input(&key));
            tokio::spawn(async move {
                write::register_or_reuse(&pool, &request, &format!("node-{n}")).await
            })
        });
        let mut ids = Vec::new();
        for racer in racers.collect::<Vec<_>>() {
            let outcome = racer.await.unwrap();
            ids.push(
                outcome
                    .unwrap_or_else(|error| panic!("{key}: {error:?}"))
                    .id,
            );
        }
        ids.dedup();
        assert_eq!(ids.len(), 1, "{key}: {ids:?}");
    }
    assert_eq!(rows(&pool).await, 8);
    pool.close().await;
    fixture.drop().await;
}
