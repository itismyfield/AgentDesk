use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::placeholder_controller::PlaceholderController;
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::discord::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder};
use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::cluster::intake_router_hook::owner_record::output;
use crate::services::discord::SharedData;
use crate::services::discord::formatting::rollback_transport_test_hook;
use crate::services::discord::gateway::DiscordGateway;
use crate::services::discord::outbound::turn_output_controller::*;
use crate::services::provider::ProviderKind;
use adk_test_support::test_support::wait::{WAIT_CAP, join_within};
use std::future::{Future, poll_fn};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, RecvTimeoutError},
};
use tokio::time::{Duration, Instant};

#[derive(Default)]
struct PacedRequests {
    io_complete: Option<mpsc::Sender<()>>,
    deadlines: Vec<Instant>,
}

impl PacedRequests {
    fn keep_clock_still_during_io(&mut self) {
        if self.io_complete.is_none() {
            // A pending blocking task inhibits Tokio's automatic clock advancement.
            let (complete, wait) = mpsc::channel();
            drop(tokio::task::spawn_blocking(move || {
                let _ = wait.recv_timeout(WAIT_CAP * 2);
            }));
            self.io_complete = Some(complete);
        }
    }
}

async fn drive_paced_delivery<F: Future>(
    delivery: F,
    shared: &Arc<SharedData>,
    channel: ChannelId,
    chunks: usize,
) -> F::Output {
    let (cancel_wall_limit, wait) = mpsc::channel::<()>();
    let (wall_result, mut wall_limit) = tokio::sync::oneshot::channel();
    let watchdog = std::thread::spawn(move || {
        let _ = wall_result.send(wait.recv_timeout(WAIT_CAP));
    });
    let requests = Arc::new(Mutex::new(PacedRequests::default()));
    requests.lock().unwrap().keep_clock_still_during_io();
    let observed = requests.clone();
    let pacing = shared.clone();
    // The fixture holds the shared test-env lock. Observe before PG or HTTP can yield.
    let _hook = rollback_transport_test_hook::install(
        Box::new(move |seen_channel, _, _, _, _| {
            if seen_channel == channel {
                let deadline = *pacing
                    .api_timestamps
                    .get(&channel)
                    .expect("each chunk needs a pacing deadline");
                assert!(Instant::now() >= deadline, "POST began before its deadline");
                let mut observed = observed.lock().unwrap();
                observed.deadlines.push(deadline);
                observed.keep_clock_still_during_io();
            }
            None
        }),
        Box::new(|_, _| None),
    );
    let initial_target = shared.api_timestamps.get(&channel).map(|entry| *entry);
    tokio::time::pause();
    let mut delivery = std::pin::pin!(delivery);
    let paced_delivery = poll_fn(|cx| {
        let outcome = delivery.as_mut().poll(cx);
        if outcome.is_pending()
            && let Some(target) = shared.api_timestamps.get(&channel).map(|entry| *entry)
        {
            let mut requests = requests.lock().unwrap();
            let entered = requests.deadlines.last().copied().or(initial_target);
            if entered != Some(target) {
                // Let Tokio wake the actual pacing timer. The POST hook freezes I/O again.
                drop(requests.io_complete.take());
            }
        }
        outcome
    });
    let result = tokio::select! {
        result = paced_delivery => result,
        _ = &mut wall_limit => panic!("paced delivery exceeded the real-time test limit"),
    };
    tokio::time::resume();
    drop(cancel_wall_limit);
    assert_eq!(
        wall_limit.await.unwrap(),
        Err(RecvTimeoutError::Disconnected)
    );
    join_within(watchdog, WAIT_CAP);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.deadlines.len(), chunks);
    assert!(
        requests
            .deadlines
            .windows(2)
            .all(|pair| pair[1].duration_since(pair[0]) >= Duration::from_secs(1)),
        "each chunk needs a full second of pacing"
    );
    result
}

