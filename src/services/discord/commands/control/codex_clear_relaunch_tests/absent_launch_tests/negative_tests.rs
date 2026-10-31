//! Failed absence evidence preserves the restored head before provider effects.

use super::super::CanaryFixture;
use super::*;

pub(super) fn persisted_queue_bytes(fixture: &CanaryFixture) -> Vec<u8> {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("codex")
        .join(&fixture.shared.token_hash)
        .join(format!("{CANARY_CHANNEL}.json"));
    fs::read(path).unwrap()
}

pub(super) fn payload_snapshot(
    queue: &[crate::services::turn_orchestrator::Intervention],
) -> Vec<(std::time::Instant, serde_json::Value)> {
    queue.iter().map(|item| {
        let sources: Vec<_> = item.source_message_queued_generations.iter().map(|source| json!({
            "message_id": source.message_id.get(), "queued_generation": source.queued_generation,
            "enqueued_at_epoch_us": source.enqueued_at_epoch_us, "preserve_on_cancel": source.preserve_on_cancel,
            "route": source.route,
        })).collect();
        let segments: Vec<_> = item.source_text_segments.iter().map(|part| json!({
            "message_id": part.message_id.get(), "text": part.text,
        })).collect();
        (item.created_at, json!({
            "author_id": item.author_id.get(), "author_is_bot": item.author_is_bot,
            "message_id": item.message_id.get(), "queued_generation": item.queued_generation,
            "source_message_ids": item.source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
            "source_message_queued_generations": sources, "source_text_segments": segments,
            "text": item.text, "mode": format!("{:?}", item.mode), "reply_context": item.reply_context,
            "has_reply_boundary": item.has_reply_boundary, "merge_consecutive": item.merge_consecutive,
            "pending_uploads": item.pending_uploads,
        }))
    }).collect()
}

#[derive(Clone, Copy, Debug)]
enum Refusal {
    OtherSpawnNonce,
    BlankNonce,
    MissingContext,
    PresentSession,
    PresentDeadSession,
    OtherOwnership,
    UnprovedSuccessor,
    ProcessPid,
    ProbeFailed,
    MissingEndpoint,
}

#[tokio::test(flavor = "current_thread")]
async fn rn1_foreign_spawn_nonce_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::rn1_foreign_spawn_nonce_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::OtherSpawnNonce).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rn2_blank_nonce_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::rn2_blank_nonce_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::BlankNonce).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rn2_missing_context_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::rn2_missing_context_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::MissingContext).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rn3_present_session_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::rn3_present_session_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::PresentSession).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rn3_present_dead_session_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::rn3_present_dead_session_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::PresentDeadSession).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rn4_foreign_ownership_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::rn4_foreign_ownership_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::OtherOwnership).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rn5_unproved_successor_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::rn5_unproved_successor_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::UnprovedSuccessor).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn process_pid_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::process_pid_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::ProcessPid).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn probe_failure_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::probe_failure_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::ProbeFailed).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_endpoint_keeps_restored_head_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_endpoint_keeps_restored_head_pg"
    )) {
        restored_head_stays_held(Refusal::MissingEndpoint).await;
    }
}

