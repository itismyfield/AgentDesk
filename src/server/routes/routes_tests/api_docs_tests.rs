//! Domain-split routes tests — `api_docs` group.
//! Extracted verbatim from the original `routes_tests.rs` (test bodies unchanged).
//! Helpers and the `sqlite_params!` macro live in `super::common` and the parent
//! `routes_tests` module respectively (the macro is in lexical scope through
//! the parent `mod.rs`).

#![allow(unused_imports)]

// Reach into the parent `routes` module for the symbols the original
// `use super::*;` brought in, plus the shared test helpers in `common`.
use super::super::*;
use super::common::*;

use axum::body::{Body, HttpBody as _};
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::Row;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::MutexGuard;
use tower::ServiceExt;

#[tokio::test]
async fn api_docs_returns_group_hierarchy_by_default() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let groups = json["groups"]
        .as_array()
        .expect("docs must return group array");

    let names: Vec<&str> = groups
        .iter()
        .filter_map(|group| group["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "runtime",
            "kanban",
            "agents",
            "integrations",
            "automation",
            "config",
            "observability",
            "internal",
        ],
        "docs must expose the #1063 eight-group hierarchy"
    );

    let runtime = groups
        .iter()
        .find(|group| group["name"] == "runtime")
        .expect("runtime group must be present");
    let runtime_categories = runtime["categories"]
        .as_array()
        .expect("runtime group must list categories");
    assert!(
        runtime_categories
            .iter()
            .any(|category| category == "dispatches"),
        "runtime group must contain the dispatches category: {runtime}"
    );
    assert!(
        runtime["description"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("runtime")
            || runtime["description"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("dispatches"),
        "runtime group description must mention runtime surfaces: {runtime}"
    );
    assert!(
        json.get("endpoints").is_none(),
        "default docs response must return grouped hierarchy, not flat endpoints"
    );
    assert!(
        json.get("categories").is_none(),
        "default docs response must return groups (not the legacy flat categories field)"
    );
}

/// #1063: `GET /api/docs/{group}` lists categories under a group.
#[tokio::test]
async fn api_docs_group_kanban_lists_cards_and_reviews() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/kanban")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "kanban");
    let categories = json["categories"]
        .as_array()
        .expect("group detail must include categories array");
    let category_names: Vec<&str> = categories
        .iter()
        .filter_map(|category| category["name"].as_str())
        .collect();
    assert!(
        category_names.contains(&"kanban"),
        "kanban group must contain the kanban cards category: {category_names:?}"
    );
    assert!(
        category_names.contains(&"reviews"),
        "kanban group must contain the reviews category: {category_names:?}"
    );
    assert!(
        category_names.contains(&"pipeline"),
        "kanban group must contain the pipeline category: {category_names:?}"
    );
}

/// #1063: `GET /api/docs/{group}/{category}` returns endpoints for that
/// category (e.g. `kanban/reviews`).
#[tokio::test]
async fn api_docs_group_category_kanban_reviews_returns_endpoints() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/kanban/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "kanban");
    assert_eq!(json["category"], "reviews");
    let endpoints = json["endpoints"]
        .as_array()
        .expect("group/category response must include endpoints array");
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["path"] == "/api/reviews/verdict"),
        "kanban/reviews must include the review verdict endpoint"
    );
}

#[tokio::test]
async fn api_docs_group_category_automation_routines_returns_session_controls() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/automation/routines")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "automation");
    assert_eq!(json["category"], "routines");
    let endpoints = json["endpoints"]
        .as_array()
        .expect("automation/routines response must include endpoints array");
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["path"] == "/api/routines/metrics"),
        "automation/routines must include routine metrics"
    );
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["path"] == "/api/routines/runs/search"),
        "automation/routines must include routine run result search"
    );
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["path"] == "/api/routines/{id}/session/reset"),
        "automation/routines must include routine session reset"
    );
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["path"] == "/api/routines/{id}/session/kill"),
        "automation/routines must include routine session kill"
    );
}

#[tokio::test]
async fn api_docs_query_category_routines_returns_only_routine_endpoints() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?category=routines")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "automation");
    assert_eq!(json["category"], "routines");
    let endpoints = json["endpoints"]
        .as_array()
        .expect("category query response must include endpoint array");
    assert!(endpoints.iter().any(|ep| ep["path"] == "/api/routines"));
    assert!(
        endpoints.iter().all(|ep| ep["category"] == "routines"),
        "category=routines must not include queue/cron endpoints"
    );
}

/// #1063: unknown group → 404.
#[tokio::test]
async fn api_docs_unknown_group_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/not-a-real-group")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// #1063: mismatched group/category → 404.
#[tokio::test]
async fn api_docs_group_category_mismatch_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    // `reviews` belongs to the `kanban` group, not `automation`.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/automation/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// #1063 backward compat: `GET /api/docs/{category}` still works for the
/// legacy category names but responds with `X-Deprecated` header that points
/// at the new `/group/category` path.
#[tokio::test]
async fn api_docs_legacy_category_route_emits_deprecation_header() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let deprecated = response
        .headers()
        .get("x-deprecated")
        .expect("legacy category route must emit X-Deprecated header")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(deprecated, "/api/docs/kanban/reviews");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deprecated"], true);
}

#[tokio::test]
async fn api_docs_flat_format_mentions_skip_outbox_for_dispatch_create() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs?format=flat must return endpoint array");
    let dispatch_post = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/dispatches")
        .expect("dispatch create endpoint must be documented");

    let description = dispatch_post["description"]
        .as_str()
        .expect("dispatch docs description must be string");
    assert!(
        description.contains("skip_outbox"),
        "dispatch create docs must mention skip_outbox option: {description}"
    );
    assert_eq!(dispatch_post["params"]["skip_outbox"]["type"], "boolean");
}

