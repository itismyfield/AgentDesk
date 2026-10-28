use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::{AppState, domains};

fn router_with(config: crate::config::Config, pg_pool: Option<sqlx::PgPool>) -> Router {
    let engine = crate::engine::PolicyEngine::new(&config).expect("policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    let state = AppState {
        pg_pool,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    };
    domains::runtime::router(state.clone()).with_state(state)
}

async fn get_from(
    app: &Router,
    uri: &str,
    peer: Option<&str>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(peer) = peer {
        request = request.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

async fn local_get(app: &Router, uri: &str) -> (StatusCode, Value) {
    get_from(app, uri, Some("127.0.0.1:50000"), &[]).await
}

/// A request's peer, its headers and the status and error it gets back.
type Case<'a> = (
    Option<&'a str>,
    Vec<(&'a str, &'a str)>,
    (StatusCode, Option<&'a str>),
);

/// The human-input caller check stands on the route itself: without a configured token the
/// API middleware admits everyone, and only the extractor refuses a remote caller.
#[tokio::test]
async fn only_a_local_process_or_the_server_token_reaches_the_lookup() {
    let open = router_with(crate::config::Config::default(), None);
    let uri = "/agents/a/input-result?origin_id=G-1";
    let looked_up = (
        StatusCode::SERVICE_UNAVAILABLE,
        Some("postgres pool unavailable"),
    );
    let refused = (StatusCode::FORBIDDEN, Some("caller_not_allowed"));
    #[rustfmt::skip]
    let cases: Vec<Case> = vec![
        (Some("127.0.0.1:50000"), vec![], looked_up),
        (Some("[::1]:50000"), vec![], looked_up),
        (Some("100.71.169.10:50000"), vec![], refused),
        (None, vec![], refused),
        (Some("127.0.0.1:50000"), vec![("x-forwarded-for", "100.71.169.10")], refused),
        (Some("127.0.0.1:50000"), vec![("origin", "https://evil.example")], refused),
    ];
    for (peer, headers, expected) in cases {
        let (status, body) = get_from(&open, uri, peer, &headers).await;
        assert_eq!(
            (status, body["error"].as_str()),
            expected,
            "{peer:?} {headers:?}"
        );
    }
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("server-token".into());
    let guarded = router_with(config, None);
    let remote = Some("100.71.169.10:50000");
    #[rustfmt::skip]
    let cases = [
        ("Bearer server-token", looked_up),
        ("Bearer other-token", (StatusCode::UNAUTHORIZED, Some("unauthorized"))),
    ];
    for (authorization, expected) in cases {
        let headers = [("authorization", authorization)];
        let (status, body) = get_from(&guarded, uri, remote, &headers).await;
        assert_eq!(
            (status, body["error"].as_str()),
            expected,
            "{authorization}"
        );
    }
}

#[tokio::test]
async fn an_origin_the_contract_cannot_name_is_refused_before_any_lookup() {
    let app = router_with(crate::config::Config::default(), None);
    let long = "x".repeat(257);
    for (query, error) in [
        ("", "origin_id_required"),
        ("?origin_id=", "origin_id_required"),
        ("?origin_id=%20%20", "origin_id_required"),
        (&*format!("?origin_id={long}"), "origin_id_too_long"),
    ] {
        let (status, body) = local_get(&app, &format!("/agents/a/input-result{query}")).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some(error)),
            "{query}"
        );
    }
}

/// An inflight row in the runtime the server reads, written as the execution's owner leaves it.
fn write_row(provider: &str, channel: u64, contents: &str) {
    let dir = crate::config::runtime_root()
        .unwrap()
        .join("runtime/discord_inflight")
        .join(provider);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{channel}.json")), contents).unwrap();
}

fn running_row(message: u64, origins: &[&str]) -> String {
    json!({"version": 1, "provider": "claude", "user_msg_id": message, "input_origin_ids": origins})
        .to_string()
}

