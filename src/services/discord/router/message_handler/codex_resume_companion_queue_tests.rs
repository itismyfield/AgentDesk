//! Confirmed Codex queues follow the live binding log, source proof and native turn boundary.

use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
};
use crate::services::claude_tui::hook_server::adoption_retry::DurableKind;
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, observe_binding_hook,
};
use crate::services::discord;
use crate::services::discord::tmux::watcher_install_fixture as install;
use crate::services::discord::turn_presence::activity::{Activity, tests as presence};
use crate::services::tmux_common as tc;
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
    PreparedIncarnation,
};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as p5};
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, SourceMessageQueuedGeneration,
};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(in crate::services::discord) const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const PROMPT: &str = "preserve the queued instruction\n한글";

pub(in crate::services::discord) struct Fixture {
    pub install: discord::tmux::InstallFixture,
    pub native: PathBuf,
    pub context: BindingContext,
    pub pending: [u64; 2],
    pub root: PathBuf,
    _discord: discord::health::test_support::MockDiscord,
    _confirmation: TestConfirmation,
    prior_binding_root: Option<PathBuf>,
    _ready: crate::config::TestEnvVarGuard,
    _database: crate::db::auto_queue::test_support::TestPostgresDb,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _live: crate::config_live_reload::TestLiveGuard,
}

