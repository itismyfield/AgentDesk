use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

fn id(nonce: &str) -> Identity {
    Identity {
        channel: 7_199,
        input: 1,
        nonce: nonce.into(),
        execution: "codex".into(),
        source: "turn:v1".into(),
    }
}

fn turn(from: u64) -> Closure {
    Closure {
        session_id: "s-codex".into(),
        path: "rollout.jsonl".into(),
        dev: 1,
        ino: 2,
        from,
        until: Some(from + 10),
    }
}

async fn stored(pool: &PgPool, id: &Identity) -> Option<(String, Option<serde_json::Value>)> {
    sqlx::query_as("SELECT reason, closure FROM tui_output_dispositions WHERE output_key=$1")
        .bind(id.key())
        .fetch_optional(pool)
        .await
        .unwrap()
}

// A reason and the turn it closed commit together: a closure that cannot be written leaves no
// reason without it. A stored closure is kept, and a reason stored without one takes one once.
#[tokio::test]
async fn a_reason_and_its_closure_commit_together_and_a_stored_closure_is_kept_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let refused = Reason::AdoptionRefused;
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_closure() RETURNS trigger AS $$ BEGIN \
         IF NEW.closure IS DISTINCT FROM OLD.closure THEN RAISE EXCEPTION 'closure refused'; END IF; \
         RETURN NEW; END $$ LANGUAGE plpgsql; \
         CREATE TRIGGER refuse_closure BEFORE UPDATE ON tui_output_dispositions \
         FOR EACH ROW EXECUTE FUNCTION refuse_closure()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let blocked = id("blocked");
    assert!(
        persist(&pool, &blocked, refused, Some(&turn(5)))
            .await
            .is_err()
    );
    assert_eq!(
        stored(&pool, &blocked).await,
        None,
        "no reason without its closure"
    );
    sqlx::raw_sql("DROP TRIGGER refuse_closure ON tui_output_dispositions")
        .execute(&pool)
        .await
        .unwrap();
    let kept = id("kept");
    persist(&pool, &kept, refused, Some(&turn(5)))
        .await
        .unwrap();
    persist(&pool, &kept, refused, Some(&turn(9)))
        .await
        .unwrap();
    let first = serde_json::to_value(turn(5)).unwrap();
    assert_eq!(stored(&pool, &kept).await.unwrap().1, Some(first.clone()));
    let filled = id("filled");
    persist(&pool, &filled, refused, None).await.unwrap();
    assert_eq!(stored(&pool, &filled).await.unwrap().1, None);
    persist(&pool, &filled, refused, Some(&turn(5)))
        .await
        .unwrap();
    persist(&pool, &filled, refused, Some(&turn(9)))
        .await
        .unwrap();
    assert_eq!(stored(&pool, &filled).await.unwrap().1, Some(first));
    let closed = of_channel(&pool, 7_199).await.unwrap();
    assert_eq!(closed, [turn(5), turn(5)]);
    assert!(of_channel(&pool, 7_198).await.unwrap().is_empty());
    pool.close().await;
}
