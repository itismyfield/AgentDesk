use super::*;
use crate::services::discord::{mailbox_snapshot, turn_bridge::STREAM_TEST_EVENTS};
use crate::services::tui_o::cutover::test_override;

const CHANNEL: u64 = 583_357_453;
const CHILD: &str = "ADK_7453_NATIVE_CHILD";

fn isolated(name: &str, mode: &str) -> bool {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let channels = match mode {
        "legacy" => vec![],
        "held" => vec![(CHANNEL, RuntimeHandoffKind::ClaudeTui)],
        _ => vec![(CHANNEL, RuntimeHandoffKind::CodexTui)],
    };
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
        .env(CHILD, name)
        .env(
            test_override::CHANNELS_ENV,
            serde_json::to_string(&channels).unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored;"));
    false
}

async fn case(mode: &str, restore: bool) {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let mut f = Fixture::new(temp.path(), 7453, ProviderKind::Codex).await;
    f.local.full_response = if mode == "owned_empty" {
        ""
    } else if restore {
        "answer"
    } else if mode == "legacy" {
        "old"
    } else {
        "answer\n\n"
    }
    .into();
    f.local.response_sent_offset = f.local.full_response.len();
    f.local.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    f.local.external_turn_id = Some("external-7453".into());
    f.local.injected_prompt_message_id = Some(f.local.user_msg_id);
    f.local.request_owner_user_id =
        crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
    f.local
        .set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::None);
    let first = serde_json::json!({"type":"response_item","payload":{"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"answer"}]}});
    let raw =
        format!("{first}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n");
    f.local.session_id = Some("session-7453".into());
    let mut binding = dedupe::runtime_binding_for_tmux_session(&f.tmux).unwrap();
    binding.session_id = f.local.session_id.clone();
    dedupe::register_tmux_runtime_binding(&f.tmux, binding);
    std::fs::write(&f.transcript, raw).unwrap();
    f.end = std::fs::metadata(&f.transcript).unwrap().len();
    save_inflight_state_in_root(&inflight_runtime_root().unwrap(), &f.local).unwrap();
    f.baseline = serde_json::from_slice(&f.durable()).unwrap();
    f.local = f.baseline.clone();
    f.expected = InflightTurnIdentity::from_state(&f.local);
    let frame = f.frame();
    STREAM_TEST_EVENTS
        .lock()
        .unwrap()
        .retain(|(ch, _)| *ch != CHANNEL);
    let (_, range, _) = f.admit(frame).await.expect("captured admission");
    let range = range.unwrap();
    let stored: InflightTurnState = serde_json::from_slice(&f.durable()).unwrap();
    let expected_body = if mode == "owned_empty" {
        ""
    } else if mode == "legacy" || restore {
        "answer"
    } else {
        "answer\n\n"
    };
    assert_eq!(stored.full_response, expected_body);
    assert_eq!(
        stored.response_sent_offset,
        if mode == "owned_empty" {
            0
        } else if restore {
            6
        } else if mode == "legacy" {
            3
        } else {
            8
        }
    );
    assert_eq!(stored.last_offset, f.end);
    assert_eq!(
        serde_json::to_value(&f.baseline).unwrap(),
        serde_json::to_value(&stored).unwrap(),
        "baseline is the actual committed snapshot"
    );
    assert_eq!(range.result, "answer");
    let events = STREAM_TEST_EVENTS.lock().unwrap().clone();
    let peek = events
        .iter()
        .find(|(ch, event)| *ch == CHANNEL && event.starts_with("admission-peek="))
        .expect("peek inside blocking admission");
    if mode == "held" {
        assert!(peek.1.contains("Err(KindMismatch"), "{peek:?}");
    } else {
        assert_eq!(peek.1, format!("admission-peek=Ok({})", mode != "legacy"));
    }
    if !restore {
        return;
    }
    // Drop the entire old mailbox and its actor before loading durable bytes into a new runtime.
    drop(f);
    let name = std::env::var(CHILD).unwrap();
    let outcome = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
        .env("ADK_7453_RESTORE_ROOT", temp.path())
        .env("ADK_7453_RESTORE_AHEAD", "false")
        .output()
        .unwrap();
    assert!(
        outcome.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&outcome.stdout),
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert!(String::from_utf8_lossy(&outcome.stdout).contains("1 passed; 0 failed; 0 ignored;"));
}

async fn restore_phase() -> bool {
    let Some(root) = std::env::var_os("ADK_7453_RESTORE_ROOT") else {
        return false;
    };
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", Path::new(&root));
    let ahead = std::env::var("ADK_7453_RESTORE_AHEAD").unwrap() == "true";
    let row =
        crate::services::discord::inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL)
            .unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    assert!(
        mailbox_snapshot(&shared, ChannelId::new(CHANNEL))
            .await
            .cancel_token
            .is_none()
    );
    let output = PathBuf::from(row.output_path.as_ref().unwrap());
    assert!(!row.terminal_delivery_committed);
    assert_eq!(row.last_offset, std::fs::metadata(&output).unwrap().len());
    if !ahead {
        assert_eq!(row.full_response.len(), row.response_sent_offset);
    }
    let tmux = row.tmux_session_name.as_deref().unwrap();
    let binding = crate::services::discord::tui_prompt_relay::rehydration::codex_tui_rehydrated_binding_from_rollout_path(tmux, &output, row.session_id.clone()).unwrap();
    dedupe::register_rehydrated_tmux_runtime_binding("codex", tmux, CHANNEL, binding);
    assert!(row.requires_pinned_terminal_recovery());
    assert!(
        TuiTerminalRange::from_retained_tui_terminal(&row)
            .unwrap()
            .revalidated_source(&row)
            .unwrap()
            .is_some()
    );
    let saved_suffix =
        !ahead || (row.full_response == "answer\n\nsuffx" && row.response_sent_offset == 6);
    let gateway = NoBody;
    let http = Arc::new(serenity::all::Http::new("Bot test-token"));
    let recovered =
        crate::services::discord::recovery_engine::recover_idle_partial_response_from_ready_source(
            &http, &shared, &row, &output, &gateway,
        )
        .await;
    if ahead {
        assert!(
            recovered && saved_suffix,
            "saved suffix={saved_suffix}, restored={recovered}, without new Text or Done"
        );
        assert!(
            crate::services::discord::inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL)
                .is_none()
        );
        assert!(
            mailbox_snapshot(&shared, ChannelId::new(CHANNEL))
                .await
                .cancel_token
                .is_none()
        );
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    } else {
        assert!(
            !recovered,
            "fully-consumed EOF is the existing recovery limitation"
        );
        assert!(
            crate::services::discord::inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL)
                .is_some()
        );
    }
    true
}