async fn restored_head_stays_held(refusal: Refusal) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let endpoint = (!matches!(refusal, Refusal::MissingEndpoint)).then(|| {
        crate::services::claude_tui::hook_server::publish_hook_endpoint("http://127.0.0.1:9".into())
    });
    let _cli = fixture.cold_cli();
    fixture.fake.mark_missing();
    if !matches!(refusal, Refusal::ProcessPid) {
        fixture.alive.store(false, Ordering::SeqCst);
        crate::services::session_backend::remove_process_session(CANARY_TMUX);
    }
    let nonce_path = PathBuf::from(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        "spawn_nonce",
    ));
    let ownership_path = PathBuf::from(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ));
    match refusal {
        Refusal::OtherSpawnNonce => {
            fs::write(&nonce_path, uuid::Uuid::new_v4().simple().to_string()).unwrap();
        }
        Refusal::BlankNonce => fs::write(&nonce_path, "").unwrap(),
        Refusal::MissingContext => {
            let path = crate::config::runtime_root()
                .unwrap()
                .join("runtime/binding_contexts/codex")
                .join(format!("{}.json", fixture.context.execution_nonce));
            fs::remove_file(path).unwrap();
        }
        Refusal::PresentSession => fixture.fake.mark_present(),
        Refusal::PresentDeadSession => {
            fixture.fake.mark_dead_pane();
            assert_eq!(
                crate::services::platform::tmux::session_presence(CANARY_TMUX),
                crate::services::platform::tmux::SessionPresence::Present,
            );
            assert_eq!(
                crate::services::platform::tmux::pane_liveness(CANARY_TMUX),
                crate::services::platform::tmux::PaneLiveness::DeadOrAbsent,
            );
        }
        Refusal::OtherOwnership => {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&ownership_path).unwrap()).unwrap();
            value["codex_ownership"]["execution_nonce"] =
                json!(uuid::Uuid::new_v4().simple().to_string());
            fs::write(&ownership_path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        Refusal::UnprovedSuccessor => {
            let mut context = fixture.context.clone();
            context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
            context.created_at = chrono::Utc::now();
            let prepared = PreparedIncarnation::create(context).unwrap();
            crate::services::discord::stamp_spawn_markers(CANARY_TMUX, Some(&prepared)).unwrap();
        }
        Refusal::ProbeFailed => fixture.fake.fail_probes(),
        Refusal::ProcessPid => {
            assert!(crate::services::session_backend::process_session_pid(CANARY_TMUX).is_some());
        }
        Refusal::MissingEndpoint => {
            assert!(crate::services::claude_tui::hook_server::current_hook_endpoint().is_none());
        }
    }
    assert!(dedupe::evict_dead_tmux_mirror(CANARY_TMUX));
    let identity = restore_disk_head(&mut fixture).await;
    let queue_before = persisted_queue_bytes(&fixture);
    let payload_before = payload_snapshot(
        &fixture
            .shared
            .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
            .snapshot()
            .await
            .intervention_queue,
    );
    let nonce_before = fs::read(&nonce_path).unwrap();
    let ownership_before = fs::read(&ownership_path).unwrap();
    let history_before = fs::read(fixture.history()).unwrap();
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    fixture.fake.take_calls();
    let blocked = crate::services::discord::router::hosted_tui_promote_readiness_blocked(
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
    )
    .await;
    assert!(
        blocked,
        "{refusal:?}: incomplete evidence must preserve the head"
    );
    if matches!(refusal, Refusal::UnprovedSuccessor) {
        assert_eq!(
            dedupe::codex_verified_hold_detail(CANARY_TMUX),
            "successor_unproved"
        );
    }
    let after = fixture.shared.mailbox(channel).snapshot().await;
    assert_eq!(payload_snapshot(&after.intervention_queue), payload_before);
    assert_eq!(persisted_queue_bytes(&fixture), queue_before);
    assert_eq!(after.intervention_queue.len(), 1);
    assert_eq!(QueueIdentity::of(&after.intervention_queue[0]), identity);
    assert!(after.cancel_token.is_none() && after.pending_user_dispatch.is_none());
    let (disk, _) =
        crate::services::turn_orchestrator::test_support::load_channel_pending_queue_checked(
            &ProviderKind::Codex,
            &fixture.shared.token_hash,
            channel,
        )
        .unwrap();
    assert_eq!(disk.len(), 1);
    assert_eq!(QueueIdentity::of(&disk[0]), identity);
    assert_eq!(fs::read(&nonce_path).unwrap(), nonce_before);
    assert_eq!(fs::read(&ownership_path).unwrap(), ownership_before);
    assert_eq!(fs::read(fixture.history()).unwrap(), history_before);
    assert!(!fixture.fake.take_calls().iter().any(|call| {
        ["new-session", "kill-session", "send-keys", "paste-buffer"]
            .iter()
            .any(|effect| call.starts_with(effect))
    }));
    drop(endpoint);
}

#[tokio::test(flavor = "current_thread")]
async fn unobserved_preflight_preserves_absent_verified_pin_pg() {
    let test = concat!(
        module_path!(),
        "::unobserved_preflight_preserves_absent_verified_pin_pg"
    );
    if !isolated(test) {
        return;
    }
    let fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    fixture.fake.mark_missing();
    fixture.alive.store(false, Ordering::SeqCst);
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    let token = Arc::new(CancelToken::new());
    token.cancelled.store(true, Ordering::SeqCst);
    fixture.fake.take_calls();
    assert!(crate::services::codex_tui::verified_hold::existing_incarnation(CANARY_TMUX));
    assert!(
        crate::services::codex_tui::verified_hold::preflight(CANARY_TMUX, Some(&token)).is_some()
    );
    assert!(
        !fixture
            .fake
            .take_calls()
            .iter()
            .any(|call| { call.starts_with("kill-session") || call.starts_with("new-session") })
    );
}
