use super::*;

const PREFIX: &str = "invalid scheduled message JSON";

/// Drives the real create route; extraction fails before the absent pool is touched.
async fn post_raw(body: &str) -> (StatusCode, String) {
    use std::sync::Arc;
    use tower::ServiceExt;
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("json-error-token".into());
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = crate::server::routes::AppState {
        pg_pool: None,
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: None,
    };
    let app = crate::server::routes::domains::ops::router(state.clone()).with_state(state);
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/scheduled-messages")
                .header("authorization", "Bearer json-error-token")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .unwrap();
    let body: JsonValue = serde_json::from_slice(&bytes).unwrap();
    (status, body["error"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn create_json_data_errors_name_the_offending_field() {
    for (body, field) in [
        (
            r#"{"content":"body","scheduled_at":"2030-01-01T00:00:00Z"}"#,
            "scheduledAt",
        ),
        (
            r#"{"content":5,"scheduledAt":"2030-01-01T00:00:00Z"}"#,
            "content",
        ),
    ] {
        let (status, error) = post_raw(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert!(
            error.starts_with(PREFIX) && error.contains(field),
            "{error}"
        );
    }
}

#[tokio::test]
async fn create_json_errors_keep_image_and_non_data_rejections_generic() {
    let image = STANDARD.encode(b"\x89PNG\r\n\x1a\nsecret-image-bytes");
    let duplicate = format!(
        r#"{{"content":"body","scheduledAt":"2030-01-01T00:00:00Z","imageAttachment":{{"filename":"a.png","contentType":"image/png","dataBase64":"{image}"}},"imageAttachment":null}}"#
    );
    for body in [duplicate.as_str(), r#"{"content":"#] {
        let (status, error) = post_raw(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error, PREFIX);
    }
    let (_, error) = post_raw(&duplicate).await;
    assert!(!error.contains(&image[..16]), "{error}");
}

#[tokio::test]
async fn create_json_data_error_detail_is_capped_at_200_chars() {
    let body = format!(
        r#"{{"content":"body","scheduledAt":"2030-01-01T00:00:00Z","discordMentionUserIds":"{}"}}"#,
        "가".repeat(300)
    );
    let (status, error) = post_raw(&body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let detail = error
        .strip_prefix(PREFIX)
        .unwrap()
        .strip_prefix(": ")
        .unwrap();
    assert!(detail.contains("discordMentionUserIds"), "{error}");
    assert_eq!(detail.chars().count(), 200, "{error}");
    assert!(detail.ends_with('가'), "{error}");
}
