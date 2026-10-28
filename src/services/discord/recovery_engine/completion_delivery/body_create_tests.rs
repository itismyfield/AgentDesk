use super::*;
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::discord::{gateway::TurnGateway, mailbox_try_start_turn};
use crate::services::provider::CancelToken;
use crate::services::tui_prompt_dedupe as dedupe;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

type PostHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

struct Remote {
    http: Arc<serenity::Http>,
    calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    messages: Arc<Mutex<Vec<serde_json::Value>>>,
    fault: Arc<Mutex<Option<(usize, bool)>>>,
    on_post: PostHook,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Remote {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Remote {
    async fn new(channel: u64) -> Self {
        use axum::{
            Json, Router,
            body::Bytes,
            http::{Method, Uri},
            response::IntoResponse,
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let fault = Arc::new(Mutex::new(None::<(usize, bool)>));
        let on_post = Arc::new(Mutex::new(None::<Box<dyn FnOnce() + Send>>));
        let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (log, store, fail, hook) = (
            calls.clone(),
            messages.clone(),
            fault.clone(),
            on_post.clone(),
        );
        let router = Router::new().fallback(move |method: Method, uri: Uri, bytes: Bytes| {
            let (log, store, fail, hook, posts) = (log.clone(), store.clone(), fail.clone(), hook.clone(), posts.clone());
            async move {
                let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
                log.lock().unwrap().push((format!("{method} {}", uri.path()), payload.clone()));
                let mut messages = store.lock().unwrap();
                if method == Method::POST {
                    let number = posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    let fault = {
                        let mut fail = fail.lock().unwrap();
                        if fail.as_ref().is_some_and(|(n,_)| *n == number) { fail.take() } else { None }
                    };
                    if fault == Some((number, false)) { return (axum::http::StatusCode::OK, "lost reply").into_response(); }
                    let old = if payload["enforce_nonce"] == true { messages.iter().find(|m|m["nonce"] == payload["nonce"]).cloned() } else { None };
                    let message = match old {
                        Some(old) => old,
                        None => {
                            let message = serde_json::json!({
                                "id": (750_700 + messages.len() as u64).to_string(), "channel_id":channel.to_string(),
                                "content": payload["content"], "nonce":payload["nonce"], "message_reference":payload.get("message_reference"),
                                "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null,"bot":true},
                                "timestamp":"2026-10-28T00:00:00+00:00", "edited_timestamp":null,
                                "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                                "attachments":[],"embeds":[],"pinned":false,"type":0
                            });
                            messages.push(message.clone()); message
                        }
                    };
                    if let Some(mutate) = hook.lock().unwrap().take() { mutate(); }
                    if fault.is_some() { return (axum::http::StatusCode::OK, "lost reply").into_response(); }
                    return Json(message).into_response();
                }
                let id = uri.path().rsplit('/').next().unwrap_or("");
                if let Some(message) = messages.iter_mut().find(|m| m["id"] == id) {
                    if method == Method::PATCH { message["content"] = payload["content"].clone(); }
                    return Json(message.clone()).into_response();
                }
                if uri.path().ends_with("/users/@me") {
                    return Json(serde_json::json!({"id":"1","username":"bot","discriminator":"0001","avatar":null,"bot":true})).into_response();
                }
                axum::http::StatusCode::NOT_FOUND.into_response()
            }
        });
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", socket.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let task = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
        Self {
            http,
            calls,
            messages,
            fault,
            on_post,
            task,
        }
    }
}

async fn admitted(
    root: &std::path::Path,
    channel: u64,
    body: &str,
    current: u64,
) -> inflight::InflightTurnState {
    admitted_provider(root, channel, body, current, ProviderKind::Claude).await
}
async fn admitted_provider(
    root: &std::path::Path,
    channel: u64,
    body: &str,
    current: u64,
    provider: ProviderKind,
) -> inflight::InflightTurnState {
    admitted_episode(root, channel, body, current, provider, 0).await
}
async fn admitted_episode(
    root: &std::path::Path,
    channel: u64,
    body: &str,
    current: u64,
    provider: ProviderKind,
    episode: u64,
) -> inflight::InflightTurnState {
    admitted_episode_kind(root, channel, body, current, provider, episode, false).await
}

async fn admitted_episode_kind(
    root: &std::path::Path,
    channel: u64,
    body: &str,
    current: u64,
    provider: ProviderKind,
    episode: u64,
    synthetic: bool,
) -> inflight::InflightTurnState {
    let shared = make_shared_data_for_tests();
    let kind = if provider == ProviderKind::Codex {
        RuntimeHandoffKind::CodexTui
    } else {
        RuntimeHandoffKind::ClaudeTui
    };
    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(format!(
        "body-{channel}-{episode}"
    ))));
    let tmux = format!("test-body-create-{channel}");
    let path = root.join(format!("{channel}.jsonl"));
    std::fs::write(
        &path,
        if synthetic && provider == ProviderKind::Codex {
            "{\"type\":\"turn.completed\"}\n".to_string()
        } else {
            format!(
                "{{\"type\":\"result\",\"result\":{}}}\n",
                serde_json::to_string(body).unwrap()
            )
        },
    )
    .unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    let generation_path = crate::services::tmux_common::session_temp_path(&tmux, "generation");
    std::fs::create_dir_all(std::path::Path::new(&generation_path).parent().unwrap()).unwrap();
    std::fs::write(&generation_path, "1").unwrap();
    let generation =
        super::super::super::outbound::delivery_record::current_generation_mtime_ns(&tmux);
    dedupe::register_tmux_runtime_binding(
        &tmux,
        dedupe::TuiRuntimeBinding {
            runtime_kind: kind,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        channel,
        None,
        7,
        channel + episode + 1,
        current,
        "prompt".into(),
        None,
        Some(tmux.clone()),
        Some(path.display().to_string()),
        None,
        0,
    );
    row.runtime_kind = Some(kind);
    row.turn_nonce = actor.turn_nonce().map(str::to_owned);
    row.turn_start_offset = Some(0);
    if synthetic {
        row.request_owner_user_id =
            crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
        row.turn_source = inflight::TurnSource::ExternalInput;
        row.external_turn_id = Some(format!("idle-{channel}"));
        row.injected_prompt_message_id = Some(row.user_msg_id);
    }
    mailbox_try_start_turn(
        &shared,
        ChannelId::new(channel),
        actor.clone(),
        UserId::new(row.request_owner_user_id),
        MessageId::new(row.user_msg_id),
    )
    .await;
    inflight::save_inflight_state(&row).unwrap();
    let mut baseline = inflight::load_inflight_state(&provider, channel).unwrap();
    row = baseline.clone();
    let identity = inflight::InflightTurnIdentity::from_state(&row);
    let frame = if provider == ProviderKind::Codex {
        StreamMessage::CodexTuiTerminalDone {
            result: body.into(),
            session_id: None,
            rollout_path: path.display().to_string(),
            tmux_session_name: tmux.clone(),
            turn_nonce: row.turn_nonce.clone().unwrap(),
            source_start: 0,
            complete_record_end: metadata.len(),
            kind: NativeTerminalKind::Completed,
            captured_source: Some(crate::services::agent_protocol::CapturedTuiTerminalSource {
                generation_mtime_ns: generation,
                source_file_dev: metadata.dev(),
                source_file_ino: metadata.ino(),
                actor: Arc::downgrade(&actor),
            }),
        }
    } else {
        StreamMessage::ClaudeTuiTerminalDone {
            result: body.into(),
            session_id: None,
            transcript_path: path.display().to_string(),
            tmux_session_name: tmux,
            turn_nonce: row.turn_nonce.clone().unwrap(),
            source_start: 0,
            complete_record_end: metadata.len(),
            generation_mtime_ns: generation,
            source_file_dev: metadata.dev(),
            source_file_ino: metadata.ino(),
            actor: Arc::downgrade(&actor),
            kind: NativeTerminalKind::Completed,
        }
    };
    let (_, proof, _, _) = row
        .admit_tui_terminal_frame(
            &mut baseline,
            &identity,
            true,
            (&shared, &actor),
            body,
            frame,
        )
        .await
        .expect("actual captured admission");
    assert!(proof.is_some());
    inflight::load_inflight_state(&provider, channel).unwrap()
}

fn a2_idle_ready_case(name: &str, nonzero: bool) {
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for (index, provider) in [ProviderKind::Claude, ProviderKind::Codex].into_iter().enumerate() {
            let channel = 75_072_820 + index as u64 + u64::from(nonzero) * 10;
            let remote = Remote::new(channel).await;
            let shared = runtime(2);
            let gateway = DiscordGateway::new(remote.http.clone(), shared.clone(), provider.clone(), None);
            let current = if nonzero {
                gateway.send_message(ChannelId::new(channel), "⠋ 계속 처리 중").await.unwrap().get()
            } else { 0 };
            let row = admitted_episode_kind(root.path(), channel, "idle native answer🙂", current, provider.clone(), 0, true).await;
            let output = std::path::Path::new(row.output_path.as_ref().unwrap());
            assert!(row.requires_pinned_terminal_recovery());
            assert_eq!(crate::services::tui_turn_state::jsonl_ready_for_input(&provider, row.runtime_kind, output, Some(row.last_offset)),
                Some(crate::services::tui_turn_state::TuiReadyState::Ready), "positive readiness premise");
            assert!(!crate::services::tui_turn_state::pane_ready_fallback_allowed(&provider, row.runtime_kind));
            let before = remote.calls.lock().unwrap().len();
            assert!(super::super::idle_captured_response::recover_idle_partial_response(&remote.http, &shared, &row, output).await,
                "the public idle entry must settle a ready native source");
            let messages = remote.messages.lock().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["content"], "idle native answer🙂");
            let actual = messages[0]["id"].as_str().unwrap().parse::<u64>().unwrap();
            if nonzero { assert_eq!(actual, current); }
            drop(messages);
            let calls = remote.calls.lock().unwrap();
            assert_eq!(calls[before..].iter().filter(|(route,_)| route.starts_with("POST")).count(), usize::from(!nonzero));
            assert_eq!(calls[before..].iter().filter(|(route,_)| route.starts_with("PATCH")).count(), usize::from(nonzero));
            assert!(!calls.iter().any(|(route,_)| route.starts_with("DELETE")));
            let count = calls.len();
            drop(calls);
            assert!(inflight::load_inflight_state_read_only(&provider, channel).is_none());
            let source = inflight::CodexRange::from_retained_tui_terminal(&row).unwrap().source;
            assert!(crate::services::discord::outbound::delivery_record::confirmed_delivery_receipt_exists(&provider, ChannelId::new(channel), actual, &source));
            assert!(!super::super::idle_captured_response::recover_idle_partial_response(&remote.http, &shared, &row, output).await);
            assert_eq!(remote.calls.lock().unwrap().len(), count, "second idle invocation is transport-free");
        }
    });
}