impl Fixture {
    pub async fn new(channel: u64, closed: bool) -> Self {
        assert!(
            std::env::var_os("ADK_WATCHER_FIXTURE_CHILD").is_some(),
            "shared dedupe state requires this fixture's single-test child process"
        );
        let environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(environment);
        let hosts = crate::config::session_hosts::force_for_test(None, &[]);
        let mut config: crate::config::Config = serde_yaml::from_str(&format!(
            "server: {{}}\nagents:\n  - id: resume-fixture\n    name: resume-fixture\n    provider: codex\n    channels:\n      codex: {{id: '{channel}', name: wfx-{channel}-cdx, runtime: tui}}\n"
        )).unwrap();
        config.session_hosts =
            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
        crate::config_live_reload::install(config);
        dedupe::reset_state_for_tests();
        let database = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let (install, discord) =
            install::fixture_with_pool(ProviderKind::Codex, channel, Some(pool)).await;
        seed_legacy_session(&install).await;
        std::fs::write(
            install::root().join("tmux-sessions"),
            format!("{}\n", install.session),
        )
        .unwrap();
        let ready = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_TEST_CODEX_PROMOTE_READY",
            std::ffi::OsStr::new("1"),
        );
        let (_, channel_name) =
            crate::services::provider::parse_provider_and_channel_from_tmux_name(&install.session)
                .unwrap();
        install.shared.core.lock().await.sessions.insert(
            install.channel,
            DiscordSession {
                session_id: Some(ID.into()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(channel),
                channel_name: Some(channel_name),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
            },
        );
        dedupe::reset_state_for_tests();
        let root = install::root();
        let prior_binding_root = p5::test_root();
        p5::set_test_root(Some(&root));
        let sessions = root.join("codex/sessions").canonicalize().unwrap();
        let native = sessions.join(format!("rollout-{ID}.jsonl"));
        let prepared = PreparedIncarnation::prepare_pinned(
            "codex",
            &install.session,
            Some(channel),
            Some(ID),
            true,
            Some(sessions),
            (
                Some(format!("sha256:{:x}", Sha256::digest(PROMPT.as_bytes()))),
                Some("verified".into()),
            ),
        )
        .unwrap();
        let context = prepared.context;
        let at = context.created_at - chrono::Duration::days(4);
        let metadata = json!({"type":"session_meta", "timestamp":at.to_rfc3339(),
            "payload":{"id":ID,"timestamp":at.to_rfc3339(),"cwd":root,
                "source":"cli","originator":"codex-tui"}});
        let mut rows = vec![
            metadata,
            json!({"type":"event_msg","timestamp":at.to_rfc3339(),
            "payload":{"type":"task_started","turn_id":"resume-old-turn"}}),
            json!({"type":"response_item","timestamp":at.to_rfc3339(),
                "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old settled prompt"}]}}),
        ];
        if closed {
            rows.push(json!({"type":"event_msg","timestamp":at.to_rfc3339(),
                "payload":{"type":"task_complete","turn_id":"resume-old-turn"}}));
        }
        std::fs::write(
            &native,
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        discord::health::test_support::age_file(&native, 4 * 24 * 60 * 60);
        assert!(
            std::fs::metadata(&native)
                .unwrap()
                .modified()
                .unwrap()
                .elapsed()
                .unwrap()
                >= Duration::from_secs(4 * 24 * 60 * 60)
        );
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for(ID, &native).unwrap();
        let mut previous = context.clone();
        previous.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        PreparedIncarnation::create(previous.clone()).unwrap();
        let old_claim = p5::codex::Claim {
            session_id: ID.into(),
            path: Some(native.clone()),
            evidence: p5::codex::ClaimEvidence::ExplicitResume {
                executed_session_id: ID.into(),
            },
        };
        let old = p5::codex::commit_claim(
            &previous,
            &old_claim,
            p5::codex::Decision::Verified(source),
            || Ok(()),
        )
        .unwrap();
        assert!(
            old.verified.is_some(),
            "previous incarnation fixture must hold its own source"
        );
        std::fs::write(
            tc::session_temp_path(&install.session, "spawn_nonce"),
            &context.execution_nonce,
        )
        .unwrap();
        dedupe::register_tmux_channel(&install.session, channel);
        dedupe::set_codex_delivery_permission_for_tests(
            &context,
            dedupe::CodexDeliveryPermissionForTests::Allowed,
        );
        let mut fixture = Self {
            install,
            native,
            context,
            pending: [0, 0],
            root,
            _discord: discord,
            _confirmation: TestConfirmation::new(channel),
            prior_binding_root,
            _ready: ready,
            _database: database,
            _hosts: hosts,
            _live: live,
        };
        assert!(
            fixture.fold().verified.is_none(),
            "old nonce is no current ownership proof"
        );
        assert_eq!(
            fixture.hook("session-start", Some("resume")),
            IngressOutcome::Durable(DurableKind::Pending)
        );
        assert_eq!(
            fixture.hook("user-prompt-submit", None),
            IngressOutcome::Durable(DurableKind::Pending)
        );
        let fold = fixture.fold();
        assert!(fold.verified.is_none());
        assert_eq!(fold.pending.len(), 2);
        fixture.pending = [fold.pending[0].seq, fold.pending[1].seq];
        assert!(matches!(
            &fold.pending[1].claim.evidence,
            p5::codex::ClaimEvidence::NativeHook { source: None, .. }
        ));
        assert!(
            p5::binding_events_since(channel, 0)
                .unwrap()
                .iter()
                .all(|event| event.execution_nonce.as_deref()
                    != Some(&fixture.context.execution_nonce)
                    || matches!(event.new, p5::BindingTarget::Pending { .. }))
        );
        fixture.enqueue().await;
        fixture
    }

    pub fn channel(&self) -> ChannelId {
        self.install.channel
    }
    pub fn shared(&self) -> &Arc<SharedData> {
        &self.install.shared
    }
    pub fn fold(&self) -> p5::codex::Fold {
        p5::codex::read_ownership(&self.context).unwrap()
    }
    pub fn marker(&self) -> PathBuf {
        PathBuf::from(tc::session_temp_path(
            &self.install.session,
            tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
    }
    fn hook(&self, event: &str, source: Option<&str>) -> IngressOutcome {
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(self.context.clone().into()),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        let mut payload = json!({"session_id":ID,"transcript_path":self.native,"prompt":PROMPT});
        if let Some(source) = source {
            payload["source"] = json!(source);
        }
        observe_binding_hook("codex", event, Some(ID), Some(ID), &payload, &headers)
    }
    async fn enqueue(&self) {
        let id = MessageId::new(self.channel().get() + 1);
        let generation = discord::runtime_store::process_generation();
        let item = Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: id,
            queued_generation: generation,
            source_message_ids: vec![id],
            source_message_queued_generations: vec![
                SourceMessageQueuedGeneration::user_instruction(id, generation),
            ],
            source_text_segments: Vec::new(),
            text: PROMPT.into(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
        };
        let persistence =
            discord::queue_persistence_context(self.shared(), &ProviderKind::Codex, self.channel());
        self.shared()
            .mailbox(self.channel())
            .replace_queue(vec![item], persistence)
            .await;
        self.assert_queue_one().await;
    }
    pub async fn queue_identity(&self) -> Value {
        let snapshot = discord::mailbox_snapshot(self.shared(), self.channel()).await;
        queue_identity(&snapshot.intervention_queue)
    }
    pub async fn assert_queue_one(&self) {
        let memory = self.queue_identity().await;
        assert_eq!(
            memory.as_array().unwrap().len(),
            1,
            "one exact queued input must survive"
        );
        let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &ProviderKind::Codex,
            &self.shared().token_hash,
            self.channel(),
        );
        assert_eq!(
            queue_identity(&disk),
            memory,
            "durable queue must preserve all source identities"
        );
    }
    pub fn cold_memory(&self) {
        dedupe::reset_state_for_tests();
        p5::forget_channel_for_tests(self.channel().get());
        dedupe::set_codex_delivery_permission_for_tests(
            &self.context,
            dedupe::CodexDeliveryPermissionForTests::Allowed,
        );
        assert!(dedupe::peek_tmux_runtime_binding(&self.install.session).is_none());
        assert_eq!(
            dedupe::owner_channel_for_tmux_session(&self.install.session),
            None
        );
        assert!(
            self.shared()
                .tmux_watchers
                .channel_binding(&self.channel())
                .is_none()
        );
        assert!(
            discord::inflight::load_inflight_state_read_only(
                &ProviderKind::Codex,
                self.channel().get()
            )
            .is_none()
        );
    }
    pub fn recover(&self) {
        assert!(
            dedupe::recover_discovered_codex_binding(&self.install.session, self.channel().get()),
            "actual resolver must adopt"
        );
        self.assert_settled();
    }
    pub fn assert_settled(&self) {
        let fold = self.fold();
        assert!(fold.pending.is_empty() && !fold.conflicted);
        let proof = fold
            .verified
            .expect("the executed resume must make a durable proof");
        assert!(matches!(
            proof.ownership.claim.evidence,
            p5::codex::ClaimEvidence::ExplicitResume { .. }
        ));
        assert_eq!(
            proof.source,
            crate::services::tui_o::shadow::binding_reader::source_id_for(ID, &self.native)
                .unwrap()
        );
        assert_eq!(proof.ownership.pending_seq, Some(self.pending[0]));
        let events = p5::binding_events_since(self.channel().get(), 0).unwrap();
        let current: Vec<_> = events
            .iter()
            .filter(|event| event.execution_nonce.as_deref() == Some(&self.context.execution_nonce))
            .collect();
        assert_eq!(current.len(), 4);
        assert!(
            matches!(&current[2].new, p5::BindingTarget::Rejected { reason, .. } if reason == "superseded_by_executed_resume")
        );
        assert!(
            matches!(&current[3].new, p5::BindingTarget::Resolved { pending_seq, source } if *pending_seq == self.pending[0] && *source == proof.source)
        );
        let projected =
            crate::services::tui_o::writer::binding::BindingEvents::binding_events_since(
                &crate::services::tui_o::writer::binding::ChannelBindingLog::new(
                    self.channel().get(),
                    crate::services::tui_o::shadow::ShadowProvider::Codex,
                ),
                self.channel().get(),
                0,
            )
            .unwrap();
        assert!(projected.iter().any(|event| matches!(&event.record,
            crate::services::tui_o::writer::binding::BindingRecord::Rejected { closes: Some(seq), .. } if *seq == self.pending[1])));
        let marker: Value = serde_json::from_slice(&std::fs::read(self.marker()).unwrap()).unwrap();
        assert_eq!(marker["codex_ownership"]["proof_seq"], proof.seq);
        assert_eq!(
            marker["codex_ownership"]["execution_nonce"],
            self.context.execution_nonce
        );
        let binding = dedupe::runtime_binding_for_tmux_session(&self.install.session)
            .expect("publication must restore the runtime binding");
        assert_eq!(binding.session_id.as_deref(), Some(ID));
        assert_eq!(
            Path::new(&binding.output_path).canonicalize().unwrap(),
            self.native
        );
    }
    pub fn attach(&self) {
        presence::companion_attach_log_watcher(
            self.shared(),
            self.channel().get(),
            &self.install.session,
            &self.native,
        );
    }
    pub async fn assert_activity(&self, activity: Activity, reason: &str) {
        let observed =
            presence::companion_settled_activity(self.shared(), self.channel().get()).await;
        assert_eq!((observed.activity, observed.reason), (activity, reason));
    }
    pub async fn assert_automatic_held(&self) {
        assert!(crate::services::tui_o::turn_mode::transcript_turns(
            self.channel().get()
        ));
        let before = self.queue_identity().await;
        let result = discord::idle_queue_take_next_soft_if_ready(
            self.shared(),
            &ProviderKind::Codex,
            self.channel(),
        )
        .await;
        assert!(result.intervention.is_none() && result.dispatch_lease.is_none());
        assert!(result.persistence_error.is_none());
        assert_eq!(self.queue_identity().await, before);
        self.assert_queue_one().await;
    }
    pub async fn admit_once(&self) {
        self.assert_activity(Activity::Idle, "closed").await;
        let before = self.queue_identity().await;
        let result = discord::idle_queue_take_next_soft_if_ready(
            self.shared(),
            &ProviderKind::Codex,
            self.channel(),
        )
        .await;
        assert!(result.persistence_error.is_none());
        assert!(
            result.dispatch_lease.is_some(),
            "real automatic take must reserve dispatch"
        );
        let item = result
            .intervention
            .expect("closed confirmed turn must admit its original head");
        assert_eq!(queue_identity(std::slice::from_ref(&item)), before);
        assert!(
            discord::mailbox_try_start_turn(
                self.shared(),
                self.channel(),
                Arc::new(CancelToken::new()),
                item.author_id,
                item.message_id
            )
            .await
        );
        let after = discord::mailbox_snapshot(self.shared(), self.channel()).await;
        assert_eq!(after.active_user_message_id, Some(item.message_id));
        assert!(after.intervention_queue.is_empty());
        let again = discord::idle_queue_take_next_soft_if_ready(
            self.shared(),
            &ProviderKind::Codex,
            self.channel(),
        )
        .await;
        assert!(
            again.intervention.is_none(),
            "the original head must be handed off exactly once"
        );
    }
}

