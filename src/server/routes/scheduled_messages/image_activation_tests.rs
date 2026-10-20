use super::*;
use serde_json::json;

fn upload(data: &[u8]) -> JsonValue {
    json!({"filename":"image.png","contentType":"image/png","dataBase64":STANDARD.encode(data)})
}

#[test]
fn image_upload_validates_filename_mime_magic_and_base64() {
    for (filename, mime, bytes) in [
        ("image.png", "image/png", &b"\x89PNG\r\n\x1a\n"[..]),
        ("image.JPG", "image/jpeg", &b"\xff\xd8\xff"[..]),
        ("image.gif", "image/gif", &b"GIF89a"[..]),
        ("image.webp", "image/webp", &b"RIFF\0\0\0\0WEBP"[..]),
    ] {
        let value =
            json!({"filename":filename,"contentType":mime,"dataBase64":STANDARD.encode(bytes)});
        assert_eq!(decode(Some(&value)).unwrap().unwrap().data, bytes);
    }
    for filename in [
        "",
        "../image.png",
        "a\\image.png",
        "a\".png",
        "a\r.png",
        "a\n.png",
        ".png",
        "png",
        "한글.png",
    ] {
        let mut value = upload(b"\x89PNG\r\n\x1a\n");
        value["filename"] = json!(filename);
        assert!(decode(Some(&value)).is_err(), "invalid filename must fail");
    }
    for (field, value) in [
        ("filename", json!("image.gif")),
        ("contentType", json!("image/jpeg")),
        ("dataBase64", json!(STANDARD.encode(b"GIF89a"))),
    ] {
        let mut input = upload(b"\x89PNG\r\n\x1a\n");
        input[field] = value;
        assert!(
            decode(Some(&input)).is_err(),
            "image magic and MIME mismatch must fail closed"
        );
    }
    for (length, accepted) in [(124, true), (125, false)] {
        let mut value = upload(b"\x89PNG\r\n\x1a\n");
        value["filename"] = json!(format!("{}.png", "a".repeat(length)));
        assert_eq!(decode(Some(&value)).is_ok(), accepted);
    }
    for encoded in ["", "AAAA-___", "aA", "aA==\n", "aB=="] {
        let mut value = upload(b"\x89PNG\r\n\x1a\n");
        value["dataBase64"] = json!(encoded);
        assert!(decode(Some(&value)).is_err());
    }
    let mut extra = upload(b"\x89PNG\r\n\x1a\n");
    extra["unexpected"] = json!(true);
    assert!(decode(Some(&extra)).is_err());
    assert!(decode(Some(&JsonValue::Null)).unwrap().is_none());
}

#[test]
fn image_context_rejects_mixed_targets_and_rendered_utf16_overflow() {
    let context = |providers| Context {
        kind: "push",
        target: Some("7001"),
        providers,
        content: "body",
        mentions: &[],
    };
    assert!(
        validate_context(context(true)).is_err(),
        "mixed image provider targets must be rejected"
    );
    validate_context(context(false)).unwrap();
    let body = "😀".repeat(1000);
    validate_context(Context {
        content: &body,
        ..context(false)
    })
    .unwrap();
    assert!(
        validate_context(Context {
            content: &body,
            mentions: &["123".into()],
            ..context(false)
        })
        .is_err()
    );
    for content in ["", "   "] {
        assert!(
            validate_context(Context {
                content,
                ..context(false)
            })
            .is_err()
        );
    }
    assert!(
        validate_context(Context {
            kind: "agent",
            ..context(false)
        })
        .is_err()
    );
    assert!(
        validate_context(Context {
            target: None,
            ..context(false)
        })
        .is_err()
    );
}

fn state(pool: sqlx::PgPool) -> crate::server::routes::AppState {
    use std::sync::Arc;
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("image-test-token".into());
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    crate::server::routes::AppState {
        pg_pool: Some(pool),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    }
}

async fn request(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: JsonValue,
) -> (StatusCode, JsonValue) {
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", "Bearer image-test-token")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(JsonValue::Null),
    )
}

