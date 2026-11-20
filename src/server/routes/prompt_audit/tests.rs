use std::io::{Read, Seek};
use std::sync::Arc;

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
use crate::services::memory::ExternalMemoryBackendState;

use super::*;

const CREDENTIALS: &str = concat!(
    "Ordinary instructions.\n",
    "--key fixture-command-value\n",
    "Authorization: Bearer fixture-bearer-value\n",
    "SERVICE_PASSWORD=\"fixture password with spaces\"\n",
    "{\"api_key\":\"fixture-key-with-escaped-\\\"-quote\"}\n",
    "postgres://audit:fixture-uri-password@localhost/db\n",
    "ghp_fixtureStandaloneToken\n",
    "-----BEGIN PRIVATE KEY-----\nfixture-private-material\n-----END PRIVATE KEY-----\n",
);

const PEM_CASES: &[(&str, &str, &[&str])] = &[
    (
        "assigned PEM",
        "GITHUB_PRIVATE_KEY=-----BEGIN RSA PRIVATE KEY-----\nFAKE_PRIVATE_MATERIAL_X7Z8\nfixture-last-pem-row\n-----END RSA PRIVATE KEY-----",
        &[
            "FAKE_PRIVATE_MATERIAL_X7Z8",
            "fixture-last-pem-row",
            "END RSA PRIVATE KEY",
        ],
    ),
    (
        "truncated PEM",
        "private_key=-----BEGIN OPENSSH PRIVATE KEY-----\nfixture-truncated-pem\nfixture-truncated-tail",
        &["fixture-truncated-pem", "fixture-truncated-tail"],
    ),
];

const SYNTAX_CASES: &[(&str, &str, &[&str])] = &[
    (
        "cookie",
        r#"Cookie: sid="fixture-cookie-value""#,
        &["fixture-cookie-value"],
    ),
    (
        "digest",
        r#"Authorization: Digest username="u", realm="r", nonce="n", response="fixture-response""#,
        &["fixture-response"],
    ),
    (
        "bearer",
        "Authorization: Bearer fixture-review-bearer",
        &["fixture-review-bearer"],
    ),
    (
        "key header",
        r#"X-Api-Key: "fixture-review-header-key""#,
        &["fixture-review-header-key"],
    ),
    (
        "curl cookie",
        "curl --cookie sid=fixture-curl-cookie https://example.test",
        &["fixture-curl-cookie"],
    ),
    (
        "curl short cookie",
        "curl -b'sid=fixture-short-cookie' https://example.test",
        &["fixture-short-cookie"],
    ),
    (
        "shell folded header",
        "curl -H 'Authorization: Digest response=\"fixture-header-quoted\"\n fixture-header-fold' https://example.test",
        &["fixture-header-quoted", "fixture-header-fold"],
    ),
    (
        "shell long header",
        r#"curl --header='X-Api-Key: '"fixture-shell-key" https://example.test"#,
        &["fixture-shell-key"],
    ),
    (
        "token argument",
        r#"agent --token "fixture-token-one ""fixture-token-two""#,
        &["fixture-token-one", "fixture-token-two"],
    ),
    (
        "password argument",
        r#"tool -p"fixture-short-password fixture-short-password-tail""#,
        &["fixture-short-password", "fixture-short-password-tail"],
    ),
    (
        "yaml empty first line",
        "password:\t\n fixture-yaml-password\n fixture-second-line",
        &["fixture-yaml-password", "fixture-second-line"],
    ),
    (
        "yaml literal",
        "password: |- # note\n  fixture-yaml-literal\n  fixture-yaml-literal-tail",
        &["fixture-yaml-literal", "fixture-yaml-literal-tail"],
    ),
    (
        "yaml folded",
        "  password: >2+\r\n    fixture-yaml-folded\r\n    fixture-yaml-tail",
        &["fixture-yaml-folded", "fixture-yaml-tail"],
    ),
    (
        "basic",
        "Authorization: Basic Zml4dHVyZS1iYXNpYw==",
        &["Zml4dHVyZS1iYXNpYw=="],
    ),
    (
        "plain query",
        "https://example.test/?api_key=fixture-plain-query",
        &["fixture-plain-query"],
    ),
    (
        "base64 field",
        r#"{"api_key_b64":"Zml4dHVyZS1zZWNyZXQ="}"#,
        &["Zml4dHVyZS1zZWNyZXQ="],
    ),
    (
        "encoded query",
        "https://example.test/?a%70i_%6Bey=fixture-encoded-key",
        &["fixture-encoded-key"],
    ),
];

