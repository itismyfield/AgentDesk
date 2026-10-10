//! The gateway side and the receiver's pre-effect refusals, against real PG rows and a
//! scripted holder over HTTP.
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::{Value, json};

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::channel_home;

const CHANNEL: u64 = 5_340_300_000;
const HOLDER: &str = "holder-a";

type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
type Script = fn(&Value) -> Option<(u16, Value)>;

/// A holder answering each request with `script(request)`; `None` drops the connection unanswered.
async fn holder(script: Script) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/", listener.local_addr().unwrap());
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut raw = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break (head.to_string(), body.to_string());
                    }
                };
                let mut headers = HeaderMap::new();
                for line in head.lines().skip(1) {
                    if let Some((key, value)) = line.split_once(':') {
                        let name = axum::http::HeaderName::from_bytes(key.trim().as_bytes());
                        if let (Ok(name), Ok(value)) = (name, HeaderValue::from_str(value.trim())) {
                            headers.insert(name, value);
                        }
                    }
                }
                let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let answer = script(&request);
                log.lock().unwrap().push((headers, request));
                let Some((status, answer)) = answer else {
                    return;
                };
                let answer = answer.to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (origin, seen)
}

fn echo(request: &Value) -> Value {
    json!({"v": 1, "request_id": request["request_id"], "channel_id": request["channel_id"],
        "provider": request["provider"], "holder": request["expected_holder"],
        "home_epoch": request["home_epoch"], "terminal_confirmed": false,
        "outcome": "herdr", "intent": "recorded", "delivery": "sent", "effect_started": true})
}

fn without(mut answer: Value, key: &str) -> Value {
    answer.as_object_mut().unwrap().remove(key);
    answer
}

struct Gateway {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    _on: crate::services::cluster::home_availability::Registration,
}