async fn ready(pool: &sqlx::PgPool) {
    let caps = json!({"scheduled_messages":{"image_consumer_v1":true,"discord_mention_consumer_v1":true},"message_outbox":{"image_consumer_v1":true}});
    for id in ["book", "mini"] {
        sqlx::query("INSERT INTO worker_nodes (instance_id, capabilities) VALUES ($1,$2)")
            .bind(id)
            .bind(&caps)
            .execute(pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn postgres_image_api_create_patch_rollout_and_redaction() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "image_api_activation",
        "image API activation",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let state = state(pool.clone());
    let app = crate::server::routes::domains::ops::router(state.clone()).with_state(state);
    let image = upload(b"\x89PNG\r\n\x1a\nexact API bytes");
    let body = json!({"content":"body", "targetChannelId":"7001", "scheduledAt":"2030-01-01T00:00:00Z", "dedupeKey":"image-api", "imageAttachment":image});
    assert_eq!(
        request(&app, "POST", "/scheduled-messages", body.clone())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    ready(&pool).await;
    let (status, response) = request(&app, "POST", "/scheduled-messages", body.clone()).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "image create must activate for upgraded consumers: {response}"
    );
    let id = response["scheduledMessage"]["id"].as_str().unwrap();
    assert_eq!(
        response["scheduledMessage"]["imageAttachment"]["sizeBytes"],
        23
    );
    assert!(!response.to_string().contains("dataBase64"));
    let (status, conflict) = request(&app, "POST", "/scheduled-messages", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["scheduledMessage"]["id"], id);
    assert!(!conflict.to_string().contains("dataBase64"));
    let metadata = crate::db::scheduled_messages::find_active_by_dedupe_key_pg(&pool, "image-api")
        .await
        .unwrap()
        .unwrap();
    assert!(
        metadata.image_attachment.unwrap().data.is_none(),
        "dedupe conflict lookup must not load image bytes"
    );
    let path = format!("/scheduled-messages/{id}");
    let (status, detail) = request(&app, "GET", &path, JsonValue::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["scheduledMessage"], response["scheduledMessage"]);
    assert!(!detail.to_string().contains("dataBase64"));
    let metadata = crate::db::scheduled_messages::get_scheduled_message_metadata_pg(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        metadata.image_attachment.unwrap().data.is_none(),
        "GET metadata loader must omit bytes"
    );
    sqlx::query("UPDATE worker_nodes SET capabilities=capabilities-'message_outbox' WHERE instance_id='mini'").execute(&pool).await.unwrap();
    assert_eq!(
        request(&app, "PATCH", &path, json!({"title":"gate closed"}))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(&app, "PATCH", &path, json!({"imageAttachment":null}))
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE worker_nodes SET capabilities=capabilities || $1 WHERE instance_id='mini'")
        .bind(json!({"message_outbox":{"image_consumer_v1":true}}))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "PATCH",
            &path,
            json!({"imageAttachment":upload(b"\x89PNG\r\n\x1a\nexact API bytes")})
        )
        .await
        .0,
        StatusCode::OK
    );
    for patch in [
        json!({"deliveryKind":"agent"}),
        json!({"targetChannelId":null}),
        json!({"content":"😀".repeat(1001)}),
        json!({"providerTargets":{}}),
    ] {
        assert_eq!(
            request(&app, "PATCH", &path, patch).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(&app, "PATCH", &path, json!({"title":"still an image"}))
            .await
            .0,
        StatusCode::OK
    );
    let loaded = crate::db::scheduled_messages::get_scheduled_message_pg(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.image_attachment.unwrap().require_loaded().unwrap(),
        b"\x89PNG\r\n\x1a\nexact API bytes"
    );
    assert_eq!(
        request(&app, "PATCH", &path, json!({"imageAttachment":null}))
            .await
            .0,
        StatusCode::OK
    );
    for method in ["POST", "PATCH"] {
        let path = if method == "POST" {
            "/scheduled-messages"
        } else {
            &path
        };
        let invalid = json!({"content":"body", "targetChannelId":"7001", "scheduledAt":"2030-01-01T00:00:00Z", "imageAttachment":{}});
        assert_eq!(
            request(&app, method, path, invalid).await.0,
            StatusCode::BAD_REQUEST,
            "create/PATCH invalid images must share status 400"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_image_route_body_limit_and_eight_mib_boundary() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "image_body_limit",
        "scheduled image body boundary",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    ready(&pool).await;
    let state = state(pool.clone());
    let app = crate::server::routes::domains::ops::router(state.clone()).with_state(state);
    let mut bytes = vec![0; crate::db::scheduled_messages::MAX_IMAGE_BYTES];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut body = json!({"content":"body", "targetChannelId":"7001", "scheduledAt":"2030-01-01T00:00:00Z", "imageAttachment":upload(&bytes)});
    assert_eq!(
        request(&app, "POST", "/scheduled-messages", body.clone())
            .await
            .0,
        StatusCode::CREATED
    );
    bytes.push(0);
    body["imageAttachment"] = upload(&bytes);
    assert_eq!(
        request(&app, "POST", "/scheduled-messages", body).await.0,
        StatusCode::BAD_REQUEST
    );
    let oversized = json!({"content":"x".repeat(BODY_LIMIT)});
    assert_eq!(
        request(&app, "POST", "/scheduled-messages", oversized.clone())
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        request(&app, "PATCH", "/scheduled-messages/missing", oversized)
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let neighbor = json!({"content":"x".repeat(3*1024*1024)});
    assert_eq!(
        request(&app, "POST", "/routines", neighbor).await.0,
        StatusCode::PAYLOAD_TOO_LARGE,
        "scheduled body limit must stay route-local"
    );
    pool.close().await;
    db.drop().await;
}
