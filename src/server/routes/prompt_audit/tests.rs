use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, header},
};
use serde_json::{Value, json, to_value};
use tower::ServiceExt;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::prompt_manifests::{
    PromptContentVisibility, PromptManifestBuilder, PromptManifestLayer,
};
use crate::domain::memory::AnchorRecord;
use crate::services::discord::prompt_builder::system_prompt_audit::redact;
use crate::services::memory::ExternalMemoryBackendState;

use super::*;

const CREDENTIALS: &str =
    "Ordinary instructions.\nAPI_TOKEN=fixture-token-value --key fixture-command-value\n";

// Both APIs show what Discord /system-prompt's redact() shows for the same saved row.
fn assert_same_redaction(body: &[u8], saved: &SessionPrompt) {
    let response: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(response["session_id"], saved.provider_session_id.as_str());
    if let Some(sections) = response["sections"].as_array() {
        let (expected, count) = redact(&saved.system_prompt);
        assert_eq!(response["masked_count"], count);
        assert!(!sections.is_empty());
        for section in sections {
            assert!(expected.contains(section["text"].as_str().unwrap()));
        }
    } else {
        let records = &saved.anchors.as_ref().unwrap().0;
        let anchors = response["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), records.len());
        for (anchor, record) in anchors.iter().zip(records) {
            let (text, count) = redact(&record.content);
            assert_eq!(
                (&anchor["text"], &anchor["masked_count"]),
                (&json!(text), &json!(count))
            );
        }
    }
}

fn snapshot() -> SessionPrompt {
    SessionPrompt {
        provider: "claude".into(),
        channel_id: "400642".into(),
        provider_session_id: "fixture-session".into(),
        source_turn_id: "fixture-start-turn".into(),
        started_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        system_prompt: format!(
            "Role guidance.\n\n[Review Rules]\nTest the result.\n\n{CREDENTIALS}\n[Memento anchor context]\nPolicy\n\n[ANCHOR MEMORY]\n- Reuse canon guidance.\n- password=fixture-anchor-password\n\n[End Memento anchor context]",
        ),
        anchor_count: 2,
        anchors: Some(sqlx::types::Json(vec![
            AnchorRecord {
                id: Some("frag-fixture".into()),
                kind: Some("procedure".into()),
                scope: Some("example-project".into()),
                limit: Some(300),
                content: "Reuse canon guidance.".into(),
            },
            AnchorRecord {
                id: Some("frag-fixture-second".into()),
                kind: Some("decision".into()),
                scope: None,
                limit: None,
                content: "password=fixture-anchor-password".into(),
            },
        ])),
    }
}

#[test]
fn prompt_audit_disconnected_memento_keeps_structured_session_copy() {
    let saved = snapshot();
    let canon = audit::CanonPrompt {
        name: "shared_agent_rules",
        path: "agents/_shared.prompt.md",
        content: "Summary\nReuse canon guidance.".into(),
    };
    let health = ExternalMemoryBackendState {
        configured: true,
        active: false,
        consecutive_failures: 3,
        last_checked_at: Some(std::time::UNIX_EPOCH),
        last_error: Some("fixture-only connection error".into()),
        ..Default::default()
    };
    let response = to_value(anchors::response(
        "audit-fixture",
        &ProviderKind::Claude,
        Some("fixture-session".into()),
        Some(&saved),
        Some(health),
        &[canon],
    ))
    .unwrap();
    assert_eq!(response["source"], "current_session");
    assert_eq!(
        response["memento"],
        json!({"status":"disconnected","checked_at":"1970-01-01T00:00:00Z"})
    );
    assert_eq!(response["anchor_count"], 2);
    assert_eq!(
        response["anchors"][0],
        json!({
            "id":"frag-fixture","type":"procedure","scope":"example-project","limit":300,
            "chars":21,"text":"Reuse canon guidance.","masked_count":0,
            "overlap_candidates":[{"source":"canon_file","path":"agents/_shared.prompt.md"}],
            "receiving_agents":["audit-fixture"]
        })
    );
    assert_eq!(response["anchors"][1]["masked_count"], 1);
    assert!(!response.to_string().contains("fixture-anchor-password"));
}

#[test]
fn prompt_audit_legacy_session_copy_preserves_multiline_anchor_without_guessing_metadata() {
    let mut saved = snapshot();
    saved.anchors = None;
    saved.system_prompt = saved.system_prompt.replace(
        "Reuse canon guidance.",
        "Reuse canon guidance.\n[Memento anchor context]\nThe second line.",
    );
    let response = to_value(anchors::response(
        "audit-fixture",
        &ProviderKind::Claude,
        None,
        Some(&saved),
        None,
        &[],
    ))
    .unwrap();
    assert_eq!(response["source"], "latest_saved");
    assert_eq!(
        response["anchors"][0]["text"],
        "Reuse canon guidance.\n[Memento anchor context]\nThe second line."
    );
    assert!(response["anchors"][0]["id"].is_null());
    assert_eq!(response["anchors"][1]["masked_count"], 1);
}

#[test]
fn prompt_audit_anchor_context_keeps_the_last_closed_legacy_block() {
    use crate::domain::memory::{ANCHOR_CONTEXT_END as END, ANCHOR_CONTEXT_START as START};
    let last = format!("{START}[ANCHOR MEMORY]\n- Injected item.{END}");
    let prompt = format!("{START}[ANCHOR MEMORY]\n- Example item.{END}\nPlain text.\n{last}");
    let range = projection::anchor_context(&prompt).unwrap();
    assert_eq!(&prompt[range], last);
}

#[tokio::test]
async fn prompt_audit_docs_publish_both_response_contracts() {
    use super::super::docs::api_docs_group_category;
    let (status, Json(document)) =
        api_docs_group_category(Path(("agents".to_owned(), "agents".to_owned()))).await;
    assert_eq!(status, StatusCode::OK);
    let endpoints = document["endpoints"].as_array().unwrap();
    for (path, field) in [
        ("/api/agents/{id}/prompt-audit", "sections"),
        ("/api/agents/{id}/anchors", "memento"),
    ] {
        let endpoint = endpoints
            .iter()
            .find(|endpoint| endpoint["method"] == "GET" && endpoint["path"] == path)
            .unwrap();
        assert!(!endpoint["example"]["response"][field].is_null());
        assert_eq!(endpoint["params"]["provider"]["location"], "query");
    }
}

fn app(pool: sqlx::PgPool, config: crate::config::Config) -> Router {
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast = crate::eventbus::new_broadcast();
    let state = AppState {
        pg_pool: Some(pool),
        engine,
        config: std::sync::Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast.clone()),
        broadcast_tx: broadcast,
        health_registry: None,
        cluster_instance_id: None,
    };
    Router::new().nest(
        "/api",
        super::super::domains::runtime::router(state.clone()).with_state(state),
    )
}

