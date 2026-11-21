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

/// An origin still queued reads accepted, one its owner removed unrun reads the owed cancelled
/// receipt before any database write, and a pool that cannot read the binding is 503, never 404.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_queued_origin_reads_accepted_then_its_owed_receipt_pg() {
    use crate::services::turn_orchestrator::{ChannelMailboxRegistry, QueuePersistenceContext};
    use poise::serenity_prelude::ChannelId;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 7_340_601_u64;
    let insert = crate::db::agents::insert_agent_channels_for_tests;
    insert(&pool, "owed-agent", Some(&channel.to_string()), None).await;
    let app = router_with(crate::config::Config::default(), Some(pool.clone()));
    let uri = "/agents/owed-agent/input-result?origin_id=G-owed";

    let persistence =
        QueuePersistenceContext::new(&crate::services::provider::ProviderKind::Claude, "", None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, ChannelId::new(channel))
            .unwrap();
    let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(channel));
    handle.v2_adopt(path, namespace).await.unwrap();
    let id = 8_000_000_000_000_340_601_u64;
    let original = serde_json::from_value(json!({"author_id":1,"message_id":id,
        "source_message_ids":[id],"channel_id":channel,"queued_generation":5,"text":"start"}))
    .unwrap();
    let context = json!({"origin":"headless","metadata":{"human_input":{"origin_id":"G-owed"}}});
    let admitted = handle.v2_admit_ticket_with_context(original, "t-owed".into(), context);
    admitted.await.unwrap();
    let (status, body) = local_get(&app, uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"ok": true, "contract": "input-result-v1", "origin_id": "G-owed",
            "agent_id": "owed-agent", "channel_id": channel.to_string(), "consumption": "accepted",
            "execution_id": null, "turn_id": null, "terminal": null})
    );

    assert_eq!(
        handle.clear(persistence).await.discarded_message_ids.len(),
        1
    );
    let owed = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    let at = owed.await.unwrap()[0].at_ms as i64;
    let at = chrono::DateTime::from_timestamp_millis(at).unwrap();
    let (status, body) = local_get(&app, uri).await;
    let execution = format!("discord:{channel}:{id}");
    assert_eq!(
        (status, &body["execution_id"]),
        (StatusCode::OK, &json!(execution)),
        "{body}"
    );
    assert_eq!(body["terminal"]["outcome"], "cancelled");
    let confirmed = body["terminal"]["confirmed_at"].as_str().unwrap();
    let confirmed = chrono::DateTime::parse_from_rfc3339(confirmed).unwrap();
    assert_eq!(confirmed, at);

    pool.close().await;
    let (status, body) = local_get(&app, uri).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("input_result_unavailable")
        )
    );
    db.drop().await;
}

/// The body the agy headless codec reports as its terminal `Done`, from a real stream.
fn agy_done_body(lines: &[Value]) -> String {
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::stream_json_cli::codec::{AgyCodec, StreamJsonCodec};
    let mut codec = AgyCodec::new();
    let mut out = Vec::new();
    for line in lines {
        out.extend(codec.push_stdout_line(&line.to_string()).unwrap());
    }
    out.extend(codec.finish(Some(0), "").unwrap());
    match out.pop() {
        Some(StreamMessage::Done { result, .. }) => result,
        other => panic!("agy stream did not finish: {other:?}"),
    }
}

/// The transcript the turn bridge commits for one execution: the `Done` body becomes its single
/// `result` event, next to the streamed assistant text and tool steps.
async fn persist_transcript(pool: &sqlx::PgPool, turn_id: &str, provider: &str, results: &[&str]) {
    use crate::db::session_transcripts::{
        PersistSessionTranscript, SessionTranscriptEvent, SessionTranscriptEventKind as Kind,
    };
    let event = |kind, content: &str, status: Option<&str>| SessionTranscriptEvent {
        kind,
        tool_name: None,
        summary: None,
        content: content.to_string(),
        status: status.map(str::to_string),
        is_error: false,
    };
    let mut events = vec![
        event(Kind::Assistant, "progress note", None),
        event(Kind::ToolUse, "ls", None),
    ];
    events.extend(
        results
            .iter()
            .map(|r| event(Kind::Result, r, Some("success"))),
    );
    let channel = turn_id.split(':').nth(1).unwrap();
    let entry = PersistSessionTranscript {
        turn_id,
        session_key: None,
        channel_id: Some(channel),
        agent_id: Some("answer-agent"),
        provider: Some(provider),
        dispatch_id: None,
        user_message: "question",
        assistant_message: "progress note final",
        events: &events,
        duration_ms: Some(1),
        turn_started_at_millis: None,
    };
    assert!(
        crate::db::session_transcripts::persist_turn_db(Some(pool), entry)
            .await
            .unwrap()
    );
}

async fn commit_receipt(
    pool: &sqlx::PgPool,
    turn_id: &str,
    provider: &str,
    origin: &str,
    outcome: crate::db::turns::TerminalOutcome,
) {
    let channel = turn_id.split(':').nth(1).unwrap();
    let ids = vec![origin.to_string()];
    let receipt = crate::db::turns::TurnReceipt {
        turn_id,
        channel_id: channel,
        agent_id: Some("answer-agent"),
        provider,
        session_key: None,
        started_at: None,
        consumed_origin_ids: &ids,
        outcome,
        confirmed_at: chrono::Utc::now(),
    };
    crate::db::turns::record_turn_receipt(pool, &receipt)
        .await
        .unwrap();
}

