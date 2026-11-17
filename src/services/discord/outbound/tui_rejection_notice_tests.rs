use super::*;
use crate::services::tui_o::channel_policy::output_disposition::tests::fenced_fixture;
use crate::services::tui_o::channel_policy::output_disposition::{Output, Phase};
use std::sync::Mutex;

struct Fake {
    calls: Mutex<Vec<(u64, String, String)>>,
    response: Mutex<Option<PostResult>>,
    history: Mutex<Option<Observation>>,
}
impl Fake {
    fn new(response: PostResult) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            response: Mutex::new(Some(response)),
            history: Mutex::new(None),
        }
    }
}
impl NoticePort for Fake {
    fn bot_id(&self) -> u64 {
        20
    }
    async fn post(&self, channel: u64, content: &str, nonce: &str) -> PostResult {
        self.calls
            .lock()
            .unwrap()
            .push((channel, content.into(), nonce.into()));
        self.response
            .lock()
            .unwrap()
            .take()
            .expect("unexpected second POST")
    }
    async fn lookup(&self, _: u64, _: u64) -> Result<Option<Observation>, String> {
        Ok(self.history.lock().unwrap().take())
    }
}
fn identity(input: u64) -> Identity {
    Identity {
        channel: 10,
        input,
        nonce: "turn-digest".into(),
        execution: "execution-digest".into(),
        source: "source-digest".into(),
    }
}
fn seen(nonce: String) -> Observation {
    Observation {
        message: 30,
        channel: 10,
        author: 20,
        nonce: Some(nonce),
        content: Reason::Deadline.message().into(),
    }
}

pub(crate) async fn emit_fixture_notice(pool: &PgPool, output: &Output) -> (Receipt, usize) {
    struct FixturePort {
        reason: Reason,
        posts: std::sync::atomic::AtomicUsize,
    }
    impl NoticePort for FixturePort {
        fn bot_id(&self) -> u64 {
            20
        }
        async fn post(&self, channel: u64, content: &str, nonce: &str) -> PostResult {
            self.posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(content, self.reason.message());
            PostResult::Created(Observation {
                message: 30,
                channel,
                author: 20,
                nonce: Some(nonce.into()),
                content: content.into(),
            })
        }
        async fn lookup(&self, _: u64, _: u64) -> Result<Option<Observation>, String> {
            panic!("a confirmed fixture never scans history")
        }
    }
    let Phase::RejectPendingNotice(reason) = output.phase() else {
        panic!("output must reject before notice")
    };
    let port = FixturePort {
        reason,
        posts: std::sync::atomic::AtomicUsize::new(0),
    };
    let receipt = deliver(pool, output.id(), reason, &port)
        .await
        .unwrap()
        .unwrap();
    (
        receipt,
        port.posts.load(std::sync::atomic::Ordering::SeqCst),
    )
}

