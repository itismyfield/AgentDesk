//! Legacy stop and request-shape refusals after delegated boot registration removal.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use axum::http::HeaderValue;
use std::sync::Arc;
const CHANNEL: u64 = 5_340_300_000;
const HOLDER: &str = "holder-a";

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
    TEST_COUNTS.with(|counts| *counts.borrow_mut() = (0, 0));
    for pool in [Some(pool.clone()), None] {
        let result = gateway_stop(&context(pool), channel, "claude").await;
        assert_eq!(
            TEST_COUNTS.with(|counts| *counts.borrow()),
            (0, 0),
            "off reads and sends nothing"
        );
        assert_eq!(result, GatewayStop::Legacy);
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
        (StatusCode::OK, Some("delegation_off"))
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
