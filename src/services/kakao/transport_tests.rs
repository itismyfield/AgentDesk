use super::*;
use axum::{
    Json, Router,
    extract::{Form, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
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

#[tokio::test]
async fn calendar_uses_fixed_methods_query_form_and_independent_success_contracts() {
    async fn create(Form(form): Form<HashMap<String, String>>) -> Json<Value> {
        assert_eq!(form["calendar_id"], "primary");
        assert_eq!(
            serde_json::from_str::<Value>(&form["event"]).unwrap()["title"],
            "test"
        );
        Json(json!({"event_id":"remote"}))
    }
    async fn update(Form(form): Form<HashMap<String, String>>) -> Json<Value> {
        assert_eq!(form["event_id"], "remote");
        Json(json!({"event_id":"remote"}))
    }
    async fn remove(Query(query): Query<HashMap<String, String>>) -> Json<Value> {
        assert_eq!(query["event_id"], "remote");
        Json(json!({"event_id":"remote"}))
    }
    let (origin, task) = test_support::server(
        Router::new()
            .route("/v2/api/calendar/create/event", post(create))
            .route("/v2/api/calendar/update/event/host", post(update))
            .route("/v2/api/calendar/delete/event", delete(remove))
            .route(
                "/v1/user/access_token_info",
                get(|| async { Json(json!({"id":100,"app_id":10,"expires_in":3600})) }),
            )
            .route(
                "/v2/user/scopes",
                get(|| async {
                    Json(json!({"id":100,"scopes":[{"id":"talk_calendar","agreed":true}]}))
                }),
            ),
    )
    .await;
    let client = test_support::client(&origin, "default");
    let identity = client.calendar_identity().await.unwrap();
    assert_eq!(identity.user_id, 100);
    assert_eq!(
        client
            .calendar_create(&json!({"title":"test"}))
            .await
            .unwrap(),
        "remote"
    );
    client
        .calendar_update("remote", &json!({"title":"test"}))
        .await
        .unwrap();
    client.calendar_delete("remote").await.unwrap();
    task.abort();
}

#[tokio::test]
async fn ambiguous_create_is_not_repeated_and_response_size_is_bounded() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let (origin, task) = test_support::server(Router::new().route(
        "/v2/api/calendar/create/event",
        post(move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                "x".repeat(RESPONSE_MAX_BYTES + 1)
            }
        }),
    ))
    .await;
    let client = test_support::client(&origin, "default");
    assert!(matches!(
        client.calendar_create(&json!({})).await,
        Err(KakaoError::DeliveryUnknown)
    ));
    assert_eq!(observed.load(Ordering::SeqCst), 1);
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

#[test]
fn calendar_adoption_requires_owned_complete_matching_content() {
    let desired = json!({"title":"meeting","description":"details","location":{"name":"office"},"reminders":[5,10],"time":{"start_at":"2026-09-30T01:00:00Z","end_at":"2026-09-30T02:00:00Z","time_zone":"Asia/Seoul"}});
    let mut remote = desired.clone();
    remote["id"] = json!("remote");
    remote["calendar_id"] = json!("primary");
    remote["is_host"] = json!(true);
    remote["time"]["is_all_day"] = json!(false);
    remote["time"]["start_at"] = json!("2026-09-30T10:00:00+09:00");
    assert!(calendar::matches_adoption(&remote, "remote", &desired));
    assert!(!calendar::matches_adoption(&remote, "another", &desired));
    for value in [json!(true), Value::Null, json!("false")] {
        let mut altered = remote.clone();
        altered["time"]["is_all_day"] = value;
        assert!(!calendar::matches_adoption(&altered, "remote", &desired));
    }
    let mut incomplete = remote.clone();
    incomplete["time"]
        .as_object_mut()
        .unwrap()
        .remove("is_all_day");
    assert!(!calendar::matches_adoption(&incomplete, "remote", &desired));
    for (field, value) in [
        ("is_host", json!(false)),
        ("calendar_id", json!("other")),
        ("title", json!("different")),
        ("description", json!("different")),
        ("location", json!({"name":"elsewhere"})),
        ("reminders", json!([5])),
        ("rrule", json!("daily")),
    ] {
        let mut altered = remote.clone();
        altered[field] = value;
        assert!(
            !calendar::matches_adoption(&altered, "remote", &desired),
            "accepted mismatching {field}"
        );
    }
    assert!(!calendar::matches_adoption(
        &json!({"time":remote["time"]}),
        "remote",
        &desired
    ));
}