#[tokio::test]
async fn api_docs_category_exposes_dispatch_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "dispatches");
    assert!(
        json["count"].as_u64().unwrap_or(0) >= 4,
        "dispatches detail should include documented endpoints"
    );

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let dispatch_post = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/dispatches")
        .expect("dispatch create endpoint must be present in detail view");
    assert_eq!(
        dispatch_post["params"]["kanban_card_id"]["location"],
        "body"
    );
    assert_eq!(dispatch_post["params"]["skip_outbox"]["type"], "boolean");
    assert_eq!(
        dispatch_post["example"]["request"]["body"]["skip_outbox"],
        serde_json::json!(true)
    );
    assert_eq!(
        dispatch_post["example"]["response"]["dispatch"]["status"],
        "pending"
    );

    let dispatch_patch = endpoints
        .iter()
        .find(|ep| ep["method"] == "PATCH" && ep["path"] == "/api/dispatches/{id}")
        .expect("dispatch update endpoint must be present in detail view");
    let patch_description = dispatch_patch["description"]
        .as_str()
        .expect("PATCH dispatch docs description must be a string");
    assert!(
        patch_description.contains("Allowed status values")
            && patch_description.contains("result_summary")
            && patch_description.contains("completed_at"),
        "PATCH dispatch docs must spell out status/result lifecycle semantics: {patch_description}"
    );
    let status_enum = dispatch_patch["params"]["status"]["enum"]
        .as_array()
        .expect("PATCH dispatch status param must expose allowed enum values");
    for expected in ["pending", "dispatched", "completed", "cancelled", "failed"] {
        assert!(
            status_enum.iter().any(|value| value == expected),
            "PATCH dispatch docs must include allowed status {expected}"
        );
    }
    assert_eq!(
        dispatch_patch["example"]["response"]["dispatch"]["result_summary"],
        "done"
    );
    assert!(
        dispatch_patch["example"]["response"]["dispatch"]["updated_at"].is_string(),
        "PATCH dispatch example must expose updated_at"
    );
    assert!(
        dispatch_patch["example"]["response"]["dispatch"]["completed_at"].is_string(),
        "PATCH dispatch example must expose completed_at"
    );
    assert_eq!(dispatch_patch["error_example"]["status"], 400);

    let dispatch_cancel = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/dispatches/{id}/cancel")
        .expect("dispatch cancel endpoint must be present in dispatches detail view");
    assert_eq!(dispatch_cancel["params"]["id"]["location"], "path");
    let cancel_description = dispatch_cancel["description"]
        .as_str()
        .expect("cancel dispatch docs description must be a string");
    assert!(
        cancel_description.contains("pending or dispatched")
            && cancel_description.contains("Terminal dispatches return 409"),
        "cancel dispatch docs must describe lifecycle scope and terminal conflict: {cancel_description}"
    );
    assert_eq!(dispatch_cancel["example"]["response"]["ok"], true);
    assert_eq!(dispatch_cancel["error_example"]["status"], 409);
}

#[tokio::test]
async fn api_docs_category_exposes_auto_queue_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/queue")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "queue");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let generate = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/queue/generate")
        .expect("auto-queue generate endpoint must be present");
    assert!(
        generate["params"].get("mode").is_none(),
        "generate docs should not expose legacy mode selection"
    );
    assert!(
        generate["params"].get("parallel").is_none(),
        "generate docs should not expose legacy parallel toggle"
    );
    assert_eq!(
        generate["example"]["response"]["run"]["unified_thread"],
        serde_json::json!(false)
    );
    assert_eq!(
        generate["params"]["auto_assign_agent"]["default"],
        serde_json::json!(false)
    );

    let dispatch_next = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/queue/dispatch-next")
        .expect("auto-queue dispatch-next endpoint must be present");
    assert_eq!(dispatch_next["params"]["run_id"]["required"], false);
    assert_eq!(dispatch_next["params"]["active_only"]["default"], false);
    assert_eq!(dispatch_next["example"]["response"]["count"], 1);

    let pause = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/queue/pause")
        .expect("auto-queue pause endpoint must be present");
    assert_eq!(pause["params"]["force"]["type"], "boolean");
    assert_eq!(pause["params"]["force"]["default"], false);
    assert_eq!(pause["example"]["response"]["paused_runs"], 1);
    assert_eq!(pause["example"]["response"]["cancelled_dispatches"], 0);
    assert_eq!(pause["example"]["response"]["released_slots"], 0);
    assert_eq!(pause["example"]["response"]["cleared_slot_sessions"], 0);
}

