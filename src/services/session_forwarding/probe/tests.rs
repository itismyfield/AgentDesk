use super::*;
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