#[test]
fn a2_t08_public_idle_absent_ready_has_one_body() {
    a2_idle_ready_case(
        concat!(
            module_path!(),
            "::a2_t08_public_idle_absent_ready_has_one_body"
        ),
        false,
    );
}

#[test]
fn a2_t08_public_idle_nonzero_ready_retires_same_message_once() {
    a2_idle_ready_case(
        concat!(
            module_path!(),
            "::a2_t08_public_idle_nonzero_ready_retires_same_message_once"
        ),
        true,
    );
}

fn absent_case(name: &str, long: bool, provider: ProviderKind) {
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channel = if provider == ProviderKind::Codex {
                75_070_504
            } else if long {
                75_070_502
            } else {
                75_070_501
            };
            let body = if long {
                "long🙂".repeat(900)
            } else {
                "answer🙂".to_string()
            };
            let remote = Remote::new(channel).await;
            let row = admitted_provider(root.path(), channel, &body, 0, provider).await;
            assert_eq!(row.response_sent_offset, 0);
            let shared = runtime(2);
            let delivery = recovery_on(&remote, &row, &shared).await;
            assert_eq!(
                delivery.outcome,
                RecoveryRelayOutcome::Delivered,
                "current0 must deliver the admitted source"
            );
            let stored = finish(&shared, &row, delivery).await;
            assert_ne!(stored.current_msg_id, 0);
            let chunks = crate::services::discord::body_create::split_for_create(&body);
            let messages = remote.messages.lock().unwrap();
            assert_eq!(messages.len(), chunks.len());
            assert_eq!(
                messages
                    .iter()
                    .map(|m| m["content"].as_str().unwrap())
                    .collect::<String>(),
                chunks.concat()
            );
            assert!(
                !remote
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(route, _)| route.starts_with("DELETE"))
            );
        });
}
#[test]
fn t05_absent_short_uses_captured_recovery() {
    absent_case(
        concat!(module_path!(), "::t05_absent_short_uses_captured_recovery"),
        false,
        ProviderKind::Claude,
    );
}
#[test]
fn t05_absent_long_uses_captured_recovery() {
    absent_case(
        concat!(module_path!(), "::t05_absent_long_uses_captured_recovery"),
        true,
        ProviderKind::Claude,
    );
}

