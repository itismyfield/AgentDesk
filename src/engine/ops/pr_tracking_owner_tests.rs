use crate::db::auto_queue::test_support::TestPostgresDb;
use rquickjs::{Context, Runtime};
use serde_json::{Value, json};
use sqlx::PgPool;

struct Policy {
    context: Context,
    _runtime: Runtime,
}

impl Policy {
    fn new(pool: &PgPool) -> Self {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        context.with(|ctx| {
            super::register_globals_with_supervisor_and_pg(
                &ctx,
                Some(pool.clone()),
                crate::supervisor::BridgeHandle::new(),
            )
            .unwrap();
            ctx.eval::<(), _>(include_str!("../../../policies/00-pr-tracking.js"))
                .unwrap();
        });
        Self {
            context,
            _runtime: runtime,
        }
    }

    fn eval(&self, script: &str) -> Value {
        self.context.with(|ctx| {
            let raw: String = ctx
                .eval(format!("JSON.stringify((function() {{ {script} }})())"))
                .unwrap_or_else(|error| {
                    let detail = ctx.catch().into_exception().and_then(|e| e.message());
                    panic!("policy evaluation: {error}; {detail:?}");
                });
            serde_json::from_str(&raw).unwrap()
        })
    }

    fn reload(&self) {
        self.context
            .with(|ctx| ctx.eval::<(), _>(include_str!("../../../policies/00-pr-tracking.js")))
            .unwrap();
    }
}

async fn setup() -> (TestPostgresDb, PgPool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider) VALUES ('pr-owner', 'pr-owner', 'claude')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id) SELECT 'pr-' || n, 'PR', 'review', 'pr-owner', 'owner/repo' FROM generate_series(1, 12) n")
        .execute(&pool).await.unwrap();
    (db, pool)
}

