//! Routine handlers refuse explicit execution modes before any row is written.
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::routing::{patch, post};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::AppState;
use super::{attach_routine, patch_routine};
use crate::db::auto_queue::test_support::TestPostgresDb;

fn test_state(pool: sqlx::PgPool) -> AppState {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).expect("construct policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    AppState {
        pg_pool: Some(pool),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    }
}

async fn model_request(
    router: &Router,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn model_patch_preserves_named_persistent_codex_routine_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let user = tempfile::tempdir().unwrap();
    let _home =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("HOME", user.path());
    let home = crate::services::provider_auth_profile::extra_account_home(
        &crate::domain::provider::ProviderKind::Codex,
        "work",
    )
    .unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let org = serde_json::json!({
        "version": 1,
        "agents": {
            "work": {"display_name":"Work", "provider":"codex", "auth_profile":"work"},
            "regular": {"display_name":"Regular", "provider":"codex"}
        },
        "provider_auth_profiles": {"work":{"provider":"codex","home":home}}
    });
    let path = crate::runtime_layout::org_schema_path(&crate::config::runtime_root().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_yaml::to_string(&org).unwrap()).unwrap();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id,name,provider,discord_channel_cc,discord_channel_cdx) VALUES ('work','Work','codex','7401','7402'), ('regular','Regular','codex','7501','7502'), ('claude','Claude','claude','7601',NULL), ('legacy','Legacy','codex',NULL,'7702')")
        .execute(&pool).await.unwrap();
    let router = Router::new()
        .route("/api/routines", post(attach_routine))
        .route("/api/routines/{id}", patch(patch_routine))
        .with_state(test_state(pool.clone()));

    let legacy_label =
        crate::services::routines::agent_executor::routine_agent_session_name("original", "legacy");
    let legacy_tmux =
        crate::domain::provider::ProviderKind::Codex.build_tmux_session_name(&legacy_label);
    crate::services::tmux_common::write_tmux_session_auth_profile(&legacy_tmux, "work").unwrap();

    let mut refusals = Vec::new();
    for (agent, provider, strategy, update, refuse) in [
        (
            "work",
            "codex",
            "persistent",
            json!({"model":"model-b","name":"edited"}),
            true,
        ),
        (
            "work",
            "codex",
            "persistent",
            json!({"model":null,"name":"edited"}),
            true,
        ),
        (
            "regular",
            "codex",
            "persistent",
            json!({"agent_id":"work","model":"model-b"}),
            true,
        ),
        (
            "work",
            "claude",
            "persistent",
            json!({"provider":"codex","model":"model-b"}),
            true,
        ),
        (
            "work",
            "codex",
            "persistent",
            json!({"model":"model-a","name":"edited"}),
            false,
        ),
        (
            "regular",
            "codex",
            "persistent",
            json!({"model":"model-b"}),
            false,
        ),
        (
            "claude",
            "claude",
            "persistent",
            json!({"model":null}),
            false,
        ),
        (
            "legacy",
            "codex",
            "persistent",
            json!({"model":"model-b"}),
            true,
        ),
        (
            "work",
            "codex",
            "persistent",
            json!({"provider":null,"model":null}),
            true,
        ),
        (
            "regular",
            "codex",
            "persistent",
            json!({"fallback_agent_id":"work","model":"model-b"}),
            true,
        ),
        (
            "work",
            "codex",
            "persistent",
            json!({"execution_strategy":"fresh","model":"model-b"}),
            false,
        ),
        ("work", "codex", "fresh", json!({"model":"model-b"}), false),
    ] {
        let (status, created) = model_request(&router, "POST", "/api/routines", json!({
            "script_ref":"provider.js", "name":"original", "agent_id":agent, "provider":provider,
            "model":"model-a", "execution_strategy":strategy,
        })).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["routine"]["model"], "model-a");
        let id = created["routine"]["id"].as_str().unwrap();
        let uri = format!("/api/routines/{id}");
        let before = rows(&pool).await;
        let (status, body) = model_request(&router, "PATCH", &uri, update.clone()).await;
        if refuse {
            refusals.push((update, status, body, rows(&pool).await == before));
        } else {
            assert_eq!(status, StatusCode::OK, "{update}: {body}");
            assert_eq!(body["routine"]["model"], update["model"]);
        }
    }
    pool.close().await;
    db.drop().await;
    for (update, status, body, unchanged) in &refusals {
        assert!(
            *status == StatusCode::UNPROCESSABLE_ENTITY
                && body["error"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("named account"))
                && *unchanged,
            "model PATCH must be refused without writing: {refusals:?}; observed {update}"
        );
    }
}