#[test]
fn nonzero_eager_post_then_edit_is_unchanged() {
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(concat!(
        module_path!(),
        "::nonzero_eager_post_then_edit_is_unchanged"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channel = 75_070_503;
            let remote = Remote::new(channel).await;
            let shared = make_shared_data_for_tests();
            let gateway = DiscordGateway::new(
                remote.http.clone(),
                shared.clone(),
                ProviderKind::Claude,
                None,
            );
            let eager = "⠋ 계속 처리 중";
            let message = gateway
                .send_message(ChannelId::new(channel), eager)
                .await
                .unwrap();
            assert_eq!(
                remote.calls.lock().unwrap().len(),
                1,
                "eager POST precedes body admission"
            );
            let row = admitted(root.path(), channel, "nonzero answer", message.get()).await;
            let shared = runtime(2);
            let delivery = recovery_on(&remote, &row, &shared).await;
            assert_eq!(delivery.outcome, RecoveryRelayOutcome::Delivered);
            finish(&shared, &row, delivery).await;
            let calls = remote.calls.lock().unwrap();
            assert_eq!(calls.len(), 2, "one initial POST and one EDIT");
            assert!(calls[0].0.starts_with("POST"));
            assert_eq!(calls[0].1["content"], eager);
            assert!(calls[0].1.get("nonce").is_none());
            assert!(calls[1].0.starts_with("PATCH"));
            assert_eq!(calls[1].1["content"], "nonzero answer");
            assert_eq!(
                remote.messages.lock().unwrap()[0]["id"],
                message.get().to_string()
            );
        });
}

fn runtime(generation: u64) -> Arc<SharedData> {
    let mut shared = make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("independent runtime")
        .restart
        .current_generation = generation;
    shared
}
async fn recovery_on(
    remote: &Remote,
    row: &inflight::InflightTurnState,
    shared: &Arc<SharedData>,
) -> CapturedRecoveryDelivery {
    let provider = row.provider_kind().unwrap();
    let gateway = DiscordGateway::new(remote.http.clone(), shared.clone(), provider.clone(), None);
    relay_captured_recovery_terminal_notice_with_gateway(
        &remote.http,
        shared,
        &provider,
        row,
        &row.full_response[row.response_sent_offset..],
        &gateway,
    )
    .await
}
async fn finish(
    shared: &Arc<SharedData>,
    row: &inflight::InflightTurnState,
    delivery: CapturedRecoveryDelivery,
) -> inflight::InflightTurnState {
    assert!(
        shared
            .mailbox(ChannelId::new(row.channel_id))
            .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
                shared: shared.clone(),
                state: row.clone(),
                actor: None,
                delivery,
            })
            .await
            .is_some(),
        "captured mailbox commit"
    );
    let provider = row.provider_kind().unwrap();
    let stored = inflight::load_inflight_state_read_only(&provider, row.channel_id).unwrap();
    assert!(stored.terminal_delivery_committed);
    assert_eq!(stored.response_sent_offset, stored.full_response.len());
    let source = inflight::CodexRange::from_retained_tui_terminal(&stored)
        .unwrap()
        .source;
    assert!(
        crate::services::discord::outbound::delivery_record::confirmed_delivery_receipt_exists(
            &provider,
            ChannelId::new(row.channel_id),
            stored.current_msg_id,
            &source
        )
    );
    assert_eq!(
        shared.committed_relay_offset(ChannelId::new(source.offset_authority_channel_id)),
        source.range.1,
        "raw frontier follows the actual full-source receipt"
    );
    stored
}
fn attempt_file(root: &std::path::Path, channel: u64) -> std::path::PathBuf {
    let mut dirs = vec![root.join("runtime/discord_body_creates")];
    let mut files = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "json")
                && path.parent().unwrap().file_name().unwrap() == channel.to_string().as_str()
            {
                files.push(path);
            }
        }
    }
    assert_eq!(files.len(), 1);
    files.pop().unwrap()
}
fn no_body_delete(remote: &Remote) {
    assert!(
        !remote
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| r.starts_with("DELETE"))
    );
}
fn post_calls(remote: &Remote) -> Vec<serde_json::Value> {
    remote
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(r, _)| r.starts_with("POST"))
        .map(|(_, p)| p.clone())
        .collect()
}
fn isolated(name: &str) -> bool {
    crate::services::tui_o::cutover::test_override::in_empty_list_process(name)
}

#[test]
fn t01_response_loss_restarts_with_same_nonce_and_actual_id() {
    if !isolated(concat!(
        module_path!(),
        "::t01_response_loss_restarts_with_same_nonce_and_actual_id"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (channel, accepted) in [(75_070_601, true), (75_070_602, false)] {
                let remote = Remote::new(channel).await;
                *remote.fault.lock().unwrap() = Some((1, accepted));
                let row = admitted(root.path(), channel, "ordered🙂\nraw body", 0).await;
                let old = runtime(1);
                let failed = recovery_on(&remote, &row, &old).await;
                assert_eq!(failed.outcome, RecoveryRelayOutcome::TransientFailure);
                assert_eq!(old.committed_relay_offset(ChannelId::new(channel)), 0);
                drop(failed);
                drop(old);
                assert_eq!(post_calls(&remote).len(), 1);
                let restored =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                        .unwrap();
                let new = runtime(2);
                let delivered = recovery_on(&remote, &restored, &new).await;
                assert_eq!(delivered.outcome, RecoveryRelayOutcome::Delivered);
                assert_eq!(
                    new.committed_relay_offset(ChannelId::new(channel)),
                    0,
                    "ACK is not a terminal receipt"
                );
                let stored = finish(&new, &restored, delivered).await;
                let posts = post_calls(&remote);
                assert_eq!(posts.len(), 2);
                assert_eq!(
                    posts[0], posts[1],
                    "identical frozen payload, nonce and reference"
                );
                assert_eq!(posts[0]["enforce_nonce"], true);
                assert!(posts[0]["nonce"].as_str().unwrap().len() <= 25);
                let messages = remote.messages.lock().unwrap();
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0]["content"], row.full_response);
                assert_eq!(messages[0]["id"], stored.current_msg_id.to_string());
                no_body_delete(&remote);
            }
        });
}