async fn row(pool: &PgPool, card: &str) -> Value {
    sqlx::query_scalar(
        "SELECT to_jsonb(p) - 'created_at' - 'updated_at' FROM pr_tracking p WHERE card_id = $1",
    )
    .bind(card)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_observation_merge_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    policy.eval("agentdesk.prTracking.upsert('pr-1', 'old/repo', '/old', 'old', 41, 'old-head', 'escalated', 'old-error'); return true;");
    sqlx::query("UPDATE pr_tracking SET dispatch_generation = 'g1', review_round = 4, retry_count = 2, created_at = '2000-01-01T00:00:00Z', updated_at = '2000-01-01T00:00:00Z' WHERE card_id = 'pr-1'")
        .execute(&pool).await.unwrap();
    policy.eval("agentdesk.prTracking.upsert('pr-1', null, null, null, null, null, 'wait-ci', null); return true;");
    assert_eq!(
        row(&pool, "pr-1").await,
        json!({
            "card_id":"pr-1", "repo_id":"old/repo", "worktree_path":"/old", "branch":null,
            "pr_number":41, "head_sha":null, "state":"wait-ci", "last_error":null,
            "dispatch_generation":"g1", "review_round":4, "retry_count":2
        })
    );
    policy.eval(
        "agentdesk.prTracking.upsert('pr-1', '', '', '', 0, '', 'create-pr', ''); return true;",
    );
    let empty = row(&pool, "pr-1").await;
    for field in [
        "repo_id",
        "worktree_path",
        "branch",
        "head_sha",
        "last_error",
    ] {
        assert_eq!(empty[field], "", "empty {field} replaces the old value");
    }
    assert_eq!(empty["pr_number"], 0);
    assert_eq!(empty["dispatch_generation"], "g1");
    assert_eq!(empty["retry_count"], 2);
    let timestamps: (bool, bool) = sqlx::query_as("SELECT created_at = '2000-01-01T00:00:00Z'::timestamptz, updated_at > created_at FROM pr_tracking WHERE card_id = 'pr-1'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(timestamps, (true, true));
    assert_eq!(policy.eval("try { agentdesk.prTracking.upsert('pr-1', 'discarded', null, 'discarded', null, null, null, null); return false; } catch(e) { return true; }"), true);
    assert_eq!(
        row(&pool, "pr-1").await,
        empty,
        "NULL state rejects the entire observation"
    );
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_legacy_import_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    let result = policy.eval(r#"
        agentdesk.kv.set('pr:pr-1', '{"number":71,"headRefName":"legacy","head_sha":"h1"}', 0);
        agentdesk.kv.set('pr:pr-2', '{"pr_number":72,"branch":"b2","repo":"other/repo","state":"escalated"}', 0);
        agentdesk.kv.set('pr:pr-3', '{"branch":"b3"}', 0);
        agentdesk.kv.set('pr:pr-4', 'invalid', 0);
        agentdesk.prTracking.importLegacyOnce('wait-ci');
        return [agentdesk.prTracking.load('pr-1'), agentdesk.prTracking.load('pr-2'), agentdesk.prTracking.load('pr-3'), agentdesk.prTracking.load('pr-4')];
    "#);
    assert_eq!(result[0]["repo_id"], "owner/repo");
    assert_eq!(result[0]["state"], "wait-ci");
    assert_eq!(result[0]["head_sha"], "h1");
    assert_eq!(result[0]["branch"], "legacy");
    assert_eq!(result[0]["pr_number"], 71);
    assert_eq!(result[0]["dispatch_generation"], "");
    assert_eq!(result[0]["review_round"], 0);
    assert_eq!(result[0]["retry_count"], 0);
    assert_eq!(result[1]["repo_id"], "other/repo");
    assert_eq!(result[1]["state"], "escalated");
    assert_eq!(result[2]["state"], "wait-ci");
    assert!(result[3].is_null());
    assert!(
        policy
            .eval(
                r#"
        agentdesk.kv.set('pr:pr-4', '{"branch":"repaired"}', 0);
        agentdesk.prTracking.importLegacyOnce('wait-ci');
        return agentdesk.prTracking.loadCanonical('pr-4');
    "#
            )
            .is_null(),
        "same key set retains the scan stamp"
    );
    policy.eval("globalThis.previousTracking = agentdesk.prTracking; return true;");
    policy.reload();
    assert!(policy.eval("previousTracking.importLegacyOnce('wait-ci'); return previousTracking.loadCanonical('pr-4');").is_null(), "an old policy object keeps its own stamp after another evaluation");
    assert_eq!(policy.eval("agentdesk.prTracking.importLegacyOnce('wait-ci'); return agentdesk.prTracking.loadCanonical('pr-4').branch;"), "repaired", "reloading a policy resets the scan stamp in its existing Context");
    let restarted = Policy::new(&pool);
    assert_eq!(restarted.eval("agentdesk.prTracking.importLegacyOnce('wait-ci'); return agentdesk.prTracking.loadCanonical('pr-4').branch;"), "repaired");
    drop(restarted);
    sqlx::query("UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'pr-6'")
        .execute(&pool)
        .await
        .unwrap();
    let inferred = policy.eval(r#"
        agentdesk.kv.set('pr:pr-5', '{"number":75}', 0);
        agentdesk.kv.set('pr:pr-6', '{"number":76}', 0);
        agentdesk.kv.set('pr:pr-7', '{"branch":"b7"}', 0);
        return [agentdesk.prTracking.load('pr-5'), agentdesk.prTracking.load('pr-6'), agentdesk.prTracking.load('pr-7')];
    "#);
    assert_eq!(inferred[0]["state"], "merge");
    assert_eq!(inferred[1]["state"], "wait-ci");
    assert_eq!(inferred[2]["state"], "create-pr");
    sqlx::query("INSERT INTO kv_meta (key, value, expires_at) VALUES ('pr:pr-8', '{\"number\":78}', NOW() - INTERVAL '1 second')")
        .execute(&pool).await.unwrap();
    assert!(
        policy
            .eval("return agentdesk.prTracking.load('pr-8');")
            .is_null()
    );
    sqlx::query("UPDATE pr_tracking SET dispatch_generation = 'new-generation', retry_count = 2, state = 'create-pr' WHERE card_id = 'pr-1'")
        .execute(&pool).await.unwrap();
    assert_eq!(
        policy.eval(
            "return agentdesk.prTracking.importLegacyForCard('pr-1', 'merge').dispatch_generation;"
        ),
        "new-generation"
    );
    assert_eq!(row(&pool, "pr-1").await["retry_count"], 2);
    assert_eq!(row(&pool, "pr-1").await["state"], "create-pr");
    assert!(
        policy
            .eval("return agentdesk.kv.get('pr:pr-1');")
            .is_string(),
        "import retains legacy key"
    );
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_generation_retry_reset_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    policy.eval("agentdesk.prTracking.upsert('pr-1', 'old/repo', '/old', 'old', 71, 'old-head', 'escalated', 'old-error'); return true;");
    sqlx::query("UPDATE pr_tracking SET dispatch_generation = 'old-generation', retry_count = 2, review_round = 3 WHERE card_id = 'pr-1'")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO card_review_state (card_id, review_round) VALUES ('pr-1', 5)")
        .execute(&pool)
        .await
        .unwrap();
    let first = policy.eval("return agentdesk.reviewAutomation.handoffCreatePr('pr-1', {repo_id:'new/repo', worktree_path:null, branch:'new', head_sha:null, agent_id:'pr-owner', title:'PR'});");
    assert_eq!(first["ok"], true);
    assert_eq!(first["reused"], false);
    let generation = first["generation"].as_str().unwrap();
    assert_ne!(generation, "old-generation");
    assert_eq!(
        row(&pool, "pr-1").await,
        json!({
            "card_id":"pr-1", "repo_id":"new/repo", "worktree_path":null, "branch":"new",
            "pr_number":71, "head_sha":null, "state":"create-pr", "last_error":null,
            "dispatch_generation":generation, "review_round":5, "retry_count":0
        })
    );
    sqlx::query("UPDATE pr_tracking SET retry_count = 2, last_error = 'retry', state = 'escalated', head_sha = 'preserved' WHERE card_id = 'pr-1'")
        .execute(&pool).await.unwrap();
    let reused = policy.eval("return agentdesk.reviewAutomation.handoffCreatePr('pr-1', {repo_id:'ignored/repo', worktree_path:'/ignored', branch:'ignored', head_sha:'ignored', agent_id:'pr-owner', title:'PR'});");
    assert_eq!(reused["reused"], true);
    assert_eq!(reused["generation"], generation);
    let state = row(&pool, "pr-1").await;
    assert_eq!(state["repo_id"], "new/repo");
    assert_eq!(state["branch"], "new");
    assert_eq!(state["head_sha"], "preserved");
    assert_eq!(state["retry_count"], 0);
    assert_eq!(state["last_error"], Value::Null);
    assert_eq!(state["state"], "create-pr");
    assert_eq!(policy.eval("return agentdesk.reviewAutomation.recordPrCreateFailure('pr-1', 'stale', 'old-generation').reason;"), "stale_generation");
    for retry in 1..=3 {
        let failure = policy.eval(&format!("return agentdesk.reviewAutomation.recordPrCreateFailure('pr-1', 'failed', {generation:?});"));
        assert_eq!(failure["retry_count"], retry);
        assert_eq!(failure["escalated"], retry >= 3);
    }
    assert_eq!(row(&pool, "pr-1").await["state"], "escalated");
    let reset = policy.eval("return agentdesk.reviewAutomation.reseedPrTracking('pr-1');");
    assert_ne!(reset["generation"], generation);
    let state = row(&pool, "pr-1").await;
    assert_eq!(state["head_sha"], "preserved");
    assert_eq!(state["retry_count"], 0);
    assert_eq!(state["last_error"], Value::Null);
    assert_eq!(state["state"], "create-pr");
    assert_eq!(state["review_round"], 5);
    assert_eq!(policy.eval("return agentdesk.reviewAutomation.recordPrCreateFailure('pr-2', 'seed-failure', '').retry_count;"), 1);
    let missing = row(&pool, "pr-2").await;
    assert_eq!(missing["dispatch_generation"], "");
    assert_eq!(missing["state"], "create-pr");
    assert_eq!(missing["last_error"], "seed-failure");
    sqlx::query("ALTER TABLE task_dispatches ADD CONSTRAINT reject_pr_three CHECK (kanban_card_id <> 'pr-3')")
        .execute(&pool).await.unwrap();
    let rolled_back = policy.eval("try { agentdesk.reviewAutomation.handoffCreatePr('pr-3', {repo_id:'owner/repo', branch:'new', agent_id:'pr-owner', title:'PR'}); return ''; } catch(e) { return String(e); }");
    assert!(
        rolled_back.as_str().unwrap().contains("reject_pr_three"),
        "the dispatch INSERT must fail after the tracking seed: {rolled_back}"
    );
    assert!(
        policy
            .eval("return agentdesk.prTracking.loadCanonical('pr-3');")
            .is_null(),
        "failed dispatch attachment rolls back the tracking seed"
    );
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_owner_without_sql_writes_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    policy.eval("agentdesk.kv.set('pr:pr-2', '{\"number\":72,\"branch\":\"legacy\"}', 0); agentdesk.db.execute = function() { throw new Error('policy SQL writes disabled'); }; return true;");
    let result = policy.eval("agentdesk.prTracking.upsert('pr-1', 'owner/repo', null, 'branch', 71, 'head', 'wait-ci', null); return [agentdesk.prTracking.load('pr-1'), agentdesk.prTracking.load('pr-2')];");
    assert_eq!(result[0]["pr_number"], 71);
    assert_eq!(result[1]["pr_number"], 72);
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_import_overlapping_handoff_keeps_generation_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    policy.eval(
        "agentdesk.kv.set('pr:pr-1', '{\"number\":71,\"branch\":\"legacy\"}', 0); return true;",
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let import_pool = pool.clone();
    let import_barrier = barrier.clone();
    let import = tokio::task::spawn_blocking(move || {
        let policy = Policy::new(&import_pool);
        import_barrier.wait();
        policy.eval("return agentdesk.prTracking.importLegacyForCard('pr-1', 'wait-ci');")
    });
    let handoff_pool = pool.clone();
    let handoff = tokio::task::spawn_blocking(move || {
        let policy = Policy::new(&handoff_pool);
        barrier.wait();
        policy.eval("return agentdesk.reviewAutomation.handoffCreatePr('pr-1', {repo_id:'owner/repo', worktree_path:null, branch:'new', head_sha:'new-head', agent_id:'pr-owner', title:'PR'});")
    });
    let (import, handoff) = tokio::join!(import, handoff);
    import.unwrap();
    let handoff = handoff.unwrap();
    assert_eq!(
        row(&pool, "pr-1").await["dispatch_generation"],
        handoff["generation"]
    );
    let final_load = policy.eval(
        "agentdesk.prTracking.importLegacyOnce('merge'); return agentdesk.prTracking.load('pr-1');",
    );
    assert_eq!(final_load["dispatch_generation"], handoff["generation"]);
    assert_eq!(final_load["retry_count"], 0);
    drop(policy);
    pool.close().await;
    db.drop().await;
}