struct NoBody;
impl crate::services::discord::gateway::TurnGateway for NoBody {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        _: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async { panic!("bridge body send") })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        text: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        assert!(
            !text.contains("answer") && !text.contains("suffx"),
            "bridge body edit: {text}"
        );
        Box::pin(async { Ok(()) })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<
        'a,
        Result<crate::services::discord::formatting::ReplaceLongMessageOutcome, String>,
    > {
        Box::pin(async { panic!("bridge body replace") })
    }
    fn can_deliver_directly(&self) -> bool {
        true
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, ()> {
        Box::pin(async { panic!("unexpected retry") })
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a crate::services::discord::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { panic!("unexpected dispatch") })
    }
    fn validate_live_routing<'a>(
        &'a self,
        _: ChannelId,
    ) -> crate::services::discord::gateway::GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        true
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Codex)
    }
}

async fn crash_then_restore() {
    if std::env::var_os("ADK_7453_CRASH_CHANNEL").is_some() {
        let root = std::env::var_os("ADK_7453_CRASH_ROOT").unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", Path::new(&root));
        let mut f = Fixture::new(Path::new(&root), 7453, ProviderKind::Codex).await;
        f.local.session_id = Some("session-7453".into());
        f.local.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
        f.local.external_turn_id = Some("external-7453".into());
        f.local.injected_prompt_message_id = Some(f.local.user_msg_id);
        f.local.request_owner_user_id =
            crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
        let first = serde_json::json!({"type":"response_item","payload":{"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"answer"}]}});
        let second = serde_json::json!({"type":"response_item","payload":{"type":"message","id":"m2","role":"assistant","content":[{"type":"output_text","text":"suffx"}]}});
        let raw = format!(
            "{first}\n{second}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n"
        );
        std::fs::write(&f.transcript, raw).unwrap();
        f.end = std::fs::metadata(&f.transcript).unwrap().len();
        let mut binding = dedupe::runtime_binding_for_tmux_session(&f.tmux).unwrap();
        binding.session_id = f.local.session_id.clone();
        dedupe::register_tmux_runtime_binding(&f.tmux, binding);
        save_inflight_state_in_root(&inflight_runtime_root().unwrap(), &f.local).unwrap();
        let context = crate::services::discord::turn_bridge::TurnBridgeContext {
            provider: ProviderKind::Codex,
            gateway: Arc::new(NoBody),
            channel_id: ChannelId::new(CHANNEL),
            user_msg_id: Some(MessageId::new(f.local.user_msg_id)),
            user_text_owned: "source witness".into(),
            request_owner_name: String::new(),
            role_binding: None,
            adk_session_key: None,
            adk_session_name: None,
            adk_session_info: None,
            adk_cwd: None,
            dispatch_id: None,
            dispatch_kind: None,
            memory_recall_usage: crate::services::memory::TokenUsage::default(),
            context_window_tokens: 0,
            context_compact_percent: 0,
            current_msg_id: Some(MessageId::new(18)),
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: None,
            new_session_id: None,
            defer_watcher_resume: false,
            reuse_status_panel_message: false,
            completion_tx: None,
            is_external_input_tui_direct: true,
            inflight_state: f.local.clone(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        crate::services::discord::turn_bridge::spawn_turn_bridge(
            f.shared.clone(),
            f.actor.clone(),
            rx,
            context,
        );
        let mut decoder = crate::services::codex_tui::rollout_tail::RolloutRecordDecoder::default();
        for message in decoder.decode(&first).unwrap() {
            tx.send(message).unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let durable: InflightTurnState = serde_json::from_slice(&f.durable()).unwrap();
                if durable.full_response == "answer" && durable.response_sent_offset == 6 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first real assistant saved as 6/6");
        for message in decoder.decode(&second).unwrap() {
            tx.send(message).unwrap();
        }
        let mut frame = f.frame();
        if let StreamMessage::CodexTuiTerminalDone {
            result, session_id, ..
        } = &mut frame
        {
            *result = "answer\n\nsuffx".into();
            *session_id = f.local.session_id.clone();
        }
        tx.send(frame).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        panic!("admission never reached the crash boundary");
    }
    let root = tempfile::tempdir().unwrap();
    let name = concat!(
        module_path!(),
        "::i7453_restart_keeps_unflushed_local_suffix"
    );
    let qualified = name.split_once("::").unwrap().1;
    let channels = serde_json::to_string(&[(CHANNEL, RuntimeHandoffKind::CodexTui)]).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", qualified, "--nocapture"])
        .env(CHILD, name)
        .env(test_override::CHANNELS_ENV, &channels)
        .env("ADK_7453_CRASH_CHANNEL", CHANNEL.to_string())
        .env("ADK_7453_CRASH_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(73),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", qualified, "--nocapture"])
        .env(CHILD, name)
        .env(test_override::CHANNELS_ENV, channels)
        .env("ADK_7453_RESTORE_ROOT", root.path())
        .env("ADK_7453_RESTORE_AHEAD", "true")
        .output()
        .unwrap();
    assert!(
        outcome.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&outcome.stdout),
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert!(String::from_utf8_lossy(&outcome.stdout).contains("1 passed; 0 failed; 0 ignored;"));
}

#[tokio::test]
async fn i7453_native_owned_preserves_body_frontier() {
    let name = concat!(
        module_path!(),
        "::i7453_native_owned_preserves_body_frontier"
    );
    if isolated(name, "owned") {
        case("owned", false).await;
    }
}
#[tokio::test]
async fn i7453_native_unclear_preserves_body_frontier() {
    let name = concat!(
        module_path!(),
        "::i7453_native_unclear_preserves_body_frontier"
    );
    if isolated(name, "held") {
        case("held", false).await;
    }
}
#[tokio::test]
async fn i7453_native_legacy_keeps_result_adoption() {
    let name = concat!(
        module_path!(),
        "::i7453_native_legacy_keeps_result_adoption"
    );
    if isolated(name, "legacy") {
        case("legacy", false).await;
    }
}
#[tokio::test]
async fn i7453_restart_keeps_unflushed_local_suffix() {
    if restore_phase().await {
        return;
    }
    crash_then_restore().await;
}
#[tokio::test]
async fn i7453_fully_consumed_eof_records_existing_limit() {
    if restore_phase().await {
        return;
    }
    let name = concat!(
        module_path!(),
        "::i7453_fully_consumed_eof_records_existing_limit"
    );
    if isolated(name, "owned") {
        case("owned", true).await;
    }
}

#[tokio::test]
async fn i7453_native_prefix_conflict_keeps_durable_bytes() {
    let name = concat!(
        module_path!(),
        "::i7453_native_prefix_conflict_keeps_durable_bytes"
    );
    if !isolated(name, "owned") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let mut f = Fixture::new(root.path(), 7453, ProviderKind::Codex).await;
    f.local.full_response = "base".into();
    f.local.response_sent_offset = 4;
    save_inflight_state_in_root(&inflight_runtime_root().unwrap(), &f.local).unwrap();
    f.baseline = serde_json::from_slice(&f.durable()).unwrap();
    f.local.full_response = "other".into();
    let bytes = f.durable();
    let frame = f.frame();
    assert!(matches!(
        f.admit(frame).await,
        Err(GuardedSaveOutcome::AuthorityPinned)
    ));
    assert_eq!(f.durable(), bytes);
}

#[tokio::test]
async fn i7453_native_empty_stream_keeps_original_result() {
    let name = concat!(
        module_path!(),
        "::i7453_native_empty_stream_keeps_original_result"
    );
    if isolated(name, "owned") {
        case("owned_empty", false).await;
    }
}

#[tokio::test]
async fn i7453_native_restart_marker_refuses_admission() {
    let name = concat!(
        module_path!(),
        "::i7453_native_restart_marker_refuses_admission"
    );
    if !isolated(name, "owned") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let mut f = Fixture::new(root.path(), 7453, ProviderKind::Codex).await;
    f.local.full_response = "answer".into();
    f.local.response_sent_offset = 6;
    f.local
        .set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
    save_inflight_state_in_root(&inflight_runtime_root().unwrap(), &f.local).unwrap();
    let bytes = f.durable();
    let frame = f.frame();
    assert!(matches!(
        f.admit(frame).await,
        Err(GuardedSaveOutcome::SuccessorOwned)
    ));
    assert_eq!(f.durable(), bytes);
}

#[tokio::test]
async fn i7453_native_same_file_alias_keeps_stream_coordinates() {
    let name = concat!(
        module_path!(),
        "::i7453_native_same_file_alias_keeps_stream_coordinates"
    );
    if !isolated(name, "owned") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let mut f = Fixture::new(root.path(), 7453, ProviderKind::Codex).await;
    let raw = format!(
        "{}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n",
        serde_json::json!({"type":"response_item","payload":{"type":"message","id":"alias-answer","role":"assistant","content":[{"type":"output_text","text":"answer"}]}})
    );
    std::fs::write(&f.transcript, raw).unwrap();
    f.end = std::fs::metadata(&f.transcript).unwrap().len();
    f.local.full_response = "answer".into();
    f.local.response_sent_offset = 6;
    save_inflight_state_in_root(&inflight_runtime_root().unwrap(), &f.local).unwrap();
    let alias = root.path().join("alias.jsonl");
    std::os::unix::fs::symlink(&f.transcript, &alias).unwrap();
    f.local.output_path = Some(alias.display().to_string());
    let frame = f.frame();
    f.admit(frame)
        .await
        .expect("an alias of this same source remains admissible");
    let stored: InflightTurnState = serde_json::from_slice(&f.durable()).unwrap();
    assert_eq!(
        (stored.full_response.as_str(), stored.response_sent_offset),
        ("answer", 6)
    );
    assert_eq!(
        f.local.output_path,
        Some(f.transcript.display().to_string())
    );
}
