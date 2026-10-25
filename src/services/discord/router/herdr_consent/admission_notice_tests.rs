//! A durably held input whose consent card cannot be offered tells its author why, once.
use super::*;
use crate::services::discord::router::intake_dispatch::{
    self as intake, QueuedAdmissionDisposition,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::session_host::policy::{Unverified, diagnostics};
use crate::services::turn_orchestrator::QueuePersistenceContext;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::atomic::{AtomicUsize, Ordering};

const SOURCE: u64 = 8_917_101;

struct Fixture {
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    state: discord_mock::DiscordMockState,
    server: tokio::task::JoinHandle<()>,
    _config: crate::config::TestEnvVarGuard,
    _routing: crate::config::TestEnvVarGuard,
    _env: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let routing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            "disabled".as_ref(),
        );
        let config_path = root.path().join("isolated.yaml");
        std::fs::write(&config_path, "server: {}\n").unwrap();
        let config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &config_path,
        );
        let state = discord_mock::DiscordMockState::new();
        let (proxy, _gateway, server) = discord_mock::start(state.clone()).await;
        let http = Arc::new(
            serenity::HttpBuilder::new("isolated-notice-token")
                .proxy(proxy)
                .ratelimiter_disabled(true)
                .build(),
        );
        Self {
            shared: crate::services::discord::make_shared_data_for_tests(),
            http,
            state,
            server,
            _config: config,
            _routing: routing,
            _env: env,
            _root: root,
        }
    }
    fn deps(&self) -> IntakeDeps<'_> {
        IntakeDeps {
            http: &self.http,
            shared: &self.shared,
            token: "isolated-notice-token",
            cache: None,
            ctx_for_chained_dispatch: None,
        }
    }
    fn queued(&self) -> Vec<u64> {
        crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &ProviderKind::Claude,
            &self.shared.token_hash,
            ChannelId::new(discord_mock::CHANNEL_ID),
        )
        .unwrap()
        .0
        .iter()
        .map(|item| item.message_id.get())
        .collect()
    }
    fn notices(&self) -> usize {
        self.state.local_note_posts.load(Ordering::SeqCst)
    }
    /// The live intake entry with the cause the policy last recorded for the configured socket.
    async fn live(&self, cause: Unverified, checkpoints: &Arc<AtomicUsize>) -> bool {
        diagnostics::record(
            std::path::Path::new("/adk/herdr/agentdesk.sock"),
            &crate::services::session_host::policy::ProofCache::default(),
            &Err(cause),
        );
        let saved = checkpoints.clone();
        let request = IntakeRequest {
            intake_outbox_id: None,
            channel_id: ChannelId::new(discord_mock::CHANNEL_ID),
            user_msg_id: MessageId::new(SOURCE),
            source_message_ids: vec![MessageId::new(SOURCE)],
            busy_followup_retry_user_msg_id: MessageId::new(SOURCE),
            request_owner: UserId::new(1),
            request_owner_name: "owner".into(),
            user_text: "original held prompt".into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: None,
            turn_kind: crate::services::discord::TurnKind::Foreground,
            preserve_on_cancel: true,
        };
        let submission = IntakeSubmission {
            provider: ProviderKind::Claude,
            request,
            origin: IntakeOrigin::LiveMessage,
            preserve_on_cancel: true,
            has_nonportable_uploads: false,
            attachments: Vec::new(),
            preloaded_uploads: Vec::new(),
            voice_announcement: None,
        };
        intake::dispatch_live_text_intake(&self.deps(), submission, move || {
            saved.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .is_err()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn consent_unverified_hold_notices_once_per_source_and_cause_and_keeps_custody() {
    let f = Fixture::new().await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    assert!(
        f.live(Unverified::Identity, &checkpoints).await,
        "unverified_hold_keeps_its_error_result_contract"
    );
    assert_eq!(
        (f.notices(), checkpoints.load(Ordering::SeqCst), f.queued()),
        (1, 0, vec![SOURCE]),
        "unverified_hold_notices_once_and_keeps_original_custody_contract"
    );
    let sent = f.state.written_texts();
    assert!(
        sent.iter().any(|text| text.contains("원 입력은 보존됨")
            && text.contains("next_restart_policy_unverified:Identity")
            && text.contains("loaded executable identity")),
        "unverified_hold_notice_names_reason_and_action_contract: {sent:?}"
    );

    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let taken = f
        .shared
        .mailbox(channel)
        .take_next_soft(QueuePersistenceContext::new(
            &ProviderKind::Claude,
            &f.shared.token_hash,
            None,
        ))
        .await;
    let original = taken.intervention.as_ref().expect("held original head");
    let drained = intake::admit_queued_intake(
        &f.deps(),
        ProviderKind::Claude,
        channel,
        original,
        original.author_id,
        "owner".into(),
        false,
        false,
        "consent-notice-test",
        taken.dispatch_lease.clone(),
    )
    .await;
    assert!(matches!(drained, QueuedAdmissionDisposition::Deferred));
    assert_eq!(
        (f.notices(), f.queued()),
        (1, vec![SOURCE]),
        "queued_redrain_of_same_cause_sends_no_notice_and_keeps_custody_contract"
    );

    assert!(f.live(Unverified::Identity, &checkpoints).await);
    assert_eq!(
        f.notices(),
        1,
        "unread_source_replay_with_same_cause_is_suppressed_contract"
    );
    assert!(f.live(Unverified::Ambiguous, &checkpoints).await);
    assert_eq!(
        f.notices(),
        2,
        "changed_cause_for_same_source_notices_once_more_contract"
    );
    assert!(
        f.state
            .written_texts()
            .iter()
            .any(|text| text.contains("next_restart_policy_ambiguous")),
        "changed_cause_notice_names_the_new_reason_contract"
    );
    assert_eq!(checkpoints.load(Ordering::SeqCst), 0);
    assert!(f.queued().contains(&SOURCE));
}