async fn seed_legacy_session(install: &discord::tmux::InstallFixture) {
    let pool = install.shared.pg_pool.as_ref().unwrap();
    let channel = install.channel.to_string();
    let key = discord::adk_session::build_namespaced_session_key(
        &install.shared.token_hash,
        &ProviderKind::Codex,
        &install.session,
    );
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &install.shared.token_hash,
        channel_id: &channel,
    };
    upsert_hook_session_with_identity_pg(
        pool,
        HookSessionUpsert {
            session_key: &key,
            instance_id: Some("resume-fixture"),
            agent_id: None,
            provider: "codex",
            status: "idle",
            session_info: None,
            model: None,
            tokens: None,
            cwd: None,
            active_dispatch_id: None,
            thread_channel_id: None,
            channel_id: Some(&channel),
            claude_session_id: None,
            raw_provider_session_id: None,
            turn_start_nonce: None,
            dispatched_origin: false,
        },
        Some(identity),
    )
    .await
    .unwrap();
    let exact = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await;
    let canonical = load_hosted_execution_pg(
        pool,
        HostedLookupKey::Canonical {
            provider: "codex",
            identity,
        },
    )
    .await;
    assert_eq!(
        exact, canonical,
        "both real identity lookups must find the fixture row"
    );
    assert!(matches!(exact, HostedLookup::Found(found) if found.record == HostedRecord::Legacy));
}

impl Drop for Fixture {
    fn drop(&mut self) {
        p5::forget_channel_for_tests(self.channel().get());
        p5::set_test_root(self.prior_binding_root.as_deref());
        dedupe::reset_state_for_tests();
    }
}

fn queue_identity(items: &[Intervention]) -> Value {
    json!(items.iter().map(|item| json!({"id":item.message_id.get(),"body":item.text,
        "author":item.author_id.get(),"generation":item.queued_generation,
        "sources":item.source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
        "source_generations":item.source_message_queued_generations.iter().map(|source| json!({
            "id":source.message_id.get(),"generation":source.queued_generation,
            "enqueued":source.enqueued_at_epoch_us,"preserve":source.preserve_on_cancel,"route":source.route})).collect::<Vec<_>>()
    })).collect::<Vec<_>>())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn resume_companion_settlement_preserves_queue_then_admits_once() {
    if !install::isolated(
        module_path!(),
        "resume_companion_settlement_preserves_queue_then_admits_once",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_701, true).await;
        let before = fixture.queue_identity().await;
        fixture
            .assert_activity(Activity::Unknown, "watcher_unbound")
            .await;
        fixture.assert_automatic_held().await;
        fixture.attach();
        fixture
            .assert_activity(Activity::Unknown, "binding_pending")
            .await;
        fixture.assert_automatic_held().await;
        assert!(
            hosted_tui_promote_readiness_blocked(
                fixture.shared(),
                &ProviderKind::Codex,
                fixture.channel()
            )
            .await
        );
        fixture.recover();
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
        fixture.admit_once().await;
    });
}

#[test]
fn resume_companion_partial_settlement_failure_preserves_queue_and_retries() {
    if !install::isolated(
        module_path!(),
        "resume_companion_partial_settlement_failure_preserves_queue_and_retries",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_761, true).await;
        fixture.attach();
        fixture
            .assert_activity(Activity::Unknown, "binding_pending")
            .await;
        let before = fixture.queue_identity().await;
        struct ResetFault;
        impl Drop for ResetFault {
            fn drop(&mut self) {
                p5::APPEND_FAULT.with(|fault| fault.set(None));
            }
        }
        let _fault = ResetFault;
        p5::codex::after_companion_for_tests(|| {
            p5::APPEND_FAULT.with(|fault| fault.set(Some("sync")))
        });
        let result = dedupe::codex_verified::record_executed_resume(
            &fixture.context,
            ID,
            &fixture.native,
            0,
        );
        assert!(
            result.is_err(),
            "anchor append must actually fail after durable UPS rejection"
        );
        p5::APPEND_FAULT.with(|fault| fault.set(None));
        let partial = fixture.fold();
        assert_eq!(partial.pending.len(), 1);
        assert_eq!(partial.pending[0].seq, fixture.pending[0]);
        assert!(partial.verified.is_none());
        assert!(dedupe::peek_tmux_runtime_binding(&fixture.install.session).is_none());
        assert!(!fixture.marker().exists());
        let events = p5::binding_events_since(fixture.channel().get(), 0).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.execution_nonce.as_deref()
                    == Some(&fixture.context.execution_nonce)
                    && matches!(event.new, p5::BindingTarget::Rejected { .. }))
                .count(),
            1
        );
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
        fixture
            .assert_activity(Activity::Unknown, "binding_pending")
            .await;
        fixture.assert_automatic_held().await;
        dedupe::reset_state_for_tests();
        p5::forget_channel_for_tests(fixture.channel().get());
        dedupe::set_codex_delivery_permission_for_tests(
            &fixture.context,
            dedupe::CodexDeliveryPermissionForTests::Allowed,
        );
        fixture.recover();
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
        fixture.admit_once().await;
    });
}