fn assert_review_cases(body: &str, cases: &[(&str, &str, &[&str])]) {
    for (name, _, fragments) in cases {
        assert!(
            fragments.iter().all(|fragment| !body.contains(fragment)),
            "{name}: credential fragment leaked"
        );
    }
}

#[test]
fn prompt_audit_p1_pem_masks_complete_and_truncated_blocks_once() {
    let mut failed = Vec::new();
    for (name, input, fragments) in PEM_CASES {
        let split = input.find('\n').unwrap() + 1;
        let spans = redaction::ranges(input);
        if spans.len() != 1 {
            failed.push(*name);
            continue;
        }
        let (left, first) = redaction::mask_range(input, 0..split, &spans);
        let (right, second) = redaction::mask_range(input, split..input.len(), &spans);
        if (first, second) != (1, 0)
            || fragments
                .iter()
                .any(|fragment| left.contains(fragment) || right.contains(fragment))
        {
            failed.push(*name);
        }
    }
    assert!(failed.is_empty(), "PEM masking/count failures: {failed:?}");
}

#[test]
fn prompt_audit_p1_syntax_masks_review_fixtures_and_counts() {
    let failed: Vec<_> = SYNTAX_CASES
        .iter()
        .filter_map(|(name, input, fragments)| {
            let (text, count) = redaction::mask(input);
            (count != 1 || fragments.iter().any(|fragment| text.contains(fragment))).then_some(name)
        })
        .collect();
    assert!(
        failed.is_empty(),
        "credential masking/count failures: {failed:?}"
    );
}