#[test]
fn t02_accepted_before_bind_and_edited_winner_are_readopted() {
    if !isolated(concat!(
        module_path!(),
        "::t02_accepted_before_bind_and_edited_winner_are_readopted"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channel = 75_070_603;
            let remote = Remote::new(channel).await;
            let row = admitted(root.path(), channel, "answer before bind", 0).await;
            let old = runtime(1);
            let delivered = recovery_on(&remote, &row, &old).await;
            assert_eq!(delivered.outcome, RecoveryRelayOutcome::Delivered);
            assert_eq!(post_calls(&remote).len(), 1);
            assert_eq!(old.committed_relay_offset(ChannelId::new(channel)), 0);
            drop(delivered);
            drop(old);
            let path = attempt_file(root.path(), channel);
            let mut accepted: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            accepted["last"]["first_ms"] = (chrono::Utc::now().timestamp_millis() - 600_000).into();
            std::fs::write(path, serde_json::to_vec(&accepted).unwrap()).unwrap();
            let new = runtime(2);
            let delivered = recovery_on(&remote, &row, &new).await;
            finish(&new, &row, delivered).await;
            assert_eq!(post_calls(&remote).len(), 1, "durable Accepted is GET-only");
            assert!(
                remote
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(r, _)| r.starts_with("GET") && r.contains("/messages/"))
            );
            no_body_delete(&remote);

            use crate::services::discord::body_create::{CreateOutcome, create_terminal};
            let channel = 75_070_604;
            let remote = Remote::new(channel).await;
            let mut row = admitted(root.path(), channel, "P+T", 0).await;
            let shared = runtime(1);
            let gateway = DiscordGateway::new(
                remote.http.clone(),
                shared.clone(),
                ProviderKind::Claude,
                None,
            );
            let CreateOutcome::Ready(pending) = create_terminal(&shared, &gateway, &row, "P").await
            else {
                panic!("initial ACK");
            };
            let id = pending.anchor;
            assert_eq!(
                pending.bind_after_actor_check(&shared, &mut row),
                inflight::GuardedSaveOutcome::Saved
            );
            gateway
                .edit_message(ChannelId::new(channel), id, "P+T")
                .await
                .unwrap();
            assert_eq!(
                inflight::bind_recovery_anchor_for_snapshot(
                    &ProviderKind::Claude,
                    &mut row,
                    id.get(),
                    3
                ),
                inflight::GuardedSaveOutcome::Saved
            );
            let calls = remote.calls.lock().unwrap().len();
            drop(gateway);
            drop(shared);
            let new = runtime(2);
            let gateway =
                DiscordGateway::new(remote.http.clone(), new.clone(), ProviderKind::Claude, None);
            let CreateOutcome::Ready(pending) = create_terminal(&new, &gateway, &row, "P+T").await
            else {
                panic!("bound winner precedes frozen P comparison");
            };
            assert_eq!(pending.anchor, id);
            assert_eq!(
                pending.bind_after_actor_check(&new, &mut row),
                inflight::GuardedSaveOutcome::Saved
            );
            assert_eq!(
                remote.calls.lock().unwrap().len(),
                calls,
                "no POST/DELETE/old-P PATCH or frozen GET"
            );
            assert_eq!(remote.messages.lock().unwrap()[0]["content"], "P+T");
            assert!(!row.terminal_delivery_committed);
            assert_eq!(row.response_sent_offset, 0);
        });
}

#[test]
fn t02_same_episode_winner_accepts_an_old_zero_snapshot() {
    if !isolated(concat!(
        module_path!(),
        "::t02_same_episode_winner_accepts_an_old_zero_snapshot"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use crate::services::discord::body_create::{CreateOutcome, create_terminal};
            let channel = 75_070_605;
            let remote = Remote::new(channel).await;
            let mut first = admitted(root.path(), channel, "winner", 0).await;
            let shared = runtime(1);
            let gateway = DiscordGateway::new(
                remote.http.clone(),
                shared.clone(),
                ProviderKind::Claude,
                None,
            );
            let CreateOutcome::Ready(pending_a) =
                create_terminal(&shared, &gateway, &first, "winner").await
            else {
                panic!("first ACK")
            };
            let id = pending_a.anchor;
            let mut second = first.clone();
            let CreateOutcome::Ready(pending_b) =
                create_terminal(&shared, &gateway, &second, "winner").await
            else {
                panic!("second ACK")
            };
            assert_eq!(
                pending_b.bind_after_actor_check(&shared, &mut second),
                inflight::GuardedSaveOutcome::Saved
            );
            assert_eq!(first.current_msg_id, 0);
            assert_eq!(
                pending_a.bind_after_actor_check(&shared, &mut first),
                inflight::GuardedSaveOutcome::Saved
            );
            assert_eq!(first.current_msg_id, id.get());
            assert_eq!(post_calls(&remote).len(), 1);
            assert_eq!(first.response_sent_offset, 0);
            assert!(!first.terminal_delivery_committed);
            no_body_delete(&remote);
        });
}
#[test]
fn t05_codex_absent_short_uses_captured_recovery() {
    absent_case(
        concat!(
            module_path!(),
            "::t05_codex_absent_short_uses_captured_recovery"
        ),
        false,
        ProviderKind::Codex,
    );
}
#[test]
fn t02_new_episode_same_channel_and_body_gets_a_new_message() {
    if !isolated(concat!(
        module_path!(),
        "::t02_new_episode_same_channel_and_body_gets_a_new_message"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channel = 75_070_680;
            let remote = Remote::new(channel).await;
            let mut ids = Vec::new();
            for ordinal in 0..2 {
                let row = admitted_episode(
                    root.path(),
                    channel,
                    "same body",
                    0,
                    ProviderKind::Claude,
                    ordinal,
                )
                .await;
                let shared = runtime(ordinal + 1);
                let ready = recovery_on(&remote, &row, &shared).await;
                ids.push(finish(&shared, &row, ready).await.current_msg_id);
            }
            assert_ne!(ids[0], ids[1]);
            assert_eq!(remote.messages.lock().unwrap().len(), 2);
            let posts = post_calls(&remote);
            assert_ne!(posts[0]["nonce"], posts[1]["nonce"]);
            no_body_delete(&remote);
        });
}

#[test]
fn t03_expired_unknown_is_held_across_two_restarts_and_done() {
    if !isolated(concat!(
        module_path!(),
        "::t03_expired_unknown_is_held_across_two_restarts_and_done"
    )) {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (channel, accepted) in [(75_070_610, true), (75_070_611, false)] {
                let remote = Remote::new(channel).await;
                *remote.fault.lock().unwrap() = Some((1, accepted));
                let row = admitted(root.path(), channel, "held source body", 0).await;
                let old = runtime(1);
                let failed = recovery_on(&remote, &row, &old).await;
                assert_eq!(failed.outcome, RecoveryRelayOutcome::TransientFailure);
                drop(failed);
                drop(old);
                let path = attempt_file(root.path(), channel);
                let mut attempt: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                attempt["pending"]["first_ms"] =
                    (chrono::Utc::now().timestamp_millis() - 600_000).into();
                std::fs::write(&path, serde_json::to_vec(&attempt).unwrap()).unwrap();
                let retained = std::fs::read(&path).unwrap();
                for incarnation in 2..=4 {
                    let row =
                        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                            .expect("source obligation remains");
                    let shared = runtime(incarnation);
                    let held = recovery_on(&remote, &row, &shared).await;
                    assert_eq!(
                        held.outcome,
                        RecoveryRelayOutcome::TransientFailure,
                        "held never means delivered"
                    );
                    assert!(
                        shared
                            .mailbox(ChannelId::new(channel))
                            .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
                                shared: shared.clone(),
                                state: row.clone(),
                                actor: None,
                                delivery: held,
                            })
                            .await
                            .is_none()
                    );
                    let durable =
                        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                            .expect("held must not clear");
                    assert_eq!(durable.full_response, "held source body");
                    assert_eq!(durable.output_path, row.output_path);
                    assert_eq!(
                        durable.tui_terminal_source_file_identity,
                        row.tui_terminal_source_file_identity
                    );
                    assert_eq!(
                        inflight::CodexRange::from_retained_tui_terminal(&durable)
                            .map(|r| r.source),
                        inflight::CodexRange::from_retained_tui_terminal(&row).map(|r| r.source)
                    );
                    assert_eq!(durable.response_sent_offset, 0);
                    assert!(!durable.terminal_delivery_committed);
                    assert_eq!(shared.committed_relay_offset(ChannelId::new(channel)), 0);
                    assert_eq!(
                        post_calls(&remote).len(),
                        1,
                        "Done must not open a terminal-purpose POST"
                    );
                    let saved: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    assert_eq!(saved, attempt);
                }
                assert!(!retained.is_empty());
                assert_eq!(remote.messages.lock().unwrap().len(), usize::from(accepted));
                no_body_delete(&remote);
            }
        });
}

