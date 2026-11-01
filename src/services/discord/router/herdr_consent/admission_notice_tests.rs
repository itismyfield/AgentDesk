//! A durably held input whose consent card cannot be offered tells its author why, once.
use super::*;
use crate::services::discord::router::intake_dispatch::{
    self as intake, QueuedAdmissionDisposition,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::session_host::policy::{PeerSource, ProofCache, Unverified, diagnostics};
use crate::services::turn_orchestrator::QueuePersistenceContext;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::atomic::{AtomicUsize, Ordering};

const SOURCE: u64 = 8_917_101;
const SOCKET: &str = "/adk/herdr/agentdesk.sock";

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
    /// Every original source still in durable custody, including ones merged into an earlier item.
    fn held_sources(&self) -> Vec<u64> {
        crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &ProviderKind::Claude,
            &self.shared.token_hash,
            ChannelId::new(discord_mock::CHANNEL_ID),
        )
        .unwrap()
        .0
        .iter()
        .flat_map(|item| item.source_message_queued_generations())
        .map(|source| source.message_id.get())
        .collect()
    }
    fn notices(&self) -> usize {
        self.state.local_note_posts.load(Ordering::SeqCst)
    }
    fn last_notice(&self) -> String {
        self.state.written_texts().pop().unwrap_or_default()
    }
    fn store_dir(&self) -> std::path::PathBuf {
        self._root.path().join("runtime/herdr_policy/consents")
    }
    /// Leaves an interrupted publication marker, which every consent store read refuses.
    fn interrupt_store(&self, digit: char) {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.store_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let marker = dir.join(format!("{}.pending", digit.to_string().repeat(64)));
        std::fs::write(marker, b"").unwrap();
    }
    /// Asserts no runtime path, raw or canonical, reaches `text`.
    fn assert_no_runtime_path(&self, text: &str) {
        let raw = self._root.path().to_path_buf();
        let canonical = raw.canonicalize().unwrap();
        for path in [raw, canonical] {
            let path = path.display().to_string();
            assert!(!text.contains(&path), "runtime path {path} in {text:?}");
        }
        assert!(!text.contains("herdr_policy"), "store path in {text:?}");
    }
    /// The live intake entry for `source`; returns the error text a command reply would carry.
    async fn live(&self, source: u64, checkpoints: &Arc<AtomicUsize>) -> Option<String> {
        let saved = checkpoints.clone();
        let request = IntakeRequest {
            intake_outbox_id: None,
            channel_id: ChannelId::new(discord_mock::CHANNEL_ID),
            user_msg_id: MessageId::new(source),
            source_message_ids: vec![MessageId::new(source)],
            busy_followup_retry_user_msg_id: MessageId::new(source),
            request_owner: UserId::new(1),
            request_owner_name: "owner".into(),
            // Distinct text, so the mailbox never folds one source into another as a repeat.
            user_text: format!("original held prompt {source}"),
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
            owner: None,
        };
        intake::dispatch_live_text_intake(&self.deps(), submission, move || {
            saved.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .err()
        .map(|error| error.to_string())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn consent_unverified_hold_notices_once_per_source_and_cause_and_keeps_custody() {
    let f = Fixture::new().await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    assert!(
        f.live(SOURCE, &checkpoints).await.is_some(),
        "unverified_hold_keeps_its_error_result_contract"
    );
    assert_eq!(
        (f.notices(), checkpoints.load(Ordering::SeqCst), f.queued()),
        (1, 0, vec![SOURCE]),
        "unverified_hold_notices_once_and_keeps_original_custody_contract"
    );
    let sent = f.last_notice();
    assert!(
        sent.contains("원 입력은 보존됨")
            && sent.contains("원인: not_observed")
            && sent.contains("hosted_row_unverified"),
        "unverified_hold_notice_names_this_request_cause_contract: {sent:?}"
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

    assert!(f.live(SOURCE, &checkpoints).await.is_some());
    assert_eq!(
        f.notices(),
        1,
        "unread_source_replay_with_same_cause_is_suppressed_contract"
    );
    f.interrupt_store('a');
    assert!(f.live(SOURCE, &checkpoints).await.is_some());
    assert_eq!(
        f.notices(),
        2,
        "changed_cause_for_same_source_notices_once_more_contract"
    );
    assert!(
        f.last_notice().contains("consent_store_unavailable"),
        "changed_cause_notice_names_the_new_code_contract"
    );
    assert_eq!(checkpoints.load(Ordering::SeqCst), 0);
    assert!(f.queued().contains(&SOURCE));
}

#[tokio::test(flavor = "current_thread")]
async fn consent_store_refusal_reaches_discord_as_a_stable_code_without_runtime_paths() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new().await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let (loose, interrupted) = (SOURCE + 1, SOURCE + 2);
    std::fs::create_dir_all(f.store_dir()).unwrap();
    std::fs::set_permissions(f.store_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
    for (source, case) in [(loose, "store_mode"), (interrupted, "pending_marker")] {
        if source == interrupted {
            f.interrupt_store('b');
        }
        let before = f.notices();
        let reply = f.live(source, &checkpoints).await.expect(case);
        assert_eq!(f.notices(), before + 1, "{case}: one notice");
        let notice = f.last_notice();
        assert!(
            notice.contains("원 입력은 보존됨") && notice.contains("consent_store_unavailable"),
            "{case}: stable code contract: {notice:?}"
        );
        f.assert_no_runtime_path(&notice);
        f.assert_no_runtime_path(&reply);
        assert!(f.held_sources().contains(&source), "{case}: custody kept");
    }
    // Another marker names another file; the same cause stays one notice for the same source.
    f.interrupt_store('c');
    assert!(f.live(interrupted, &checkpoints).await.is_some());
    assert_eq!(
        f.notices(),
        2,
        "path_free_semantic_suppresses_the_same_cause_contract"
    );
    assert_eq!(checkpoints.load(Ordering::SeqCst), 0);
}

/// Fails the first observation with `Identity`; the next one first records another request's
/// `Ambiguous` observation of the same socket, then fails.
struct OverwrittenPeer(AtomicUsize);
impl PeerSource for OverwrittenPeer {
    fn snapshot(
        &self,
    ) -> Result<crate::services::session_host::policy::peer::PeerSnapshot, Unverified> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(Unverified::Identity);
        }
        let other = Err(Unverified::Ambiguous);
        diagnostics::record(std::path::Path::new(SOCKET), &ProofCache::default(), &other);
        Err(Unverified::Peer)
    }
}

struct ScopedPeer(Option<Arc<dyn PeerSource>>);
impl Drop for ScopedPeer {
    fn drop(&mut self) {
        TEST_SOURCE.with(|source| *source.borrow_mut() = self.0.take());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn consent_hold_notice_keeps_its_own_observation_after_another_request_records_pg() {
    let mut f = Fixture::new().await;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    f.shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let peer = Arc::new(OverwrittenPeer(AtomicUsize::new(0)));
    let source: Arc<dyn PeerSource> = peer.clone();
    let _peer = ScopedPeer(TEST_SOURCE.with(|slot| slot.replace(Some(source))));
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let held = SOURCE + 3;
    assert!(f.live(held, &checkpoints).await.is_some());
    assert_eq!(
        peer.0.load(Ordering::SeqCst),
        2,
        "the other observation landed before this hold was reported"
    );
    assert_eq!(f.notices(), 1);
    let notice = f.last_notice();
    assert!(
        notice.contains("원인: next_restart_policy_unverified:Identity")
            && notice.contains("loaded executable identity")
            && notice.contains("consent_preview_unavailable")
            && !notice.contains("ambiguous"),
        "notice_keeps_this_request_observation_contract: {notice:?}"
    );
    assert_eq!(
        (checkpoints.load(Ordering::SeqCst), f.queued()),
        (0, vec![held]),
        "observed_hold_keeps_original_custody_contract"
    );
    drop(f);
    pool.close().await;
    db.drop().await;
}
