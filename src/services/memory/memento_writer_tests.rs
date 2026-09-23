use super::*;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

struct MockMemento {
    endpoint: String,
    writes: Arc<AtomicUsize>,
    status: Arc<AtomicU16>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockMemento {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockMemento {
    async fn start(fail: bool) -> Self {
        Self::start_with_status(if fail {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        })
        .await
    }

    async fn start_with_status(status: StatusCode) -> Self {
        let writes = Arc::new(AtomicUsize::new(0));
        let status = Arc::new(AtomicU16::new(status.as_u16()));
        let app = Router::new()
            .route("/mcp", post(|State((writes, status)): State<(Arc<AtomicUsize>, Arc<AtomicU16>)>, Json(body): Json<Value>| async move {
                assert_eq!(body["method"], "tools/call");
                assert_eq!(body["params"]["name"], "remember");
                writes.fetch_add(1, Ordering::SeqCst);
                let status = StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap();
                if status == StatusCode::BAD_REQUEST {
                    (StatusCode::OK, Json(json!({"jsonrpc":"2.0", "id":2, "error":{"code":-32602,"message":"Invalid params"}})))
                } else if status == StatusCode::UNPROCESSABLE_ENTITY {
                    (StatusCode::OK, Json(json!({"jsonrpc":"2.0", "id":2, "error":{"code":-32003,"message":"SYMBOLIC_POLICY_VIOLATION"}})))
                } else if !status.is_success() {
                    // An upstream may have committed before returning a misleading
                    // session error. A mutation must not be blindly replayed.
                    (status, Json(json!({"error": "session expired after write"})))
                } else {
                    (StatusCode::OK, Json(json!({"jsonrpc":"2.0", "id":2, "result":{"content":[{"type":"text","text":"{\"success\":true}"}]}})))
                }
            }))
            .with_state((writes.clone(), status.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            endpoint,
            writes,
            status,
            task,
        }
    }

    fn backend(&self) -> (MementoBackend, MementoRuntimeConfig) {
        let backend = MementoBackend::new(ResolvedMemorySettings::default());
        *backend
            .mcp_session
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(CachedMcpSession {
            endpoint: self.endpoint.clone(),
            session_id: "fixture-session".into(),
        });
        let config = MementoRuntimeConfig {
            endpoint: self.endpoint.clone(),
            access_key: "fixture-key-not-live".into(),
            workspace_override: None,
        };
        (backend, config)
    }
}

fn family_fact() -> Value {
    json!({"content":"윤호는 매주 화요일 수영 수업에 간다.", "topic":"family-schedule", "type":"fact", "workspace":"family-counsel", "agentId":"default", "assertionStatus":"verified"})
}

#[tokio::test]
async fn repeated_fact_and_metadata_only_turns_write_once_across_backend_recreation() {
    let mock = MockMemento::start(false).await;
    let dir = tempfile::tempdir().unwrap();
    let (backend, config) = mock.backend();
    backend
        .remember_guarded(&config, family_fact(), dir.path())
        .await
        .unwrap();
    let (restarted, config) = mock.backend();
    let mut repeated = family_fact();
    // The public remember adapter normalizes before this transport seam.
    repeated["content"] = json!(normalize_whitespace(
        "  윤호는  매주 화요일 수영 수업에 간다.\n"
    ));
    repeated["source"] = json!("another-turn");
    repeated["importance"] = json!(0.99);
    let result = restarted
        .remember_guarded(&config, repeated, dir.path())
        .await
        .unwrap();
    assert_eq!(result.payload["reason"], "already_stored");
    assert_eq!(mock.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn new_family_fact_and_new_assertion_or_scope_reach_writer() {
    let mock = MockMemento::start(false).await;
    let dir = tempfile::tempdir().unwrap();
    let (backend, config) = mock.backend();
    backend
        .remember_guarded(&config, family_fact(), dir.path())
        .await
        .unwrap();
    for (field, value) in [
        ("content", "윤호는 매주 목요일 수영 수업에 간다."),
        ("assertionStatus", "observed"),
        ("workspace", "personal-counsel"),
        ("caseId", "new-event"),
        ("outcome", "confirmed-by-parent"),
    ] {
        let mut new = family_fact();
        new[field] = json!(value);
        backend
            .remember_guarded(&config, new, dir.path())
            .await
            .unwrap();
    }
    assert_eq!(mock.writes.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn concurrent_same_fact_never_dispatches_twice() {
    let mock = MockMemento::start(false).await;
    let dir = tempfile::tempdir().unwrap();
    let (one, config) = mock.backend();
    let (two, _) = mock.backend();
    let (first, second) = tokio::join!(
        one.remember_guarded(&config, family_fact(), dir.path()),
        two.remember_guarded(&config, family_fact(), dir.path()),
    );
    assert!(first.is_ok() || second.is_ok());
    assert_eq!(mock.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ambiguous_write_error_is_neither_replayed_nor_reported_as_success() {
    let mock = MockMemento::start(true).await;
    let dir = tempfile::tempdir().unwrap();
    let (backend, config) = mock.backend();
    assert!(
        backend
            .remember_guarded(&config, family_fact(), dir.path())
            .await
            .is_err()
    );
    let (restarted, config) = mock.backend();
    assert!(
        restarted
            .remember_guarded(&config, family_fact(), dir.path())
            .await
            .is_err()
    );
    assert_eq!(mock.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn connection_failure_releases_claim_for_same_new_fact() {
    let mock = MockMemento::start(false).await;
    let (backend, mut config) = mock.backend();
    let closed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.endpoint = format!("http://{}", closed_listener.local_addr().unwrap());
    drop(closed_listener);
    *backend
        .mcp_session
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(CachedMcpSession {
        endpoint: config.endpoint.clone(),
        session_id: "cached".into(),
    });
    let dir = tempfile::tempdir().unwrap();
    assert!(
        backend
            .remember_guarded(&config, family_fact(), dir.path())
            .await
            .is_err()
    );
    let key = writer_fingerprint(
        dir.path(),
        &config.endpoint,
        &config.access_key,
        &family_fact(),
    )
    .unwrap();
    WriterClaim::acquire(dir.path(), &key)
        .unwrap()
        .unwrap()
        .release_before_send()
        .unwrap();
}

#[tokio::test]
async fn unauthorized_write_clears_session_and_releases_without_replay() {
    let mock = MockMemento::start_with_status(StatusCode::UNAUTHORIZED).await;
    let (backend, config) = mock.backend();
    let dir = tempfile::tempdir().unwrap();
    assert!(
        backend
            .remember_guarded(&config, family_fact(), dir.path())
            .await
            .is_err()
    );
    assert!(backend.cached_session_id(&config.endpoint).is_none());
    assert_eq!(mock.writes.load(Ordering::SeqCst), 1);
    let key = writer_fingerprint(
        dir.path(),
        &config.endpoint,
        &config.access_key,
        &family_fact(),
    )
    .unwrap();
    WriterClaim::acquire(dir.path(), &key)
        .unwrap()
        .unwrap()
        .release_before_send()
        .unwrap();
}

#[tokio::test]
async fn explicit_rejection_can_retry_same_new_family_fact_after_recovery() {
    for rejected in [
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_REQUEST,
        StatusCode::UNPROCESSABLE_ENTITY,
    ] {
        let mock = MockMemento::start_with_status(rejected).await;
        let dir = tempfile::tempdir().unwrap();
        let (backend, config) = mock.backend();
        assert!(
            backend
                .remember_guarded(&config, family_fact(), dir.path())
                .await
                .is_err()
        );
        assert_eq!(mock.writes.load(Ordering::SeqCst), 1);
        mock.status.store(200, Ordering::SeqCst);
        let (restarted, config) = mock.backend();
        restarted
            .remember_guarded(&config, family_fact(), dir.path())
            .await
            .unwrap();
        assert_eq!(mock.writes.load(Ordering::SeqCst), 2, "{rejected}");
        restarted
            .remember_guarded(&config, family_fact(), dir.path())
            .await
            .unwrap();
        assert_eq!(
            mock.writes.load(Ordering::SeqCst),
            2,
            "successful retry must then deduplicate"
        );
    }
}

#[tokio::test]
async fn observed_amend_or_forget_allows_backend_to_restore_previously_stored_fact() {
    let mock = MockMemento::start(false).await;
    let dir = tempfile::tempdir().unwrap();
    let (backend, config) = mock.backend();
    backend
        .remember_guarded(&config, family_fact(), dir.path())
        .await
        .unwrap();
    super::super::memento_writer_guard::invalidate_writer_receipts(
        dir.path(),
        &config.endpoint,
        &config.access_key,
    )
    .unwrap();
    backend
        .remember_guarded(&config, family_fact(), dir.path())
        .await
        .unwrap();
    assert_eq!(mock.writes.load(Ordering::SeqCst), 2);
}