async fn get(app: &Router, uri: &str, authorized: bool) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().uri(uri);
    if authorized {
        request = request.header(header::AUTHORIZATION, "Bearer fixture-dashboard-auth");
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    if response.status() == StatusCode::OK {
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    (
        response.status(),
        to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
}

#[tokio::test]
async fn prompt_audit_http_reuses_system_prompt_redaction_and_checks_binding_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('audit-fixture','Fixture','claude','400642'), ('canon-fixture','Canon','claude','400643')")
        .execute(&pool).await.unwrap();
    let saved = snapshot();
    provider_session_prompts::save(&pool, &saved).await.unwrap();
    let mut wrong = saved.clone();
    wrong.channel_id = "400644".into();
    wrong.started_at += chrono::Duration::hours(1);
    wrong.system_prompt = "different agent private text".into();
    provider_session_prompts::save(&pool, &wrong).await.unwrap();
    let mut manifest = PromptManifestBuilder::new(&saved.source_turn_id, &saved.channel_id);
    for (name, content) in [
        ("role_prompt", "Role guidance."),
        ("review_rules", "[Review Rules]\nTest the result."),
    ] {
        manifest = manifest.layer(PromptManifestLayer::from_content(
            name,
            true,
            None::<String>,
            None::<String>,
            PromptContentVisibility::AdkProvided,
            content,
        ));
    }
    prompt_manifests::save_prompt_manifest(Some(&pool), &manifest.build().unwrap())
        .await
        .unwrap();

    let canon_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        canon_file.path(),
        "Canon fixture.\nPASSWORD=fixture-canon-password",
    )
    .unwrap();
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("fixture-dashboard-auth".into());
    config.shared_prompt = Some(canon_file.path().display().to_string());
    let router = app(pool.clone(), config.clone());
    let uri = "/api/agents/audit-fixture/prompt-audit";
    for (path, authenticated, expected) in [
        (uri, false, StatusCode::UNAUTHORIZED),
        (
            "/api/agents/missing/prompt-audit",
            true,
            StatusCode::NOT_FOUND,
        ),
    ] {
        assert_eq!(get(&router, path, authenticated).await.0, expected);
    }
    let (status, bytes) = get(&router, uri, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_same_redaction(&bytes, &saved);
    let serialized = std::str::from_utf8(&bytes).unwrap();
    assert!(!serialized.contains("fixture-anchor-password"));
    assert!(!serialized.contains("different agent private text"));
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["origin"], "latest_saved");
    let memento = "[Memento anchor context]\nPolicy\n\n[ANCHOR MEMORY]\n- Reuse canon guidance.\n- password=***\n\n[End Memento anchor context]";
    let expected: Vec<_> = [
        ("role_prompt", "canon_file", json!("agents/role.prompt.md"), "Role guidance.", 0),
        ("review_rules", "conditional", Value::Null, "[Review Rules]\nTest the result.", 0),
        ("runtime", "runtime", Value::Null, "\n\nOrdinary instructions.\nAPI_TOKEN=*** --key ***\n\n", 2),
        ("memento_anchors", "memento", Value::Null, memento, 1),
    ]
    .into_iter()
    .map(|(name, source, path, text, masked_count)| {
        let chars = text.chars().count();
        json!({"name": name, "source": source, "path": path, "chars": chars, "text": text, "masked_count": masked_count})
    })
    .collect();
    assert_eq!(response["sections"], json!(expected));

    let anchor_uri = "/api/agents/audit-fixture/anchors";
    let (status, bytes) = get(&router, anchor_uri, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_same_redaction(&bytes, &saved);
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["source"], "latest_saved");
    assert_eq!(response["memento"]["status"], "unconfigured");
    assert_eq!(response["anchors"][0]["id"], "frag-fixture");
    assert_eq!(response["anchors"][0]["overlap_candidates"], json!([]));

    // Without a saved copy there is no other source to fall back on.
    let (status, bytes) = get(&router, "/api/agents/canon-fixture/prompt-audit", true).await;
    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["origin"], "unavailable");
    assert!(response["session_id"].is_null() && response.get("canon_modified_at").is_none());
    assert_eq!(response["masked_count"], 0);
    assert_eq!(response["sections"], json!([]));

    let live_prompt = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(live_prompt.path(), "Live canon.\nReuse canon guidance.").unwrap();
    config.shared_prompt = Some(live_prompt.path().display().to_string());
    crate::config_live_reload::install(config);
    let (status, bytes) = get(&router, anchor_uri, true).await;
    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        response["anchors"][0]["overlap_candidates"],
        json!([{"source":"canon_file","path":"agents/_shared.prompt.md"}])
    );

    let mut latest = snapshot();
    latest.provider_session_id = "latest-fixture-session".into();
    latest.started_at += chrono::Duration::seconds(1);
    latest.system_prompt = latest
        .system_prompt
        .replace("fixture-token-value", "fixture-latest-token");
    latest.anchors.as_mut().unwrap()[1].content = "password=fixture-latest-password".into();
    provider_session_prompts::save(&pool, &latest)
        .await
        .unwrap();
    for path in [uri, anchor_uri] {
        let (status, bytes) = get(&router, path, true).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_same_redaction(&bytes, &latest);
    }

    sqlx::query("UPDATE provider_session_system_prompts SET anchors = $1 WHERE channel_id = $2")
        .bind(sqlx::types::Json(vec!["malformed anchor"]))
        .bind(&saved.channel_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = get(&router, anchor_uri, true).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    drop(router);
    pool.close().await;
    db.drop().await;
}