fn assert_masked(body: &str) {
    for (index, secret) in [
        "fixture-command-value",
        "fixture-bearer-value",
        "fixture password with spaces",
        "fixture-key-with-escaped",
        "fixture-uri-password",
        "ghp_fixtureStandaloneToken",
        "fixture-private-material",
    ]
    .iter()
    .enumerate()
    {
        assert!(!body.contains(secret), "credential fixture {index} leaked");
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
fn prompt_audit_credential_patterns_are_masked_and_counted() {
    let (text, count) = redaction::mask(CREDENTIALS);
    assert_masked(&text);
    assert!(text.starts_with("Ordinary instructions."));
    assert_eq!(count, 7);
    assert_eq!(text.matches("***").count(), 7);
}

#[test]
fn prompt_audit_counts_a_registered_secret_once_across_section_boundaries() {
    let fixture = "opaque fixture secret across sections";
    crate::utils::redact::register_known_secret(fixture);
    let text = format!("Public prefix: {fixture}. Public suffix.");
    let split = text.find("secret across").unwrap();
    let ranges = redaction::ranges(&text);
    let (left, first_count) = redaction::mask_range(&text, 0..split, &ranges);
    let (right, second_count) = redaction::mask_range(&text, split..text.len(), &ranges);
    assert_eq!(left, "Public prefix: ***");
    assert_eq!(right, "***. Public suffix.");
    assert_eq!((first_count, second_count), (1, 0));
}

#[test]
fn prompt_audit_disconnected_memento_keeps_structured_session_copy() {
    let saved = snapshot();
    let canon = audit::CanonPrompt {
        name: "shared_agent_rules",
        path: "agents/_shared.prompt.md",
        content: "Summary\nReuse canon guidance.".into(),
        modified_at: None,
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

#[tokio::test]
async fn prompt_audit_docs_publish_both_response_contracts() {
    use super::super::docs::api_docs_group_category;
    let (status, Json(document)) =
        api_docs_group_category(Path(("agents".to_owned(), "agents".to_owned()))).await;
    assert_eq!(status, StatusCode::OK);
    assert_review_cases(&document.to_string(), SYNTAX_CASES);
    assert_review_cases(&document.to_string(), PEM_CASES);
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
async fn prompt_audit_http_masks_saved_prompt_and_checks_binding_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    let mut log = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(Arc::new(log.try_clone().unwrap()))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let trace = tracing::subscriber::set_default(subscriber);
    tracing::debug!("audit fixture started");
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('audit-fixture','Fixture','claude','400642'), ('canon-fixture','Canon','claude','400643')")
        .execute(&pool).await.unwrap();
    let fixtures = PEM_CASES[..1]
        .iter()
        .chain(SYNTAX_CASES)
        .map(|(_, input, _)| *input)
        .collect::<Vec<_>>()
        .join("\n\nPublic fixture boundary.\n\n");
    let mut saved = snapshot();
    saved.system_prompt.push_str("\n\n");
    saved.system_prompt.push_str(&fixtures);
    saved.anchors.as_mut().unwrap()[1]
        .content
        .push_str(&format!("\n\n{fixtures}\n\n{}", PEM_CASES[1].1));
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
        let (status, body) = get(&router, path, authenticated).await;
        assert_eq!(status, expected);
        let body = std::str::from_utf8(&body).unwrap();
        assert_review_cases(body, SYNTAX_CASES);
        assert_review_cases(body, PEM_CASES);
    }
    let (status, bytes) = get(&router, uri, true).await;
    assert_eq!(status, StatusCode::OK);
    let serialized = std::str::from_utf8(&bytes).unwrap();
    assert_masked(serialized);
    assert_review_cases(serialized, SYNTAX_CASES);
    assert_review_cases(serialized, PEM_CASES);
    assert!(!serialized.contains("fixture-anchor-password"));
    assert!(!serialized.contains("different agent private text"));
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["origin"], "latest_saved");
    assert_eq!(response["session_id"], "fixture-session");
    assert_eq!(response["masked_count"], 9 + SYNTAX_CASES.len());
    let sources: Vec<_> = response["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|section| section["source"].as_str().unwrap())
        .collect();
    assert!(
        sources.contains(&"runtime")
            && sources.contains(&"canon_file")
            && sources.contains(&"memento")
            && sources.contains(&"conditional")
    );

    let anchor_uri = "/api/agents/audit-fixture/anchors";
    let (status, bytes) = get(&router, anchor_uri, true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !std::str::from_utf8(&bytes)
            .unwrap()
            .contains("fixture-anchor-password")
    );
    let body = std::str::from_utf8(&bytes).unwrap();
    assert_review_cases(body, SYNTAX_CASES);
    assert_review_cases(body, PEM_CASES);
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["source"], "latest_saved");
    assert_eq!(response["memento"]["status"], "unconfigured");
    assert_eq!(response["anchors"][0]["id"], "frag-fixture");
    assert_eq!(
        response["anchors"][1]["masked_count"],
        3 + SYNTAX_CASES.len()
    );
    assert_eq!(response["anchors"][0]["overlap_candidates"], json!([]));

    let (status, bytes) = get(&router, "/api/agents/canon-fixture/prompt-audit", true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !std::str::from_utf8(&bytes)
            .unwrap()
            .contains("fixture-canon-password")
    );
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["origin"], "canon_reassembled");
    assert!(response["canon_modified_at"].is_string());
    assert!(response["session_id"].is_null());
    assert_eq!(response["masked_count"], 1);
    assert_eq!(response["sections"][0]["text"], "Canon fixture.\n***");

    let live_prompt = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(live_prompt.path(), "Live canon.\nReuse canon guidance.").unwrap();
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(42);
    live_prompt
        .as_file()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    config.shared_prompt = Some(live_prompt.path().display().to_string());
    crate::config_live_reload::install(config);
    let (status, bytes) = get(&router, "/api/agents/canon-fixture/prompt-audit", true).await;
    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        response["sections"][0]["text"],
        "Live canon.\nReuse canon guidance."
    );
    assert_eq!(response["canon_modified_at"], "1970-01-01T00:00:42Z");
    let (status, bytes) = get(&router, anchor_uri, true).await;
    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        response["anchors"][0]["overlap_candidates"],
        json!([{"source":"canon_file","path":"agents/_shared.prompt.md"}])
    );

    sqlx::query("UPDATE provider_session_system_prompts SET anchors = $1 WHERE channel_id = $2")
        .bind(sqlx::types::Json(&fixtures))
        .bind(&saved.channel_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = get(&router, anchor_uri, true).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let body = std::str::from_utf8(&body).unwrap();
    assert_review_cases(body, SYNTAX_CASES);
    assert_review_cases(body, PEM_CASES);

    drop(trace);
    log.rewind().unwrap();
    let mut captured = String::new();
    log.read_to_string(&mut captured).unwrap();
    assert!(captured.contains("audit fixture started"));
    assert_review_cases(&captured, SYNTAX_CASES);
    assert_review_cases(&captured, PEM_CASES);

    drop(router);
    pool.close().await;
    db.drop().await;
}