#[test]
fn codex_resume_companion_unregistered_discovery_pass_publishes() {
    if !install::isolated(
        module_path!(),
        "codex_resume_companion_unregistered_discovery_pass_publishes",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_711, true).await;
        let before = fixture.queue_identity().await;
        fixture.cold_memory();
        assert_eq!(fixture.fold().pending.len(), 2);
        crate::services::discord::tui_prompt_relay::rehydration::run_codex_rehydrate_pass_for_tests(
            fixture.shared(),
            &fixture.install.session,
        );
        fixture.assert_settled();
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
        assert!(
            fixture
                .shared()
                .tmux_watchers
                .channel_binding(&fixture.channel())
                .is_none()
        );
        fixture
            .assert_activity(Activity::Unknown, "watcher_unbound")
            .await;
        fixture.assert_automatic_held().await;
    });
}

#[test]
fn codex_companion_settled_open_turn_stays_held_after_six_hundred_seconds() {
    if !install::isolated(
        module_path!(),
        "codex_companion_settled_open_turn_stays_held_after_six_hundred_seconds",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_721, false).await;
        // Keep the four-day-old native metadata, but start this observation recently for the age transition.
        discord::health::test_support::age_file(&fixture.native, 0);
        fixture.recover();
        fixture.attach();
        fixture.assert_activity(Activity::Busy, "open").await;
        let bytes = std::fs::read(&fixture.native).unwrap();
        let modified = std::fs::metadata(&fixture.native)
            .unwrap()
            .modified()
            .unwrap();
        let before = fixture.queue_identity().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(600)).await;
        assert_eq!(
            std::fs::read(&fixture.native).unwrap(),
            bytes,
            "no native progress may be manufactured"
        );
        assert_eq!(
            std::fs::metadata(&fixture.native)
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        // The production watch uses std::Instant; mirror the paused Tokio clock only into its age.
        presence::companion_age_open_watch(fixture.channel().get(), Duration::from_secs(600));
        fixture
            .assert_activity(Activity::Unknown, "open_without_progress")
            .await;
        fixture.assert_automatic_held().await;
        assert_eq!(fixture.queue_identity().await, before);
        tokio::time::resume();
    });
}

#[test]
fn resume_companion_backstop_rechecks_after_source_publication() {
    if !install::isolated(
        module_path!(),
        "resume_companion_backstop_rechecks_after_source_publication",
    ) {
        return;
    }
    runtime().block_on(async {
        let fixture = Fixture::new(7_061_731, true).await;
        fixture.recover();
        fixture
            .assert_activity(Activity::Unknown, "watcher_unbound")
            .await;
        let before = fixture.queue_identity().await;
        let channel = fixture.channel();
        let (completed, mut completions) = tokio::sync::mpsc::unbounded_channel();
        // Replace transport/provider effects only; the real readiness, automatic take and claim run.
        let _transport = discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
            move |shared, provider, target, _reason| {
                let completed = completed.clone();
                Box::pin(async move {
                    if target != channel {
                        return None;
                    }
                    let mut started = false;
                    let mut handed_off = None;
                    if !hosted_tui_promote_readiness_blocked(&shared, &provider, target).await {
                        let taken =
                            discord::idle_queue_take_next_soft_if_ready(&shared, &provider, target)
                                .await;
                        assert!(taken.persistence_error.is_none());
                        if let Some(item) = taken.intervention {
                            assert!(taken.dispatch_lease.is_some());
                            started = discord::mailbox_try_start_turn(
                                &shared,
                                target,
                                Arc::new(CancelToken::new()),
                                item.author_id,
                                item.message_id,
                            )
                            .await;
                            assert!(started);
                            handed_off = Some(item.message_id.get());
                        }
                    }
                    completed.send((started, handed_off)).unwrap();
                    Some(discord::IdleQueueKickoffChannelOutcome { started })
                })
            },
        ));
        assert!(
            discord::queue_io::arm_slow_idle_queue_backstop_if_queue_nonempty(
                fixture.shared(),
                &ProviderKind::Codex,
                channel,
                "resume_companion_backstop"
            )
            .await
        );
        let waiting = async {
            while !discord::queue_io::cancel_backstop_test_support::backstop_waiting(channel) {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("actual backstop never armed");
        tokio::time::pause();
        tokio::time::advance(discord::queue_io::idle_queue_backstop_delay_for_tests()).await;
        tokio::time::resume();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), completions.recv())
                .await
                .unwrap(),
            Some((false, None))
        );
        let rearmed = async {
            while discord::queue_io::cancel_backstop_test_support::completed_fires(channel) != 1
                || !discord::queue_io::cancel_backstop_test_support::backstop_waiting(channel)
            {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), rearmed)
            .await
            .expect("first actual backstop cycle did not rearm");
        assert_eq!(fixture.queue_identity().await, before);
        fixture.assert_queue_one().await;
        fixture.attach();
        fixture.assert_activity(Activity::Idle, "closed").await;
        tokio::time::pause();
        tokio::time::advance(discord::queue_io::idle_queue_backstop_delay_for_tests()).await;
        tokio::time::resume();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), completions.recv())
                .await
                .unwrap(),
            Some((true, Some(channel.get() + 1)))
        );
        let finished = async {
            while discord::queue_io::cancel_backstop_test_support::completed_fires(channel) != 2 {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .expect("second actual backstop cycle did not finish");
        let after = discord::mailbox_snapshot(fixture.shared(), channel).await;
        assert!(after.intervention_queue.is_empty());
        assert_eq!(
            after.active_user_message_id,
            Some(MessageId::new(channel.get() + 1))
        );
        assert!(
            completions.try_recv().is_err(),
            "one exact original input handoff"
        );
    });
}

