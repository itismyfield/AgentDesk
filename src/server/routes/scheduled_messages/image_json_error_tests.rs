use super::*;

const PREFIX: &str = "invalid scheduled message JSON";

async fn post_raw(body: &str) -> (StatusCode, String) {
    send_raw("POST", "/scheduled-messages", body).await
}

/// Drives the real route; extraction fails before the absent pool is touched.
async fn send_raw(method: &str, uri: &str, body: &str) -> (StatusCode, String) {
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
                .method(method)
                .uri(uri)
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
    assert!(
        error.contains("discordMentionUserIds") && !error.contains('가'),
        "{error}"
    );
    // Redacted router details stay short, so the 200-char cap is checked on the helper directly.
    let long = format!(
        "{}: invalid type: string \"{}\", expected {}",
        "가".repeat(150),
        "나".repeat(300),
        "다".repeat(100)
    );
    let detail = redacted_detail(&long).unwrap();
    assert_eq!(detail.chars().count(), 200, "{detail}");
    assert!(detail.ends_with('다') && !detail.contains('나'), "{detail}");
}

#[tokio::test]
async fn create_json_data_errors_redact_client_values() {
    let field =
        r#"{"content":"body","scheduledAt":"2030-01-01T00:00:00Z","discordMentionUserIds":"#;
    for (body, kept) in [
        (
            format!(r#"{field}"c2VjcmV0"}}"#),
            "discordMentionUserIds: invalid type: string",
        ),
        (format!(r#"{field}"x\"c2VjcmV0"}}"#), "expected a sequence"),
        (
            format!(r#"{field}["1",73757265]}}"#),
            "discordMentionUserIds[1]: invalid type: integer",
        ),
        (
            r#""c2VjcmV0""#.to_owned(),
            "expected struct CreateScheduledMessageBody",
        ),
    ] {
        let (status, error) = post_raw(&body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert!(error.starts_with(PREFIX) && error.contains(kept), "{error}");
        assert!(
            !error.contains("c2VjcmV0") && !error.contains("73757265"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn dynamic_json_value_errors_stay_generic() {
    let raw = r#"{"$serde_json::private::RawValue":73757265}"#;
    // A raw value that fails to reparse errors at the parent path, without the token segment.
    let nested = r#"{"$serde_json::private::RawValue":"[1,"}"#;
    let head = r#"{"content":"body","scheduledAt":"2030-01-01T00:00:00Z","providerTargets":"#;
    // Array form fills fields in declaration order: 0 imageAttachment, 1 content, 10 scheduledAt, 17 providerTargets.
    let array = |index: usize, value: &str| {
        let mut items = vec!["null".to_owned(); 20];
        items[1] = r#""body""#.to_owned();
        items[10] = r#""2030-01-01T00:00:00Z""#.to_owned();
        items[index] = value.to_owned();
        format!("[{}]", items.join(","))
    };
    let mut responses = Vec::new();
    for body in [
        format!(r#"{head}{{"`":{raw}}}}}"#),
        format!(r#"{head}{{"field `c2VjcmV0`":{raw}}}}}"#),
        array(17, &format!(r#"{{"field `c2VjcmV0`":{raw}}}"#)),
        array(0, &format!(r#"{{"field `c2VjcmV0`":{raw}}}"#)),
        format!(r#"{head}{{"c2VjcmV0":{nested}}}}}"#),
    ] {
        responses.push(post_raw(&body).await);
    }
    for body in [
        format!(r#"{{"field `c2VjcmV0`":{raw}}}"#),
        format!(r#"{{"c2VjcmV0":{nested}}}"#),
    ] {
        responses.push(send_raw("PATCH", "/scheduled-messages/sm-1", &body).await);
    }
    assert_eq!(
        responses,
        vec![(StatusCode::BAD_REQUEST, PREFIX.to_owned()); 7]
    );
}