async fn rows(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(r) FROM routines r ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_modes_are_refused_with_422_and_leave_routines_untouched_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let state = test_state(pool.clone());

    let (status, Json(created)) = attach_routine(
        State(state.clone()),
        Json(serde_json::from_value(json!({"script_ref": "daily.js"})).unwrap()),
    )
    .await
    .expect("legacy attach");
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["routine"]["execution_mode"], Value::Null);
    assert_eq!(
        created["routine"]["execution_compatibility"],
        "legacy_fresh"
    );
    assert_eq!(created["routine"]["headless_failure_notice_limit"], 3);
    let routine_id = created["routine"]["id"].as_str().unwrap().to_string();
    let before = rows(&pool).await;

    for mode in [
        json!("headless"),
        json!("thread"),
        json!("channel"),
        json!("script"),
    ] {
        let body = json!({"script_ref": "other.js", "execution_mode": mode});
        let refused = attach_routine(
            State(state.clone()),
            Json(serde_json::from_value(body).unwrap()),
        )
        .await
        .expect_err("explicit mode attach");
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY, "{mode}");
    }
    for mode in [
        json!("headless"),
        json!("thread"),
        json!("channel"),
        json!("script"),
        Value::Null,
    ] {
        let body = json!({"name": "renamed", "execution_mode": mode});
        let refused = patch_routine(
            State(state.clone()),
            Path(routine_id.clone()),
            Json(serde_json::from_value(body).unwrap()),
        )
        .await
        .expect_err("explicit mode patch");
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY, "{mode}");
    }
    assert_eq!(rows(&pool).await, before, "refused requests must not write");

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_choices_an_agent_cannot_run_are_refused_and_the_view_marks_inherited_values_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cc, discord_channel_cdx) VALUES ('duo','Duo','claude','7101','7102'), ('solo','Solo','claude','7201',NULL)")
        .execute(&pool).await.unwrap();
    let state = test_state(pool.clone());
    let attach = |body: Value| {
        attach_routine(
            State(state.clone()),
            Json(serde_json::from_value(body).unwrap()),
        )
    };
    for body in [
        json!({"script_ref": "a.js", "fallback_agent_id": "duo", "provider": "codex"}),
        json!({"script_ref": "a.js", "agent_id": "solo", "provider": "codex"}),
        json!({"script_ref": "a.js", "agent_id": "duo", "model": "gpt-x"}),
        json!({"script_ref": "a.js", "agent_id": "duo", "provider": "nope"}),
        json!({"script_ref": "a.js", "agent_id": "duo", "fallback_agent_id": "solo", "provider": "codex"}),
    ] {
        let refused = attach(body.clone()).await.expect_err("unrunnable choice");
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    assert!(
        rows(&pool).await.is_empty(),
        "refused attaches must not write"
    );

    let body =
        json!({"script_ref": "a.js", "agent_id": "duo", "provider": "codex", "model": "gpt-x"});
    let (_, Json(created)) = attach(body).await.expect("runnable choice");
    let routine = &created["routine"];
    assert_eq!(
        (&routine["provider"], &routine["model"]),
        (&json!("codex"), &json!("gpt-x"))
    );
    assert_eq!(routine["provider_source"], "explicit");
    let id = routine["id"].as_str().unwrap().to_string();
    let patch = |body: Value| {
        patch_routine(
            State(state.clone()),
            Path(id.clone()),
            Json(serde_json::from_value(body).unwrap()),
        )
    };
    let before = rows(&pool).await;
    for body in [
        json!({"agent_id": "solo"}),
        json!({"provider": null}),
        json!({"agent_id": null}),
    ] {
        let refused = patch(body.clone()).await.expect_err("unrunnable patch");
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    assert_eq!(rows(&pool).await, before, "refused patches must not write");

    let Json(patched) = patch(json!({"provider": null, "model": null, "agent_id": "solo"}))
        .await
        .unwrap();
    let routine = &patched["routine"];
    assert_eq!(
        (&routine["agent_id"], &routine["provider"]),
        (&json!("solo"), &Value::Null)
    );
    assert_eq!(routine["effective_provider"], "claude");
    assert_eq!(
        (&routine["provider_source"], &routine["model_source"]),
        (&json!("inherited"), &json!("inherited"))
    );
    pool.close().await;
    db.drop().await;
}
