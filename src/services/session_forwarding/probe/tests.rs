use super::*;
use axum::Router;
use axum::extract::{ConnectInfo, Request};
use axum::http::header;
use axum::middleware::{Next, from_fn, from_fn_with_state, map_request};
use axum::routing::get;
use std::net::SocketAddr;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn context() -> ForwardCallerContext {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("probe-fixture-token".into());
    ForwardCallerContext {
        pg_pool: None,
        config: Arc::new(config),
        cluster_instance_id: Some("leader".into()),
    }
}

#[tokio::test]
async fn reachability_requires_trusted_origin_before_any_credentials_are_sent() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = json!({"instance_id":"worker","status":"online",
        "api_base_url":format!("http://{}",listener.local_addr().unwrap()),
        "capabilities":{"agentdesk_api":{"node_probe_v1":true}}});
    let result = probe(&context(), &node).await;
    assert_eq!(
        result["reachability_status"],
        "trusted_forward_origin_missing"
    );
    assert_eq!(result["reachability_verified"], false);
    assert_eq!(result["trust_validated"], false);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn reachability_checks_authenticated_response_identity_and_protocol() {
    for (status, body, expected) in [
        (
            "200 OK",
            json!({"protocol":1,"instance_id":"worker"}),
            Ok(()),
        ),
        (
            "200 OK",
            json!({"protocol":1,"instance_id":"wrong"}),
            Err("forwarding_probe_identity_mismatch"),
        ),
        (
            "401 Unauthorized",
            json!({}),
            Err("forwarding_probe_auth_failed"),
        ),
        (
            "403 Forbidden",
            json!({}),
            Err("forwarding_probe_auth_failed"),
        ),
        (
            "200 OK",
            json!({"protocol":2,"instance_id":"worker"}),
            Err("forwarding_probe_identity_mismatch"),
        ),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 8192];
            let count = connection.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]).to_ascii_lowercase();
            assert!(request.contains("authorization: bearer probe-fixture-token"));
            assert!(request.contains("x-agentdesk-session-owner: worker"));
            let body = body.to_string();
            connection.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        });
        let target =
            super::super::trusted_target::TrustedForwardTarget::for_test("worker", &origin)
                .unwrap();
        assert_eq!(probe_target(&context(), &target).await, expected);
        server.await.unwrap();
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReceiverObservation {
    strength: &'static str,
    authorization_present: bool,
    forwarded_by: Option<String>,
    session_owner: Option<String>,
}

fn tokenless_context() -> ForwardCallerContext {
    ForwardCallerContext {
        pg_pool: None,
        config: Arc::new(crate::config::Config::default()),
        cluster_instance_id: Some("leader".into()),
    }
}

async fn probe_receiver(
    peer: Option<SocketAddr>,
) -> (
    super::super::TrustedForwardTarget,
    Arc<Mutex<Vec<ReceiverObservation>>>,
    tokio::task::JoinHandle<()>,
) {
    let config = crate::config::Config::default();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = crate::app_state::AppState {
        pg_pool: None,
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: Some("worker".into()),
    };
    let observations = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&observations);
    let routes = Router::new()
        .route(
            "/internal/node-probe",
            get(crate::server::routes::cluster::node_probe),
        )
        .route_layer(from_fn(move |request: Request, next: Next| {
            let observed = Arc::clone(&observed);
            async move {
                let observation = {
                    let principal = request
                        .extensions()
                        .get::<crate::api_caller_observability::RequestPrincipal>()
                        .expect("auth inserts a principal before the receiver spy");
                    let header_value = |name: &str| {
                        request
                            .headers()
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned)
                    };
                    ReceiverObservation {
                        strength: principal.auth_strength.as_str(),
                        authorization_present: request
                            .headers()
                            .contains_key(header::AUTHORIZATION),
                        forwarded_by: header_value("x-agentdesk-forwarded-by"),
                        session_owner: header_value("x-agentdesk-session-owner"),
                    }
                };
                observed.lock().unwrap().push(observation);
                next.run(request).await
            }
        }))
        .route_layer(from_fn_with_state(
            state.clone(),
            crate::server::routes::auth::auth_middleware,
        ));
    let app = Router::new()
        .nest("/api", routes)
        .with_state(state)
        .layer(map_request(move |mut request: Request| async move {
            if let Some(peer) = peer {
                request.extensions_mut().insert(ConnectInfo(peer));
            }
            request
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let target = super::super::TrustedForwardTarget::for_test("worker", &origin).unwrap();
    let server = tokio::spawn(async move {
        // No listener-derived ConnectInfo: a missing test injection must fail closed.
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap();
    });
    (target, observations, server)
}

#[tokio::test]
async fn tokenless_probe_reaches_tailnet_authenticated_receiver() {
    let (target, observations, server) =
        probe_receiver(Some("100.64.0.7:40000".parse().unwrap())).await;
    let result = probe_target(&tokenless_context(), &target).await;
    server.abort();
    assert_eq!(result, Ok(()));
    assert_eq!(
        observations.lock().unwrap().as_slice(),
        &[ReceiverObservation {
            strength: "TailnetPeer",
            authorization_present: false,
            forwarded_by: Some("leader".into()),
            session_owner: Some("worker".into()),
        }]
    );
}

#[tokio::test]
async fn tokenless_probe_receiver_without_peer_fails_closed() {
    let (target, observations, server) = probe_receiver(None).await;
    let result = probe_target(&tokenless_context(), &target).await;
    server.abort();
    assert_eq!(result, Err("forwarding_probe_auth_failed"));
    assert!(observations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tokenless_probe_receiver_loopback_fixture_is_detectable() {
    let (target, observations, server) =
        probe_receiver(Some("127.0.0.1:40000".parse().unwrap())).await;
    let result = probe_target(&tokenless_context(), &target).await;
    server.abort();
    assert_eq!(result, Ok(()));
    let observed = observations.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].strength, "Loopback");
}
