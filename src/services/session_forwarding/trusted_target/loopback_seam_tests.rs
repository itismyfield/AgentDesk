//! The test-only loopback seam admits one exact registered socket on its own thread, as an
//! operator-allowed private address: every other address and both forwarding consents still apply.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::loopback_seam::allow_loopback_for_test;
use super::{
    ClusterConfig, ClusterNodeConfig, ForwardDnsResolver, TrustedForwardTarget, TrustedTargetError,
    build_trusted_target_with_resolver,
};

const OWNER: &str = "owner-node";

struct Answers(Vec<SocketAddr>);

#[async_trait]
impl ForwardDnsResolver for Answers {
    async fn resolve(&self, _: &str, _: u16) -> Result<Vec<SocketAddr>, ()> {
        Ok(self.0.clone())
    }
}

fn cluster(origin: &str, allow_private: bool, allow_insecure_http: bool) -> ClusterConfig {
    let node = ClusterNodeConfig {
        max_concurrent_dispatches: None,
        trusted_forward_origin: Some(origin.into()),
        allow_private_forwarding: allow_private,
        allow_insecure_http_forwarding: allow_insecure_http,
    };
    ClusterConfig {
        nodes: BTreeMap::from([(OWNER.to_string(), node)]),
        ..ClusterConfig::default()
    }
}

/// The target for `origin`, configured and advertised alike, with both consents as given.
async fn target(
    origin: &str,
    (allow_private, allow_insecure_http): (bool, bool),
    answers: Vec<SocketAddr>,
) -> Result<TrustedForwardTarget, TrustedTargetError> {
    let capabilities = json!({"agentdesk_api": {"session_forwarding": true}});
    build_trusted_target_with_resolver(
        &cluster(origin, allow_private, allow_insecure_http),
        OWNER,
        origin,
        "session_forwarding",
        &capabilities,
        &Answers(answers),
    )
    .await
}

fn origin(address: SocketAddr) -> String {
    format!("http://{address}/")
}

/// A listener on a free loopback port and its socket.
async fn listener() -> (tokio::net::TcpListener, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

// The registered socket with both consents is a target whose pinned client reaches that socket.
#[tokio::test]
async fn registered_with_consent_is_pinned_and_reachable() {
    let (listener, address) = listener().await;
    let _seam = allow_loopback_for_test(address);
    let target = target(&origin(address), (true, true), Vec::new()).await;
    let target = target.expect("the registered socket is admitted");
    let served = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 1024];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
        let reply = "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n";
        socket.write_all(reply.as_bytes()).await.unwrap();
        request
    });
    let request = target.request(reqwest::Method::GET, "/probe").unwrap();
    let response = request.send().await.expect("the pinned socket answers");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(served.await.unwrap().starts_with("GET /probe HTTP/1.1"));

    // A name resolving to the registered socket alone passes the same checks.
    let named = format!("http://owner.test:{}/", address.port());
    assert!(target_ok(&named, (true, true), vec![address]).await);
}

async fn target_ok(origin: &str, consents: (bool, bool), answers: Vec<SocketAddr>) -> bool {
    target(origin, consents, answers).await.is_ok()
}

// Without a registration the loopback owner is refused as before.
#[tokio::test]
async fn no_guard_keeps_loopback_refused() {
    let (_listener, address) = listener().await;
    let refused = target(&origin(address), (true, true), Vec::new()).await;
    assert_eq!(refused.unwrap_err(), TrustedTargetError::UnsafeAddress);
}

// Only the exact registered socket passes: another port, another loopback, IPv6 loopback forms,
// metadata, and a DNS answer mixing the registered socket with another loopback are refused.
#[tokio::test]
async fn other_loopbacks_stay_refused() {
    let (_listener, address) = listener().await;
    let (_other, other) = listener().await;
    let _seam = allow_loopback_for_test(address);
    let port = address.port();
    for refused in [
        other,
        format!("127.0.0.2:{port}").parse().unwrap(),
        format!("[::1]:{port}").parse().unwrap(),
        format!("[::ffff:127.0.0.1]:{port}").parse().unwrap(),
        format!("169.254.169.254:{port}").parse().unwrap(),
    ] {
        let result = target(&origin(refused), (true, true), Vec::new()).await;
        assert_eq!(
            result.unwrap_err(),
            TrustedTargetError::UnsafeAddress,
            "{refused}"
        );
    }
    let named = format!("http://owner.test:{port}/");
    let mixed = vec![address, format!("127.0.0.2:{port}").parse().unwrap()];
    let result = target(&named, (true, true), mixed).await;
    assert_eq!(result.unwrap_err(), TrustedTargetError::UnsafeAddress);
}

// The registration replaces no consent: private-address consent is still required, cleartext
// still needs both, and HTTPS keeps its own transport rule.
#[tokio::test]
async fn consents_still_apply_to_the_registered_socket() {
    let (_listener, address) = listener().await;
    let _seam = allow_loopback_for_test(address);
    let http = origin(address);
    let private_off = target(&http, (false, true), Vec::new()).await;
    assert_eq!(private_off.unwrap_err(), TrustedTargetError::UnsafeAddress);
    let cleartext_off = target(&http, (true, false), Vec::new()).await;
    assert_eq!(
        cleartext_off.unwrap_err(),
        TrustedTargetError::InsecureTransport
    );
    let https = format!("https://{address}/");
    assert!(target_ok(&https, (true, false), Vec::new()).await);
    let https_private_off = target(&https, (false, false), Vec::new()).await;
    assert_eq!(
        https_private_off.unwrap_err(),
        TrustedTargetError::UnsafeAddress
    );
}

// Dropping the guard restores the refusal; while one is held a second registration panics.
#[tokio::test]
async fn dropping_the_guard_restores_the_refusal() {
    let (_listener, address) = listener().await;
    let seam = allow_loopback_for_test(address);
    let again = std::panic::catch_unwind(|| allow_loopback_for_test(address));
    assert!(again.is_err(), "a second registration is refused");
    assert!(target_ok(&origin(address), (true, true), Vec::new()).await);
    drop(seam);
    let refused = target(&origin(address), (true, true), Vec::new()).await;
    assert_eq!(refused.unwrap_err(), TrustedTargetError::UnsafeAddress);
}

// A registration held on one thread admits nothing on another.
#[tokio::test]
async fn registration_is_confined_to_its_thread() {
    let (_listener, address) = listener().await;
    let _seam = allow_loopback_for_test(address);
    let elsewhere = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(target(&origin(address), (true, true), Vec::new()))
    });
    let result = elsewhere.join().unwrap();
    assert_eq!(result.unwrap_err(), TrustedTargetError::UnsafeAddress);
    assert!(target_ok(&origin(address), (true, true), Vec::new()).await);
}