#[tokio::test]
async fn uncertainty_is_metadata_only_and_history_requires_exact_effect_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "refusal_uncertain",
        "rejection notice reconciliation",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let id = identity(1);
    let port = Fake::new(PostResult::Uncertain);
    assert!(
        deliver(&pool, &id, Reason::Deadline, &port)
            .await
            .unwrap()
            .is_none()
    );
    let reopened = db.connect_and_migrate_with_max_connections(2).await;
    assert!(db::record(&reopened, &id).await.unwrap().is_some());
    assert!(
        deliver(&reopened, &id, Reason::Deadline, &port)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    let nonce = port.calls.lock().unwrap()[0].2.clone();
    *port.history.lock().unwrap() = Some(seen(nonce));
    let receipt = reconcile(&reopened, &id, Reason::Deadline, 30, &port)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            receipt.key(),
            receipt.reason(),
            receipt.template_version(),
            receipt.message()
        ),
        (id.key().as_str(), Reason::Deadline, 1, 30)
    );
    assert_eq!(
        deliver(&reopened, &id, Reason::Deadline, &port)
            .await
            .unwrap()
            .unwrap()
            .message(),
        30
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT identity FROM tui_output_dispositions WHERE output_key=$1")
            .bind(id.key())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored.as_object().unwrap().len(), 5);
    assert!(stored.get("body").is_none());
    assert!(db::record(&pool, &identity(2)).await.unwrap().is_none());
    reopened.close().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn strict_notice_evidence_is_bound_to_reserved_effect_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "refusal_evidence",
        "strict rejection notice evidence",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(2).await;
    let id = identity(1);
    let nonce = db::reserve(&pool, &id, Reason::Deadline)
        .await
        .unwrap()
        .unwrap()
        .nonce();
    let port = Fake::new(PostResult::Uncertain);
    for field in ["channel", "author", "nonce", "body", "id"] {
        let mut wrong = seen(nonce.clone());
        match field {
            "channel" => wrong.channel += 1,
            "author" => wrong.author += 1,
            "nonce" => wrong.nonce = None,
            "body" => wrong.content = "generic stop acknowledgement".into(),
            _ => wrong.message += 1,
        }
        *port.history.lock().unwrap() = Some(wrong);
        assert!(
            reconcile(&pool, &id, Reason::Deadline, 30, &port)
                .await
                .is_err(),
            "{field}"
        );
    }
    *port.history.lock().unwrap() = Some(seen(nonce));
    let receipt = reconcile(&pool, &id, Reason::Deadline, 30, &port)
        .await
        .unwrap()
        .unwrap();
    let wrong_id = fenced_fixture(identity(2), Reason::Deadline);
    assert!(!wrong_id.confirm(&receipt));
    let wrong_reason = fenced_fixture(id.clone(), Reason::SourceRejected);
    assert!(!wrong_reason.confirm(&receipt));
    assert!(
        db::persist_intent(&pool, &id, Reason::SourceRejected)
            .await
            .is_err()
    );
    assert!(
        port.calls.lock().unwrap().is_empty(),
        "history reconciliation never creates a message"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn only_proven_no_effect_reopens_the_same_notice_nonce_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "refusal_no_effect",
        "proven no-effect notice retry",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let id = identity(1);
    let port = Fake::new(PostResult::NoEffect);
    assert!(
        deliver(&pool, &id, Reason::Deadline, &port)
            .await
            .unwrap()
            .is_none()
    );
    assert!(db::record(&pool, &id).await.unwrap().is_some());
    let nonce = port.calls.lock().unwrap()[0].2.clone();
    assert!(nonce.len() <= 25);
    *port.response.lock().unwrap() = Some(PostResult::Created(seen(nonce.clone())));
    let receipt = deliver(&pool, &id, Reason::Deadline, &port)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.message(), 30);
    {
        let calls = port.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].2, nonce);
    }
    assert!(
        deliver(&pool, &id, Reason::Invalidated, &port)
            .await
            .is_err()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn gateway_adapter_posts_only_reason_and_confirms_real_response_pg() {
    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::{Value, json};
    let payloads = Arc::new(Mutex::new(Vec::<Value>::new()));
    async fn handle(
        State(calls): State<Arc<Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        calls.lock().unwrap().push(body.clone());
        Json(
            json!({ "id":"30", "channel_id":"10", "author":{"id":"20", "username":"test", "discriminator":"0001", "avatar":null, "bot":true}, "content":body["content"], "nonce":body["nonce"], "timestamp":"2026-11-17T00:00:00Z", "edited_timestamp":null, "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[], "attachments":[], "embeds":[], "pinned":false, "type":0 }),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/api/v10/channels/10/messages", post(handle))
        .with_state(payloads.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let port = GatewayNoticePort::new(
        Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(proxy)
                .ratelimiter_disabled(true)
                .build(),
        ),
        20,
    );
    assert!(matches!(
        super::super::census_scope(
            Some(json!({"output_class":"LegacyBody"})),
            port.post(
                10,
                Reason::Deadline.message(),
                "body-route-cannot-post-notice"
            ),
        )
        .await,
        PostResult::NoEffect
    ));
    assert!(payloads.lock().unwrap().is_empty());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "refusal_wire",
        "reason-only real gateway HTTP",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(2).await;
    let id = identity(1);
    let requested = Output::new(id.clone(), tokio::time::Instant::now());
    assert!(requested.reject(Reason::Deadline));
    assert_eq!(requested.phase(), Phase::RejectRequested(Reason::Deadline));
    assert!(requested.persist_rejection(&pool).await.is_err());
    assert!(db::record(&pool, &id).await.unwrap().is_none());
    assert!(payloads.lock().unwrap().is_empty());
    db::persist_intent(&pool, &id, Reason::Deadline)
        .await
        .unwrap();
    let output = Output::restore(&pool, id.clone(), tokio::time::Instant::now())
        .await
        .unwrap();
    let receipt = deliver(&pool, &id, Reason::Deadline, &port)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.message(), 30);
    assert!(!requested.confirm(&receipt));
    assert!(output.confirm(&receipt));
    assert_eq!(output.phase(), Phase::Rejected(Reason::Deadline));
    assert!(!output.confirm(&receipt));
    assert_eq!(
        deliver(&pool, &id, Reason::Deadline, &port)
            .await
            .unwrap()
            .unwrap()
            .message(),
        30
    );
    {
        let calls = payloads.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["content"], Reason::Deadline.message());
        assert_eq!(calls[0]["enforce_nonce"], true);
        assert_eq!(calls[0]["allowed_mentions"]["parse"], json!(["users"]));
    }
    pool.close().await;
    db.drop().await;
    server.abort();
}