#[test]
fn t02_captured_bind_refuses_actor_successor_source_and_save_generation_changes() {
    if !isolated(concat!(
        module_path!(),
        "::t02_captured_bind_refuses_actor_successor_source_and_save_generation_changes"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (index, changed) in ["actor", "birth", "source", "save_generation", "winner"]
                .into_iter()
                .enumerate()
            {
                let channel = 75_070_670 + index as u64;
                let remote = Remote::new(channel).await;
                let row = admitted(root.path(), channel, "guarded terminal", 0).await;
                let shared = runtime(1);
                let delivery = recovery_on(&remote, &row, &shared).await;
                assert_eq!(delivery.outcome, RecoveryRelayOutcome::Delivered);
                let mut fresh = row.clone();
                let mut carrier = CapturedReadyDeliveryCommit {
                    shared: shared.clone(),
                    state: row.clone(),
                    actor: None,
                    delivery,
                };
                match changed {
                    "actor" => {
                        carrier.actor = Some(Arc::new(CancelToken::from_persisted_turn_nonce(
                            Some("different-actor".into()),
                        )))
                    }
                    "birth" => fresh.turn_nonce = Some("successor".into()),
                    "source" => {
                        fresh.output_path =
                            Some(root.path().join("different.jsonl").display().to_string())
                    }
                    "save_generation" => {
                        carrier.state.save_generation =
                            carrier.state.save_generation.saturating_add(1)
                    }
                    "winner" => fresh.current_msg_id = 99,
                    _ => unreachable!(),
                }
                if matches!(changed, "birth" | "source" | "winner") {
                    let path = root
                        .path()
                        .join(format!("runtime/discord_inflight/claude/{channel}.json"));
                    std::fs::write(&path, serde_json::to_vec(&fresh).unwrap()).unwrap();
                }
                assert!(
                    shared
                        .mailbox(ChannelId::new(channel))
                        .commit_captured_ready_delivery(carrier)
                        .await
                        .is_none(),
                    "{changed}"
                );
                let stored =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                        .unwrap();
                assert!(!stored.terminal_delivery_committed, "{changed}");
                assert_eq!(stored.response_sent_offset, 0);
                assert_eq!(shared.committed_relay_offset(ChannelId::new(channel)), 0);
                assert_eq!(post_calls(&remote).len(), 1);
                no_body_delete(&remote);
            }
        });
}

#[test]
fn t05_receipt_failure_keeps_raw_offset_until_restart_confirms_actual_tail() {
    if !isolated(concat!(
        module_path!(),
        "::t05_receipt_failure_keeps_raw_offset_until_restart_confirms_actual_tail"
    )) {
        return;
    }
    use crate::services::discord::runtime_store::durable_publish::flush_probe::{
        Plan, Step, Watch,
    };
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channel = 75_070_675;
            let remote = Remote::new(channel).await;
            let row = admitted(root.path(), channel, "full-source receipt debt", 0).await;
            let old = runtime(1);
            let delivery = recovery_on(&remote, &row, &old).await;
            assert_eq!(delivery.outcome, RecoveryRelayOutcome::Delivered);
            let scope = root.path().join("runtime/discord_delivery_records");
            let watch = Watch::new(
                &scope,
                Plan {
                    fail: vec![Step::FileSync],
                    ..Default::default()
                },
            );
            assert!(
                old.mailbox(ChannelId::new(channel))
                    .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
                        shared: old.clone(),
                        state: row,
                        actor: None,
                        delivery,
                    })
                    .await
                    .is_none()
            );
            assert!(
                watch
                    .events()
                    .iter()
                    .any(|e| e.step == Step::FileSync && !e.ok),
                "receipt write actually failed"
            );
            assert_eq!(
                old.committed_relay_offset(ChannelId::new(channel)),
                0,
                "no raw advance before receipt"
            );
            let retained =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel).unwrap();
            assert_ne!(
                retained.current_msg_id, 0,
                "actual body bind survives receipt failure"
            );
            assert_eq!(retained.response_sent_offset, 0);
            assert!(!retained.terminal_delivery_committed);
            drop(watch);
            drop(old);
            let new = runtime(2);
            let delivery = recovery_on(&remote, &retained, &new).await;
            finish(&new, &retained, delivery).await;
            assert_eq!(post_calls(&remote).len(), 1);
            assert_eq!(remote.messages.lock().unwrap().len(), 1);
            no_body_delete(&remote);
        });
}

#[test]
fn t03_prepare_flush_failure_never_allows_http() {
    if !isolated(concat!(
        module_path!(),
        "::t03_prepare_flush_failure_never_allows_http"
    )) {
        return;
    }
    use crate::services::discord::runtime_store::durable_publish::flush_probe::{
        Plan, Step, Watch,
    };
    for (index, step) in [
        Step::FileSync,
        Step::ParentOpen,
        Step::ParentSync,
        Step::Ancestors,
    ]
    .into_iter()
    .enumerate()
    {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let channel = 75_070_620 + index as u64;
                let remote = Remote::new(channel).await;
                let row = admitted(root.path(), channel, "flush body", 0).await;
                let scope = root.path().join("runtime/discord_body_creates");
                let watch = Watch::new(
                    &scope,
                    Plan {
                        fail: vec![step],
                        ..Default::default()
                    },
                );
                let shared = runtime(1);
                let held = recovery_on(&remote, &row, &shared).await;
                assert_eq!(held.outcome, RecoveryRelayOutcome::TransientFailure);
                assert!(
                    post_calls(&remote).is_empty(),
                    "{step:?}: uncertain durability must forbid HTTP"
                );
                let prepared = if step == Step::FileSync {
                    None
                } else {
                    let path = attempt_file(root.path(), channel);
                    Some(
                        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                            .unwrap(),
                    )
                };
                assert!(
                    watch
                        .events()
                        .iter()
                        .any(|event| event.step == step && !event.ok)
                );
                drop(held);
                drop(shared);
                drop(watch);
                let audit = Watch::new(&scope, Plan::default());
                let shared = runtime(2);
                let ready = recovery_on(&remote, &row, &shared).await;
                assert_eq!(ready.outcome, RecoveryRelayOutcome::Delivered);
                assert!(
                    audit
                        .flushed(Step::Ancestors)
                        .contains(&root.path().join("runtime")),
                    "flush actual runtime root"
                );
                if let Some(prepared) = prepared {
                    let posts = post_calls(&remote);
                    assert_eq!(posts[0]["nonce"], prepared["pending"]["nonce"]);
                    assert_eq!(posts[0]["content"], prepared["pending"]["text"]);
                }
                finish(&shared, &row, ready).await;
                no_body_delete(&remote);
            });
    }
}