mod herdr_owner_resume {
    //! An evidenced Pending launch can lose its pane before its owner ever reserves the input.

    use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
    use crate::services::claude_tui::hook_server::observation_ingress::tests::uuid;
    use crate::services::discord;
    use crate::services::discord::router::message_handler::provider_dispatch::{
        self as dispatch, herdr_tests,
    };
    use crate::services::discord::shared_state::test_rest;
    use crate::services::provider::ProviderKind;
    use crate::services::session_host::herdr_socket_rig_tests::{KEY, NODE, PANE, SESSION};
    use crate::services::tui_o::cutover::{self, test_override};
    use crate::services::tui_prompt_dedupe::binding_context::HookBindingEnvelope;
    use crate::services::turn_orchestrator::{ChannelMailboxHandle, OwnerFront};
    use herdr_tests::{
        CHANNEL, Fixture, LIMIT, Launcher, Started,
        busy_entry_tests::{
            ExpectedPin, INPUT, Intake, missing_input::recording_runtime, source_retry_drive,
            wait_for_idle_turn_to_unwind,
        },
        context_of, open_admission, reconnect,
    };
    use poise::serenity_prelude::ChannelId;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex, atomic::Ordering};
    use std::time::Duration;

    use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
    use crate::services::discord::queue_io::input_owner::drive;
    use crate::services::herdr_launch::unplaced::QUIET;
    use std::sync::mpsc;

    struct WakeGuard;

    impl Drop for WakeGuard {
        fn drop(&mut self) {
            set_owner_wake_for_tests(ChannelId::new(CHANNEL), None);
        }
    }

    struct KeyReply {
        sent: mpsc::Sender<()>,
        reply: mpsc::Receiver<()>,
    }

    #[derive(Clone, Copy)]
    enum PaneLoss {
        BeforeAttach,
        BeforeKey(PreKeyRecovery),
        AfterKey,
    }

    #[derive(Clone, Copy)]
    enum PreKeyRecovery {
        Hold,
        Requeue,
        Restart,
    }

    fn start(
        fx: &Fixture,
        intake: &Intake,
        front: OwnerFront,
        launch_signal: Option<mpsc::Sender<Result<(), String>>>,
    ) -> std::thread::JoinHandle<Result<(), String>> {
        let runtime = intake.runtime.handle().clone();
        let shared = intake.shared.clone();
        let install = fx.rig.thread_installer_with_launch(NODE, KEY, SESSION);
        let config = crate::config_live_reload::current().unwrap();
        let boot = cutover::boot_snapshot().unwrap();
        let root = crate::services::tui_prompt_dedupe::binding_events::test_root();
        let http = intake._api.http.clone();
        std::thread::spawn(move || {
            install();
            QUIET.with(|quiet| quiet.set(Some(Duration::ZERO)));
            crate::services::tui_prompt_dedupe::binding_events::set_test_root(root.as_deref());
            let _boot = test_override::force_boot(boot);
            let _config = crate::config::session_hosts::force_config_for_test(&config);
            let _admission = open_admission();
            let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
            let result = runtime.block_on(async {
                let (entered, resume) =
                    discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                        .await
                        .pause();
                let start = dispatch::start_idle_for_test(shared, ChannelId::new(CHANNEL), front);
                let recording = async {
                    tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
                    let guard = test_rest::install(http);
                    resume.send(()).unwrap();
                    guard
                };
                let (result, _rest) = tokio::join!(tokio::time::timeout(LIMIT, start), recording);
                result.unwrap()
            });
            if let (Some(send), Err(error)) = (launch_signal, &result) {
                let _ = send.send(Err(error.clone()));
            }
            result
        })
    }

    fn launch(
        fx: &Fixture,
        intake: &Intake,
        front: OwnerFront,
        prior: (&str, &str),
        lose_before_attach: bool,
        mut key_reply: Option<KeyReply>,
    ) -> (std::thread::JoinHandle<Result<(), String>>, String, Started) {
        let (command, command_seen) = mpsc::channel();
        let (allow_evidence, allowed) = mpsc::channel();
        if key_reply.is_some() {
            fx.rig.answer_sends_then_leave_unanswered(2);
        }
        let launch_signal = command.clone();
        let mut command = Some(command);
        fx.rig.on_send(move |params| {
            if let Some(command) = command.take() {
                command.send(Ok(())).unwrap();
                crate::test_support::wait::recv_within(&allowed, LIMIT);
            } else if let Some(key) = key_reply.take_if(|_| params["keys"] == json!(["enter"])) {
                key.sent.send(()).unwrap();
                crate::test_support::wait::recv_within(&key.reply, LIMIT);
            }
        });
        let mut events = fx.ingress.state.subscribe();
        let caller = start(fx, intake, front, Some(launch_signal));
        crate::test_support::wait::recv_within(&command_seen, LIMIT)
            .expect("the owner starts its successor before the binding read can fail");
        let HostedRecord::Known(pending) = fx.record() else {
            panic!("a create must have installed its Pending row");
        };
        assert_eq!(pending.state, HostedState::Pending);
        let nonce = pending.execution_nonce;
        assert_ne!(nonce, prior.0);
        let context = context_of(&nonce);
        fx.rig.run_provider(&context, false);
        fx.rig.foreground(&[20]);
        fx.rig.show_panes(&[PANE]);
        allow_evidence.send(()).unwrap();
        fx.rt.block_on(async {
            tokio::time::timeout(LIMIT, async {
                loop {
                    let raw: Value = sqlx::query_scalar(
                        "SELECT hosted_execution FROM sessions WHERE channel_id=$1",
                    )
                    .bind(CHANNEL.to_string())
                    .fetch_one(&fx.pool)
                    .await
                    .unwrap();
                    if matches!(HostedRecord::decode(Some(&raw)), HostedRecord::Known(record)
                        if record.execution_nonce == nonce && record.expected.is_some())
                    {
                        break;
                    }
                    assert!(
                        !caller.is_finished(),
                        "launch ended before recording evidence"
                    );
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
        let session = crate::services::tui_prompt_dedupe::binding_context::execution_context(
            "claude", &nonce,
        )
        .unwrap()
        .expected_native_session_id
        .unwrap();
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            fx.cwd.path(),
            &session,
            fx.home().as_deref(),
        )
        .unwrap();
        let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
            (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.clone().into_os_string())
        })
        .encode()
        .unwrap();
        let started = Started {
            session,
            path,
            envelope,
        };
        if lose_before_attach {
            fx.rig.show_panes(&[]);
            fx.rig.exists(20, false);
        } else {
            crate::services::claude_tui::readiness::test_support::pre_source(
                fx.logical(),
                &started.path,
            );
        }
        let source = if started.session == prior.1 {
            "resume"
        } else {
            "startup"
        };
        assert_eq!(
            started.hook(
                fx,
                "SessionStart",
                &json!({
                    "session_id":started.session,"transcript_path":started.path,"source":source
                })
            ),
            202
        );
        while let Ok(event) = events.try_recv() {
            crate::services::claude_tui::hook_server::publish_hook_event_for_tests(event);
        }
        (caller, nonce, started)
    }

    fn tick_and_grant(intake: &Intake, original: &OwnerFront) -> Option<OwnerFront> {
        intake
            .runtime
            .block_on(tick_and_grant_on_mailbox(&intake.mailbox, original.clone()))
            .unwrap()
    }

    async fn tick_and_grant_on_mailbox(
        mailbox: &ChannelMailboxHandle,
        original: OwnerFront,
    ) -> Result<Option<OwnerFront>, String> {
        mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                if let Some(due) = owner.settle_due() {
                    owner.tick(due)?;
                }
                let front = owner.front();
                if let Some(front) = &front {
                    assert_eq!(front.0, original.0);
                    assert_eq!(front.1.message_id, original.1.message_id);
                    assert_eq!(front.1.queued_generation, original.1.queued_generation);
                    assert!(owner.grant_front(front)?);
                }
                Ok(front)
            })
            .await
    }

    fn snapshot(intake: &Intake) -> Value {
        intake
            .runtime
            .block_on(
                intake
                    .mailbox
                    .v2_request(|slot| slot.as_ref().unwrap().json()),
            )
            .unwrap()
    }

    fn assert_wait(intake: &Intake, front: &OwnerFront, nonce: &str) {
        let state = snapshot(intake);
        let wait = &state["retry"]["wait"];
        assert_eq!(
            wait["cause"],
            json!({"kind":"pending_attach","nonce":nonce}),
            "{state}"
        );
        assert_eq!(wait["ticket"], json!(front.0));
        assert_eq!(state["items"], json!([front.1]));
        assert_eq!(state["retained_inputs"], json!([]));
        assert!(state["head_injection"].is_null());
        let original = front.1.clone();
        let failures = intake
            .runtime
            .block_on(intake.mailbox.v2_request(move |slot| {
                Ok(slot.as_ref().unwrap().start_failures_for_test(&original))
            }))
            .unwrap();
        assert_eq!(failures, 0, "the pane loss does not spend a start failure");
    }

    fn placed_attach(loss: PaneLoss) {
        let fx = Fixture::new("placed-attach-owner", None);
        let (old_nonce, previous) = reconnect::launch(&fx, &Arc::new(Launcher::default()));
        let _registry = fx.rig.registry_on_this_thread();
        let _admission = open_admission();
        let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
        let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
        let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:1".into(),
        );
        assert!(set_owner_wake_for_tests(ChannelId::new(CHANNEL), Some(Arc::new(|| {}))).is_none());
        let _wake = WakeGuard;
        assert_eq!(
            previous.hook(
                &fx,
                "Stop",
                &json!({"session_id":previous.session,"transcript_path":previous.path}),
            ),
            202,
        );
        let original_runtime;
        let mut intake = Intake::with_launch(
            &fx,
            &previous,
            ExpectedPin::Verified,
            Arc::default(),
            (true, None),
            None,
        );
        original_runtime = recording_runtime(&mut intake, &fx, true);
        let HostedRecord::Known(mut old) = fx.record() else {
            panic!("bootstrap row")
        };
        old.state = HostedState::Retired;
        fx.store(&old);
        fx.rt.block_on(async {
            let id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE channel_id=$1")
                .bind(CHANNEL.to_string())
                .fetch_one(&fx.pool)
                .await
                .unwrap();
            assert!(
                crate::db::dispatched_sessions::provider_resume_fence::set_for_execution_pg(
                    &fx.pool, id, &old,
                )
                .await
                .unwrap()
            );
        });
        fx.rig.answer(
            "ping",
            json!({"type":"pong","version":"0.9.3","protocol":22}),
        );
        fx.rig.answer("workspace.create",json!({"type":"workspace_created","root_pane":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":0,"cwd":fx.cwd.path()}}));
        fx.rig.answer("pane.process_info", json!({"type":"ok"}));
        fx.rig.show_panes(&[]);
        let front = intake.runtime.block_on(async {
            intake
                .shared
                .core
                .lock()
                .await
                .sessions
                .get_mut(&ChannelId::new(CHANNEL))
                .unwrap()
                .session_id = None;
            intake.front().await
        });
        assert!(front.0.is_some());
        let before = fx.rig.sends().len();
        let (sent, key_seen) = mpsc::channel();
        let (release, reply) = mpsc::channel();
        let (capture_ready, resume_capture) = match loss {
            PaneLoss::BeforeKey(_) => {
                let (entered, ready) = mpsc::channel();
                let (resume, continued) = mpsc::channel();
                let continued = Mutex::new(Some(continued));
                let (mailbox, runtime) = (intake.mailbox.clone(), intake.runtime.handle().clone());
                fx.rig.on_reply(move |call| {
                    use crate::services::session_host::herdr_socket_rig_tests::HerdrRequest;
                    if !matches!(call.request, HerdrRequest::PaneRead { .. }) {
                        return;
                    }
                    let mut continued = continued.lock().unwrap();
                    if continued.is_none() {
                        return;
                    }
                    let reserved =
                        tokio::task::block_in_place(|| {
                            runtime.block_on(mailbox.v2_request(|slot| {
                                Ok(slot.as_ref().unwrap().active_attempt().is_ok())
                            }))
                        })
                        .unwrap();
                    if reserved {
                        let continued = continued.take().unwrap();
                        entered.send(()).unwrap();
                        crate::test_support::wait::recv_within(&continued, LIMIT);
                    }
                });
                (Some(ready), Some(resume))
            }
            _ => (None, None),
        };
        let (first, nonce, started) = launch(
            &fx,
            &intake,
            front.clone(),
            (&old_nonce, &previous.session),
            matches!(loss, PaneLoss::BeforeAttach),
            matches!(loss, PaneLoss::AfterKey).then_some(KeyReply { sent, reply }),
        );
        assert_ne!(started.session, previous.session);
        assert!(
            fx.rt
                .block_on(
                    crate::db::dispatched_sessions::provider_resume_fence::load_for_owner_pg(
                        &fx.pool, &old.owner,
                    )
                )
                .unwrap()
                .is_none()
        );
        let after_command = before + 1;
        match loss {
            PaneLoss::BeforeAttach => {
                let result = crate::test_support::wait::join_within(first, LIMIT);
                assert!(result.is_err(), "{result:?}");
                intake
                    .runtime
                    .block_on(wait_for_idle_turn_to_unwind(&intake));
                assert_eq!(fx.rig.sends().len(), after_command);
                assert_eq!(fx.row(), Some(HostedState::Pending));
                let first_state = snapshot(&intake);

                // Let RED reach the retire boundary even if the first refusal spent a start failure.
                let retry = tick_and_grant(&intake, &front).expect("the same front can retry");
                let retry = start(&fx, &intake, retry, None);
                assert!(crate::test_support::wait::join_within(retry, LIMIT).is_err());
                intake
                    .runtime
                    .block_on(wait_for_idle_turn_to_unwind(&intake));
                assert_eq!(fx.row(), Some(HostedState::Retired));
                assert_wait(&intake, &front, &nonce);
                assert!(
                    snapshot(&intake)["retry"]["wait"]["wait_seq"]
                        .as_u64()
                        .unwrap()
                        > first_state["retry"]["wait"]["wait_seq"].as_u64().unwrap()
                );
                assert_eq!(
                    first_state["retry"]["wait"]["cause"],
                    json!({"kind":"pending_attach","nonce":nonce})
                );
                assert_eq!(fx.rig.sends().len(), after_command);

                assert_successor(
                    &fx,
                    &intake,
                    &front,
                    (&nonce, &started.session),
                    after_command,
                );
            }
            PaneLoss::BeforeKey(recovery) => {
                crate::test_support::wait::recv_within(&capture_ready.unwrap(), LIMIT);
                let state = snapshot(&intake);
                assert_eq!(
                    state["head_injection"]["attempt"]["target"]["execution_nonce"],
                    nonce
                );
                assert_eq!(state["head_injection"]["original"], json!(front.1));
                assert_eq!(fx.row(), Some(HostedState::Pending));
                assert_eq!(fx.rig.sends().len(), after_command);
                let head = state["head_injection"].clone();
                fx.rig.show_panes(&[]);
                fx.rig.exists(20, false);
                resume_capture.unwrap().send(()).unwrap();
                assert!(crate::test_support::wait::join_within(first, LIMIT).is_err());
                intake
                    .runtime
                    .block_on(wait_for_idle_turn_to_unwind(&intake));
                let state = snapshot(&intake);
                assert!(state["head_injection"].is_null(), "{state}");
                assert_eq!(state["items"], json!([front.1]));
                assert_eq!(state["retained_inputs"], json!([]));
                assert!(state["retry"]["wait"].is_null());
                match recovery {
                    PreKeyRecovery::Hold => {}
                    PreKeyRecovery::Requeue => {
                        retain_and_requeue(&fx, &intake, &front, &nonce, &started);
                    }
                    PreKeyRecovery::Restart => {
                        restore_reserved(&intake, &front, head);
                        let restored = snapshot(&intake);
                        assert!(restored["head_injection"].is_null());
                        assert_eq!(restored["items"], json!([front.1]));
                    }
                }
                periodic_retire(&fx, &intake);
                assert!(snapshot(&intake)["retry"]["wait"].is_null());
                assert_eq!(fx.rig.sends().len(), after_command);
                assert_successor(
                    &fx,
                    &intake,
                    &front,
                    (&nonce, &started.session),
                    after_command,
                );
            }
            PaneLoss::AfterKey => {
                crate::test_support::wait::recv_within(&key_seen, LIMIT);
                assert_eq!(fx.rig.sends().len(), after_command + 2);
                assert_eq!(fx.row(), Some(HostedState::Pending));
                let state = snapshot(&intake);
                assert_eq!(state["head_injection"]["phase"], "may_have_sent", "{state}");
                fx.rig.show_panes(&[]);
                fx.rig.exists(20, false);
                release.send(()).unwrap();
                assert!(crate::test_support::wait::join_within(first, LIMIT).is_err());
                intake
                    .runtime
                    .block_on(wait_for_idle_turn_to_unwind(&intake));
                periodic_retire(&fx, &intake);
                assert!(tick_and_grant(&intake, &front).is_none());
                let state = snapshot(&intake);
                assert_eq!(state["retained_inputs"][0]["reason"]["kind"], "unconfirmed");
                assert_eq!(
                    state["retained_inputs"][0]["input"]["original"],
                    json!(front.1)
                );
                let mut owner_drive = source_retry_drive(&intake);
                let (done, complete) = tokio::sync::mpsc::unbounded_channel();
                Arc::get_mut(&mut owner_drive).unwrap().settled = Some(Arc::new(move || {
                    let _ = done.send(());
                    Box::pin(async {})
                }));
                intake.runtime.block_on(async {
                    let mut complete = complete;
                    drive::wake(owner_drive);
                    tokio::time::timeout(LIMIT, complete.recv())
                        .await
                        .unwrap()
                        .unwrap();
                });
                assert_eq!(fx.rig.sends().len(), after_command + 2);
            }
        }
        drop(intake);
        drop(original_runtime);
    }

    fn restore_reserved(intake: &Intake, front: &OwnerFront, head: Value) {
        let mut attempt = head["attempt"].clone();
        assert_eq!(attempt["source"]["kind"], "pre_source");
        attempt["nonce"] = json!(uuid());
        let attempt = serde_json::from_value(attempt).unwrap();
        let payload = serde_json::from_value(head["payload"].clone()).unwrap();
        let channel = ChannelId::new(CHANNEL);
        let persistence =
            discord::queue_persistence_context(&intake.shared, &ProviderKind::Claude, channel);
        intake.runtime.block_on(async {
            let granted = tick_and_grant_on_mailbox(&intake.mailbox, front.clone())
                .await
                .unwrap()
                .unwrap();
            let claim = drive::claim_turn(&intake.mailbox, front.1.message_id, &granted)
                .await
                .unwrap()
                .unwrap();
            intake
                .mailbox
                .v2_request(move |slot| {
                    let owner = slot.as_mut().unwrap();
                    owner.reserve_claimed((claim.front(), claim.number()), attempt, payload)?;
                    assert_eq!(owner.json()?["head_injection"]["phase"], "reserved");
                    slot.take();
                    Ok(())
                })
                .await
                .unwrap();
            intake
                .mailbox
                .v2_ensure_channel(&persistence, channel)
                .await
                .unwrap();
        });
    }

    fn periodic_retire(fx: &Fixture, intake: &Intake) {
        fx.rig.thread_installer_with_launch(NODE, KEY, SESSION)();
        let _runtime = fx.rt.enter();
        QUIET.with(|quiet| quiet.set(Some(Duration::ZERO)));
        discord::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared(
            &intake.shared,
            &ProviderKind::Claude,
        );
        QUIET.with(|quiet| quiet.set(None));
        assert_eq!(fx.row(), Some(HostedState::Retired));
    }

    fn retain_and_requeue(
        fx: &Fixture,
        intake: &Intake,
        front: &OwnerFront,
        nonce: &str,
        started: &Started,
    ) {
        use crate::services::discord::queue_io::input_owner::{effect, pump};
        let prepared = effect::PreparedPayload {
            frame: front.1.text.clone(),
            sources: vec![front.1.message_id.to_string()],
            attempt: json!({
                "nonce":uuid(), "provider":"claude",
                "target":{"host":"herdr","owner":fx.logical(),"root":fx.rig.socket(),"execution_nonce":nonce},
                "source":{"kind":"pre_source","expected_session_id":started.session,
                    "expected_transcript":started.path,"binding_watermark":null}
            }),
        };
        intake.runtime.block_on(async {
            let granted = tick_and_grant_on_mailbox(&intake.mailbox, front.clone())
                .await
                .unwrap();
            let claim = drive::claim_turn(&intake.mailbox, front.1.message_id, &granted)
                .await
                .unwrap()
                .unwrap();
            let clock = Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64);
            let result = pump::submit_front(
                &intake.mailbox,
                Some(claim),
                prepared,
                clock,
                Box::new(|_| Ok(effect::refused("pane unavailable".into(), false))),
            )
            .await;
            assert_eq!(
                result,
                Ok(effect::Completion::NotSent("pane unavailable".into()))
            );
            let original = front.1.clone();
            intake
                .mailbox
                .v2_request(move |slot| {
                    let owner = slot.as_mut().unwrap();
                    let state = owner.json()?;
                    assert_eq!(state["retained_inputs"][0]["disposition"], "known_not_sent");
                    assert_eq!(
                        state["retained_inputs"][0]["input"]["original"],
                        json!(original)
                    );
                    assert!(state["retry"]["wait"].is_null());
                    assert_eq!(owner.requeue_known_not_sent()?.len(), 1);
                    Ok(())
                })
                .await
                .unwrap();
        });
    }

    fn assert_successor(
        fx: &Fixture,
        intake: &Intake,
        front: &OwnerFront,
        prior: (&str, &str),
        after_command: usize,
    ) {
        let original = front.1.clone();
        intake
            .runtime
            .block_on(intake.mailbox.v2_request(move |slot| {
                assert_eq!(slot.as_ref().unwrap().start_failures_for_test(&original), 0);
                Ok(())
            }))
            .unwrap();
        let retry = tick_and_grant(intake, front).unwrap();
        let (successor, new_nonce, new) = launch(fx, intake, retry, prior, false, None);
        assert_ne!(new_nonce, prior.0);
        assert_ne!(new.session, prior.1);
        fx.finished.store(false, Ordering::SeqCst);
        fx.answer_after(&new, after_command + 1);
        assert_eq!(
            crate::test_support::wait::join_within(successor, LIMIT),
            Ok(())
        );
        let sends = fx.rig.sends();
        assert_eq!(sends.len(), after_command + 3);
        assert!(
            sends[after_command + 1]["text"]
                .as_str()
                .unwrap()
                .contains(INPUT)
        );
        assert_eq!(sends[after_command + 2]["keys"], json!(["enter"]));
        let state = snapshot(intake);
        assert_eq!(state["items"], json!([]));
        assert_eq!(state["retained_inputs"], json!([]));
        let original = front.1.clone();
        intake
            .runtime
            .block_on(intake.mailbox.v2_request(move |slot| {
                assert_eq!(slot.as_ref().unwrap().start_failures_for_test(&original), 0);
                Ok(())
            }))
            .unwrap();
    }

    #[test]
    fn s2b1_placed_attach_missing_before_key_retries_same_ticket_pg() {
        placed_attach(PaneLoss::BeforeAttach);
    }

    #[test]
    fn s2b1_placed_attach_missing_after_key_does_not_resubmit_pg() {
        placed_attach(PaneLoss::AfterKey);
    }

    #[test]
    fn s2b1_placed_attach_missing_after_reserve_retries_same_ticket_pg() {
        placed_attach(PaneLoss::BeforeKey(PreKeyRecovery::Hold));
    }

    #[test]
    fn s2b1_placed_attach_missing_known_not_sent_requeues_same_ticket_pg() {
        placed_attach(PaneLoss::BeforeKey(PreKeyRecovery::Requeue));
    }

    #[test]
    fn s2b1_retired_resume_restored_reserved_owner_starts_fresh_pg() {
        placed_attach(PaneLoss::BeforeKey(PreKeyRecovery::Restart));
    }
}
