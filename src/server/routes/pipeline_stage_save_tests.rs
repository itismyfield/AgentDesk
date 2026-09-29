use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use super::{AppState, GetStagesQuery, PutStagesBody, get_stages, put_stages};
use crate::dispatch::test_support::DispatchPostgresTestDb;

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

async fn save_stages(state: &AppState, repo: &str, stages: Value) -> (StatusCode, Value) {
    let body: PutStagesBody = serde_json::from_value(json!({ "repo": repo, "stages": stages }))
        .expect("decode stage save request");
    let (status, Json(body)) = put_stages(State(state.clone()), Json(body))
        .await
        .expect("stage save handler response");
    (status, body)
}

async fn read_stages(state: &AppState, repo: &str) -> Value {
    let (status, Json(body)) = get_stages(
        State(state.clone()),
        Query(GetStagesQuery {
            repo: Some(repo.to_string()),
            agent_id: None,
        }),
    )
    .await
    .expect("stage list handler response");
    assert_eq!(status, StatusCode::OK);
    body
}

async fn assert_rejected_without_changes(
    state: &AppState,
    repo: &str,
    stages: Value,
    expected: &Value,
    case: &str,
) {
    let (status, response) = save_stages(state, repo, stages).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {response}");
    assert!(response["error"].is_string(), "{case}: {response}");
    assert_eq!(&read_stages(state, repo).await, expected, "{case}");
}

#[tokio::test]
async fn stage_save_routes_reject_unsupported_settings_without_writes_pg() {
    let Some(pg_db) = DispatchPostgresTestDb::try_create(
        "agentdesk_stage_save_routes",
        "pipeline stage save route validation",
    )
    .await
    else {
        eprintln!("SOFT-SKIP: pipeline stage save route validation requires PostgreSQL");
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let state = test_state(pool.clone());
    let repo = "stage-save-routes";
    let stages = json!([
        {
            "stage_name": "review",
            "stage_order": 4,
            "trigger_after": "review_pass",
            "entry_skill": "review-skill",
            "provider": "self",
            "agent_override_id": "review-agent",
            "timeout_minutes": 45,
            "on_failure": "retry-with-backoff",
            "on_failure_target": "build",
            "max_retries": 2,
            "backoff": "linear",
            "skip_condition": "",
            "parallel_with": "build"
        },
        { "stage_name": "build", "stage_order": 8, "provider": "self" }
    ]);

    let (status, saved) = save_stages(&state, repo, stages.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["stages"].as_array().expect("saved stages").len(), 2);
    for (index, requested) in stages
        .as_array()
        .expect("requested stages")
        .iter()
        .enumerate()
    {
        for (key, value) in requested.as_object().expect("requested stage") {
            assert_eq!(&saved["stages"][index][key], value, "stage {index}: {key}");
        }
    }
    assert_eq!(read_stages(&state, repo).await, saved);

    let mut counter = stages.clone();
    counter[0]["provider"] = json!("counter");
    let mut skip = stages.clone();
    skip[0]["skip_condition"] = json!("label:hotfix");
    let mut duplicate_order = stages;
    duplicate_order[1]["stage_order"] = json!(4);
    for (case, candidate) in [
        ("new counter provider", counter),
        ("new skip condition", skip),
        ("duplicate explicit order", duplicate_order),
    ] {
        assert_rejected_without_changes(&state, repo, candidate, &saved, case).await;
    }

    drop(state);
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn stage_save_routes_preserve_legacy_settings_only_for_the_same_stage_pg() {
    let Some(pg_db) = DispatchPostgresTestDb::try_create(
        "agentdesk_stage_save_legacy",
        "pipeline legacy stage save compatibility",
    )
    .await
    else {
        eprintln!("SOFT-SKIP: pipeline legacy stage save compatibility requires PostgreSQL");
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let state = test_state(pool.clone());
    let repo = "stage-save-legacy";
    sqlx::query(
        "INSERT INTO pipeline_stages
            (repo_id, stage_name, stage_order, provider, skip_condition, agent_override_id)
         VALUES ($1, 'review', 1, 'counter', NULL, 'legacy-agent'),
                ($1, 'build', 2, 'self', 'label:hotfix', NULL)",
    )
    .bind(repo)
    .execute(&pool)
    .await
    .expect("seed legacy stage settings");

    let original = read_stages(&state, repo).await;
    let (status, unchanged) = save_stages(&state, repo, original["stages"].clone()).await;
    assert_eq!(status, StatusCode::OK, "{unchanged}");
    assert_eq!(unchanged, original);

    let mut edited = original["stages"].clone();
    edited[0]["timeout_minutes"] = json!(45);
    let (status, saved) = save_stages(&state, repo, edited).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let mut expected = original;
    expected["stages"][0]["timeout_minutes"] = json!(45);
    assert_eq!(saved, expected);
    assert_eq!(read_stages(&state, repo).await, saved);

    let mut counter_agent = saved["stages"].clone();
    counter_agent[0]["agent_override_id"] = json!("replacement-agent");
    let mut skip = saved["stages"].clone();
    skip[1]["skip_condition"] = json!("label:docs");
    let mut renamed_counter = saved["stages"].clone();
    renamed_counter[0]["stage_name"] = json!("renamed-review");
    let mut renamed_skip = saved["stages"].clone();
    renamed_skip[1]["stage_name"] = json!("renamed-build");
    for (case, candidate) in [
        ("changed counter agent", counter_agent),
        ("changed legacy skip", skip),
        ("renamed counter stage", renamed_counter),
        ("renamed skip stage", renamed_skip),
    ] {
        assert_rejected_without_changes(&state, repo, candidate, &saved, case).await;
    }

    let other_repo = "stage-save-legacy-copy";
    let empty = read_stages(&state, other_repo).await;
    assert_eq!(empty, json!({ "stages": [] }));
    assert_rejected_without_changes(
        &state,
        other_repo,
        saved["stages"].clone(),
        &empty,
        "legacy settings copied to another repo",
    )
    .await;
    assert_eq!(read_stages(&state, repo).await, saved);

    drop(state);
    pool.close().await;
    pg_db.drop().await;
}
