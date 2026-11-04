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
    policy.eval("agentdesk.db.execute = function() { throw new Error('policy SQL writes disabled'); }; return true;");
    let result = policy.eval("agentdesk.prTracking.upsert('pr-1', 'owner/repo', null, 'branch', 71, 'head', 'wait-ci', null); agentdesk.prTracking.markHandedOff('pr-1', 'ignored'); agentdesk.prTracking.touchGeneration('pr-1', ''); return [agentdesk.prTracking.load('pr-1'), agentdesk.prTracking.load('pr-2')];");
    assert_eq!(result[0]["pr_number"], 71);
    assert!(result[1].is_null());
    assert_eq!(policy.eval("return [agentdesk.prTracking.list('', []).length, agentdesk.prTracking.findByRepoPr('owner/repo', 71).card_id, agentdesk.prTracking.findByRepoPr('owner/repo', 999), typeof agentdesk.prTracking.importLegacyOnce, typeof agentdesk.prTracking.importLegacyForCard];"), json!([1, "pr-1", null, "undefined", "undefined"]));
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[test]
fn policy_pr_tracking_sources_have_no_tracking_writes() {
    fn check(
        dir: &std::path::Path,
        writes: &regex::Regex,
        joins: &regex::Regex,
        legacy: &regex::Regex,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() && path.file_name().unwrap() != "__tests__" {
                check(&path, writes, joins, legacy);
            } else if path.extension().is_some_and(|ext| ext == "js") {
                let source = std::fs::read_to_string(&path).unwrap();
                let joined = joins.replace_all(&source, "");
                assert!(
                    !writes.is_match(&joined),
                    "policy tracking SQL write: {}",
                    path.display()
                );
                assert!(
                    !legacy.is_match(&joined),
                    "policy legacy PR key: {}",
                    path.display()
                );
            }
        }
    }
    let writes = regex::Regex::new(r#"(?i)\b(?:insert(?:\s+or\s+\w+)?\s+into|replace\s+into|merge\s+into|update|delete\s+from|truncate(?:\s+table)?)\s+(?:\\?["`]?\w+\\?["`]?\s*\.\s*)?\\?["`]?pr_tracking\b"#).unwrap();
    let joins = regex::Regex::new(r#"["'`]\s*\+\s*["'`]"#).unwrap();
    let legacy = regex::Regex::new(r#"["'`]pr:(?:["'`]|\$\{)"#).unwrap();
    for fixture in [
        r#"UPDATE public.pr_tracking SET state = 'x'"#,
        r#"UPDATE \"pr_tracking\" SET state = 'x'"#,
        "MERGE INTO pr_tracking USING x ON true",
    ] {
        assert!(writes.is_match(fixture), "scanner must detect {fixture}");
    }
    for fixture in [r#"`pr:${id}`"#, r#""pr" + ":""#] {
        assert!(legacy.is_match(&joins.replace_all(fixture, "")));
    }
    check(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("policies"),
        &writes,
        &joins,
        &legacy,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_handoff_bookkeeping_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    policy.eval("['create-pr', 'escalated', 'wait-ci'].forEach(function(state, i) { agentdesk.prTracking.upsert('pr-' + (i+1), 'owner/repo', '/wt', 'branch', 71+i, 'head', state, 'old'); }); return true;");
    sqlx::query("UPDATE pr_tracking SET dispatch_generation = 'g1', review_round = 4, retry_count = 2, created_at = '2000-01-01T00:00:00Z', updated_at = '2000-01-01T00:00:00Z'")
        .execute(&pool).await.unwrap();
    let before = row(&pool, "pr-1").await;
    policy.eval("agentdesk.prTracking.touchGeneration('pr-1', 'stale'); agentdesk.prTracking.touchGeneration('pr-2', ''); agentdesk.prTracking.markHandedOff('pr-3', 'ignored'); return true;");
    let unchanged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pr_tracking WHERE updated_at = created_at")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        unchanged, 3,
        "stale/empty generations and inactive states do not touch rows"
    );
    assert_eq!(row(&pool, "pr-3").await["last_error"], "old");
    policy.eval("agentdesk.prTracking.markHandedOff('pr-1', 'failed'); agentdesk.prTracking.markHandedOff('pr-2', ''); agentdesk.prTracking.touchGeneration('pr-3', 'g1'); agentdesk.prTracking.markHandedOff('missing', 'ignored'); return true;");
    let mut expected = before;
    expected["last_error"] = json!("handed-off:failed");
    assert_eq!(
        row(&pool, "pr-1").await,
        expected,
        "marker preserves lifecycle fields and metadata"
    );
    assert_eq!(row(&pool, "pr-2").await["last_error"], "handed-off:");
    assert_eq!(row(&pool, "pr-3").await["last_error"], "old");
    let advanced: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pr_tracking WHERE updated_at > created_at")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(advanced, 3);
    sqlx::query("UPDATE pr_tracking SET dispatch_generation = '', updated_at = created_at WHERE card_id = 'pr-1'")
        .execute(&pool).await.unwrap();
    policy.eval("agentdesk.prTracking.touchGeneration('pr-1', ''); return true;");
    let touched: bool = sqlx::query_scalar(
        "SELECT updated_at > created_at FROM pr_tracking WHERE card_id = 'pr-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(touched, "the legacy empty generation still matches itself");
    assert_eq!(policy.eval("try { agentdesk.prTracking.markHandedOff(null, 'bad'); return false; } catch (e) { return true; }"), true);
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_raw_write_guard_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    policy.eval("agentdesk.prTracking.upsert('pr-1', 'owner/repo', null, 'owner-branch', 71, 'head', 'create-pr', null); return true;");
    assert_eq!(policy.eval("try { agentdesk.db.execute(['UPDATE', 'public.pr_tracking'].join(' ') + \" SET branch='corrupt' WHERE card_id='pr-1'\"); return false; } catch(e) { return String(e).indexOf('blocked') >= 0; }"), true);
    let result = crate::engine::intent::execute_intents_with_backends(
        Some(&pool),
        None,
        vec![crate::engine::intent::Intent::ExecuteSQL {
            sql: "UPDATE pr_tracking SET branch='corrupt' WHERE card_id='pr-1'".into(),
            params: vec![],
        }],
    );
    assert_eq!(
        result.errors, 1,
        "execute_sql intent must use the same guard"
    );
    assert_eq!(row(&pool, "pr-1").await["branch"], "owner-branch");
    policy.eval("agentdesk.prTracking.markHandedOff('pr-1', 'failed'); agentdesk.prTracking.touchGeneration('pr-1', ''); return true;");
    assert_eq!(
        row(&pool, "pr-1").await["last_error"],
        "handed-off:failed",
        "native owner writes remain allowed"
    );
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_pr_tracking_stranded_list_pg() {
    let (db, pool) = setup().await;
    let policy = Policy::new(&pool);
    sqlx::query("INSERT INTO pr_tracking (card_id, state, retry_count, last_error, updated_at) VALUES ('pr-1', 'create-pr', 1, 'failed', NOW() - INTERVAL '2 days'), ('pr-2', 'escalated', 3, 'failed', NOW() - INTERVAL '1 day'), ('pr-3', 'wait-ci', 1, 'failed', NOW()), ('pr-4', 'create-pr', 0, 'failed', NOW()), ('pr-5', 'create-pr', 1, 'failed', NOW() - INTERVAL '40 days')")
        .execute(&pool).await.unwrap();
    assert_eq!(policy.eval("return agentdesk.prTracking.listStrandedCreatePr(20).map(function(r) { return r.card_id; });"), json!(["pr-1", "pr-2"]));
    assert_eq!(policy.eval("return agentdesk.prTracking.listStrandedCreatePr(1).map(function(r) { return r.card_id; });"), json!(["pr-1"]));
    policy.eval("agentdesk.prTracking.markHandedOff('pr-1', 'failed'); return true;");
    assert_eq!(policy.eval("return agentdesk.prTracking.listStrandedCreatePr(20).map(function(r) { return r.card_id; });"), json!(["pr-2"]), "marker and list must share their prefix contract");
    drop(policy);
    pool.close().await;
    db.drop().await;
}