#[tokio::test]
async fn output_origin_real_gateway_seventy_paced_chunks_commit_pg() {
    let (_db, pool, _origin) = output::tests::fixture("7336950").await;
    let posts = Arc::new(AtomicUsize::new(0));
    let recorded = posts.clone();
    let bytes = Arc::new(AtomicUsize::new(0));
    let received = bytes.clone();
    let checking_pool = pool.clone();
    let app = axum::Router::new().route("/api/v10/{*path}", axum::routing::any(move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
        let checking_pool = checking_pool.clone();
        let received = received.clone();
        let n = if method == axum::http::Method::POST { recorded.fetch_add(1, Ordering::SeqCst) + 1 } else { 0 };
        async move {
            if method == axum::http::Method::GET {
                eprintln!("ancillary read {}", uri.path());
                let response = if uri.path().ends_with("/messages") { serde_json::json!([]) }
                    else { serde_json::json!({"id":"7336950", "type":0, "name":"fixture", "username":"bot", "discriminator":"0001", "avatar":null, "bot":true}) };
                return (axum::http::StatusCode::OK, axum::Json(response));
            }
            if method == axum::http::Method::DELETE {
                return (axum::http::StatusCode::NO_CONTENT, axum::Json(serde_json::Value::Null));
            }
            assert_eq!(method, axum::http::Method::POST);
            let mut tx = checking_pool.begin().await.unwrap();
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new("claude", "7336950").advisory_key())
                .fetch_one(&mut *tx).await.unwrap();
            assert!(!free, "the actual raw HTTP future must own the PG guard");
            tx.rollback().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            received.fetch_add(body["content"].as_str().unwrap().len(), Ordering::SeqCst);
            (axum::http::StatusCode::OK, axum::Json(serde_json::json!({
                "id": (1000+n).to_string(), "channel_id":"7336950", "content":body["content"],
                "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                "timestamp":"2026-10-28T00:00:00+00:00","edited_timestamp":null,
                "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                "attachments":[],"embeds":[],"pinned":false,"type":0
            })))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let gateway: Arc<dyn TurnGateway> = Arc::new(DiscordGateway::new(
        http.clone(),
        shared.clone(),
        ProviderKind::Claude,
        None,
    ));
    let bridge =
        crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway(
            http,
            shared.clone(),
        );
    let channel = ChannelId::new(7336950);
    for (kind, gateway) in [("discord", gateway), ("bridge", bridge)] {
        for chunks in [1, 70] {
            let body = if chunks == 70 {
                "x".repeat(139300)
            } else {
                "one chunk".into()
            };
            assert_eq!(
                crate::services::discord::formatting::split_message(&body).len(),
                chunks
            );
            let lease = DeliveryLeaseCell::new(channel);
            let controller = PlaceholderController::default();
            let advanced = AtomicUsize::new(0);
            let advance = |_| {
                advanced.fetch_add(1, Ordering::SeqCst);
                true
            };
            let started = tokio::time::Instant::now();
            let turn = TurnKey::new(channel, 90001, chunks as u64);
            let delivery = deliver_turn_output(
                gateway.as_ref(),
                TurnOutputCtx {
                    turn,
                    lease_key: Some(DeliveryLeaseKey::from_turn_key(turn)),
                    owner: RelayOwnerKind::Watcher,
                    holder: LeaseHolder::Watcher { instance_id: 1 },
                    lease: &lease,
                    channel_id: channel,
                    placeholder_controller: &controller,
                    placeholder: PlaceholderSlot::None,
                    body: &body,
                    send_range: (0, body.len() as u64),
                    plan: OutputPlan::SendNewChunks {
                        chunk_count: chunks,
                        delete_anchor: false,
                    },
                    edit_fail_policy: EditFailPlaceholderPolicy::DeleteIfProvenStale,
                    fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
                    acquire_failure_mode: AcquireFailureMode::Transient,
                    advance: Some(&advance),
                    heartbeat: None,
                    body_claim: None,
                },
            );
            let outcome = if chunks == 70 {
                drive_paced_delivery(delivery, &shared, channel, chunks).await
            } else {
                delivery.await
            };
            eprintln!(
                "paced case {kind} chunks={chunks} posts={} elapsed={:?} delivered={}",
                posts.load(Ordering::SeqCst),
                started.elapsed(),
                matches!(outcome, DeliveryOutcome::Delivered { .. })
            );
            assert!(
                matches!(outcome, DeliveryOutcome::Delivered { committed_to, .. } if committed_to == body.len() as u64),
                "all raw chunks must reach terminal commit"
            );
            assert_eq!(
                advanced.load(Ordering::SeqCst),
                1,
                "terminal commit must advance once"
            );
            if chunks == 70 {
                assert!(
                    started.elapsed() >= Duration::from_secs(69),
                    "production pacing must run"
                );
            }
        }
    }
    assert_eq!(posts.load(Ordering::SeqCst), 142);
    assert_eq!(
        bytes.load(Ordering::SeqCst),
        2 * (139300 + "one chunk".len())
    );
    task.abort();
}

#[tokio::test]
async fn output_origin_real_gateway_rechecks_after_pacing_and_blocks_rollback_pg() {
    let (_db, pool, origin) = output::tests::fixture("7336951").await;
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = methods.clone();
    let (sent, first_post) = tokio::sync::oneshot::channel();
    let sent = Arc::new(std::sync::Mutex::new(Some(sent)));
    let app = axum::Router::new().route(
        "/api/v10/{*path}",
        axum::routing::any(move |method: axum::http::Method, body: axum::body::Bytes| {
            recorded.lock().unwrap().push(method.to_string());
            if let Some(sent) = sent.lock().unwrap().take() {
                sent.send(()).unwrap();
            }
            async move {
                if method == axum::http::Method::DELETE {
                    return (
                        axum::http::StatusCode::NO_CONTENT,
                        axum::Json(serde_json::Value::Null),
                    );
                }
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "id":"2001", "channel_id":"7336951", "content":body["content"],
                        "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                        "timestamp":"2026-10-28T00:00:00+00:00","edited_timestamp":null,
                        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                        "attachments":[],"embeds":[],"pinned":false,"type":0
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let gateway = DiscordGateway::new(http, shared, ProviderKind::Claude, None);
    let channel = ChannelId::new(7336951);
    let body = "x".repeat(5000);
    let artifact = origin.seal(&body);
    let revoked_pool = pool.clone();
    let revocation = tokio::spawn(async move {
        first_post.await.unwrap();
        output::tests::revoke(&revoked_pool, &origin).await;
    });
    let guarded =
        crate::services::discord::outbound::turn_output_controller::transport::OriginGateway {
            inner: &gateway,
            scope: Some(output::Scope {
                pool: pool.clone(),
                provider: "claude".into(),
            }),
            origin: Some(&artifact),
            body: &body,
        };
    assert!(
        guarded
            .send_long_message_with_rollback(channel, MessageId::new(1), &body)
            .await
            .is_err()
    );
    revocation.await.unwrap();
    assert_eq!(
        *methods.lock().unwrap(),
        ["POST"],
        "next POST and rollback DELETE must recheck generation"
    );
    assert!(
        guarded
            .edit_message(channel, MessageId::new(2001), "answer")
            .await
            .is_err()
    );
    assert!(
        guarded
            .delete_message(channel, MessageId::new(2001))
            .await
            .is_err()
    );
    assert_eq!(*methods.lock().unwrap(), ["POST"]);
    server.abort();
}

#[tokio::test]
async fn output_origin_real_gateway_fallback_rollback_and_edit_raw_guards_pg() {
    let (_db, pool, origin) = output::tests::fixture("7336954").await;
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = methods.clone();
    let posts = Arc::new(AtomicUsize::new(0));
    let checking_pool = pool.clone();
    let app = axum::Router::new().route("/api/v10/{*path}", axum::routing::any(move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
        recorded.lock().unwrap().push(method.to_string());
        let checking_pool = checking_pool.clone();
        let posts = posts.clone();
        async move {
            let mut tx = checking_pool.begin().await.unwrap();
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new("claude", "7336954").advisory_key())
                .fetch_one(&mut *tx).await.unwrap();
            assert!(!free, "fallback, rollback and retry must hold the owner key during HTTP");
            tx.rollback().await.unwrap();
            if method == axum::http::Method::DELETE {
                return (axum::http::StatusCode::NO_CONTENT, axum::Json(serde_json::Value::Null));
            }
            let n = if method == axum::http::Method::POST { posts.fetch_add(1, Ordering::SeqCst) + 1 } else { 0 };
            if (method == axum::http::Method::PATCH && uri.path().ends_with("/1")) || n == 3 {
                return (axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"code":50035,"message":"fixture request rejected"})));
            }
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let id = if method == axum::http::Method::PATCH { uri.path().rsplit('/').next().unwrap().to_owned() } else { (3000+n).to_string() };
            (axum::http::StatusCode::OK, axum::Json(serde_json::json!({
                "id": id, "channel_id":"7336954", "content":body["content"],
                "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                "timestamp":"2026-10-28T00:00:00+00:00","edited_timestamp":null,
                "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                "attachments":[],"embeds":[],"pinned":false,"type":0
            })))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let gateway = DiscordGateway::new(http, shared, ProviderKind::Claude, None);
    let channel = ChannelId::new(7336954);
    let artifact = origin.seal("answer");
    let scope = output::Scope {
        pool,
        provider: "claude".into(),
    };
    let guarded =
        crate::services::discord::outbound::turn_output_controller::transport::OriginGateway {
            inner: &gateway,
            scope: Some(scope.clone()),
            origin: Some(&artifact),
            body: "answer",
        };
    assert!(matches!(guarded.replace_message_with_outcome(channel, MessageId::new(1), "answer").await.unwrap(),
        crate::services::discord::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. }));
    guarded
        .delete_message(channel, MessageId::new(3001))
        .await
        .unwrap();
    let body = "x".repeat(3980);
    let artifact = origin.seal(&body);
    let guarded =
        crate::services::discord::outbound::turn_output_controller::transport::OriginGateway {
            inner: &gateway,
            scope: Some(scope),
            origin: Some(&artifact),
            body: &body,
        };
    assert!(
        guarded
            .send_long_message_with_rollback(channel, MessageId::new(9), &body)
            .await
            .is_err()
    );
    guarded
        .edit_message(channel, MessageId::new(3002), "edited")
        .await
        .unwrap();
    assert_eq!(
        *methods.lock().unwrap(),
        ["PATCH", "POST", "DELETE", "POST", "POST", "DELETE", "PATCH"]
    );
    server.abort();
}

mod ready_without_output_rehydration_tests {
    use std::sync::Arc;

    use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};

    use crate::services::discord::recovery_engine::RecoveryDeliveryContext;
use crate::services::discord::recovery_engine::restore_inflight::ready_without_output_fixture_tests::Fixture;
use crate::services::discord::recovery_paths::shared::RecoveryRelayOutcome;
use crate::services::discord::{inflight, mailbox_snapshot, make_shared_data_for_tests};
use crate::services::provider::ProviderKind;

    #[tokio::test(flavor = "current_thread")]
    async fn partial_eof_actual_controller_preserves_frozen_prefix_and_streamed_current_anchor() {
        let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        use crate::services::agent_protocol::RuntimeHandoffKind;
        use crate::services::discord::formatting::ReplaceLongMessageOutcome;
        use crate::services::discord::outbound::delivery_record as dr;
        use crate::services::discord::recovery_paths::controller_cutover::{
            deliver_recovery_replace_via_controller, tests::RecoveryFakeGateway,
        };
        use crate::services::tui_prompt_dedupe as dedupe;
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        for typed_provider in [None, Some(ProviderKind::Claude), Some(ProviderKind::Codex)] {
            let typed = typed_provider.is_some();
            let native = typed_provider == Some(ProviderKind::Codex);
            let provider = typed_provider.unwrap_or(ProviderKind::Claude);
            let runtime = if native {
                RuntimeHandoffKind::CodexTui
            } else {
                RuntimeHandoffKind::ClaudeTui
            };
            #[cfg(not(unix))]
            if typed {
                continue;
            }
            let root = tempfile::tempdir().expect("runtime root");
            let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_ROOT_DIR",
                root.path(),
            );
            let mut fixture = Fixture::new(5_071_805, root, env, make_shared_data_for_tests());
            fixture.state.provider = provider.as_str().into();
            if native {
                fixture.state.turn_source = inflight::TurnSource::ExternalInput;
                fixture.state.request_owner_user_id = 1;
                fixture.state.injected_prompt_message_id = Some(fixture.state.user_msg_id);
                fixture.state.external_turn_id = Some("native-retained-original".into());
                fixture.state.relay_owner_kind = inflight::RelayOwnerKind::None;
                fixture.state.session_id = None;
            }
            fixture.state.streaming_rollover_frozen_msg_ids = vec![40];
            fixture.state.current_msg_id = 41;
            // Rollover froze prefix in 40; 41 already shows the beginning of its suffix.
            // A normal streaming edit does not advance response_sent_offset.
            let mut messages = std::collections::BTreeMap::from([
                (MessageId::new(40), "published prefix".to_string()),
                (MessageId::new(41), "unposted".to_string()),
            ]);
            if typed {
                fixture.state.runtime_kind = Some(runtime);
                fixture.state.turn_nonce = Some("typed-prefix-recovery".into());
                let assistant = serde_json::json!({"type":"assistant", "sessionId":fixture.state.session_id, "message":{"content":[
            {"type":"text", "text":fixture.state.full_response}]}});
                let terminal = serde_json::json!({"type":"result", "session_id":fixture.state.session_id, "subtype":"success", "result":fixture.state.full_response});
                let (assistant, terminal) = if native {
                    (
                        serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":fixture.state.full_response}]}}),
                        serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":fixture.state.full_response}}),
                    )
                } else {
                    (assistant, terminal)
                };
                let output = fixture.state.output_path.as_ref().unwrap();
                std::fs::write(output, format!("{assistant}\n{terminal}\n")).unwrap();
                fixture.state.last_offset = std::fs::metadata(output).unwrap().len();
                let tmux = fixture.state.tmux_session_name.as_ref().unwrap();
                std::fs::write(
                    crate::services::tmux_common::session_temp_path(tmux, "generation"),
                    b"typed-prefix",
                )
                .unwrap();
                dedupe::register_tmux_runtime_binding(
                    tmux,
                    dedupe::TuiRuntimeBinding {
                        runtime_kind: runtime,
                        output_path: output.clone(),
                        relay_output_path: None,
                        input_fifo_path: None,
                        session_id: fixture.state.session_id.clone(),
                        last_offset: 0,
                        relay_last_offset: None,
                    },
                );
            }
            fixture.claim().await;
            fixture.state = fixture.load().unwrap();
            let channel = ChannelId::new(fixture.state.channel_id);
            let original_actor = mailbox_snapshot(&fixture.shared, channel)
                .await
                .cancel_token
                .unwrap();
            let source: Option<dr::ExactJsonlSourceIdentity> = if typed {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let output = fixture.state.output_path.clone().unwrap();
                    let file = std::fs::File::open(&output).unwrap();
                    let metadata = file.metadata().unwrap();
                    let tmux = fixture.state.tmux_session_name.clone().unwrap();
                    let raw = fixture.state.full_response.clone();
                    let frame = crate::services::discord::StreamMessage::ClaudeTuiTerminalDone {
                        result: raw.clone(),
                        session_id: fixture.state.session_id.clone(),
                        transcript_path: std::fs::canonicalize(&output)
                            .unwrap()
                            .display()
                            .to_string(),
                        tmux_session_name: tmux.clone(),
                        turn_nonce: fixture.state.turn_nonce.clone().unwrap(),
                        source_start: 0,
                        complete_record_end: metadata.len(),
                        generation_mtime_ns: dr::current_generation_mtime_ns(&tmux),
                        source_file_dev: metadata.dev(),
                        source_file_ino: metadata.ino(),
                        actor: Arc::downgrade(&original_actor),
                        kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
                    };
                    let frame = if native {
                        crate::services::discord::StreamMessage::CodexTuiTerminalDone {
                            result: raw.clone(),
                            session_id: fixture.state.session_id.clone(),
                            rollout_path: std::fs::canonicalize(&output)
                                .unwrap()
                                .display()
                                .to_string(),
                            tmux_session_name: tmux.clone(),
                            turn_nonce: fixture.state.turn_nonce.clone().unwrap(),
                            source_start: 0,
                            complete_record_end: metadata.len(),
                            captured_source: Some(
                                crate::services::agent_protocol::CapturedTuiTerminalSource {
                                    generation_mtime_ns: dr::current_generation_mtime_ns(&tmux),
                                    source_file_dev: metadata.dev(),
                                    source_file_ino: metadata.ino(),
                                    actor: Arc::downgrade(&original_actor),
                                },
                            ),
                            kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
                        }
                    } else {
                        frame
                    };
                    let mut baseline = fixture.state.clone();
                    let identity = inflight::InflightTurnIdentity::from_state(&fixture.state);
                    let (_, admitted, _, _) = fixture
                        .state
                        .admit_tui_terminal_frame(
                            &mut baseline,
                            &identity,
                            true,
                            (&fixture.shared, &original_actor),
                            &raw,
                            frame,
                        )
                        .await
                        .expect("the real retained source admits with a nonzero sent prefix");
                    let admitted = admitted.expect("typed source range");
                    assert_eq!(admitted.result, raw);
                    assert_eq!(admitted.source.range, (0, metadata.len()));
                    assert_eq!(
                        fixture.load().unwrap().response_sent_offset,
                        "published prefix\n".len()
                    );
                    assert!(fixture.state.requires_pinned_terminal_recovery());
                    Some(admitted.source)
                }
                #[cfg(not(unix))]
                {
                    unreachable!()
                }
            } else {
                None
            };
            #[cfg(unix)]
            if native {
                // The admitted body survives a process loss; later raw turns are not
                // part of its saved terminal range or its Discord receipt.
                drop(original_actor);
                fixture.shared =
                    crate::services::discord::make_shared_data_for_tests_with_storage(None);
                fixture.state = fixture.load().unwrap();
                let retained = fixture.state.clone();
                let mut legacy = retained.clone();
                legacy.tui_terminal_source_file_identity = None;
                assert!(
                    !legacy.requires_pinned_terminal_recovery(),
                    "legacy Codex generation-only terminals retain their previous path"
                );
                let output = std::path::PathBuf::from(retained.output_path.as_ref().unwrap());
                let next_raw = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"next turn\"}]}}\n";
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&output)
                    .unwrap()
                    .write_all(next_raw.as_bytes())
                    .unwrap();
                let tmux = retained.tmux_session_name.as_deref().unwrap();
                dedupe::clear_tmux_runtime_binding(tmux);
                let rebuilt = crate::services::discord::tui_prompt_relay::rehydration::codex_tui_rehydrated_binding_from_rollout_path(
                tmux, &output, retained.session_id.clone(),
            ).unwrap();
                assert!(
                    rebuilt.last_offset > retained.last_offset,
                    "startup observes EOF including the next turn"
                );
                dedupe::register_rehydrated_tmux_runtime_binding(
                    provider.as_str(),
                    tmux,
                    channel.get(),
                    rebuilt,
                );
                let expected_file = std::fs::read(&output).unwrap();
                let gateway =
                    RecoveryFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, true);
                let http = Arc::new(serenity::Http::new("Bot test-token"));
                assert!(crate::services::discord::recovery_engine::recover_idle_partial_response_from_ready_source(
                &http, &fixture.shared, &retained, &output, &gateway,
            ).await, "the actual dormant recovery publishes the saved native range after restart and append");
                let source = source.unwrap();
                assert!(dr::confirmed_delivery_receipt_exists(
                    &provider, channel, 41, &source
                ));
                assert_eq!(
                    dr::read_record(&provider, channel.get())
                        .unwrap()
                        .confirmed_deliveries
                        .len(),
                    1
                );
                assert!(fixture.load().is_none());
                assert_eq!(gateway.replacements.lock().unwrap().len(), 1);
                assert_eq!(
                    gateway.replacements.lock().unwrap()[0].1,
                    crate::services::discord::formatting::format_for_discord_with_provider(
                        "unposted suffix",
                        &provider
                    )
                );
                let mut next = retained.clone();
                next.user_msg_id += 10;
                next.turn_nonce = Some("native-next-turn".into());
                next.full_response = "NEXT_RESPONSE".into();
                next.response_sent_offset = 0;
                inflight::save_inflight_state(&next).unwrap();
                let before = fixture.load().unwrap();
                assert!(!crate::services::discord::recovery_engine::recover_idle_partial_response_from_ready_source(
                &http, &fixture.shared, &retained, &output, &gateway,
            ).await, "the old captured source cannot deliver again over a successor");
                assert_eq!(fixture.load().unwrap().turn_nonce, before.turn_nonce);
                assert_eq!(gateway.replacements.lock().unwrap().len(), 1);
                assert_eq!(std::fs::read(&output).unwrap(), expected_file);
                continue;
            }
            let gateway = RecoveryFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, true)
                .before_replace_returns({
                    let shared = fixture.shared.clone();
                    let actor = original_actor.clone();
                    move || {
                        Box::pin(async move {
                            assert!(
                                mailbox_snapshot(&shared, channel)
                                    .await
                                    .cancel_token
                                    .as_ref()
                                    .is_some_and(|current| Arc::ptr_eq(current, &actor)),
                                "the original actor owns the turn during actual gateway transport"
                            );
                        })
                    }
                });
            let http = Arc::new(serenity::Http::new("Bot test-token"));
            let context = RecoveryDeliveryContext::from_state(
                &fixture.shared,
                &ProviderKind::Claude,
                &fixture.state,
                None,
                fixture.shared.restart.current_generation,
            );
            let settled = fixture
            .settle(&fixture.state, |text| {
                let gateway = &gateway;
                let shared = &fixture.shared;
                let http = &http;
                let context = context.as_ref();
                let state = &fixture.state;
                async move {
                    if typed {
                        return crate::services::discord::recovery_engine::completion_delivery::relay_captured_recovery_terminal_notice_with_gateway(
                            http, shared, &ProviderKind::Claude, state, &text, gateway,
                        ).await;
                    }
                    deliver_recovery_replace_via_controller(
                        gateway,
                        shared,
                        &ProviderKind::Claude,
                        http,
                        channel,
                        MessageId::new(41),
                        &text,
                        context,
                        None,
                    )
                    .await.into()
                }
            },)
            .await;
            assert!(
                settled,
                "typed={typed}: captured recovery settles the original actor"
            );
            let replacements = gateway
                .replacements
                .lock()
                .expect("actual controller transport calls")
                .clone();
            let expected = crate::services::discord::formatting::format_for_discord_with_provider(
                &fixture.state.full_response[fixture.state.response_sent_offset..],
                &ProviderKind::Claude,
            );
            assert_eq!(
                replacements,
                vec![(MessageId::new(41), expected)],
                "typed={typed}: gateway receives exactly the undelivered suffix"
            );
            for (message, body) in replacements.iter() {
                messages.insert(*message, body.clone());
            }
            assert_eq!(
                messages.get(&MessageId::new(40)).unwrap(),
                "published prefix"
            );
            assert_eq!(
                messages.get(&MessageId::new(41)).unwrap(),
                &crate::services::discord::formatting::format_for_discord_with_provider(
                    "unposted suffix",
                    &ProviderKind::Claude,
                )
            );
            if let Some(source) = source {
                let record = dr::read_record(&ProviderKind::Claude, channel.get()).unwrap();
                assert_eq!(record.confirmed_deliveries.len(), 1);
                assert!(
                    dr::confirmed_delivery_receipt_exists(
                        &ProviderKind::Claude,
                        channel,
                        41,
                        &source
                    ),
                    "the suffix publication confirms the original whole-source range"
                );
            }
            assert!(fixture.load().is_none());
            assert!(
                mailbox_snapshot(&fixture.shared, channel)
                    .await
                    .cancel_token
                    .is_none()
            );
            let mut next = fixture.state.clone();
            next.user_msg_id += 10;
            next.turn_nonce = Some("next-prefix-input".into());
            assert!(fixture.next_input_claims(&next).await);
        }
    }
}