async fn ask_answer(app: &Router, origin: &str) -> Value {
    let uri = format!("/agents/answer-agent/input-result?origin_id={origin}");
    let (status, body) = local_get(app, &uri).await;
    assert_eq!(status, StatusCode::OK, "{origin}: {body}");
    body
}

/// A completed agy headless execution reports the exact body its terminal `Done` carried, bound
/// to that execution's own transcript record; every other end, or no end yet, reports null.
#[tokio::test(flavor = "current_thread")]
async fn a_completed_agy_execution_reports_its_terminal_answer_pg() {
    use crate::db::turns::TerminalOutcome;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 7_340_801_u64;
    let insert = crate::db::agents::insert_agent_channels_for_tests;
    insert(&pool, "answer-agent", Some(&channel.to_string()), None).await;
    let app = router_with(crate::config::Config::default(), Some(pool.clone()));
    let turn = |message: u64| format!("discord:{channel}:{message}");

    let answer = agy_done_body(&[
        json!({"event": "init", "init": {"conversation_id": "c0ffee00-0000-4000-8000-000000000001"}}),
        json!({"event": "step_update", "step_update": {"step_type": "agent_response",
            "text_delta": "The answer is 42."}}),
        json!({"event": "result", "result": {"status": "SUCCESS",
            "conversation_id": "c0ffee00-0000-4000-8000-000000000001"}}),
    ]);
    assert_eq!(answer, "The answer is 42.");

    // Not ended yet: the transcript may already hold the body, but no terminal is reported.
    write_row("claude", channel, &running_row(10, &["G-running"]));
    persist_transcript(&pool, &turn(10), "antigravity", &[&answer]).await;
    assert_eq!(ask_answer(&app, "G-running").await["terminal"], Value::Null);
    std::fs::remove_file(
        crate::config::runtime_root()
            .unwrap()
            .join(format!("runtime/discord_inflight/claude/{channel}.json")),
    )
    .unwrap();

    commit_receipt(
        &pool,
        &turn(10),
        "antigravity",
        "G-running",
        TerminalOutcome::Completed,
    )
    .await;
    let body = ask_answer(&app, "G-running").await;
    assert_eq!(body["terminal"]["outcome"], "completed", "{body}");
    assert_eq!(
        (
            &body["terminal"]["final_answer_text"],
            &body["terminal"]["final_answer_boundary"],
            &body["terminal"]["final_answer_source"],
        ),
        (
            &json!("The answer is 42."),
            &json!({"turn_id": turn(10), "result_event_index": 2}),
            &json!("agy_terminal_result"),
        ),
        "{body}"
    );

    // Cancelled, failed and empty ends report null, never an empty string, even when the
    // transcript holds a result event.
    let nulls = json!([null, null, null]);
    let answer_fields = |body: &Value| {
        let t = &body["terminal"];
        json!([
            t["final_answer_text"],
            t["final_answer_boundary"],
            t["final_answer_source"]
        ])
    };
    for (message, origin, outcome) in [
        (11, "G-cancelled", TerminalOutcome::Cancelled),
        (12, "G-failed", TerminalOutcome::Failed),
        (13, "G-empty", TerminalOutcome::Empty),
    ] {
        persist_transcript(&pool, &turn(message), "antigravity", &["partial words"]).await;
        commit_receipt(&pool, &turn(message), "antigravity", origin, outcome).await;
        let body = ask_answer(&app, origin).await;
        assert_eq!(body["terminal"]["outcome"], outcome.as_str(), "{body}");
        assert_eq!(answer_fields(&body), nulls, "{origin}: {body}");
    }

    // The boundary is the execution's own record: another turn's answer is never borrowed, two
    // result events are ambiguous, and Claude/Codex wait for their verified final record (#8216).
    persist_transcript(&pool, &turn(20), "antigravity", &["other turn"]).await;
    commit_receipt(
        &pool,
        &turn(21),
        "antigravity",
        "G-no-record",
        TerminalOutcome::Completed,
    )
    .await;
    persist_transcript(&pool, &turn(22), "antigravity", &["first", "second"]).await;
    commit_receipt(
        &pool,
        &turn(22),
        "antigravity",
        "G-two-results",
        TerminalOutcome::Completed,
    )
    .await;
    persist_transcript(&pool, &turn(23), "claude", &["claude final"]).await;
    commit_receipt(
        &pool,
        &turn(23),
        "claude",
        "G-claude",
        TerminalOutcome::Completed,
    )
    .await;
    for origin in ["G-no-record", "G-two-results", "G-claude"] {
        let body = ask_answer(&app, origin).await;
        assert_eq!(body["terminal"]["outcome"], "completed", "{body}");
        assert_eq!(answer_fields(&body), nulls, "{origin}: {body}");
    }

    // Retention moves the transcript; the archived record still answers.
    sqlx::query(
        "INSERT INTO session_transcripts_archive (id, turn_id, channel_id, provider, events_json)
         SELECT id, turn_id, channel_id, provider, events_json FROM session_transcripts
          WHERE turn_id = $1",
    )
    .bind(turn(10))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM session_transcripts WHERE turn_id = $1")
        .bind(turn(10))
        .execute(&pool)
        .await
        .unwrap();
    let body = ask_answer(&app, "G-running").await;
    assert_eq!(
        body["terminal"]["final_answer_text"], "The answer is 42.",
        "{body}"
    );

    pool.close().await;
    db.drop().await;
}