/// Every answer comes from an owner record: the inflight row on any bound channel, then the
/// committed receipt in either table. A delivery status kept elsewhere is never read as one.
#[tokio::test(flavor = "current_thread")]
async fn owner_records_alone_answer_for_every_bound_channel_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (primary, counterpart) = (7_340_101_u64, 7_340_102_u64);
    crate::db::agents::insert_agent_channels_for_tests(
        &pool,
        "origin-agent",
        Some(&primary.to_string()),
        Some(&counterpart.to_string()),
    )
    .await;
    let app = router_with(crate::config::Config::default(), Some(pool.clone()));
    let ask = |origin: &str| format!("/agents/origin-agent/input-result?origin_id={origin}");

    write_row("claude", primary, &running_row(55, &["G-primary"]));
    write_row(
        "codex",
        counterpart,
        &running_row(66, &["G-counter", "G-both"]),
    );
    for (origin, channel, message) in [
        ("G-primary", primary, 55),
        ("G-counter", counterpart, 66),
        ("G-both", counterpart, 66),
    ] {
        let (status, body) = local_get(&app, &ask(origin)).await;
        let execution = format!("discord:{channel}:{message}");
        assert_eq!(status, StatusCode::OK, "{origin}: {body}");
        assert_eq!(
            body,
            json!({"ok": true, "contract": "input-result-v1", "origin_id": origin,
                "agent_id": "origin-agent", "channel_id": channel.to_string(),
                "consumption": "consumed", "execution_id": execution, "turn_id": execution,
                "terminal": null}),
            "{origin}"
        );
    }

    let confirmed = chrono::DateTime::parse_from_rfc3339("2026-10-28T01:02:03Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let ids = vec!["G-primary".to_string()];
    let done = crate::db::turns::TurnReceipt {
        turn_id: &format!("discord:{primary}:55"),
        channel_id: &primary.to_string(),
        agent_id: Some("origin-agent"),
        provider: "claude",
        session_key: None,
        started_at: None,
        consumed_origin_ids: &ids,
        outcome: crate::db::turns::TerminalOutcome::Cancelled,
        confirmed_at: confirmed,
    };
    crate::db::turns::record_turn_receipt(&pool, &done)
        .await
        .unwrap();
    let (status, body) = local_get(&app, &ask("G-primary")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["terminal"],
        json!({"outcome": "cancelled", "confirmed_at": "2026-10-28T01:02:03+00:00",
            "final_answer_text": null, "final_answer_boundary": null, "final_answer_source": null}),
        "a committed receipt answers even while the row is still on disk"
    );

    sqlx::query(
        "INSERT INTO turns_archive (turn_id, channel_id, agent_id, started_at, finished_at,
            consumed_origin_ids, terminal_outcome, terminal_confirmed_at)
         VALUES ($1, $2, 'origin-agent', $3, $3, ARRAY['G-archived'], 'empty', $3)",
    )
    .bind(format!("discord:{counterpart}:9"))
    .bind(counterpart.to_string())
    .bind(confirmed)
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = local_get(&app, &ask("G-archived")).await;
    assert_eq!(
        (status, body["terminal"]["outcome"].as_str()),
        (StatusCode::OK, Some("empty")),
        "{body}"
    );

    // A delivery that answered `started` but left no owner record: its stored status is no
    // evidence of an execution or of an end.
    sqlx::query(
        "INSERT INTO imsg_reply_obligations (origin_id, agent_id, channel_id, person_id,
            request_hash, opened_at, expires_at, admission, admission_kind, admission_result,
            delivery_confirmed, admitted_turn_id)
         VALUES ('G-status-only', 'origin-agent', $1, 200, 'h', NOW(), NOW() + INTERVAL '24 hours',
            'admitted', 'started', $2, TRUE, $3)",
    )
    .bind(primary.to_string())
    .bind(json!({"status": 200, "body": {"ok": true, "delivery": "started"}}))
    .bind(format!("discord:{primary}:77"))
    .execute(&pool)
    .await
    .unwrap();
    for origin in ["G-status-only", "G-never"] {
        let (status, body) = local_get(&app, &ask(origin)).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("origin_not_found")),
            "{origin}: {body}"
        );
    }
    let (status, body) = local_get(
        &app,
        "/agents/no-such-agent/input-result?origin_id=G-primary",
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("agent_not_found"))
    );

    // An unreadable owner record is an error, never "unknown".
    write_row("claude", primary, "{not json");
    let (status, body) = local_get(&app, &ask("G-never")).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("input_result_unavailable")
        )
    );
    pool.close().await;
    db.drop().await;
}
