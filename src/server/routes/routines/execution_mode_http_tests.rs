//! Routine handlers refuse explicit execution modes before any row is written.
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

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