#[test]
fn t03_ack_flush_failure_retains_accepted_id_or_frozen_nonce() {
    if !isolated(concat!(
        module_path!(),
        "::t03_ack_flush_failure_retains_accepted_id_or_frozen_nonce"
    )) {
        return;
    }
    use crate::services::discord::runtime_store::durable_publish::flush_probe::{
        Plan, Step, Watch,
    };
    for (index, step) in [
        Step::FileSync,
        Step::ParentOpen,
        Step::ParentSync,
        Step::Ancestors,
    ]
    .into_iter()
    .enumerate()
    {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let channel = 75_070_630 + index as u64;
                let remote = Remote::new(channel).await;
                let row = admitted(root.path(), channel, "ACK flush body", 0).await;
                let _uncached =
                    crate::services::discord::runtime_store::without_directory_birth_times_for_test(
                        &root.path().join("runtime"),
                    );
                let watched = Arc::new(Mutex::new(None));
                let fault = watched.clone();
                let scope = root.path().join("runtime/discord_body_creates");
                *remote.on_post.lock().unwrap() = Some(Box::new(move || {
                    *fault.lock().unwrap() = Some(Watch::new(
                        &scope,
                        Plan {
                            fail: vec![step],
                            ..Default::default()
                        },
                    ));
                }));
                let old = runtime(1);
                let held = recovery_on(&remote, &row, &old).await;
                assert_eq!(held.outcome, RecoveryRelayOutcome::TransientFailure);
                assert_eq!(old.committed_relay_offset(ChannelId::new(channel)), 0);
                assert_eq!(post_calls(&remote).len(), 1);
                assert!(
                    watched
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .events()
                        .iter()
                        .any(|e| e.step == step && !e.ok)
                );
                drop(held);
                drop(old);
                drop(watched.lock().unwrap().take());
                let restored =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                        .unwrap();
                let new = runtime(2);
                let delivered = recovery_on(&remote, &restored, &new).await;
                finish(&new, &restored, delivered).await;
                assert_eq!(remote.messages.lock().unwrap().len(), 1);
                assert_eq!(
                    post_calls(&remote).len(),
                    if step == Step::FileSync { 2 } else { 1 }
                );
                no_body_delete(&remote);
            });
    }
}

#[test]
fn t03_corrupt_unknown_and_clock_reversal_hold_without_replacing_record() {
    if !isolated(concat!(
        module_path!(),
        "::t03_corrupt_unknown_and_clock_reversal_hold_without_replacing_record"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (index, kind) in ["corrupt", "unknown", "clock", "overflow", "other-purpose"]
                .into_iter()
                .enumerate()
            {
                let channel = 75_070_640 + index as u64;
                let remote = Remote::new(channel).await;
                *remote.fault.lock().unwrap() = Some((1, true));
                let row = admitted(root.path(), channel, "retained body", 0).await;
                drop(recovery_on(&remote, &row, &runtime(1)).await);
                let path = attempt_file(root.path(), channel);
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                match kind {
                    "unknown" => value["version"] = 999.into(),
                    "clock" => {
                        value["pending"]["first_ms"] =
                            (chrono::Utc::now().timestamp_millis() + 600_000).into()
                    }
                    "overflow" => value["pending"]["first_ms"] = i64::MIN.into(),
                    "other-purpose" => value["scope"] = "preview".into(),
                    _ => {}
                }
                let bytes = if kind == "corrupt" {
                    b"not-json".to_vec()
                } else {
                    serde_json::to_vec(&value).unwrap()
                };
                std::fs::write(&path, &bytes).unwrap();
                let result = recovery_on(&remote, &row, &runtime(2)).await;
                assert_eq!(
                    result.outcome,
                    RecoveryRelayOutcome::TransientFailure,
                    "{kind}"
                );
                assert_eq!(post_calls(&remote).len(), 1);
                if kind == "corrupt" || kind == "unknown" || kind == "other-purpose" {
                    assert_eq!(
                        std::fs::read(&path).unwrap(),
                        bytes,
                        "do not discard unreadable/foreign records"
                    );
                }
                let kept = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                    .unwrap();
                assert_eq!(kept.full_response, row.full_response);
                assert_eq!(kept.response_sent_offset, 0);
                assert!(!kept.terminal_delivery_committed);
                no_body_delete(&remote);
            }
        });
}