#[tokio::test]
async fn api_docs_category_exposes_kanban_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/kanban/kanban")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "kanban");
    assert_eq!(json["group"], "kanban");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let create = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards")
        .expect("kanban create endpoint must be present");
    assert_eq!(create["params"]["title"]["type"], "string");
    assert_eq!(create["example"]["response"]["card"]["status"], "backlog");

    let assign_issue = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards/assign-issue")
        .expect("kanban assign-issue endpoint must be present");
    let assign_issue_description = assign_issue["description"].as_str().unwrap_or_default();
    assert!(
        assign_issue_description.contains("Assignment is guaranteed")
            && assign_issue_description.contains("response.transition"),
        "assign-issue docs must describe assignment/transition partial-success semantics: {assign_issue_description}"
    );
    assert_eq!(
        assign_issue["partial_success_example"]["scenario"],
        "partial_success"
    );
    assert_eq!(
        assign_issue["partial_success_example"]["status"],
        json!(200)
    );
    assert_eq!(
        assign_issue["partial_success_example"]["response"]["assignment"]["ok"],
        true
    );
    assert_eq!(
        assign_issue["partial_success_example"]["response"]["transition"]["ok"],
        false
    );
    assert_eq!(
        assign_issue["partial_success_example"]["response"]["transition"]["next_action"],
        "inspect_transition_error"
    );
    assert!(
        assign_issue["partial_success_example"]["response"]["transition"]["error"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "assign-issue docs must show the transition error field in partial-success responses"
    );

    let resume = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards/{id}/resume")
        .expect("kanban resume endpoint must be present");
    assert_eq!(resume["params"]["force"]["type"], "boolean");
    assert_eq!(
        resume["example"]["response"]["action"]["type"],
        "new_implementation_dispatch"
    );

    let update = endpoints
        .iter()
        .find(|ep| ep["method"] == "PATCH" && ep["path"] == "/api/kanban-cards/{id}")
        .expect("kanban update endpoint must be present");
    let update_description = update["description"].as_str().unwrap_or_default();
    assert!(
        update_description.contains("backlog -> ready")
            && update_description.contains("/transition")
            && update_description.contains("/rereview"),
        "PATCH docs must describe restricted status semantics and canonical alternatives: {update_description}"
    );
    assert!(
        update["params"]["status"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("backlog -> ready"),
        "PATCH status param must not look like an unrestricted target status"
    );
    assert_eq!(update["error_example"]["status"], serde_json::json!(400));
    assert!(
        update["error_example"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("/api/kanban-cards/{id}/transition"),
        "PATCH error example must route force transitions to /transition"
    );

    let assign = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards/{id}/assign")
        .expect("kanban assign endpoint must be present");
    let assign_description = assign["description"].as_str().unwrap_or_default();
    assert!(
        assign_description.contains("Assignment is guaranteed")
            && assign_description.contains("response.transition"),
        "assign docs must describe assignment/transition partial-success semantics: {assign_description}"
    );
    assert_eq!(
        assign["example"]["response"]["transition"]["target_status"],
        "requested"
    );
    assert_eq!(
        assign["example"]["response"]["transition"]["next_action"],
        "none_required"
    );
    assert!(
        assign["example"]["response"]["transition"]["error"].is_null(),
        "assign docs must document a stable null error field on success"
    );
    assert_eq!(
        assign["partial_success_example"]["scenario"],
        "partial_success"
    );
    assert_eq!(assign["partial_success_example"]["status"], json!(200));
    assert_eq!(
        assign["partial_success_example"]["response"]["assignment"]["ok"],
        true
    );
    assert_eq!(
        assign["partial_success_example"]["response"]["transition"]["ok"],
        false
    );
    assert_eq!(
        assign["partial_success_example"]["response"]["transition"]["failed_step"],
        "requested"
    );
    assert_eq!(
        assign["partial_success_example"]["response"]["transition"]["next_action"],
        "inspect_transition_error"
    );
    assert!(
        assign["partial_success_example"]["response"]["transition"]["error"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "assign docs must show the transition error field in partial-success responses"
    );

    for path in [
        "/api/kanban-cards/{id}/retry",
        "/api/kanban-cards/{id}/redispatch",
    ] {
        let endpoint = endpoints
            .iter()
            .find(|ep| ep["method"] == "POST" && ep["path"] == path)
            .unwrap_or_else(|| panic!("{path} endpoint must be present"));
        assert_eq!(
            endpoint["example"]["response"]["next_action"],
            "none_required"
        );
        assert!(
            endpoint["example"]["response"]
                .get("new_dispatch_id")
                .is_some(),
            "{path} must document new_dispatch_id"
        );
        assert!(
            endpoint["example"]["response"]
                .get("cancelled_dispatch_id")
                .is_some(),
            "{path} must document cancelled_dispatch_id"
        );
    }

    let rereview = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards/{id}/rereview")
        .expect("kanban rereview endpoint must be present");
    let rereview_description = rereview["description"].as_str().unwrap_or_default();
    assert!(
        rereview_description.contains("instead of PATCH status=review")
            && rereview_description.contains("Bearer"),
        "rereview docs must clarify review rerun semantics and auth: {rereview_description}"
    );

    let transition = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards/{id}/transition")
        .expect("kanban transition endpoint must be present");
    let transition_description = transition["description"].as_str().unwrap_or_default();
    assert!(
        transition_description.contains("force-transition semantics")
            && transition_description.contains("/api/kanban-cards/{id}/force-transition")
            && transition_description.contains("fully removed")
            && transition_description.contains("Bearer"),
        "transition docs must clarify force-transition semantics and canonical path: {transition_description}"
    );
}

#[tokio::test]
async fn api_docs_category_exposes_agents_turn_start_contract() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/agents/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "agents");
    assert_eq!(json["group"], "agents");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let turn_start = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/agents/{id}/turn/start")
        .expect("agents turn/start endpoint must be present");
    assert_eq!(turn_start["params"]["id"]["location"], "path");
    assert_eq!(turn_start["params"]["prompt"]["required"], true);
    assert_eq!(turn_start["params"]["metadata"]["type"], "object");
    assert_eq!(turn_start["params"]["source"]["type"], "string");
    assert_eq!(turn_start["params"]["dm_user_id"]["type"], "string");
    assert_eq!(
        turn_start["example"]["response"]["status"],
        serde_json::json!("started")
    );

    let setup = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/agents/setup")
        .expect("agents setup endpoint must be present");
    assert_eq!(setup["params"]["agent_id"]["required"], true);
    assert_eq!(setup["params"]["dry_run"]["type"], "boolean");
    assert_eq!(setup["params"]["provider"]["enum"][0], "claude");
    assert_eq!(setup["example"]["response"]["dry_run"], true);

    let message = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/agents/{id}/message")
        .expect("agents message endpoint must be present");
    assert_eq!(message["params"]["from_agent_id"]["required"], true);
    assert_eq!(message["params"]["channel_kind"]["default"], "cc");
    assert_eq!(message["params"]["prefix"]["default"], true);
    assert_eq!(message["example"]["response"]["bot"], "announce");
}

#[tokio::test]
async fn api_docs_flat_format_lists_routes_missing_from_legacy_docs() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs?format=flat must return endpoint array");

    for path in [
        "/api/kanban-cards/{id}/reopen",
        "/api/reviews/decision",
        "/api/queue/dispatch-next",
        "/api/queue/entries/{id}",
        "/api/queue/slots/{agent_id}/{slot_index}/reset-thread",
        "/api/help",
        "/api/docs/{group}",
        "/api/docs/{group}/{category}",
        "/api/health/detail",
        "/api/doctor/startup/latest",
        "/api/doctor/stale-mailbox/repair",
        "/api/channels/{id}/relay-recovery",
        "/api/github/issues/create",
        "/api/sessions/{id}/tmux-output",
        "/api/stats/memento",
    ] {
        assert!(
            endpoints.iter().any(|ep| ep["path"] == path),
            "flat docs must include {path}"
        );
    }
}

#[tokio::test]
async fn api_docs_flat_format_omits_removed_legacy_routes() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs?format=flat must return endpoint array");

    for path in [
        "/api/agent-channels",
        "/api/dispatch-cancel/{id}",
        "/api/pipeline-stages",
        "/api/pipeline-stages/{id}",
        "/api/session/start",
        "/api/sessions/search",
        "/api/sessions/force-kill",
        "/api/auto-queue/enqueue",
        "/api/api-friction/events",
        "/api/api-friction/patterns",
        "/api/api-friction/process",
        // #1064 removals
        "/api/re-review",
        "/api/hook/session",
        "/api/auto-queue/activate",
        "/api/auto-queue/dispatch",
        "/api/queue/activate",
        "/api/queue/dispatch",
        "/api/kanban-cards/bulk-action",
        "/api/kanban-cards/batch-transition",
        "/api/kanban-cards/{id}/force-transition",
    ] {
        assert!(
            endpoints.iter().all(|ep| ep["path"] != path),
            "flat docs must omit removed route {path}"
        );
    }

    assert!(
        endpoints
            .iter()
            .any(|ep| ep["method"] == "POST" && ep["path"] == "/api/queue/runs/{id}/order"),
        "flat docs must keep the submit_order callback route"
    );
}

#[tokio::test]
async fn api_docs_unknown_category_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/not-a-real-category")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_docs_category_exposes_send_to_agent_endpoint() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/integrations/discord")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "integrations");
    assert_eq!(json["category"], "discord");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("integrations detail must include endpoint array");
    let send_to_agent = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/discord/send-to-agent")
        .expect("canonical send-to-agent endpoint must be documented");
    assert_eq!(send_to_agent["params"]["role_id"]["location"], "body");
    assert_eq!(send_to_agent["params"]["message"]["type"], "string");
    assert_eq!(send_to_agent["params"]["mode"]["type"], "string");
}

