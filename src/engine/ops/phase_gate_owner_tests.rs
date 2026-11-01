use crate::db::auto_queue::{
    self, PhaseGateReconciliation, PhaseGateStateWrite, test_support::TestPostgresDb,
};
use crate::engine::loader::{LoadedPolicy, load_single_policy};
use rquickjs::{Context, Runtime};
use serde_json::{Value, json};
use sqlx::PgPool;

struct Policy {
    _policy: LoadedPolicy,
    context: Context,
    _runtime: Runtime,
}

impl Policy {
    fn new(pool: Option<PgPool>) -> Self {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        context.with(|ctx| {
            super::register_globals_with_supervisor_and_pg(&ctx, pool, crate::supervisor::BridgeHandle::new()).unwrap();
            ctx.eval::<(), _>(r#"
                var __dispatch, __saved, __cleared, __paused, __resumed;
                agentdesk.config.get = function() { return null; };
                agentdesk.db.query = function(sql) {
                    if (sql.indexOf('FROM auto_queue_phase_gates') >= 0) return [{
                        dispatch_id:__dispatch.id, status:'pending', pass_verdict:'phase_gate_passed',
                        next_phase:1, final_phase:false, anchor_card_id:null
                    }];
                    if (sql.indexOf('FROM task_dispatches WHERE id') >= 0) return [__dispatch];
                    return [];
                };
                agentdesk.autoQueue.savePhaseGateState = function(run, phase, state) { __saved.push(state); };
                agentdesk.autoQueue.clearPhaseGateState = function() { __cleared++; };
                agentdesk.autoQueue.pauseRun = function() { __paused++; return {changed:true}; };
                agentdesk.autoQueue.resumeRun = function() { __resumed++; return {changed:true}; };
                agentdesk.autoQueue.activate = function() { return {count:0}; };
                agentdesk.kv = {get:function(){return null;}, set:function(){}, delete:function(){}};
                agentdesk.cards.get = function() { return null; };
                function notifyCardOwner() {}
                function notifyHumanAlert() {}
            "#).unwrap();
        });
        let policy = load_single_policy(
            &context,
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("policies/auto-queue.js"),
        )
        .unwrap();
        Self {
            _policy: policy,
            context,
            _runtime: runtime,
        }
    }

    fn eval(&self, script: &str) -> Value {
        self.context.with(|ctx| {
            let raw: String = ctx
                .eval(format!("JSON.stringify((function() {{ {script} }})())"))
                .unwrap_or_else(|error| {
                    let detail = ctx
                        .catch()
                        .into_exception()
                        .and_then(|exception| exception.message());
                    panic!("phase-gate policy: {error}; {detail:?}");
                });
            serde_json::from_str(&raw).unwrap()
        })
    }

    fn complete(&self, context: &Value, result: &Value) -> Value {
        let dispatch = json!({"id":"gate-dispatch", "dispatch_type":"phase-gate", "status":"completed", "context":context.to_string(), "result":result.to_string()});
        self.eval(&format!("__dispatch = {dispatch}; __saved=[]; __cleared=0; __paused=0; __resumed=0; __policyCapture.captured.onDispatchCompleted({{dispatch_id:__dispatch.id}}); return {{saved:__saved, cleared:__cleared, paused:__paused, resumed:__resumed}};"))
    }
}

fn context() -> Value {
    let mut gate = crate::phase_gate::resolve_declaration_value("pr-confirm").unwrap();
    gate["run_id"] = json!("gate-run");
    gate["batch_phase"] = json!(0);
    gate["next_phase"] = json!(1);
    json!({"phase_gate":gate})
}

#[test]
fn policy_phase_gate_bridge_owns_hook_decision() {
    let policy = Policy::new(None);
    policy.eval("agentdesk.phaseGate.evaluate = function() { return {verdict:'owner-veto', pass_verdict:'phase_gate_passed', matches:false, inferred:false, diagnostic:'owner-veto'}; }; return true;");
    let effects = policy.complete(&context(), &json!({"verdict":"phase_gate_passed", "checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}}));
    assert_eq!(
        effects["cleared"], 0,
        "policy must obey the owner, even when JS could infer pass"
    );
    assert_eq!(effects["saved"][0]["verdict"], "owner-veto");
    assert_eq!(effects["paused"], 1);
}

#[test]
fn policy_phase_gate_null_result_keeps_hook_error_boundary() {
    let policy = Policy::new(None);
    let dispatch = json!({"id":"gate-dispatch","dispatch_type":"phase-gate","status":"completed","context":context().to_string(),"result":"null"});
    let effects = policy.eval(&format!("__dispatch={dispatch}; __saved=[]; __cleared=0; __paused=0; __resumed=0; var threw=false; try {{ __policyCapture.captured.onDispatchCompleted({{dispatch_id:__dispatch.id}}); }} catch(e) {{ threw=true; }} return {{threw:threw,saved:__saved.length,cleared:__cleared,paused:__paused,resumed:__resumed}};"));
    assert_eq!(
        effects,
        json!({"threw":true,"saved":0,"cleared":0,"paused":0,"resumed":0})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_phase_gate_owner_boundary_matrix_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ('gate-owner', 'Gate', 'claude')")
        .execute(&pool)
        .await
        .unwrap();
    let policy = Policy::new(Some(pool.clone()));
    let checks = json!({"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"});
    let mut mixed = checks.clone();
    mixed["merge_verified"] = json!({"status":true,"result":"pass"});
    let mut array = checks.clone();
    array["merge_verified"] = json!({"status":["pass"],"result":"fail"});
    let mut fail = checks.clone();
    fail["issue_closed"] = json!("fail");
    let mut extra = checks.clone();
    extra["extra"] = json!("fail");
    let cases = [
        (
            "all_pass",
            context(),
            json!({"checks":checks}),
            true,
            json!("phase_gate_passed"),
        ),
        (
            "explicit_failure",
            context(),
            json!({"verdict":"hold","checks":checks}),
            false,
            json!("hold"),
        ),
        (
            "partial",
            context(),
            json!({"checks":{"build_passed":"pass"}}),
            false,
            Value::Null,
        ),
        (
            "missing_checks",
            context(),
            json!({"verdict":"phase_gate_passed"}),
            false,
            json!("phase_gate_passed"),
        ),
        (
            "missing_declaration",
            json!({"phase_gate":{"batch_phase":0}}),
            json!({"checks":checks}),
            false,
            Value::Null,
        ),
        (
            "truthy_non_string_explicit",
            context(),
            json!({"verdict":true,"checks":checks}),
            true,
            json!("phase_gate_passed"),
        ),
        (
            "truthy_non_string_then_failure",
            context(),
            json!({"verdict":true,"decision":"hold","checks":checks}),
            false,
            json!("hold"),
        ),
        (
            "pass_alias",
            context(),
            json!({"verdict":"passed","checks":checks}),
            true,
            json!("passed"),
        ),
        (
            "truthy_non_string_status",
            context(),
            json!({"verdict":"phase_gate_passed","checks":mixed}),
            false,
            json!("phase_gate_passed"),
        ),
        (
            "failed_check",
            context(),
            json!({"checks":fail}),
            false,
            Value::Null,
        ),
        (
            "extra_failed_check",
            context(),
            json!({"checks":extra}),
            false,
            Value::Null,
        ),
        (
            "array_non_string_status",
            context(),
            json!({"verdict":"phase_gate_passed","checks":array}),
            false,
            json!("phase_gate_passed"),
        ),
    ];
    for (index, (name, mut context, result, passed, verdict)) in cases.into_iter().enumerate() {
        let run_id = format!("gate-run-{index}");
        context["phase_gate"]["run_id"] = json!(run_id);
        let effects = policy.complete(&context, &result);
        assert_eq!(
            effects["cleared"],
            i64::from(passed),
            "actual hook clear: {name}"
        );
        assert_eq!(
            effects["resumed"],
            i64::from(passed),
            "actual hook resume: {name}"
        );
        assert_eq!(
            effects["paused"],
            i64::from(!passed),
            "actual hook pause: {name}"
        );
        if !passed {
            assert_eq!(
                effects["saved"][0]["status"], "failed",
                "actual hook save: {name}"
            );
            assert_eq!(
                effects["saved"][0]["verdict"], verdict,
                "canonical diagnostic: {name}"
            );
        }
        let decision = policy.eval(&format!(
            "return agentdesk.phaseGate.evaluate({context}, {result});"
        ));
        assert_eq!(decision["matches"], passed, "bridge decision: {name}");
        assert_eq!(decision["verdict"], verdict, "bridge verdict: {name}");
        sqlx::query("INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ($1, 'repo', 'gate-owner', 'paused')").bind(&run_id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO auto_queue_entries (id, run_id, agent_id, status, batch_phase, phase_gate_kind) VALUES ($1, $2, 'gate-owner', 'pending', 1, 'pr-confirm')")
            .bind(format!("next-entry-{index}")).bind(&run_id).execute(&pool).await.unwrap();
        let dispatch_id = format!("gate-dispatch-{index}");
        sqlx::query("INSERT INTO task_dispatches (id, to_agent_id, dispatch_type, status, title, context, result) VALUES ($1, 'gate-owner', 'phase-gate', 'completed', 'gate', $2, $3)")
            .bind(&dispatch_id).bind(context.to_string()).bind(result.to_string()).execute(&pool).await.unwrap();
        auto_queue::save_phase_gate_state_on_pg(
            &pool,
            &run_id,
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec![dispatch_id.clone()],
                next_phase: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let outcome = auto_queue::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
            &mut tx,
            &dispatch_id,
            "completed",
            Some(&context.to_string()),
            Some(&result.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(
            matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
            passed,
            "durable owner: {name}: {outcome:?}"
        );
        if !passed {
            assert!(
                matches!(outcome, PhaseGateReconciliation::MarkedFailed { .. }),
                "must persist failure: {name}: {outcome:?}"
            );
        }
        if index == 0 {
            let outside: (i64, String) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM auto_queue_phase_gates g WHERE g.run_id=r.id), r.status FROM auto_queue_runs r WHERE r.id=$1")
                .bind(&run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(
                outside,
                (1, "paused".into()),
                "gate clear and resume stay private until the caller commits"
            );
        }
        tx.commit().await.unwrap();
        let gate_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = $1")
                .bind(&run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(gate_count, i64::from(!passed), "durable gate rows: {name}");
        let status: String = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id=$1")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status,
            if passed { "active" } else { "paused" },
            "durable run: {name}"
        );
        eprintln!("PHASE_GATE_OWNER_CASE {name} passed={passed}");
    }
    drop(policy);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_phase_gate_legacy_provenance_bridge_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ('gate-owner', 'Gate', 'claude')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('gate-run', 'repo', 'gate-owner', 'paused')").execute(&pool).await.unwrap();
    let policy = Policy::new(Some(pool.clone()));
    let script = "return agentdesk.phaseGate.authoritativeContext('gate-run', 0, {phase_gate:{run_id:'gate-run',batch_phase:0,pass_verdict:'old'}});";
    assert!(
        policy.eval(script).is_null(),
        "empty provenance cannot restore a declaration"
    );
    sqlx::query("INSERT INTO auto_queue_entries (id, run_id, agent_id, status, batch_phase, phase_gate_kind) VALUES ('gate-entry', 'gate-run', 'gate-owner', 'done', 0, NULL)").execute(&pool).await.unwrap();
    assert_eq!(policy.eval(script)["phase_gate"]["kind"], "pr-confirm");
    let legacy = json!({"phase_gate":{"run_id":"gate-run","batch_phase":0,"next_phase":1,"pass_verdict":"old"}});
    let result =
        json!({"checks":{"merge_verified":"pass","issue_closed":"pass","build_passed":"pass"}});
    assert_eq!(
        policy.complete(&legacy, &result)["cleared"],
        1,
        "actual hook accepts proven legacy provenance"
    );
    sqlx::query("UPDATE auto_queue_entries SET phase_gate_kind=E' \\t\\n\\r\\f\\v'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        policy.eval(script)["phase_gate"]["pass_verdict"],
        "phase_gate_passed"
    );
    sqlx::query("INSERT INTO auto_queue_entries (id, run_id, agent_id, status, batch_phase, phase_gate_kind) VALUES ('typed-entry', 'gate-run', 'gate-owner', 'done', 0, 'pr-confirm')").execute(&pool).await.unwrap();
    assert!(
        policy.eval(script).is_null(),
        "mixed provenance cannot restore a declaration"
    );
    assert_eq!(
        policy.complete(&legacy, &result)["cleared"],
        0,
        "actual hook rejects mixed provenance"
    );
    assert!(policy.eval("return agentdesk.phaseGate.authoritativeContext('gate-run',0,{phase_gate:{kind:'pr-confirm'}});").is_null(), "partial snapshot never falls back");
    drop(policy);
    pool.close().await;
    db.drop().await;
}