#[test]
fn t05_long_parts_survive_loss_and_all_accepted_before_receipt_restart() {
    if !isolated(concat!(
        module_path!(),
        "::t05_long_parts_survive_loss_and_all_accepted_before_receipt_restart"
    )) {
        return;
    }
    use futures::FutureExt;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for (index, provider) in [ProviderKind::Claude, ProviderKind::Codex].into_iter().enumerate() {
            for all_accepted in [false, true] {
                let channel = 75_070_650 + index as u64 * 2 + u64::from(all_accepted);
                let remote = Remote::new(channel).await;
                let old = runtime(1);
                let gateway = DiscordGateway::new(remote.http.clone(), old.clone(), provider.clone(), None);
                let prefix = "frozen prefix\n";
                let frozen = gateway.send_message(ChannelId::new(channel), prefix).await.unwrap();
                let tail = (0..650).map(|index| format!("line {index:04} raw 🙂 body\n")).collect::<String>();
                let body = format!("{prefix}{tail}");
                let mut row = admitted_provider(root.path(), channel, &body, 0, provider.clone()).await;
                row.response_sent_offset = prefix.len();
                row.streaming_rollover_frozen_msg_ids = vec![frozen.get()];
                inflight::save_inflight_state(&row).unwrap();
                row = inflight::load_inflight_state_read_only(&provider, channel).unwrap();
                let parts = crate::services::discord::body_create::split_for_create(&tail);
                assert!(parts.len() >= 3);
                if all_accepted {
                    let path_root = root.path().to_path_buf();
                    let count = parts.len();
                    let parked = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let reached = parked.clone();
                    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK.lock().unwrap() = Some(Arc::new(crate::services::discord::turn_bridge::TerminalPrepareTestHook {
                        channel_id: channel,
                        after_send:Mutex::new(Some(Box::new(move || {
                            let attempt: serde_json::Value = serde_json::from_slice(&std::fs::read(attempt_file(&path_root, channel)).unwrap()).unwrap();
                            assert_eq!(attempt["accepted"].as_array().unwrap().len(), count);
                            reached.store(true, std::sync::atomic::Ordering::SeqCst);
                            panic!("park after all Accepted before receipt/bind");
                        }))), ..Default::default()
                    }));
                    let crashed = std::panic::AssertUnwindSafe(recovery_on(&remote, &row, &old)).catch_unwind().await;
                    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK.lock().unwrap() = None;
                    assert!(crashed.is_err());
                    assert!(parked.load(std::sync::atomic::Ordering::SeqCst), "all-accepted park must be reached");
                } else {
                    *remote.fault.lock().unwrap() = Some((3,true));
                    let failed = recovery_on(&remote, &row, &old).await;
                    assert_eq!(failed.outcome, RecoveryRelayOutcome::TransientFailure);
                    let attempt: serde_json::Value = serde_json::from_slice(&std::fs::read(attempt_file(root.path(), channel)).unwrap()).unwrap();
                    assert_eq!(attempt["accepted"].as_array().unwrap().len(), 1, "part0 durable");
                    assert_eq!(attempt["pending"]["text"], parts[1], "part1 remains pending after remote acceptance");
                }
                assert_eq!(old.committed_relay_offset(ChannelId::new(channel)), 0);
                let retained = inflight::load_inflight_state_read_only(&provider, channel).unwrap();
                assert_eq!(retained.response_sent_offset, prefix.len());
                assert!(!retained.terminal_delivery_committed);
                let source = inflight::CodexRange::from_retained_tui_terminal(&retained).unwrap().source;
                let pre = remote.messages.lock().unwrap().last().unwrap()["id"].as_str().unwrap().parse().unwrap();
                assert!(!crate::services::discord::outbound::delivery_record::confirmed_delivery_receipt_exists(&provider, ChannelId::new(channel), pre, &source));
                drop(gateway); drop(old);
                let new = runtime(2);
                let ready = recovery_on(&remote, &retained, &new).await;
                assert_eq!(ready.outcome, RecoveryRelayOutcome::Delivered);
                assert_eq!(new.committed_relay_offset(ChannelId::new(channel)), 0);
                let stored = finish(&new, &retained, ready).await;
                let posts = post_calls(&remote);
                for (ordinal, text) in parts.iter().enumerate() {
                    let attempts: Vec<_> = posts.iter().filter(|p|p["content"] == *text).collect();
                    assert_eq!(attempts.len(), if !all_accepted && ordinal == 1 {2} else {1}, "accepted part {ordinal} rePOST0");
                    if attempts.len() == 2 { assert_eq!(attempts[0], attempts[1]); }
                }
                let messages = remote.messages.lock().unwrap();
                assert_eq!(messages.len(), parts.len()+1);
                assert_eq!(messages.iter().map(|m|m["content"].as_str().unwrap()).collect::<String>(), body);
                assert_eq!(stored.current_msg_id.to_string(), messages.last().unwrap()["id"].as_str().unwrap());
                assert_ne!(stored.current_msg_id, frozen.get());
                assert_eq!(stored.current_msg_len, parts.last().unwrap().len());
                no_body_delete(&remote);
            }
        }
    });
}

#[test]
fn direct_tui_gateway_wire_preserves_nonce_and_reference() {
    if !isolated(concat!(
        module_path!(),
        "::direct_tui_gateway_wire_preserves_nonce_and_reference"
    )) {
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let channel = 75_070_660;
        let remote = Remote::new(channel).await;
        let shared = runtime(1);
        let _http = crate::services::discord::shared_state::test_rest::install(remote.http.clone());
        let gateway = crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway(remote.http.clone(), shared.clone());
        let first = gateway.create_body_message(&shared, ChannelId::new(channel), "wire body", "bc-stable-wire", Some((channel,17))).await.unwrap();
        let second = gateway.create_body_message(&shared, ChannelId::new(channel), "wire body", "bc-stable-wire", Some((channel,17))).await.unwrap();
        assert_eq!(first,second);
        let posts = post_calls(&remote);
        assert_eq!(posts.len(),2);
        assert_eq!(posts[0],posts[1]);
        assert_eq!(posts[0]["nonce"],"bc-stable-wire");
        assert_eq!(posts[0]["enforce_nonce"],true);
        assert_eq!(posts[0]["message_reference"]["message_id"],"17");
        assert_eq!(posts[0]["message_reference"]["channel_id"],channel.to_string());
        assert_eq!(posts[0]["message_reference"]["fail_if_not_exists"],false);
        assert_eq!(posts[0]["allowed_mentions"],serde_json::to_value(crate::services::discord::http::relay_allowed_mentions()).unwrap());
        assert_eq!(remote.messages.lock().unwrap().len(),1);
    });
}