#[tokio::test]
async fn api_docs_category_exposes_skill_prune_and_filter_params() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "admin");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("admin detail must include endpoint array");
    let catalog = endpoints
        .iter()
        .find(|ep| ep["method"] == "GET" && ep["path"] == "/api/skills/catalog")
        .expect("skills catalog endpoint must be documented");
    assert_eq!(catalog["params"]["include_stale"]["location"], "query");
    assert_eq!(catalog["params"]["include_stale"]["type"], "boolean");

    let prune = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/skills/prune")
        .expect("skills prune endpoint must be documented");
    assert_eq!(prune["params"]["dry_run"]["location"], "query");
    assert_eq!(prune["params"]["dry_run"]["type"], "boolean");
}

/// #1068 (904-6) — every path in `TOP_40_PAIRED_PATHS` must ship BOTH a
/// happy-path example AND an error example, plus a curl 1-liner.
#[tokio::test]
async fn api_docs_exposes_paired_examples_for_top_40() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("flat docs must return endpoint array");

    let mut missing = Vec::new();
    for (method, path) in crate::server::routes::docs::TOP_40_PAIRED_PATHS {
        let Some(ep) = endpoints
            .iter()
            .find(|ep| ep["method"] == *method && ep["path"] == *path)
        else {
            missing.push(format!("endpoint not found: {method} {path}"));
            continue;
        };
        if !ep["example"].is_object() {
            missing.push(format!("{method} {path}: example (happy path) is missing"));
        }
        if !ep["error_example"].is_object() {
            missing.push(format!("{method} {path}: error_example is missing"));
        }
        let curl = ep["curl_example"].as_str().unwrap_or("");
        if curl.is_empty() || !curl.starts_with("curl ") {
            missing.push(format!(
                "{method} {path}: curl_example is missing or not a curl 1-liner (got {curl:?})"
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "top-40 paired-scenario coverage is incomplete:\n- {}",
        missing.join("\n- ")
    );

    // Guard against the list shrinking below 40.
    assert_eq!(
        crate::server::routes::docs::TOP_40_PAIRED_PATHS.len(),
        40,
        "#1068 (904-6) requires exactly 40 paired-scenario endpoints"
    );
}

/// #1068 (904-6) — `/retry`, `/redispatch`, `/resume`, and `/reopen`
/// descriptions must make their semantic distinctions explicit so callers stop
/// conflating them.
#[tokio::test]
async fn api_docs_retry_redispatch_resume_reopen_semantics_are_distinguished() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("flat docs must return endpoint array");

    let find_desc = |path: &str| -> String {
        endpoints
            .iter()
            .find(|ep| ep["path"] == path)
            .and_then(|ep| ep["description"].as_str())
            .unwrap_or_default()
            .to_string()
    };

    let retry = find_desc("/api/kanban-cards/{id}/retry").to_lowercase();
    let redispatch = find_desc("/api/kanban-cards/{id}/redispatch").to_lowercase();
    let resume = find_desc("/api/kanban-cards/{id}/resume").to_lowercase();
    let reopen = find_desc("/api/kanban-cards/{id}/reopen").to_lowercase();

    // retry: re-execute the SAME failed step with the same params.
    assert!(
        retry.contains("re-execute")
            || retry.contains("re-run")
            || retry.contains("same failed step"),
        "/retry description must explain it re-executes the same failed step: {retry}"
    );
    assert!(
        retry.contains("same"),
        "/retry description must contrast against /redispatch by mentioning 'same': {retry}"
    );

    // redispatch: new dispatch id, same intent.
    assert!(
        redispatch.contains("new dispatch") || redispatch.contains("new dispatch id"),
        "/redispatch description must mention that a NEW dispatch id is created: {redispatch}"
    );

    // resume: continue from a paused/checkpointed state.
    assert!(
        resume.contains("continue") || resume.contains("checkpoint"),
        "/resume description must mention continuing from a checkpoint: {resume}"
    );
    assert!(
        resume.contains("paused") || resume.contains("stuck") || resume.contains("checkpoint"),
        "/resume description must mention paused/checkpointed state: {resume}"
    );

    // reopen: move closed/done card back to active.
    assert!(
        reopen.contains("closed") || reopen.contains("terminal") || reopen.contains("done"),
        "/reopen description must mention the card's terminal/closed/done state: {reopen}"
    );
    assert!(
        reopen.contains("active") || reopen.contains("re-admit") || reopen.contains("ready"),
        "/reopen description must mention re-admitting the card into an active state: {reopen}"
    );

    // Each of retry/redispatch/resume must reference the others to make the
    // distinction explicit (reopen already checked via 'closed' + 'active').
    for (name, desc) in [
        ("retry", &retry),
        ("redispatch", &redispatch),
        ("resume", &resume),
    ] {
        let other_refs = ["retry", "redispatch", "resume", "reopen"]
            .iter()
            .filter(|n| **n != name)
            .filter(|n| desc.contains(*n))
            .count();
        assert!(
            other_refs >= 2,
            "/{name} description must reference at least two of the sibling semantics (retry/redispatch/resume/reopen) to disambiguate; got {other_refs}: {desc}"
        );
    }
}

/// #1443: the `/api/docs/card-lifecycle-ops` decision-tree page must be
/// reachable through the standard `/api/docs/{segment}` route and must
/// surface the markers the incident response team relies on so callers
/// cannot accidentally repeat the 2026-04-30 #1435 chained-call pattern.
///
/// The page is also surfaced in the `/api/docs` index `guides` array so
/// agents discover it without reading source.
#[tokio::test]
async fn api_docs_card_lifecycle_ops_guide_is_reachable_and_complete() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    // Index lists the guide so callers discover the path.
    let index_response = app
        .clone()
        .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(index_response.status(), StatusCode::OK);
    let index_bytes = axum::body::to_bytes(index_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let index_json: serde_json::Value = serde_json::from_slice(&index_bytes).unwrap();
    let guides = index_json["guides"]
        .as_array()
        .expect("/docs index must list long-form guides under 'guides'");
    let lifecycle_entry = guides
        .iter()
        .find(|guide| guide["name"] == "card-lifecycle-ops")
        .expect("/docs index must surface the card-lifecycle-ops guide");
    assert_eq!(
        lifecycle_entry["path"], "/api/docs/card-lifecycle-ops",
        "lifecycle guide path must be /api/docs/card-lifecycle-ops"
    );

    // Guide page itself is reachable through the standard /docs/{segment}
    // route and contains the markers the incident postmortem requires.
    let guide_response = app
        .oneshot(
            Request::builder()
                .uri("/docs/card-lifecycle-ops")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(guide_response.status(), StatusCode::OK);
    let guide_bytes = axum::body::to_bytes(guide_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let guide_text = String::from_utf8(guide_bytes.to_vec()).unwrap();
    let guide_json: serde_json::Value = serde_json::from_str(&guide_text).unwrap();

    assert_eq!(guide_json["title"], "Card Lifecycle Ops Guide");
    assert_eq!(guide_json["path"], "/api/docs/card-lifecycle-ops");

    let last_refreshed = guide_json["last_refreshed"]
        .as_str()
        .expect("guide must declare a last_refreshed marker (#1432 freshness gate)");
    assert!(
        last_refreshed.starts_with("Last refreshed:"),
        "last_refreshed marker must follow the 'Last refreshed: <date> against main @ <sha>' convention: {last_refreshed}"
    );
    assert!(
        last_refreshed.contains("main @"),
        "last_refreshed marker must pin the main commit sha per #1432: {last_refreshed}"
    );

    // The doc body must mention the structured markers so callers learn the
    // contract: anti-pattern wording, the next_action_hint field name, and
    // the 409 guard.
    let lower = guide_text.to_lowercase();
    assert!(
        lower.contains("anti-pattern"),
        "guide must contain an 'Anti-pattern' section naming today's incident"
    );
    assert!(
        guide_text.contains("next_action_hint"),
        "guide must reference the next_action_hint response field (#1442)"
    );
    assert!(
        guide_text.contains("409"),
        "guide must reference the 409 Conflict guard (#1444)"
    );
    assert!(
        guide_text.contains("skipped_due_to_active_dispatch"),
        "guide must reference skipped_due_to_active_dispatch (#1444)"
    );
    assert!(
        guide_text.contains("/api/queue/generate")
            && guide_text.contains("/api/kanban-cards/{id}/redispatch")
            && guide_text.contains("/api/kanban-cards/{id}/transition"),
        "guide must enumerate the five lifecycle endpoints by exact path"
    );
}

#[tokio::test]
async fn api_docs_api_friction_markers_guide_is_reachable_and_complete() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let index_response = app
        .clone()
        .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(index_response.status(), StatusCode::OK);
    let index_bytes = axum::body::to_bytes(index_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let index_json: serde_json::Value = serde_json::from_slice(&index_bytes).unwrap();
    let guides = index_json["guides"]
        .as_array()
        .expect("/docs index must list long-form guides under 'guides'");
    let friction_entry = guides
        .iter()
        .find(|guide| guide["name"] == "api-friction-markers")
        .expect("/docs index must surface the API friction marker guide");
    assert_eq!(
        friction_entry["path"], "/api/docs/api-friction-markers",
        "API friction marker guide path must be /api/docs/api-friction-markers"
    );

    let guide_response = app
        .oneshot(
            Request::builder()
                .uri("/docs/api-friction-markers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(guide_response.status(), StatusCode::OK);
    let guide_bytes = axum::body::to_bytes(guide_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let guide_text = String::from_utf8(guide_bytes.to_vec()).unwrap();
    let guide_json: serde_json::Value = serde_json::from_str(&guide_text).unwrap();

    assert_eq!(guide_json["title"], "API Friction Marker Guide");
    assert_eq!(guide_json["marker_prefix"], "API_FRICTION:");
    assert_eq!(
        guide_json["schema"]["required"]["endpoint"],
        "HTTP endpoint or API surface, for example PATCH /api/dispatches/{id}"
    );
    assert!(
        guide_text.contains("api_friction_events")
            && guide_text.contains("api_friction_issues")
            && guide_text.contains("Memento")
            && guide_text.contains("API_FRICTION:"),
        "API friction guide must describe marker collection, persistence, and example"
    );
}

#[tokio::test]
async fn pipeline_config_pg_repo_get_set_override() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("owner/repo-a")
        .execute(&pool)
        .await
        .unwrap();

    // GET — initially null
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/repo-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(body["pipeline_config"].is_null());

    // PUT — set override
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp2 = app2
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-a")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"hooks":{"review":{"on_enter":["CustomReviewHook"],"on_exit":[]}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    // GET — now has override
    let app3 = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp3 = app3
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/repo-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body3: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp3.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        body3["pipeline_config"]["hooks"]["review"]["on_enter"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "CustomReviewHook")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_agent_get_set_override() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')",
    )
    .bind("agent-x")
    .execute(&pool)
    .await
    .unwrap();

    // PUT
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/agent/agent-x")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"timeouts":{"in_progress":{"duration":"4h","clock":"started_at"}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET
    let app2 = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp2 = app2
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/agent/agent-x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp2.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["pipeline_config"]["timeouts"]["in_progress"]["duration"],
        "4h"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_effective_merges_layers() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("owner/repo-e")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')",
    )
    .bind("agent-e")
    .execute(&pool)
    .await
    .unwrap();

    // Set repo override (hooks)
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    app.oneshot(
        Request::builder()
            .method("PUT")
            .uri("/pipeline/config/repo/owner/repo-e")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"config":{"hooks":{"in_progress":{"on_enter":["RepoHook"],"on_exit":[]}}}}"#,
            ))
            .unwrap(),
    )
    .await
    .unwrap();

    // Get effective — should include repo hook
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app2
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/effective?repo=owner/repo-e&agent_id=agent-e")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["layers"]["repo"], true);
    assert_eq!(body["layers"]["agent"], false);
    // Hooks from repo override should be in effective pipeline
    let hooks = &body["pipeline"]["hooks"]["in_progress"]["on_enter"];
    assert!(hooks.as_array().unwrap().iter().any(|v| v == "RepoHook"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_graph_returns_nodes_and_edges() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/graph")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let nodes = body["nodes"].as_array().unwrap();
    let edges = body["edges"].as_array().unwrap();
    assert!(!nodes.is_empty());
    assert!(!edges.is_empty());
    // Each node has expected fields
    assert!(nodes[0]["id"].is_string());
    assert!(nodes[0]["label"].is_string());
    // Each edge has from/to/type
    assert!(edges[0]["from"].is_string());
    assert!(edges[0]["to"].is_string());
    assert!(edges[0]["type"].is_string());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_repo_invalid_override_rejected() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "owner/repo-bad");

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-bad")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"config":{"states":"not-an-array"}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pipeline_config_pg_invalid_merge_without_override_keeps_null() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("owner/repo-merge")
        .execute(&pool)
        .await
        .unwrap();

    // Override that adds a timeout referencing an unknown clock and a non-existent state.
    // This parses as valid JSON but the merged effective pipeline should fail validate().
    let body = r#"{"config":{"timeouts":{"nonexistent_state":{"duration":"1h","clock":"no_such_clock"}}}}"#;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-merge")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("validation failed"),
        "expected merged validation error, got: {}",
        body
    );
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT pipeline_config::text AS pipeline_config FROM github_repos WHERE id = $1",
    )
    .bind("owner/repo-merge")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        stored.is_none(),
        "invalid override without existing config must keep NULL, got: {stored:?}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_repo_invalid_merge_preserves_existing_override() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let valid_override = json!({
        "hooks": {
            "review": {
                "on_enter": ["ExistingRepoHook"],
                "on_exit": []
            }
        }
    });
    sqlx::query(
        "INSERT INTO github_repos (id, display_name, pipeline_config)
         VALUES ($1, $1, $2::jsonb)",
    )
    .bind("owner/repo-preserve")
    .bind(valid_override.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-preserve")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"timeouts":{"nonexistent_state":{"duration":"1h","clock":"no_such_clock"}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let stored: Option<String> = sqlx::query_scalar(
        "SELECT pipeline_config::text AS pipeline_config FROM github_repos WHERE id = $1",
    )
    .bind("owner/repo-preserve")
    .fetch_one(&pool)
    .await
    .unwrap();
    let stored_json: serde_json::Value = serde_json::from_str(stored.as_deref().unwrap()).unwrap();
    assert_eq!(stored_json, valid_override);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_agent_invalid_merge_preserves_existing_override() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let valid_override = json!({
        "timeouts": {
            "in_progress": {
                "duration": "4h",
                "clock": "started_at"
            }
        }
    });
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt, pipeline_config)
         VALUES ($1, $1, '111', '222', $2::jsonb)",
    )
    .bind("agent-preserve")
    .bind(valid_override.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/agent/agent-preserve")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"timeouts":{"nonexistent_state":{"duration":"1h","clock":"no_such_clock"}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let stored: Option<String> = sqlx::query_scalar(
        "SELECT pipeline_config::text AS pipeline_config FROM agents WHERE id = $1",
    )
    .bind("agent-preserve")
    .fetch_one(&pool)
    .await
    .unwrap();
    let stored_json: serde_json::Value = serde_json::from_str(stored.as_deref().unwrap()).unwrap();
    assert_eq!(stored_json, valid_override);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_stages_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO github_repos (id, display_name)
         VALUES ($1, $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("owner/pg-pipeline-stages")
    .bind("PG Pipeline Stages")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    // #1097: pipeline_stages is now file-canonical (materialized from
    // policies/default-pipeline.yaml), so PUT/DELETE must be rejected with
    // HTTP 405. The test still asserts that the table is PG-only (no sqlite
    // mirror writes happen) and that GET still works as before.
    let put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/stages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "repo":"owner/pg-pipeline-stages",
                        "stages":[
                            {"stage_name":"Build","stage_order":3000000000,"entry_skill":"build","timeout_minutes":3000000002,"max_retries":3000000003},
                            {"stage_name":"Review","stage_order":3000000001,"entry_skill":"review","parallel_with":"lint","timeout_minutes":3000000004}
                        ]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let put_status = put_response.status();
    let put_body = axum::body::to_bytes(put_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        put_status,
        StatusCode::METHOD_NOT_ALLOWED,
        "pipeline stages PUT must be rejected as file-canonical; body={}",
        String::from_utf8_lossy(&put_body)
    );
    let put_json: serde_json::Value = serde_json::from_slice(&put_body).unwrap();
    assert_eq!(put_json["table"], "pipeline_stages");
    assert_eq!(put_json["source_of_truth"], "file-canonical");
    assert!(
        put_json["error"]
            .as_str()
            .unwrap_or("")
            .contains("file-canonical"),
        "expected file-canonical error message, got: {}",
        put_json["error"]
    );

    // No rows should have been written by the rejected PUT. The PG table
    // may still contain rows materialized from policies/default-pipeline.yaml
    // for *other* repos, but nothing for this test's repo.
    let pg_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM pipeline_stages WHERE repo_id = $1")
            .bind("owner/pg-pipeline-stages")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(
        pg_count, 0,
        "rejected PUT must not insert rows for the test repo"
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM pipeline_stages WHERE repo_id = 'owner/pg-pipeline-stages'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    // GET still works; since the rejected PUT wrote nothing, the list is
    // empty for this repo.
    let get_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/stages?repo=owner/pg-pipeline-stages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
    let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json["stages"].as_array().unwrap().len(),
        0,
        "GET must return empty list for a repo with no materialized stages"
    );

    // DELETE must also be rejected as file-canonical.
    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/pipeline/stages?repo=owner/pg-pipeline-stages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        delete_response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "pipeline stages DELETE must be rejected as file-canonical"
    );
    let delete_body = axum::body::to_bytes(delete_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let delete_json: serde_json::Value = serde_json::from_slice(&delete_body).unwrap();
    assert_eq!(delete_json["table"], "pipeline_stages");
    assert_eq!(delete_json["source_of_truth"], "file-canonical");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_card_views_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO github_repos (id, display_name)
         VALUES ($1, $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("owner/pg-pipeline-card")
    .bind("PG Pipeline Card")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, repo_id, title, status, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-pg-pipeline")
    .bind("owner/pg-pipeline-card")
    .bind("PG Pipeline Card")
    .bind("in_progress")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, entry_skill)
         VALUES ($1, $2, $3, $4), ($1, $5, $6, $7)",
    )
    .bind("owner/pg-pipeline-card")
    .bind("Triage")
    .bind(1_i64)
    .bind("triage")
    .bind("Implementation")
    .bind(2_i64)
    .bind("implementation")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES
            ($1, $2, $3, $4, $5, NOW() - INTERVAL '2 seconds', NOW() - INTERVAL '2 seconds'),
            ($6, $2, $7, $8, $9, NOW() - INTERVAL '1 seconds', NOW() - INTERVAL '1 seconds')",
    )
    .bind("dispatch-pg-pipeline-triage")
    .bind("card-pg-pipeline")
    .bind("triage")
    .bind("completed")
    .bind("Triage")
    .bind("dispatch-pg-pipeline-impl")
    .bind("implementation")
    .bind("running")
    .bind("Implementation")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let pipeline_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/cards/card-pg-pipeline")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let pipeline_status = pipeline_response.status();
    let pipeline_body = axum::body::to_bytes(pipeline_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        pipeline_status,
        StatusCode::OK,
        "pipeline card body={}",
        String::from_utf8_lossy(&pipeline_body)
    );
    let pipeline_json: serde_json::Value = serde_json::from_slice(&pipeline_body).unwrap();
    assert_eq!(pipeline_json["stages"].as_array().unwrap().len(), 2);
    assert_eq!(pipeline_json["history"].as_array().unwrap().len(), 2);
    assert_eq!(
        pipeline_json["current_stage"]["stage_name"],
        "Implementation"
    );

    let history_response = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/cards/card-pg-pipeline/history")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(history_response.status(), StatusCode::OK);
    let history_body = axum::body::to_bytes(history_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let history_json: serde_json::Value = serde_json::from_slice(&history_body).unwrap();
    assert_eq!(history_json["history"].as_array().unwrap().len(), 2);
    assert_eq!(
        history_json["history"][1]["dispatch_type"],
        "implementation"
    );

    let sqlite_dispatch_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-pg-pipeline'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_dispatch_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_only_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO github_repos (id, display_name)
         VALUES ($1, $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("owner/pg-pipeline-config")
    .bind("PG Pipeline Config")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-pipeline-config")
    .bind("PG Pipeline Agent")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let repo_get_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repo_get_response.status(), StatusCode::OK);
    let repo_get_body = axum::body::to_bytes(repo_get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let repo_get_json: serde_json::Value = serde_json::from_slice(&repo_get_body).unwrap();
    assert!(repo_get_json["pipeline_config"].is_null());

    let repo_put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/pg-pipeline-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"hooks":{"review":{"on_enter":["PgReviewHook"],"on_exit":[]}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repo_put_response.status(), StatusCode::OK);

    let agent_put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/agent/agent-pg-pipeline-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"timeouts":{"in_progress":{"duration":"4h","clock":"started_at"}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(agent_put_response.status(), StatusCode::OK);

    let repo_after_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let repo_after_body = axum::body::to_bytes(repo_after_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let repo_after_json: serde_json::Value = serde_json::from_slice(&repo_after_body).unwrap();
    assert_eq!(
        repo_after_json["pipeline_config"]["hooks"]["review"]["on_enter"][0],
        "PgReviewHook"
    );

    let agent_after_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/agent/agent-pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let agent_after_body = axum::body::to_bytes(agent_after_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let agent_after_json: serde_json::Value = serde_json::from_slice(&agent_after_body).unwrap();
    assert_eq!(
        agent_after_json["pipeline_config"]["timeouts"]["in_progress"]["duration"],
        "4h"
    );

    let effective_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/effective?repo=owner/pg-pipeline-config&agent_id=agent-pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(effective_response.status(), StatusCode::OK);
    let effective_body = axum::body::to_bytes(effective_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let effective_json: serde_json::Value = serde_json::from_slice(&effective_body).unwrap();
    assert_eq!(effective_json["layers"]["repo"], true);
    assert_eq!(effective_json["layers"]["agent"], true);
    assert_eq!(
        effective_json["pipeline"]["hooks"]["review"]["on_enter"][0],
        "PgReviewHook"
    );

    let graph_response = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/graph?repo=owner/pg-pipeline-config&agent_id=agent-pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(graph_response.status(), StatusCode::OK);
    let graph_body = axum::body::to_bytes(graph_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let graph_json: serde_json::Value = serde_json::from_slice(&graph_body).unwrap();
    assert!(!graph_json["nodes"].as_array().unwrap().is_empty());
    assert!(!graph_json["edges"].as_array().unwrap().is_empty());

    let pg_report_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM kv_meta WHERE key = 'pipeline_override_health_report'",
    )
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(pg_report_count, 1);

    let sqlite_repo_override_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM github_repos WHERE pipeline_config IS NOT NULL AND id = 'owner/pg-pipeline-config'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_repo_override_count, 0,
        "sqlite mirror should stay empty"
    );

    let sqlite_report_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'pipeline_override_health_report'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_report_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn v1_routes_pg_surface_dashboard_contract() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("repo-v1")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, provider, status, xp, avatar_emoji, discord_channel_id, discord_channel_alt
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9
         )",
    )
    .bind("agent-v1")
    .bind("V1 Agent")
    .bind("브이원 에이전트")
    .bind("claude")
    .bind("working")
    .bind(60_i64)
    .bind("🤖")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("live-skill")
    .bind("Live Skill")
    .bind("Live skill description")
    .bind("/tmp/live-skill/SKILL.md")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("live-skill")
    .bind("agent-v1")
    .bind("session-v1")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-v1-current")
    .bind("repo-v1")
    .bind("Current V1 Card")
    .bind("in_progress")
    .bind("high")
    .bind("agent-v1")
    .bind(791_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-v1-review")
    .bind("repo-v1")
    .bind("Review Queue Card")
    .bind("review")
    .bind("medium")
    .bind("agent-v1")
    .bind(792_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-current")
    .bind("card-v1-current")
    .bind("agent-v1")
    .bind("implementation")
    .bind("dispatched")
    .bind("Current dispatch")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
        .bind("dispatch-current")
        .bind("card-v1-current")
        .execute(&pg_pool)
        .await
        .unwrap();

    for index in 0..5_i64 {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6,
                NOW() - ($7::BIGINT || ' hours')::INTERVAL,
                NOW() - ($7::BIGINT || ' hours')::INTERVAL
             )",
        )
        .bind(format!("dispatch-completed-{index}"))
        .bind("card-v1-review")
        .bind("agent-v1")
        .bind("implementation")
        .bind("completed")
        .bind(format!("Completed dispatch {index}"))
        .bind(index + 1)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, active_dispatch_id, session_info, tokens,
            last_heartbeat, thread_channel_id, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), $8, NOW()
         )",
    )
    .bind("host:session-v1")
    .bind("agent-v1")
    .bind("claude")
    .bind("turn_active")
    .bind("dispatch-current")
    .bind("v1 session")
    .bind(321_i64)
    .bind("222000000000001")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result, created_at)
         VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind("card-v1-current")
    .bind("requested")
    .bind("in_progress")
    .bind("dispatch")
    .bind("ok")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor, timestamp)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("provider")
    .bind("claude")
    .bind("provider_restart_pending")
    .bind("system")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-v1")
    .bind("repo-v1")
    .bind("agent-v1")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("entry-v1")
    .bind("run-v1")
    .bind("card-v1-current")
    .bind("agent-v1")
    .bind("pending")
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            None,
            pg_pool.clone(),
        ),
    );

    let overview = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/overview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(overview.status(), StatusCode::OK);
    assert_eq!(
        overview.headers().get("cache-control").unwrap(),
        "max-age=30"
    );
    let overview_body = axum::body::to_bytes(overview.into_body(), usize::MAX)
        .await
        .unwrap();
    let overview_json: serde_json::Value = serde_json::from_slice(&overview_body).unwrap();
    assert_eq!(overview_json["session_count"], json!(1));
    assert_eq!(overview_json["metrics"]["agents"]["total"], json!(1));
    assert_eq!(overview_json["metrics"]["kanban"]["review_queue"], json!(1));
    assert!(overview_json["spark_14d"].as_array().is_some());

    let agents = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(agents.status(), StatusCode::OK);
    assert_eq!(agents.headers().get("cache-control").unwrap(), "max-age=10");
    let agents_body = axum::body::to_bytes(agents.into_body(), usize::MAX)
        .await
        .unwrap();
    let agents_json: serde_json::Value = serde_json::from_slice(&agents_body).unwrap();
    assert_eq!(
        agents_json["agents"][0]["current_task"]["dispatch_id"],
        json!("dispatch-current")
    );
    assert_eq!(
        agents_json["agents"][0]["skills_7d"][0]["id"],
        json!("live-skill")
    );

    let tokens = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/tokens?range=7d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tokens.status(), StatusCode::OK);
    assert_eq!(tokens.headers().get("cache-control").unwrap(), "max-age=60");
    let tokens_body = axum::body::to_bytes(tokens.into_body(), usize::MAX)
        .await
        .unwrap();
    let tokens_json: serde_json::Value = serde_json::from_slice(&tokens_body).unwrap();
    assert!(tokens_json["summary"]["total_cost"].is_string());
    assert!(tokens_json["daily"].is_array());

    let kanban = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/kanban")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(kanban.status(), StatusCode::OK);
    assert_eq!(kanban.headers().get("cache-control").unwrap(), "max-age=5");
    let kanban_body = axum::body::to_bytes(kanban.into_body(), usize::MAX)
        .await
        .unwrap();
    let kanban_json: serde_json::Value = serde_json::from_slice(&kanban_body).unwrap();
    assert_eq!(kanban_json["auto_queue"]["run"]["id"], json!("run-v1"));
    assert!(kanban_json.get("wip_limit").is_some());

    let ops = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/ops/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ops.status(), StatusCode::OK);
    assert_eq!(ops.headers().get("cache-control").unwrap(), "max-age=5");
    let ops_body = axum::body::to_bytes(ops.into_body(), usize::MAX)
        .await
        .unwrap();
    let ops_json: serde_json::Value = serde_json::from_slice(&ops_body).unwrap();
    assert!(ops_json["bottlenecks"].is_array());

    let activity = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/activity?limit=8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(activity.status(), StatusCode::OK);
    let activity_body = axum::body::to_bytes(activity.into_body(), usize::MAX)
        .await
        .unwrap();
    let activity_json: serde_json::Value = serde_json::from_slice(&activity_body).unwrap();
    let kinds = activity_json["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert!(kinds.contains("dispatch"));
    assert!(kinds.contains("kanban_transition"));
    assert!(kinds.contains("provider_event"));
    assert!(activity_json["next_cursor"].is_string() || activity_json["next_cursor"].is_null());

    let achievements = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/achievements")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(achievements.status(), StatusCode::OK);
    assert_eq!(
        achievements.headers().get("cache-control").unwrap(),
        "max-age=300"
    );
    let achievements_body = axum::body::to_bytes(achievements.into_body(), usize::MAX)
        .await
        .unwrap();
    let achievements_json: serde_json::Value = serde_json::from_slice(&achievements_body).unwrap();
    assert_eq!(
        achievements_json["achievements"][0]["rarity"],
        json!("common")
    );
    assert!(achievements_json["achievements"][0]["progress"].is_object());
    assert_eq!(
        achievements_json["daily_missions"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let settings_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(settings_get.status(), StatusCode::OK);
    assert_eq!(
        settings_get.headers().get("cache-control").unwrap(),
        "no-store"
    );
    let settings_get_body = axum::body::to_bytes(settings_get.into_body(), usize::MAX)
        .await
        .unwrap();
    let settings_get_json: serde_json::Value = serde_json::from_slice(&settings_get_body).unwrap();
    assert!(settings_get_json["entries"].as_array().is_some());

    let settings_patch = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/settings/merge_strategy")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"value":"rebase"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(settings_patch.status(), StatusCode::OK);
    let settings_patch_body = axum::body::to_bytes(settings_patch.into_body(), usize::MAX)
        .await
        .unwrap();
    let settings_patch_json: serde_json::Value =
        serde_json::from_slice(&settings_patch_body).unwrap();
    assert_eq!(settings_patch_json["key"], json!("merge_strategy"));
    assert_eq!(settings_patch_json["value"], json!("rebase"));
    assert_eq!(settings_patch_json["live_override"]["active"], json!(true));

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn v1_stream_pg_emits_snapshot_and_replays_shared_bus_events() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("repo-v1-stream")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, status, xp, avatar_emoji, discord_channel_id, discord_channel_alt
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8
         )",
    )
    .bind("agent-v1-stream")
    .bind("V1 Stream Agent")
    .bind("claude")
    .bind("working")
    .bind(60_i64)
    .bind("🤖")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-v1-stream")
    .bind("repo-v1-stream")
    .bind("Stream Card")
    .bind("in_progress")
    .bind("high")
    .bind("agent-v1-stream")
    .bind(792_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-v1-stream")
    .bind("card-v1-stream")
    .bind("agent-v1-stream")
    .bind("implementation")
    .bind("dispatched")
    .bind("Stream dispatch")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
        .bind("dispatch-v1-stream")
        .bind("card-v1-stream")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, active_dispatch_id, session_info, tokens,
            last_heartbeat, thread_channel_id, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), $8, NOW()
         )",
    )
    .bind("host:session-v1-stream")
    .bind("agent-v1-stream")
    .bind("claude")
    .bind("turn_active")
    .bind("dispatch-v1-stream")
    .bind("v1 stream session")
    .bind(321_i64)
    .bind("222000000000002")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result, created_at)
         VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind("card-v1-stream")
    .bind("requested")
    .bind("in_progress")
    .bind("dispatch")
    .bind("ok")
    .execute(&pg_pool)
    .await
    .unwrap();

    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    let app = axum::Router::new().nest(
        "/api",
        api_router_with_pg_for_tests(
            db,
            engine,
            crate::config::Config::default(),
            tx.clone(),
            buf,
            None,
            Some(pg_pool.clone()),
        ),
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");

    let mut body = response.into_body();
    let snapshot = read_sse_body_until(
        &mut body,
        &[
            "event: agent.status",
            "event: token.tick",
            "event: achievement.unlocked",
            "event: kanban.transition",
            "event: ops.health",
        ],
    )
    .await;

    assert!(snapshot.contains("\"agent_id\":\"agent-v1-stream\""));
    assert!(snapshot.contains("\"delta_tokens\":321"));
    assert!(snapshot.contains("\"achievement_id\""));
    assert!(snapshot.contains("\"from\":\"requested\""));
    assert!(snapshot.contains("\"status\":\"ok\""));
    drop(body);

    crate::server::ws::emit_event(
        &tx,
        "agent.status",
        json!({
            "agent_id": "agent-v1-stream",
            "status": "idle",
            "task": null,
        }),
    );
    crate::server::ws::emit_event(
        &tx,
        "token.tick",
        json!({
            "agent_id": "agent-v1-stream",
            "delta_tokens": 7,
            "delta_cost_usd": "0.12",
        }),
    );

    let replay_response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/stream")
                .header("Last-Event-ID", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay_response.status(), StatusCode::OK);

    let mut replay_body = replay_response.into_body();
    let replay = read_sse_body_until(
        &mut replay_body,
        &["id: 2", "event: token.tick", "\"delta_tokens\":7"],
    )
    .await;
    assert!(replay.contains("\"delta_cost_usd\":\"0.12\""));

    pg_pool.close().await;
    pg_db.drop().await;
}
