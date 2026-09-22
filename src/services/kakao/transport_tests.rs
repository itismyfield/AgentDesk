use super::*;
use axum::{
    Json, Router,
    extract::{Form, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn concurrent_old_generation_401_refreshes_once_and_retains_refresh_token() {
    #[derive(Clone)]
    struct Fixture {
        barrier: Arc<tokio::sync::Barrier>,
        refreshes: Arc<AtomicUsize>,
    }
    async fn endpoint(
        State(state): State<Fixture>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        if headers["authorization"] == "Bearer old-test-access" {
            state.barrier.wait().await;
            (StatusCode::UNAUTHORIZED, Json(json!({})))
        } else {
            (StatusCode::OK, Json(json!({"result_code":0})))
        }
    }
    async fn refresh(
        State(state): State<Fixture>,
        Form(form): Form<HashMap<String, String>>,
    ) -> Json<Value> {
        assert_eq!(form["refresh_token"], "test-refresh");
        state.refreshes.fetch_add(1, Ordering::SeqCst);
        Json(json!({"access_token":"new-test-access","expires_in":3600}))
    }
    let state = Fixture {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
        refreshes: Arc::new(AtomicUsize::new(0)),
    };
    let (origin, task) = test_support::server(
        Router::new()
            .route("/v2/api/talk/memo/default/send", post(endpoint))
            .route("/oauth/token", post(refresh))
            .with_state(state.clone()),
    )
    .await;
    let client = test_support::client(&origin, "default");
    let (a, b) = tokio::join!(
        client.authorized_form::<Value>(SELF_SEND_URL, &[]),
        client.authorized_form::<Value>(SELF_SEND_URL, &[])
    );
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(state.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        client.tokens.lock().await.refresh_token.as_deref(),
        Some("test-refresh")
    );
    task.abort();
}

#[tokio::test]
async fn refresh_transient_failure_does_not_erase_credentials() {
    let (origin, task) = test_support::server(Router::new().route(
        "/oauth/token",
        post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
    ))
    .await;
    let client = test_support::client(&origin, "default");
    assert!(matches!(
        client.access_token_generation(Some(0)).await,
        Err(KakaoError::TransientAuth)
    ));
    assert_eq!(
        client.tokens.lock().await.refresh_token.as_deref(),
        Some("test-refresh")
    );
    task.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn rotated_tokens_survive_store_reopen() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let (origin,task)=test_support::server(Router::new().route("/oauth/token",post(||async{Json(json!({"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}))}))).await;
    let mut client = test_support::client(&origin, "default");
    client.store = Some(token_store::TokenStore::for_test(temp.path(), "default"));
    // A directory at the destination prevents rename even when tests run as root.
    std::fs::create_dir(temp.path().join("default.json")).unwrap();
    assert!(matches!(
        client.access_token_generation(Some(0)).await,
        Err(KakaoError::CredentialPersistence)
    ));
    {
        let tokens = client.tokens.lock().await;
        assert!(tokens.persistence_failed);
        assert_eq!(tokens.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(tokens.generation, 1);
    }
    assert!(matches!(
        client.validate_credentials().await,
        Err(KakaoError::CredentialPersistence)
    ));
    assert!(matches!(
        client.require_durable_credentials().await,
        Err(KakaoError::CredentialPersistence)
    ));
    std::fs::remove_dir(temp.path().join("default.json")).unwrap();
    assert!(client.require_durable_credentials().await.is_ok());
    assert_eq!(client.access_token_generation(None).await.unwrap().1, 1);
    assert!(client.validate_credentials().await.is_ok());
    drop(client);
    let reopened = token_store::TokenStore::for_test(temp.path(), "default");
    let stored = reopened.load().unwrap().unwrap();
    assert_eq!(stored.access_token.as_deref(), Some("rotated"));
    assert_eq!(stored.refresh_token.as_deref(), Some("rotated-refresh"));
    assert_eq!(stored.generation, 1);
    task.abort();
}

#[tokio::test]
async fn schedule_validation_requires_loaded_credentials_without_network() {
    let mut client = test_support::client("http://127.0.0.1:1", "default");
    assert!(client.validate_credentials().await.is_ok());
    client.tokens.lock().await.access_token = None;
    assert!(client.validate_credentials().await.is_ok());
    client.rest_api_key = None;
    assert!(matches!(
        client.validate_credentials().await,
        Err(KakaoError::MissingCredentials)
    ));
    client.rest_api_key = Some("test-app-key".into());
    client.tokens.lock().await.refresh_token = None;
    assert!(matches!(
        client.validate_credentials().await,
        Err(KakaoError::MissingCredentials)
    ));
}