impl Gateway {
    async fn new() -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let on = crate::services::cluster::home_availability::install(
            "claude",
            Ok(()),
            Default::default,
        );
        Self { db, pool, _on: on }
    }

    fn context(&self, pool: Option<sqlx::PgPool>) -> ForwardCallerContext {
        ForwardCallerContext {
            pg_pool: pool,
            config: Arc::new(crate::config::Config::default()),
            cluster_instance_id: Some("gw".into()),
        }
    }

    async fn home(&self, channel: u64, state: &str, holder: Option<&str>, provider: &str) {
        let target = (state == "releasing").then_some("gw");
        sqlx::query("INSERT INTO o_channel_homes (channel_id, provider, state, holder, target, epoch, renewed_at) VALUES ($1, $2, $3, $4, $5, 7, NOW())")
            .bind(channel.to_string()).bind(provider).bind(state).bind(holder).bind(target)
            .execute(&self.pool).await.unwrap();
    }

    async fn stop(&self, channel: u64) -> GatewayStop {
        gateway_stop(&self.context(Some(self.pool.clone())), channel, "claude").await
    }

    async fn close(self) {
        TEST_ORIGINS.with(|origins| origins.borrow_mut().clear());
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn route(holder: &str, origin: &str) {
    TEST_ORIGINS.with(|origins| origins.borrow_mut().insert(holder.into(), origin.into()));
}

#[tokio::test(flavor = "current_thread")]
async fn t10_only_a_matching_typed_answer_counts_and_nothing_is_retried_or_run_here() {
    let gateway = Gateway::new().await;
    let cases: [(u64, Script, Option<&str>); 7] = [
        (1, |request| Some((200, echo(request))), None),
        (
            2,
            |request| {
                Some((
                    200,
                    json!({"ok": true, "channel_id": request["channel_id"]}),
                ))
            },
            Some("answer_mismatch"),
        ),
        (
            3,
            |_| {
                Some((
                    404,
                    json!({"error": "no active turn found for this channel", "code": "not_found"}),
                ))
            },
            Some("holder_route_missing"),
        ),
        (
            4,
            |_| Some((409, json!({"code": "session_forward_owner_conflict"}))),
            Some("holder_conflict"),
        ),
        (
            5,
            |request| Some((200, without(echo(request), "home_epoch"))),
            Some("answer_mismatch"),
        ),
        (
            6,
            |request| {
                let mut answer = echo(request);
                answer["request_id"] = "other".into();
                Some((200, answer))
            },
            Some("answer_mismatch"),
        ),
        (7, |_| None, Some("no_response")),
    ];
    for (n, script, unconfirmed) in cases {
        let channel = CHANNEL + n;
        gateway
            .home(channel, "worker", Some(HOLDER), "claude")
            .await;
        let (origin, seen) = holder(script).await;
        route(HOLDER, &origin);
        let result = gateway.stop(channel).await;
        match unconfirmed {
            None => {
                let GatewayStop::Confirmed(answer) = &result else {
                    panic!("case {n}: {result:?}");
                };
                assert_eq!(answer["delivery"], "sent");
                assert_eq!(answer["terminal_confirmed"], false);
            }
            Some(reason) => assert_eq!(result, GatewayStop::Unconfirmed(reason), "case {n}"),
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "case {n}: one send, no retry or redirect");
        let (headers, request) = &seen[0];
        assert_eq!(headers["x-agentdesk-forwarded-by"], "gw");
        assert_eq!(headers["x-agentdesk-session-owner"], HOLDER);
        assert_eq!(request["v"], 1);
        assert_eq!(request["force"], false);
        assert_eq!(request["surface"], "slash_stop");
        assert_eq!(request["intent"], "user_stop");
        assert_eq!(request["home_epoch"], 7);
        assert_eq!(request["expected_holder"], HOLDER);
    }
    gateway.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn t18_holder_comes_from_the_home_row_never_the_session_owner() {
    let gateway = Gateway::new().await;
    let channel = CHANNEL + 20;
    gateway
        .home(channel, "worker", Some(HOLDER), "claude")
        .await;
    sqlx::query("INSERT INTO sessions (session_key, provider, status, channel_id, instance_id) VALUES ('owner-b:s', 'claude', 'turn_active', $1, 'owner-b')")
        .bind(channel.to_string()).execute(&gateway.pool).await.unwrap();
    let (holder_origin, holder_seen) = holder(|request| Some((200, echo(request)))).await;
    let (owner_origin, owner_seen) = holder(|request| Some((200, echo(request)))).await;
    route(HOLDER, &holder_origin);
    route("owner-b", &owner_origin);
    assert!(matches!(
        gateway.stop(channel).await,
        GatewayStop::Confirmed(_)
    ));
    assert_eq!(holder_seen.lock().unwrap().len(), 1);
    assert_eq!(
        owner_seen.lock().unwrap().len(),
        0,
        "the session owner gets nothing"
    );
    gateway.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn t18_c_legacy_reads_no_row_and_g_without_a_row_is_refused_not_legacy() {
    let gateway = Gateway::new().await;
    let unreachable = Some(gateway.pool.clone());
    // A channel with a gate here keeps the existing D2 path without any row read.
    let registered = CHANNEL + 30;
    let _home =
        channel_home::register_for_test(registered, Some(o_channel_homes::HomeState::Worker));
    assert_eq!(
        gateway_stop(&gateway.context(None), registered, "claude").await,
        GatewayStop::Legacy
    );
    channel_home::unregister(&registered.to_string());
    // The gateway's own unregistered channel: no pool or a failed read is refused, never Legacy.
    let unregistered = CHANNEL + 31;
    assert_eq!(
        gateway_stop(&gateway.context(None), unregistered, "claude").await,
        GatewayStop::Refused(UNOBSERVED)
    );
    // A readable store without the row is the gateway rules; a failed read never is.
    assert_eq!(
        gateway_stop(
            &gateway.context(unreachable.clone()),
            unregistered,
            "claude"
        )
        .await,
        GatewayStop::Legacy
    );
    gateway.pool.close().await;
    assert_eq!(
        gateway_stop(&gateway.context(unreachable), unregistered, "claude").await,
        GatewayStop::Refused(UNOBSERVED)
    );
    gateway.db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn t18_row_states_decide_before_anything_is_sent() {
    let gateway = Gateway::new().await;
    let (origin, seen) = holder(|request| Some((200, echo(request)))).await;
    route(HOLDER, &origin);
    route("gw", &origin);
    assert_eq!(
        gateway.stop(CHANNEL + 40).await,
        GatewayStop::Legacy,
        "no row: gateway rules"
    );
    gateway
        .home(CHANNEL + 41, "releasing", Some(HOLDER), "claude")
        .await;
    assert_eq!(
        gateway.stop(CHANNEL + 41).await,
        GatewayStop::Refused("home_in_transition")
    );
    gateway
        .home(CHANNEL + 42, "worker", Some(HOLDER), "codex")
        .await;
    assert_eq!(
        gateway.stop(CHANNEL + 42).await,
        GatewayStop::Refused("home_provider_mismatch")
    );
    gateway
        .home(CHANNEL + 43, "worker", Some("gw"), "claude")
        .await;
    assert_eq!(
        gateway.stop(CHANNEL + 43).await,
        GatewayStop::Refused(UNOBSERVED)
    );
    gateway
        .home(CHANNEL + 44, "worker", Some("holder-unknown"), "claude")
        .await;
    assert_eq!(
        gateway.stop(CHANNEL + 44).await,
        GatewayStop::Refused("holder_unreachable")
    );
    assert!(seen.lock().unwrap().is_empty());
    gateway.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn off_reads_nothing_and_advertises_nothing() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = CHANNEL + 50;
    sqlx::query("INSERT INTO o_channel_homes (channel_id, provider, state, holder, epoch, renewed_at) VALUES ($1, 'claude', 'worker', $2, 7, NOW())")
        .bind(channel.to_string()).bind(HOLDER).execute(&pool).await.unwrap();
    let context = |pool| ForwardCallerContext {
        pg_pool: pool,
        config: Arc::new(crate::config::Config::default()),
        cluster_instance_id: Some("gw".into()),
    };
    // Off: even a delegated row is not read, so the existing path runs exactly as before.
    assert_eq!(home_availability::state("claude"), Availability::Off);
    for pool in [None, Some(pool.clone())] {
        assert_eq!(
            gateway_stop(&context(pool), channel, "claude").await,
            GatewayStop::Legacy
        );
    }
    let mut config = crate::config::Config::default();
    config.cluster.api_base_url = Some("http://10.0.0.5:8791".into());
    let base = crate::services::cluster::session_routing::cluster_capabilities_with_worker_api(
        &config.cluster,
    );
    for switch in [None, Some(false)] {
        config.runtime.channel_home_delegation_enabled = switch;
        let off = crate::services::cluster::session_routing::node_capabilities(&config);
        assert_eq!(off, base);
        assert_eq!(off.to_string().into_bytes(), base.to_string().into_bytes());
    }
    config.runtime.channel_home_delegation_enabled = Some(true);
    let on = crate::services::cluster::session_routing::node_capabilities(&config);
    assert_eq!(on["agentdesk_api"][CAPABILITY], true);
    assert_eq!(on["agentdesk_api"]["cancel_forwarding_v1"], true);
    // Receiver off: refused before any read or run.
    let mut headers = HeaderMap::new();
    headers.insert("x-agentdesk-forwarded-by", HeaderValue::from_static("gw"));
    headers.insert(
        "x-agentdesk-session-owner",
        HeaderValue::from_static(HOLDER),
    );
    let body = request_body(channel, HOLDER, 7, "claude");
    let (status, answer) = receive(&context(None), &headers, &body, |_, _| async {
        panic!("off must not run a stop")
    })
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["outcome"], "refused");
    assert_eq!(answer["reason"], "delegation_off");
    pool.close().await;
    db.drop().await;
}

pub(crate) fn request_body(channel: u64, holder: &str, epoch: i64, provider: &str) -> Vec<u8> {
    json!({"v": 1, "request_id": "r-1", "channel_id": channel.to_string(), "provider": provider,
        "expected_holder": holder, "home_epoch": epoch, "surface": "slash_stop",
        "intent": "user_stop", "force": false})
    .to_string()
    .into_bytes()
}

#[tokio::test(flavor = "current_thread")]
async fn f1_receiver_shape_and_trust_refusals_run_nothing() {
    let _on = home_availability::install("claude", Ok(()), Default::default);
    let context = ForwardCallerContext {
        pg_pool: None,
        config: Arc::new(crate::config::Config::default()),
        cluster_instance_id: Some(HOLDER.into()),
    };
    let mut trusted = HeaderMap::new();
    trusted.insert("x-agentdesk-forwarded-by", HeaderValue::from_static("gw"));
    trusted.insert(
        "x-agentdesk-session-owner",
        HeaderValue::from_static(HOLDER),
    );
    let valid = request_body(CHANNEL, HOLDER, 7, "claude");
    let mut extra: Value = serde_json::from_slice(&valid).unwrap();
    extra["reason"] = "operator".into();
    let mut force: Value = serde_json::from_slice(&valid).unwrap();
    force["force"] = true.into();
    let mut v2: Value = serde_json::from_slice(&valid).unwrap();
    v2["v"] = 2.into();
    let cases = [
        (HeaderMap::new(), valid.clone(), StatusCode::FORBIDDEN),
        (
            trusted.clone(),
            b"{\"force\":false}".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        (
            trusted.clone(),
            extra.to_string().into_bytes(),
            StatusCode::BAD_REQUEST,
        ),
        (
            trusted.clone(),
            force.to_string().into_bytes(),
            StatusCode::BAD_REQUEST,
        ),
        (
            trusted.clone(),
            v2.to_string().into_bytes(),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (headers, body, expected) in cases {
        let (status, _) = receive(&context, &headers, &body, |_, _| async {
            panic!("a refused request must not run a stop")
        })
        .await;
        assert_eq!(status, expected);
    }
    // Shape and trust pass, but this node cannot read its row: refused, nothing run.
    let (status, answer) = receive(&context, &trusted, &valid, |_, _| async {
        panic!("an unobserved home must not run a stop")
    })
    .await;
    assert_eq!(
        (status, answer["reason"].as_str()),
        (StatusCode::OK, Some(UNOBSERVED))
    );
    assert!(names_envelope(&valid));
    for legacy in [
        &b""[..],
        b"{\"force\":true}",
        b"{\"force\":true,\"note\":1}",
        b"junk",
        b"[1]",
    ] {
        assert!(!names_envelope(legacy));
    }
}
