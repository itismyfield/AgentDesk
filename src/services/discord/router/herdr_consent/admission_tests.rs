use super::*;
use crate::services::discord::router::intake_dispatch::{
    self as intake, IntakeAdmission, QueuedAdmissionDisposition,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::turn_orchestrator::QueuePersistenceContext;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::atomic::Ordering;

struct Fixture {
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    state: discord_mock::DiscordMockState,
    server: tokio::task::JoinHandle<()>,
    _config: crate::config::TestEnvVarGuard,
    _routing: crate::config::TestEnvVarGuard,
    _env: crate::config::TestEnvVarGuard,
    root: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(mode: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let routing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            mode.as_ref(),
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
            serenity::HttpBuilder::new("isolated-admission-token")
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
            root,
        }
    }
    fn deps(&self) -> IntakeDeps<'_> {
        IntakeDeps {
            http: &self.http,
            shared: &self.shared,
            token: "isolated-admission-token",
            cache: None,
            ctx_for_chained_dispatch: None,
        }
    }
    fn hold(&self) -> Hold {
        Hold {
            key: ConsentKey {
                node: "node".into(),
                user: 1,
                socket: "/adk/herdr/agentdesk.sock".into(),
                named_session: "agentdesk".into(),
                config: self.root.path().join("personal.toml"),
                target: self.root.path().join("personal.toml"),
            },
            preview: Preview {
                revision: 1,
                witness: crate::services::session_host::ServerWitness::for_test(7),
                target_hash: [1; 32],
                chain_hash: [2; 32],
                checker_hash: [3; 32],
            },
            owner: 1,
            bot: 2,
        }
    }
    fn disk(&self) -> Vec<crate::services::turn_orchestrator::Intervention> {
        crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &ProviderKind::Claude,
            &self.shared.token_hash,
            ChannelId::new(discord_mock::CHANNEL_ID),
        )
        .unwrap()
        .0
    }
}
fn request(id: u64) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: None,
        channel_id: ChannelId::new(discord_mock::CHANNEL_ID),
        user_msg_id: MessageId::new(id),
        source_message_ids: vec![MessageId::new(id)],
        busy_followup_retry_user_msg_id: MessageId::new(id),
        request_owner: UserId::new(1),
        request_owner_name: "owner".into(),
        user_text: format!("expanded prompt {id}"),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: Some("original reply".into()),
        has_reply_boundary: true,
        dm_hint: None,
        turn_kind: crate::services::discord::TurnKind::Foreground,
        preserve_on_cancel: true,
    }
}
fn submission(id: u64, origin: IntakeOrigin) -> IntakeSubmission {
    IntakeSubmission {
        provider: ProviderKind::Claude,
        request: request(id),
        origin,
        preserve_on_cancel: true,
        has_nonportable_uploads: false,
        attachments: Vec::new(),
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
    }
}
fn voice() -> crate::voice::prompt::VoiceTranscriptAnnouncement {
    crate::voice::prompt::VoiceTranscriptAnnouncement {
        transcript: "original voice transcript".into(),
        user_id: "1".into(),
        utterance_id: "consent-fixture".into(),
        language: "ko".into(),
        verbose_progress: false,
        started_at: None,
        completed_at: None,
        samples_written: Some(48000),
        control_channel_id: Some(discord_mock::CHANNEL_ID),
        stt_mode: Some("file".into()),
        stt_latency_ms: Some(10),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_actual_intake_and_attachment_gates_preserve_all_origins_without_consent_io() {
    let f = Fixture::new("disabled").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let reads = STORE_READS.load(Ordering::SeqCst);
    for (index, origin) in [
        IntakeOrigin::LiveMessage,
        IntakeOrigin::SlashSkill,
        IntakeOrigin::TextSkill,
        IntakeOrigin::RawAttachment,
        IntakeOrigin::QueuedDrain(None),
    ]
    .into_iter()
    .enumerate()
    {
        let mut value = submission(8_916_000 + index as u64, origin);
        if index == 3 {
            value
                .attachments
                .push(super::super::super::message_handler::AttachmentDescriptor {
                    filename: "original.bin".into(),
                    url: "https://cdn.discordapp.com/attachments/a/b/original.bin".into(),
                });
        }
        let admitted = intake::admit_text_intake(&f.deps(), &value).await;
        assert!(
            matches!(admitted, IntakeAdmission::Local(_)),
            "legacy_actual_intake_keeps_base_local_contract: {origin:?}"
        );
        assert!(
            intake::resolve_attachment_admission(admitted).is_ok(),
            "legacy_actual_attachment_gate_keeps_base_local_contract"
        );
    }
    let mut spoken = submission(8_916_010, IntakeOrigin::LiveMessage);
    spoken.voice_announcement = Some(voice());
    assert!(
        matches!(
            intake::admit_text_intake(&f.deps(), &spoken).await,
            IntakeAdmission::Local(_)
        ),
        "legacy_voice_admission_keeps_base_local_contract"
    );
    assert!(
        !worker(&f.deps(), &request(8_916_011), None).await,
        "legacy_worker_guard_does_not_consume_execution_contract"
    );
    assert_eq!(
        STORE_READS.load(Ordering::SeqCst),
        reads,
        "legacy_all_origins_store_reads_zero_contract"
    );
    assert!(
        !f.root.path().join("runtime/herdr_policy").exists(),
        "legacy_all_origins_store_files_zero_contract"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "legacy_all_origins_cards_zero_contract"
    );
    assert!(
        f.state.unhandled.lock().unwrap().is_empty(),
        "legacy_all_origins_no_extra_http_contract"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_actual_skill_dispatch_preserves_base_enforce_refusal_without_consent_io() {
    let f = Fixture::new("enforce").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let reads = STORE_READS.load(Ordering::SeqCst);
    for (index, origin) in [IntakeOrigin::SlashSkill, IntakeOrigin::TextSkill]
        .into_iter()
        .enumerate()
    {
        let result = intake::dispatch_skill_intake(
            &f.deps(),
            ProviderKind::Claude,
            ChannelId::new(discord_mock::CHANNEL_ID),
            MessageId::new(8_916_020 + index as u64),
            UserId::new(1),
            "owner".into(),
            "expanded skill prompt".into(),
            origin,
            Vec::new(),
            None,
        )
        .await;
        assert!(
            result.is_ok(),
            "legacy_actual_skill_keeps_base_blocked_result_contract: {result:?}"
        );
    }
    assert_eq!(
        STORE_READS.load(Ordering::SeqCst),
        reads,
        "legacy_actual_skill_store_reads_zero_contract"
    );
    assert!(
        !f.root.path().join("runtime/herdr_policy").exists(),
        "legacy_actual_skill_store_files_zero_contract"
    );
    assert!(
        f.shared
            .mailbox(ChannelId::new(discord_mock::CHANNEL_ID))
            .try_snapshot()
            .await
            .unwrap()
            .intervention_queue
            .is_empty(),
        "legacy_actual_skill_produces_no_consent_hold_contract"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_actual_worker_entry_retains_replay_defer_without_consent_io() {
    let f = Fixture::new("disabled").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let _active = intake::replay_admission::test_activation::activate(discord_mock::CHANNEL_ID);
    let mut request = request(8_916_025);
    request.intake_outbox_id = Some(77);
    let reads = STORE_READS.load(Ordering::SeqCst);
    let result = crate::services::discord::execute_intake_turn_core(
        &f.http,
        &f.shared,
        "isolated-admission-token",
        request,
        Vec::new(),
    )
    .await;
    assert!(
        matches!(
            result,
            Ok(crate::services::discord::IntakeExecution::Deferred)
        ),
        "legacy_actual_worker_entry_preserves_base_replay_defer_contract: {result:?}"
    );
    assert_eq!(
        STORE_READS.load(Ordering::SeqCst),
        reads,
        "legacy_actual_worker_entry_consent_io_zero_contract"
    );
    assert!(
        !f.root.path().join("runtime/herdr_policy").exists(),
        "legacy_actual_worker_entry_consent_files_zero_contract"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "legacy_actual_worker_entry_cards_zero_contract"
    );
    assert!(f.state.unhandled.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_actual_queued_entry_retains_local_admission_without_consent_io() {
    let f = Fixture::new("disabled").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let reads = STORE_READS.load(Ordering::SeqCst);
    assert!(
        intake::defer_live_submission(
            &f.deps(),
            submission(8_916_026, IntakeOrigin::LiveMessage),
            true
        )
        .await
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
    let original = taken.intervention.as_ref().unwrap();
    let result = intake::admit_queued_intake(
        &f.deps(),
        ProviderKind::Claude,
        channel,
        original,
        original.author_id,
        "owner".into(),
        false,
        false,
        "legacy-consent-test",
        taken.dispatch_lease.clone(),
    )
    .await;
    assert!(
        matches!(result, QueuedAdmissionDisposition::Admitted(_)),
        "legacy_actual_queued_entry_preserves_base_admitted_contract"
    );
    assert_eq!(
        STORE_READS.load(Ordering::SeqCst),
        reads,
        "legacy_actual_queued_entry_consent_io_zero_contract"
    );
    assert!(
        !f.root.path().join("runtime/herdr_policy").exists(),
        "legacy_actual_queued_entry_consent_files_zero_contract"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "legacy_actual_queued_entry_cards_zero_contract"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn consent_live_custody_keeps_expanded_prompt_sources_upload_and_voice_fifo() {
    let f = Fixture::new("disabled").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    for (id, origin, spoken) in [
        (8_916_030, IntakeOrigin::TextSkill, false),
        (8_916_031, IntakeOrigin::LiveMessage, true),
    ] {
        let mut value = submission(id, origin);
        value.preloaded_uploads = vec![format!("upload-{id}").into()];
        if spoken {
            value.voice_announcement = Some(voice());
        }
        let result = hold_live(&f.deps(), value, Ok(f.hold())).await;
        assert!(
            result.is_ok(),
            "consent_durable_custody_before_card_contract: {result:?}"
        );
    }
    let items = f.disk();
    assert_eq!(
        items
            .iter()
            .map(|item| item.message_id.get())
            .collect::<Vec<_>>(),
        vec![8_916_030, 8_916_031],
        "consent_original_custody_fifo_contract"
    );
    for (item, expected_origin) in items.iter().zip(["TextSkill", "Voice"]) {
        let id = item.message_id.get();
        assert_eq!(
            item.text,
            format!("expanded prompt {id}"),
            "consent_expanded_prompt_preserved_contract"
        );
        assert_eq!(
            item.pending_uploads,
            vec![
                crate::services::cluster::attachment_transfer::uploads::Upload::from(format!(
                    "upload-{id}"
                ))
            ],
            "consent_upload_preserved_contract"
        );
        assert_eq!(item.reply_context.as_deref(), Some("original reply"));
        assert!(
            item.source_message_queued_generations()
                .iter()
                .all(|source| source.origin.as_deref() == Some(expected_origin)),
            "consent_original_source_origin_preserved_contract"
        );
    }
    assert_eq!(
        items[1].voice_announcement,
        Some(voice()),
        "consent_voice_metadata_preserved_contract"
    );
    let held = store().load_all().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(
        held[0].unresolved_intents().len(),
        2,
        "consent_records_only_durable_original_refs_contract"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "consent_failed_card_owner_keeps_payload_contract"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn consent_unprepared_attachment_and_missing_durable_source_never_succeed_as_text() {
    let f = Fixture::new("disabled").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let mut value = submission(8_916_040, IntakeOrigin::RawAttachment);
    value
        .attachments
        .push(super::super::super::message_handler::AttachmentDescriptor {
            filename: "original.bin".into(),
            url: "https://cdn.discordapp.com/attachments/a/b/original.bin".into(),
        });
    let result = hold_live(&f.deps(), value, Ok(f.hold())).await;
    assert!(
        result.is_err(),
        "consent_unprepared_attachment_refuses_text_only_contract"
    );
    assert!(
        f.disk().is_empty(),
        "consent_unprepared_attachment_never_queues_text_only_contract"
    );
    let result = register_mailbox(
        &f.deps(),
        &ProviderKind::Claude,
        &request(8_916_041),
        f.hold(),
        false,
    )
    .await;
    assert!(
        result.is_err(),
        "consent_registration_requires_durable_source_contract"
    );
    assert!(
        !f.root.path().join("runtime/herdr_policy").exists(),
        "consent_no_record_before_custody_contract"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "consent_no_card_before_custody_contract"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn consent_actual_queued_readmission_restores_same_head_lease_uploads_and_origin() {
    let f = Fixture::new("disabled").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    for id in [8_916_050, 8_916_051] {
        let mut value = submission(id, IntakeOrigin::SlashSkill);
        value
            .preloaded_uploads
            .push(format!("original-{id}").into());
        assert!(hold_live(&f.deps(), value, Ok(f.hold())).await.is_ok());
    }
    let before = f.disk();
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
    assert!(
        taken.intervention.is_some() && taken.dispatch_lease.is_some(),
        "consent_queue_owns_original_head_and_lease_contract"
    );
    let original = taken.intervention.as_ref().unwrap();
    let result = intake::admit_queued_intake(
        &f.deps(),
        ProviderKind::Claude,
        channel,
        original,
        original.author_id,
        "owner".into(),
        false,
        false,
        "consent-test",
        taken.dispatch_lease.clone(),
    )
    .await;
    assert!(
        matches!(result, QueuedAdmissionDisposition::Deferred),
        "consent_queued_reuses_deferred_contract"
    );
    let after = f.disk();
    assert_eq!(
        after.iter().map(|item| item.message_id).collect::<Vec<_>>(),
        before
            .iter()
            .map(|item| item.message_id)
            .collect::<Vec<_>>(),
        "consent_queued_restore_never_rotates_fifo_contract"
    );
    assert_eq!(
        after[0].pending_uploads, before[0].pending_uploads,
        "consent_queued_restore_preserves_uploads_contract"
    );
    assert_eq!(
        after[0]
            .source_message_queued_generations()
            .into_iter()
            .map(|source| (
                source.message_id,
                source.queued_generation,
                source.enqueued_at_epoch_us,
                source.preserve_on_cancel,
                source.route,
                source.origin,
            ))
            .collect::<Vec<_>>(),
        before[0]
            .source_message_queued_generations()
            .into_iter()
            .map(|source| (
                source.message_id,
                source.queued_generation,
                source.enqueued_at_epoch_us,
                source.preserve_on_cancel,
                source.route,
                source.origin,
            ))
            .collect::<Vec<_>>(),
        "consent_queued_drain_never_overwrites_origin_contract"
    );
    assert_eq!(
        store().load_all().unwrap()[0].unresolved_intents().len(),
        2,
        "consent_queued_readmission_does_not_duplicate_refs_contract"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn consent_persistence_failure_retains_live_payload_without_success_or_card() {
    let f = Fixture::new("disabled").await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let directory = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude");
    std::fs::create_dir_all(&directory).unwrap();
    let obstacle = directory.join(&f.shared.token_hash);
    std::fs::write(&obstacle, b"fixture blocks queue directory").unwrap();
    let checkpoints = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let saved = checkpoints.clone();
    let result = intake::dispatch_live_text_intake(
        &f.deps(),
        submission(8_916_060, IntakeOrigin::TextSkill),
        move || {
            saved.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert!(
        result.is_err(),
        "consent_failed_persistence_never_reports_custody_success_contract"
    );
    assert!(
        !f.root.path().join("runtime/herdr_policy").exists(),
        "consent_failed_persistence_records_zero_refs_contract"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "consent_failed_persistence_sends_zero_cards_contract"
    );
    let snapshot = f
        .shared
        .mailbox(ChannelId::new(discord_mock::CHANNEL_ID))
        .try_snapshot()
        .await
        .unwrap();
    assert_eq!(
        checkpoints.load(Ordering::SeqCst),
        0,
        "consent_failed_persistence_keeps_discord_source_unread_contract"
    );
    assert!(
        snapshot.intervention_queue.is_empty(),
        "existing queue rolls back a failed durable enqueue"
    );
    std::fs::remove_file(obstacle).unwrap();
    let replayed = hold_live(
        &f.deps(),
        submission(8_916_060, IntakeOrigin::TextSkill),
        Ok(f.hold()),
    )
    .await;
    assert!(
        replayed.is_ok(),
        "unread source retries after custody repair: {replayed:?}"
    );
    let disk = f.disk();
    assert_eq!(disk.len(), 1, "unread source is durably replayed once");
    assert_eq!(
        (disk[0].message_id.get(), disk[0].text.as_str()),
        (8_916_060, "expanded prompt 8916060"),
        "consent_failed_persistence_retains_original_replay_payload_contract"
    );
}

#[test]
fn full_binding_validator_keeps_user_default_unreachable() {
    let mut config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {}, "cluster": {"instance_id": "test-node"},
        "agents": [{"id":"fixture", "name":"fixture", "channels":{"claude":{"id":"41","runtime":"tui"}}}],
        "session_hosts":{"herdr":{"endpoints":{"one":{"execution_node":"test-node",
            "socket_path":"/fixture/herdr.sock","herdr_home":"/fixture/herdr","herdr_session":"agentdesk"}},
            "channels":{"41":"one"}}}
    })).unwrap();
    assert!(
        crate::config::validate_config(&config).is_ok(),
        "consent_validator_complete_legacy_fixture_contract"
    );
    config
        .session_hosts
        .herdr
        .endpoints
        .get_mut("one")
        .unwrap()
        .mode = EndpointMode::UserDefault;
    let calls = crate::services::herdr_launch::admissions_on_this_thread();
    let result = crate::config::validate_config(&config);
    assert!(
        result.is_err(),
        "consent_validator_user_default_stays_unreachable_contract"
    );
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("session_hosts.herdr.endpoints.one.mode")
            && error.contains("지원 조각 미착지"),
        "consent_validator_rejects_exact_feature_guard_contract: {error}"
    );
    assert_eq!(
        crate::services::herdr_launch::admissions_on_this_thread(),
        calls,
        "consent_validator_admission_reached_zero_contract"
    );
}

type Preflight = Box<dyn FnMut(&IntakeSubmission) -> futures::future::BoxFuture<'static, ()>>;
type Local =
    Box<dyn FnMut(&IntakeSubmission) -> Result<crate::services::discord::IntakeExecution, Error>>;
thread_local! {
    static PREFLIGHT: std::cell::RefCell<Option<Preflight>> = const { std::cell::RefCell::new(None) };
    static FINAL: std::cell::RefCell<Option<Local>> = const { std::cell::RefCell::new(None) };
}
struct Hooks;
impl Drop for Hooks {
    fn drop(&mut self) {
        PREFLIGHT.with(|slot| *slot.borrow_mut() = None);
        FINAL.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(in crate::services::discord::router) async fn after_preflight(submission: &IntakeSubmission) {
    let future = PREFLIGHT.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map(|callback| callback(submission))
    });
    if let Some(future) = future {
        future.await;
    }
}
pub(in crate::services::discord::router) fn final_local(
    submission: &IntakeSubmission,
) -> Option<Result<crate::services::discord::IntakeExecution, Error>> {
    FINAL.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map(|callback| callback(submission))
    })
}
async fn seed_ready_row(pool: &sqlx::PgPool, shared: &SharedData) {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedExecution, HostedOwner, SourceRef,
    };
    let channel = discord_mock::CHANNEL_ID.to_string();
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: shared.token_hash.clone(),
        channel_id: channel.clone(),
        logical_key: "consent-fixture-session".into(),
        owner_node: "node".into(),
        runtime_root: "/fixture/runtime".into(),
    };
    let record = HostedExecution::pending(
        owner.clone(),
        "known-incarnation".into(),
        SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: channel.clone(),
            provider: "claude".into(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: "known-incarnation".into(),
            initial_source: None,
            baseline_event_seq: None,
        },
    );
    sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,$4)")
        .bind(&owner.logical_key).bind(&shared.token_hash).bind(channel).bind(serde_json::to_value(record).unwrap())
        .execute(pool).await.unwrap();
}
fn applied(consent: &Consent) -> Consent {
    let reserved = store().reserve_card(consent, now()).unwrap();
    let bound = store().bind_card(&reserved, 8_916_999).unwrap();
    let accepted = store().decide(&bound, bound.owner, true, now()).unwrap();
    store()
        .record_apply(
            &accepted,
            consent::ApplyResult {
                before: Some([1; 32]),
                after: Some([2; 32]),
                backup: None,
                detail: None,
            },
        )
        .unwrap()
}

// Only the final provider executor is fake: real receipt, sidecar, queue take, admission and finish run.
#[tokio::test(flavor = "current_thread")]
async fn actual_fifo_receipt_release_readmits_once_then_uses_one_start_and_followup_pg() {
    use consent::settlement::{CustodySink, SettlementKey};
    let mut f = Fixture::new("disabled").await;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    f.shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    seed_ready_row(&pool, &f.shared).await;
    for id in [8_916_070, 8_916_071] {
        let mut input = submission(id, IntakeOrigin::SlashSkill);
        input
            .preloaded_uploads
            .push(format!("original-{id}").into());
        assert!(hold_live(&f.deps(), input, Ok(f.hold())).await.is_ok());
    }
    let before = f.disk();
    let receipt = applied(&store().load_all().unwrap().remove(0));
    let mut sink =
        custody::LiveSink::new(f.shared.clone(), ProviderKind::Claude, &receipt.key).unwrap();
    let intents = receipt.releasable_intents();
    assert_eq!(intents.len(), 2);
    for intent in &intents {
        let key = SettlementKey {
            nonce: receipt.nonce.clone(),
            store: intent.store.clone(),
            id: intent.id.clone(),
        };
        assert!(
            sink.release(&key, intent).await.is_ok(),
            "actual_fifo_receipt_before_release_contract"
        );
        assert!(
            sink.release(&key, intent).await.is_ok(),
            "actual_fifo_release_replay_contract"
        );
    }
    let runs = Arc::new(std::sync::Mutex::new(Vec::<(u64, &'static str)>::new()));
    let observed = runs.clone();
    FINAL.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |input| {
            let mut runs = observed.lock().unwrap();
            let effect = if runs.is_empty() { "start" } else { "followup" };
            assert_eq!(
                input.preloaded_uploads,
                vec![
                    crate::services::cluster::attachment_transfer::uploads::Upload::from(format!(
                        "original-{}",
                        input.request.user_msg_id.get()
                    ))
                ],
                "actual_fifo_final_upload_preserved_contract"
            );
            runs.push((input.request.user_msg_id.get(), effect));
            Ok(crate::services::discord::IntakeExecution::Executed)
        }))
    });
    let _hooks = Hooks;
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    for expected in &before {
        let taken = f
            .shared
            .mailbox(channel)
            .take_next_soft(QueuePersistenceContext::new(
                &ProviderKind::Claude,
                &f.shared.token_hash,
                None,
            ))
            .await;
        assert!(
            taken.intervention.is_some() && taken.dispatch_lease.is_some(),
            "actual_fifo_lease_preserved_contract"
        );
        let item = taken.intervention.as_ref().unwrap();
        assert_eq!(
            item.message_id, expected.message_id,
            "actual_fifo_take_order_contract"
        );
        assert_eq!(
            item.source_message_queued_generations()
                .into_iter()
                .map(|source| (
                    source.message_id,
                    source.queued_generation,
                    source.enqueued_at_epoch_us,
                    source.preserve_on_cancel,
                    source.route,
                    source.origin,
                ))
                .collect::<Vec<_>>(),
            expected
                .source_message_queued_generations()
                .into_iter()
                .map(|source| (
                    source.message_id,
                    source.queued_generation,
                    source.enqueued_at_epoch_us,
                    source.preserve_on_cancel,
                    source.route,
                    source.origin,
                ))
                .collect::<Vec<_>>(),
            "actual_fifo_origin_preserved_before_readmission_contract"
        );
        let admitted = intake::admit_queued_intake(
            &f.deps(),
            ProviderKind::Claude,
            channel,
            item,
            item.author_id,
            "owner".into(),
            false,
            false,
            "actual-consent-fifo",
            taken.dispatch_lease.clone(),
        )
        .await;
        assert!(
            matches!(admitted, QueuedAdmissionDisposition::Admitted(_)),
            "actual_fifo_released_head_readmitted_contract"
        );
        let QueuedAdmissionDisposition::Admitted(admitted) = admitted else {
            unreachable!()
        };
        let outcome = intake::finish_admitted_queued_intake(&f.deps(), admitted).await;
        assert!(
            matches!(
                outcome,
                Ok(crate::services::discord::IntakeExecution::Executed)
            ),
            "actual_fifo_readmitted_final_executor_contract: {outcome:?}"
        );
        let settled = crate::services::discord::mailbox_abandon_pending_dispatch(
            &f.shared,
            &ProviderKind::Claude,
            channel,
            item.message_id,
        )
        .await;
        assert!(settled, "actual_fifo_executor_settlement_durable_contract");
    }
    assert_eq!(
        *runs.lock().unwrap(),
        vec![(8_916_070, "start"), (8_916_071, "followup")],
        "actual_fifo_one_start_then_one_followup_contract"
    );
    for intent in &intents {
        let key = SettlementKey {
            nonce: "later-generation".into(),
            store: intent.store.clone(),
            id: intent.id.clone(),
        };
        assert!(
            sink.release(&key, intent).await.is_ok(),
            "actual_fifo_later_nonce_release_idempotent_contract"
        );
        assert!(
            sink.wake(&key, intent).await.is_ok(),
            "actual_fifo_post_execution_wake_idempotent_contract"
        );
    }
    assert!(
        f.disk().is_empty(),
        "actual_fifo_replay_never_recreates_queue_payload_contract"
    );
    assert_eq!(
        runs.lock().unwrap().len(),
        2,
        "actual_fifo_no_replay_duplicate_executor_contract"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_live_checkpoint_waits_for_second_check_failed_custody_pg() {
    let mut f = Fixture::new("disabled").await;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    f.shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    seed_ready_row(&pool, &f.shared).await;
    let input = submission(8_916_080, IntakeOrigin::LiveMessage);
    assert!(
        matches!(
            intake::admit_text_intake(&f.deps(), &input).await,
            IntakeAdmission::Local(_)
        ),
        "checkpoint_race_first_observation_local_contract"
    );
    let hold = f.hold();
    let token = f.shared.token_hash.clone();
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let changed_hook = changed.clone();
    PREFLIGHT.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |input| {
            let hold = hold.clone();
            let token = token.clone();
            let changed = changed_hook.clone();
            let source = input.request.user_msg_id.get();
            Box::pin(async move {
                let intent = IntentRef {
                    store: "mailbox".into(),
                    id: format!("{source}:0"),
                    provider: "claude".into(),
                    channel: discord_mock::CHANNEL_ID,
                    source_message: source,
                    fifo: 0,
                };
                hold.record(vec![intent]).await.unwrap();
                let directory =
                    crate::services::discord::runtime_store::discord_pending_queue_root()
                        .unwrap()
                        .join("claude");
                std::fs::create_dir_all(&directory).unwrap();
                std::fs::write(directory.join(token), b"block persistence").unwrap();
                changed.store(true, Ordering::SeqCst);
            })
        }))
    });
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let effects = calls.clone();
    FINAL.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_| {
            effects.fetch_add(1, Ordering::SeqCst);
            Ok(crate::services::discord::IntakeExecution::Executed)
        }))
    });
    let _hooks = Hooks;
    let checkpoints = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let saved = checkpoints.clone();
    let result = intake::dispatch_live_text_intake(&f.deps(), input, move || {
        saved.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    assert!(
        changed.load(Ordering::SeqCst),
        "checkpoint_race_second_policy_published_contract"
    );
    assert!(
        result.is_err(),
        "checkpoint_race_failed_custody_is_error_contract"
    );
    assert_eq!(
        checkpoints.load(Ordering::SeqCst),
        0,
        "checkpoint_race_unpersisted_source_never_acknowledged_contract"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "checkpoint_race_failed_custody_mutation_zero_contract"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_worker_policy_flip_after_preaccept_keeps_exact_before_effects_row_pg() {
    use crate::services::cluster::intake_worker::{self as worker, test_executor};
    let mut f = Fixture::new("disabled").await;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    f.shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    Arc::get_mut(&mut f.shared).unwrap().health_registry = Arc::downgrade(&registry);
    f.shared
        .http
        .cached_bot_token
        .set("isolated-worker-token".into())
        .unwrap();
    registry.register("claude".into(), f.shared.clone()).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    seed_ready_row(&pool, &f.shared).await;
    let row = crate::db::intake_outbox::owner_fence_pg_tests::seed(
        &pool,
        &discord_mock::CHANNEL_ID.to_string(),
        "pending",
        true,
    )
    .await;
    let original = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, row).await;
    let hold = f.hold();
    let observed_shared = f.shared.clone();
    let observed_http = f.http.clone();
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_hook = seen.clone();
    let preaccept_row = Arc::new(std::sync::Mutex::new(None));
    let observed_row = preaccept_row.clone();
    let observed_pool = pool.clone();
    let _checkpoint = test_executor::hook(Box::new(move |point| {
        let hold = hold.clone();
        let seen = seen_hook.clone();
        let shared = observed_shared.clone();
        let http = observed_http.clone();
        let preaccept_row = observed_row.clone();
        let pool = observed_pool.clone();
        Box::pin(async move {
            if point == test_executor::Checkpoint::PreAccept {
                let snapshot =
                    crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, row).await;
                *preaccept_row.lock().unwrap() = Some(snapshot);
                let deps = IntakeDeps {
                    http: &http,
                    shared: &shared,
                    token: "isolated-worker-token",
                    cache: None,
                    ctx_for_chained_dispatch: None,
                };
                assert!(
                    check(
                        &deps,
                        &ProviderKind::Claude,
                        &request(discord_mock::CHANNEL_ID)
                    )
                    .await
                    .is_none(),
                    "worker_policy_flip_preaccept_ready_observation_contract"
                );
                hold.record(vec![IntentRef {
                    store: "intake_outbox".into(),
                    id: row.to_string(),
                    provider: "claude".into(),
                    channel: discord_mock::CHANNEL_ID,
                    source_message: discord_mock::CHANNEL_ID,
                    fifo: 1,
                }])
                .await
                .unwrap();
                seen.store(true, Ordering::SeqCst);
            }
        })
    }));
    let result = worker::run_intake_worker_tick(
        &pool,
        &f.shared,
        "fence-worker",
        "claude",
        "token",
        &|| false,
    )
    .await;
    assert!(
        seen.load(Ordering::SeqCst),
        "worker_policy_flip_reached_real_preaccept_contract"
    );
    assert!(
        matches!(result, Ok(worker::TickOutcome::Held)),
        "worker_policy_flip_real_entry_keeps_custody_contract: {result:?}"
    );
    let after = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, row).await;
    assert_eq!(
        after["status"], "spawned",
        "worker_policy_flip_never_general_retry_contract"
    );
    assert!(
        after["last_error"]
            .as_str()
            .is_some_and(|value| value.starts_with("herdr-consent-custody:v1:")),
        "worker_policy_flip_durable_exact_marker_contract"
    );
    for field in [
        "user_text",
        "attachment_refs",
        "attempt_no",
        "parent_outbox_id",
    ] {
        assert_eq!(
            after[field], original[field],
            "worker_policy_flip_original_payload_identity_contract: {field}"
        );
    }
    let captured = preaccept_row.lock().unwrap().take();
    assert!(
        captured.is_some(),
        "worker_policy_flip_preaccept_identity_captured_contract"
    );
    let captured = captured.unwrap();
    for field in ["claim_owner", "owner_instance_id", "owner_generation"] {
        assert_eq!(
            after[field], captured[field],
            "worker_policy_flip_preserves_preaccept_claim_contract: {field}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE channel_id=$1")
        .bind(discord_mock::CHANNEL_ID.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "worker_policy_flip_never_mints_replacement_attempt_contract"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "worker_policy_flip_no_provider_or_card_effect_contract"
    );
    pool.close().await;
    db.drop().await;
}
