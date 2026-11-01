use crate::db::auto_queue::test_support::TestPostgresDb;
use serde_json::{Value, json};
use sqlx::PgPool;

struct Policy {
    context: rquickjs::Context,
    _runtime: rquickjs::Runtime,
}

impl Policy {
    fn new(pool: &PgPool) -> Self {
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            super::register_globals_with_supervisor_and_pg(
                &ctx,
                Some(pool.clone()),
                crate::supervisor::BridgeHandle::new(),
            )
            .unwrap();
            ctx.eval::<(), _>(include_str!("../../../policies/00-escalation.js"))
                .unwrap();
            ctx.eval::<(), _>(
                r#"
                var emitted = [];
                var failEmit = false;
                agentdesk.http.post = function(url, body) {
                    emitted.push(body);
                    return failEmit ? {error: "offline"} : {status: 200};
                };
            "#,
            )
            .unwrap();
        });
        Self {
            context,
            _runtime: runtime,
        }
    }

    fn eval(&self, code: &str) -> Value {
        let raw = self
            .context
            .with(|ctx| {
                ctx.eval::<String, _>(format!("JSON.stringify((function() {{ {code} }})())"))
            })
            .expect(code);
        serde_json::from_str(&raw).unwrap()
    }
}

async fn seed(pool: &PgPool) {
    sqlx::query("INSERT INTO kanban_cards (id, title, status, review_status) VALUES ('card', '판단 필요', 'review', 'dilemma_pending')")
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ('server_port', '8080') ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value")
        .execute(pool).await.unwrap();
}

async fn sent(pool: &PgPool, value: String) {
    sqlx::query("INSERT INTO kv_meta (key, value, expires_at) VALUES ('pm_decision_sent:card', $1, NOW() + INTERVAL '1 hour') ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, expires_at = EXCLUDED.expires_at")
        .bind(value).execute(pool).await.unwrap();
}