fn r1_created_flush_case(name: &str, receipt: bool) {
    if !isolated(name) {
        return;
    }
    use crate::services::discord::runtime_store::durable_publish::flush_probe::{
        Plan, Step, Watch,
    };
    for (index, step) in [Step::ParentOpen, Step::ParentSync, Step::Ancestors]
        .into_iter()
        .enumerate()
    {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let channel = 75_071_701 + index as u64 + u64::from(receipt) * 10;
                let remote = Remote::new(channel).await;
                let row = admitted(root.path(), channel, "strict created settlement🙂", 0).await;
                let old = runtime(1);
                let delivery = recovery_on(&remote, &row, &old).await;
                assert_eq!(delivery.outcome, RecoveryRelayOutcome::Delivered);
                let _uncached =
                    crate::services::discord::runtime_store::without_directory_birth_times_for_test(
                        &root.path().join("runtime"),
                    );
                let scope = root.path().join(if receipt {
                    "runtime/discord_delivery_records"
                } else {
                    "runtime/discord_inflight"
                });
                let watch = Watch::new(
                    &scope,
                    Plan {
                        fail: vec![step],
                        ..Default::default()
                    },
                );
                assert!(
                    old.mailbox(ChannelId::new(channel))
                        .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
                            shared: old.clone(),
                            state: row,
                            actor: None,
                            delivery,
                        })
                        .await
                        .is_none(),
                    "{step:?} receipt={receipt}: uncertain flush is not a commit"
                );
                assert!(
                    watch
                        .events()
                        .iter()
                        .any(|event| event.step == step && !event.ok),
                    "fault hit {step:?}"
                );
                assert_eq!(old.committed_relay_offset(ChannelId::new(channel)), 0);
                let retained =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                        .unwrap();
                assert!(!retained.terminal_delivery_committed);
                assert_eq!(retained.response_sent_offset, 0);
                let accepted: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(attempt_file(root.path(), channel)).unwrap(),
                )
                .unwrap();
                assert_eq!(accepted["accepted"][0][0], retained.current_msg_id);
                if receipt {
                    assert!(
                        super::super::terminal_text_idempotency::captured_terminal_receipt_exists(
                            &ProviderKind::Claude,
                            &retained,
                        ),
                        "the failed flush left a readable exact receipt"
                    );
                }
                let next = runtime(2);
                let called = std::sync::atomic::AtomicBool::new(false);
                let (next_ref, remote_ref, retained_ref, called_ref) =
                    (&next, &remote, &retained, &called);
                assert!(
                    super::super::idle_captured_response::settle_ready_without_output(
                        &next,
                        &ProviderKind::Claude,
                        &retained,
                        |text| async move {
                            called_ref.store(true, std::sync::atomic::Ordering::SeqCst);
                            let gateway = DiscordGateway::new(
                                remote_ref.http.clone(),
                                next_ref.clone(),
                                ProviderKind::Claude,
                                None,
                            );
                            let delivery = relay_captured_recovery_terminal_notice_with_gateway(
                                &remote_ref.http,
                                next_ref,
                                &ProviderKind::Claude,
                                retained_ref,
                                &text,
                                &gateway,
                            )
                            .await;
                            assert_eq!(delivery.outcome, RecoveryRelayOutcome::Delivered);
                            assert!(
                                delivery.body_bind.is_some(),
                                "the actual created settlement retry is reached"
                            );
                            delivery
                        }
                    )
                    .await
                );
                assert!(
                    called.load(std::sync::atomic::Ordering::SeqCst),
                    "a visible receipt must not bypass unresolved creation"
                );
                let held = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                    .unwrap();
                assert!(!held.terminal_delivery_committed);
                assert_eq!(held.response_sent_offset, 0);
                assert_eq!(next.committed_relay_offset(ChannelId::new(channel)), 0);
                assert_eq!(post_calls(&remote).len(), 1);
                assert!(
                    watch
                        .events()
                        .iter()
                        .filter(|event| event.step == step && !event.ok)
                        .count()
                        >= 2,
                    "winner retry rechecks flush"
                );
                drop(watch);
                drop(old);
                drop(next);
                let fresh = runtime(3);
                let ready = recovery_on(&remote, &held, &fresh).await;
                let stored = finish(&fresh, &held, ready).await;
                assert_eq!(stored.current_msg_id, held.current_msg_id);
                assert_eq!(post_calls(&remote).len(), 1);
                no_body_delete(&remote);
            });
    }
}

#[test]
fn r1_created_anchor_flush_uncertainty_holds_even_a_bound_winner() {
    r1_created_flush_case(
        concat!(
            module_path!(),
            "::r1_created_anchor_flush_uncertainty_holds_even_a_bound_winner"
        ),
        false,
    );
}

#[test]
fn r1_created_receipt_flush_uncertainty_cannot_use_the_receipt_shortcut() {
    r1_created_flush_case(
        concat!(
            module_path!(),
            "::r1_created_receipt_flush_uncertainty_cannot_use_the_receipt_shortcut"
        ),
        true,
    );
}

#[test]
fn r1_returned_actual_id_survives_a_restart_marker_before_bind() {
    if !isolated(concat!(
        module_path!(),
        "::r1_returned_actual_id_survives_a_restart_marker_before_bind"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channel = 75_071_721;
            let remote = Remote::new(channel).await;
            let row = admitted(root.path(), channel, "accepted before marker", 0).await;
            *remote.on_post.lock().unwrap() = Some(Box::new(move || {
                let mut row =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel)
                        .unwrap();
                row.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
                inflight::save_inflight_state(&row).unwrap();
            }));
            let old = runtime(1);
            assert_eq!(
                recovery_on(&remote, &row, &old).await.outcome,
                RecoveryRelayOutcome::TransientFailure
            );
            let path = attempt_file(root.path(), channel);
            let mut attempt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let actual = remote.messages.lock().unwrap()[0]["id"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            assert_eq!(
                attempt["accepted"][0][0], actual,
                "save the returned ID before inspecting mutable row authority"
            );
            assert!(attempt["pending"].is_null());
            attempt["last"]["first_ms"] = (chrono::Utc::now().timestamp_millis() - 600_000).into();
            std::fs::write(&path, serde_json::to_vec(&attempt).unwrap()).unwrap();
            let mut restored =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel).unwrap();
            assert!(restored.restart_mode.is_some());
            assert_eq!(restored.current_msg_id, 0);
            assert!(!restored.terminal_delivery_committed);
            assert_eq!(old.committed_relay_offset(ChannelId::new(channel)), 0);
            let mut guard = inflight::lock_inflight_episode(
                &ProviderKind::Claude,
                channel,
                &inflight::InflightEpisodePin::from_state(&restored),
            )
            .unwrap();
            assert_eq!(
                guard.mark_readopted_under_guard(),
                inflight::GuardedSaveOutcome::Saved
            );
            restored = guard.state().clone();
            drop(guard);
            let new = runtime(2);
            let sent = recovery_on(&remote, &restored, &new).await;
            let stored = finish(&new, &restored, sent).await;
            assert_eq!(stored.current_msg_id, actual);
            assert_eq!(
                post_calls(&remote).len(),
                1,
                "expired accepted ID is GET-only"
            );
            assert!(
                remote
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(route, _)| route.starts_with("GET ")
                        && route.ends_with(&format!("/channels/{channel}/messages/{actual}")))
            );
            no_body_delete(&remote);
        });
}

#[test]
fn r1_short_terminal_boundaries_create_one_message() {
    if !isolated(concat!(
        module_path!(),
        "::r1_short_terminal_boundaries_create_one_message"
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (index, body) in [
                "a".repeat(1990),
                "a".repeat(1991),
                "a".repeat(2000),
                "🙂".repeat(1000),
                format!("{}a", "🙂".repeat(999)),
            ]
            .into_iter()
            .enumerate()
            {
                let channel = 75_071_730 + index as u64;
                let remote = Remote::new(channel).await;
                assert!(!crate::services::discord::formatting::needs_multiple_messages(&body));
                let row = admitted(root.path(), channel, &body, 0).await;
                let old = runtime(1);
                let sent = recovery_on(&remote, &row, &old).await;
                let stored = finish(&old, &row, sent).await;
                assert_eq!(
                    post_calls(&remote).len(),
                    1,
                    "short body must fit in one message"
                );
                assert_eq!(stored.current_msg_len, body.len());
                assert_eq!(remote.messages.lock().unwrap()[0]["content"], body);
                let next = runtime(2);
                let sent = recovery_on(&remote, &stored, &next).await;
                if sent.body_bind.is_some() {
                    finish(&next, &stored, sent).await;
                }
                assert_eq!(
                    post_calls(&remote).len(),
                    1,
                    "restart cannot add another short part"
                );
                no_body_delete(&remote);
            }
        });
}