async fn keys(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT key FROM kv_meta WHERE key IN ('pm_pending:card', 'pm_decision_sent:card') ORDER BY key")
        .fetch_all(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_cooldown_and_legacy_records_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let policy = Policy::new(&pool);
    assert_eq!(policy.eval("escalate('card', ['첫째', '첫째', '둘째']); flushEscalations(); flushEscalations(); return emitted;"),
        json!([{"card_id":"card", "reasons":["첫째", "둘째"]}]));
    assert_eq!(keys(&pool).await, vec!["pm_decision_sent:card"]);
    assert_eq!(
        policy.eval("escalate('card', '계속'); flushEscalations(); return emitted.length;"),
        json!(1)
    );
    let now = chrono::Utc::now().timestamp();
    sent(
        &pool,
        json!({"sent_at":now - 600, "status":"review:dilemma_pending"}).to_string(),
    )
    .await;
    assert_eq!(
        policy.eval("flushEscalations(); flushEscalations(); return emitted.length;"),
        json!(2)
    );
    sent(&pool, chrono::Utc::now().timestamp().to_string()).await;
    assert_eq!(
        policy.eval("escalate('card', 'legacy'); flushEscalations(); return emitted.length;"),
        json!(2)
    );
    sent(&pool, (chrono::Utc::now().timestamp() - 600).to_string()).await;
    assert_eq!(
        policy.eval("flushEscalations(); return emitted.length;"),
        json!(3)
    );
    sent(
        &pool,
        json!({"sent_at":chrono::Utc::now().timestamp(), "status":"blocked:old"}).to_string(),
    )
    .await;
    assert_eq!(
        policy.eval("escalate('card', '새 상태'); flushEscalations(); return emitted.length;"),
        json!(4)
    );
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_failed_emit_and_inactive_cleanup_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let policy = Policy::new(&pool);
    policy.eval("failEmit = true; escalate('card', 'retry'); flushEscalations(); return true;");
    assert_eq!(keys(&pool).await, vec!["pm_pending:card"]);
    policy.eval("failEmit = false; flushEscalations(); escalate('card', 'pending'); return true;");
    assert_eq!(keys(&pool).await.len(), 2);
    sqlx::query("UPDATE kanban_cards SET review_status = NULL WHERE id = 'card'")
        .execute(&pool)
        .await
        .unwrap();
    policy.eval("flushEscalations(); return true;");
    assert!(keys(&pool).await.is_empty());
    assert_eq!(policy.eval("return emitted.length;"), json!(2));
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_cleanup_removes_policy_keys_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let policy = Policy::new(&pool);
    policy.eval(
        "escalate('card', 'sent'); flushEscalations(); escalate('card', 'pending'); return true;",
    );
    assert_eq!(keys(&pool).await.len(), 2);
    crate::kanban::force_transition_revert_cleanup_pg_only(&pool, "card", "ready")
        .await
        .unwrap();
    assert!(
        keys(&pool).await.is_empty(),
        "public transition cleanup must remove both keys"
    );
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_cleanup_uses_callers_transaction_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    sqlx::query("UPDATE kanban_cards SET status = 'requested' WHERE id = 'card'")
        .execute(&pool)
        .await
        .unwrap();
    let policy = Policy::new(&pool);
    policy.eval(
        "escalate('card', 'sent'); flushEscalations(); escalate('card', 'pending'); return true;",
    );
    for commit in [false, true] {
        let mut tx = pool.begin().await.unwrap();
        crate::services::auto_queue::cleanup_tasks::perform_card_rollback_on_tx(
            &mut tx,
            "card",
            None,
            "test_escalation_owner",
        )
        .await
        .unwrap();
        let inside: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kv_meta WHERE key IN ('pm_pending:card', 'pm_decision_sent:card')")
            .fetch_one(&mut *tx).await.unwrap();
        assert_eq!(inside, 0);
        assert_eq!(
            keys(&pool).await.len(),
            2,
            "cleanup must stay uncommitted with the card rollback"
        );
        if commit {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
    }
    assert!(keys(&pool).await.is_empty());
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_escalation_does_not_use_generic_kv_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let policy = Policy::new(&pool);
    assert_eq!(
        policy.eval(
            r#"
        agentdesk.kv.get = agentdesk.kv.set = agentdesk.kv.delete = function() {
            throw new Error("escalation must use its Rust owner");
        };
        escalate('card', 'owner'); flushEscalations();
        return emitted.length;
    "#
        ),
        json!(1)
    );
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_expiry_and_invalid_pending_compatibility_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let policy = Policy::new(&pool);
    policy.eval("escalate('card', 'expired pending'); return true;");
    let pending_ttl: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM (expires_at - NOW()))::double precision FROM kv_meta WHERE key = 'pm_pending:card'")
        .fetch_one(&pool).await.unwrap();
    assert!(
        (590.0..=600.0).contains(&pending_ttl),
        "pending TTL: {pending_ttl}"
    );
    sent(&pool, chrono::Utc::now().timestamp().to_string()).await;
    sqlx::query("UPDATE kv_meta SET expires_at = NOW() - INTERVAL '1 second' WHERE key IN ('pm_pending:card', 'pm_decision_sent:card')")
        .execute(&pool).await.unwrap();
    assert_eq!(
        policy.eval("flushEscalations(); return emitted.length;"),
        json!(1),
        "pending scan includes expired rows; cooldown get excludes them"
    );
    let sent_ttl: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM (expires_at - NOW()))::double precision FROM kv_meta WHERE key = 'pm_decision_sent:card'")
        .fetch_one(&pool).await.unwrap();
    assert!(
        (590.0..=600.0).contains(&sent_ttl),
        "cooldown TTL: {sent_ttl}"
    );
    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ('pm_pending:card', 'bad-json')")
        .execute(&pool)
        .await
        .unwrap();
    policy.eval("flushEscalations(); return true;");
    assert_eq!(keys(&pool).await, vec!["pm_decision_sent:card"]);
    assert_eq!(policy.eval("return emitted.length;"), json!(1));
    policy.eval("escalate('card', 'deleted card'); return true;");
    sqlx::query("DELETE FROM kanban_cards WHERE id = 'card'")
        .execute(&pool)
        .await
        .unwrap();
    policy.eval("flushEscalations(); return true;");
    assert!(keys(&pool).await.is_empty());
    drop(policy);
    pool.close().await;
    db.drop().await;
}
