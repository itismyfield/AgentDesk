//! The intake start's replay check on the actual worker entry and exact-retry handoff.

use super::super::provider_dispatch::{
    INPUT_EFFECT_PROBE, INPUT_PROMPT_PROBE, InputEffectProbe, SubmittedPrompt,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::{self, BeginFrom, CanonicalInput, EffectProjection};
use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
use crate::services::discord::replay_policy::live::{RawObservation, Seen};
use crate::services::discord::replay_policy::permit::EffectTarget;
use crate::services::discord::replay_policy::{ExactHandoff, ExactRetry, RetryPermit};
use crate::services::discord::router::intake_dispatch::replay_admission::{
    input_hash, test_activation,
};
use std::time::Duration;

const STALE: &str = "stale-resume";

mod kickoff_tests {
    //! Exercises late start, durable handback and queue views through actual kickoff.

    use super::*;
    use crate::services::cluster::attachment_transfer::uploads::{PendingUploads, Upload};
    use crate::services::cluster::attachment_transfer::{
        self as attachments, AttachmentBundleV1, AttachmentEntryV1, AttachmentMessageIdentity,
    };
    use crate::services::discord::{
        Intervention, queue_dispatch, queue_marker, queue_reactions, queued_placeholders_store,
        runtime_store,
    };
    use crate::services::turn_orchestrator::{
        self, InterventionMode, SourceMessageQueuedGeneration,
    };

    // Only this test runs in its child; the process-global API never overlaps another test.
    fn isolated_child(test: &str) -> bool {
        const CHILD: &str = "ADK_6008_INPUT_B_KICKOFF_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("kickoff test child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stderr}\n{stdout}"
        );
        false
    }

    struct ConfiguredHost {
        _config: crate::config::TestEnvVarGuard,
        _routing: crate::config::TestEnvVarGuard,
        host: Host,
    }

    // Reuses the parent Host/start/receipt harness; install config before SharedData reads it.
    async fn configured_host(channels: &[ChannelId]) -> ConfiguredHost {
        let root = crate::config::TestRuntimeRootGuard::new();
        let runtime_root = runtime_store::agentdesk_root().expect("temporary runtime root");
        let config = crate::runtime_layout::config_file_path(&runtime_root);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "server: {}\ndata: {dir: data}\nagents: []\n").unwrap();
        let config_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &config,
        );
        let routing_guard = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            std::ffi::OsStr::new("disabled"),
        );
        let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
        crate::services::tui_o::channel_policy::install(&boot).unwrap();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&pool)
            .await
            .expect("real PG before kickoff assertions");
        assert!(version.contains("PostgreSQL"));
        let api = Recorder::start().await;
        crate::services::discord::internal_api::init(api.port, None);
        let mut shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        Arc::get_mut(&mut shared).unwrap().api_port = api.port;
        let workspace = tempfile::tempdir().unwrap();
        for &channel in channels {
            crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "")
                .await;
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = Some(format!("kickoff-input-b-{channel}"));
            session.current_path = Some(workspace.path().display().to_string());
            session.session_id = Some(STALE.into());
        }
        ConfiguredHost {
            _config: config_guard,
            _routing: routing_guard,
            host: Host {
                api,
                shared,
                pool,
                db,
                _workspace: workspace,
                _root: root,
            },
        }
    }

    async fn stored_upload(host: &Host, channel: ChannelId, head: MessageId) -> Upload {
        let identity = AttachmentMessageIdentity {
            provider: "claude".into(),
            channel_id: channel.to_string(),
            user_msg_id: head.to_string(),
        };
        let bytes = b"input-b attachment must survive late start and runtime replacement".to_vec();
        let entry = AttachmentEntryV1 {
            filename: "input-b-custody.txt".into(),
            sha256: attachments::attachment_sha256_hex(&bytes),
            bytes,
        };
        let bundle = AttachmentBundleV1 {
            version: attachments::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![entry],
        };
        let validated = attachments::validate_attachment_bundle_v1(bundle, &identity).unwrap();
        Upload::Bundle(
            attachments::store::put(&host.pool, &validated)
                .await
                .unwrap(),
        )
    }

    fn item(
        host: &Host,
        head: MessageId,
        source: MessageId,
        uploads: PendingUploads,
    ) -> Intervention {
        let generation = host.shared.restart.current_generation;
        let sources = if source == head {
            vec![head]
        } else {
            vec![source, head]
        };
        let generations = sources
            .iter()
            .map(|&id| SourceMessageQueuedGeneration::new(id, generation))
            .collect();
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: head,
            queued_generation: generation,
            source_message_ids: sources,
            source_message_queued_generations: generations,
            source_text_segments: Vec::new(),
            text: format!("whole original input-b request {source}\nsecond original line {head}"),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: Some(format!("reply input-b {source}")),
            has_reply_boundary: true,
            merge_consecutive: false,
            pending_uploads: uploads,
            voice_announcement: None,
        }
    }

    fn marker_path(channel: ChannelId, message: MessageId) -> std::path::PathBuf {
        runtime_store::discord_turn_view_reconciler_root()
            .unwrap()
            .join("intake_user_message")
            .join(format!("{channel}-{message}.json"))
    }

    fn marker(channel: ChannelId, message: MessageId) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(marker_path(channel, message)).unwrap()).unwrap()
    }

    async fn seed_queue_and_view(
        host: &Host,
        channel: ChannelId,
        original: &Intervention,
        tail: Option<&Intervention>,
        head_card: MessageId,
        merged_card: MessageId,
    ) {
        let queue = std::iter::once(original.clone())
            .chain(tail.cloned())
            .collect();
        host.shared
            .mailbox(channel)
            .replace_queue(
                queue,
                queue_dispatch::persistence_context(&host.shared, &ProviderKind::Claude, channel),
            )
            .await;
        let lock = host.shared.queued_placeholders_persist_lock(channel);
        {
            let _guard = lock.lock().await;
            host.shared
                .insert_queued_placeholder_locked(channel, original.message_id, head_card);
            host.shared.insert_queued_placeholder_locked(
                channel,
                original.source_message_ids[0],
                merged_card,
            );
        }
        for (message, emoji) in [
            (
                original.message_id,
                queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            ),
            (
                original.source_message_ids[0],
                queue_reactions::QUEUE_MERGED_PENDING_REACTION,
            ),
        ] {
            assert!(
                queue_marker::note_added_current(
                    &host.shared,
                    &host.api.http,
                    channel,
                    message,
                    emoji,
                    "input_b_seed_queued",
                )
                .await
            );
            assert!(marker_path(channel, message).exists());
        }
    }

    fn assert_full_item(actual: &Intervention, original: &Intervention) {
        assert_eq!(
            actual.message_id, original.message_id,
            "original head identity"
        );
        assert_eq!(
            actual.source_message_ids, original.source_message_ids,
            "every absorbed source"
        );
        assert_eq!(actual.author_id, original.author_id);
        assert_eq!(
            actual.text, original.text,
            "whole original request, including second line"
        );
        assert_eq!(actual.reply_context, original.reply_context);
        assert_eq!(actual.has_reply_boundary, original.has_reply_boundary);
        assert_eq!(actual.merge_consecutive, original.merge_consecutive);
        assert_eq!(
            actual.pending_uploads, original.pending_uploads,
            "durable bundle reference"
        );
    }

    async fn assert_no_retry_child(host: &Host, channel: ChannelId, receipt_id: i64) {
        let children: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id=$1")
                .bind(receipt_id)
                .fetch_one(&host.pool)
                .await
                .unwrap();
        assert_eq!(
            children, 0,
            "late start does not create a retry delivery child"
        );
        let delivery_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM intake_outbox WHERE channel_id=$1 AND NOT replay_only",
        )
        .bind(channel.to_string())
        .fetch_one(&host.pool)
        .await
        .unwrap();
        assert_eq!(
            delivery_rows, 0,
            "kickoff does not turn its requeue into an outbox retry"
        );
    }

    async fn assert_receipt_custody(
        host: &Host,
        channel: ChannelId,
        original: &Intervention,
    ) -> i64 {
        let (id, disposition, nonce) = receipt(host, channel, original.message_id.get())
            .await
            .expect("actual kickoff reached committed begin, not an earlier route guard");
        assert_eq!(
            disposition, "started_unclassified",
            "lost ACK leaves the canonical hold"
        );
        assert!(!nonce.is_empty());
        let stored: (
            String,
            Vec<String>,
            Option<String>,
            serde_json::Value,
            serde_json::Value,
        ) = sqlx::query_as(
            "SELECT user_text, replay_source_message_ids, reply_context, attachment_refs,
                        replay_preserved FROM intake_outbox WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&host.pool)
        .await
        .unwrap();
        let mut sources: Vec<String> = original
            .source_message_ids
            .iter()
            .map(ToString::to_string)
            .collect();
        sources.sort();
        assert_eq!(stored.0, original.text);
        assert_eq!(stored.1, sources);
        assert_eq!(stored.2, original.reply_context);
        assert_eq!(
            stored.3,
            serde_json::json!(
                original
                    .pending_uploads
                    .iter()
                    .map(Upload::history_record)
                    .collect::<Vec<_>>()
            )
        );
        assert!(
            stored.4.get("projection").is_some(),
            "committed projection survives ACK loss"
        );
        assert_no_retry_child(host, channel, id).await;
        id
    }

    async fn kickoff(
        host: &Host,
        channel: ChannelId,
    ) -> crate::services::discord::IdleQueueKickoffChannelOutcome {
        queue_dispatch::kickoff::kickoff_idle_queue_channel(
            &crate::services::discord::router::IntakeDeps {
                http: &host.api.http,
                cache: None,
                ctx_for_chained_dispatch: None,
                shared: &host.shared,
                token: "input-b-test",
            },
            &ProviderKind::Claude,
            channel,
        )
        .await
    }

    async fn assert_restart_custody(
        host: &Host,
        channel: ChannelId,
        original: &Intervention,
        tail: &Intervention,
        head_card: MessageId,
        merged_card: MessageId,
    ) {
        let (checked, role_override) = turn_orchestrator::load_channel_pending_queue_checked(
            &ProviderKind::Claude,
            &host.shared.token_hash,
            channel,
        )
        .expect("checked durable queue loader must succeed, never treat a read failure as empty");
        assert!(role_override.is_none());
        assert_eq!(checked.len(), 2);
        assert_full_item(&checked[0], original);
        assert_full_item(&checked[1], tail);
        let (loaded, restored_overrides) =
            turn_orchestrator::load_pending_queues(&ProviderKind::Claude, &host.shared.token_hash);
        assert!(restored_overrides.is_empty());
        let restored = loaded
            .get(&channel)
            .expect("channel queue survived replacement");
        assert_eq!(restored.len(), 2);
        assert_full_item(&restored[0], original);
        assert_full_item(&restored[1], tail);
        let cards = queued_placeholders_store::load_queued_placeholders(
            &ProviderKind::Claude,
            &host.shared.token_hash,
        );
        assert_eq!(cards.get(&(channel, original.message_id)), Some(&head_card));
        assert_eq!(
            cards.get(&(channel, original.source_message_ids[0])),
            Some(&merged_card)
        );
        let mut replacement = crate::services::discord::make_shared_data_for_tests_with_storage(
            Some(host.pool.clone()),
        );
        Arc::get_mut(&mut replacement).unwrap().api_port = host.api.port;
        assert_eq!(replacement.token_hash, host.shared.token_hash);
        crate::services::discord::host_defer_gate::tests::map_channel(&replacement, channel, "")
            .await;
        {
            let mut core = replacement.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = Some(format!("replacement-input-b-{channel}"));
            session.current_path = Some(host._workspace.path().display().to_string());
            session.session_id = Some(STALE.into());
        }
        assert!(
            replacement
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
        replacement
            .mailbox(channel)
            .replace_queue(
                restored.clone(),
                queue_dispatch::persistence_context(&replacement, &ProviderKind::Claude, channel),
            )
            .await;
        for ((card_channel, message), card) in
            cards.into_iter().filter(|((id, _), _)| *id == channel)
        {
            let lock = replacement.queued_placeholders_persist_lock(card_channel);
            let _guard = lock.lock().await;
            replacement.insert_queued_placeholder_locked(card_channel, message, card);
        }
        let snapshot = replacement.mailbox(channel).snapshot().await;
        assert_full_item(&snapshot.intervention_queue[0], original);
        assert_full_item(&snapshot.intervention_queue[1], tail);
        assert!(replacement.queued_placeholder_still_owned(
            channel,
            original.message_id,
            head_card
        ));
        assert!(replacement.queued_placeholder_still_owned(
            channel,
            original.source_message_ids[0],
            merged_card,
        ));
        let Upload::Bundle(reference) = &snapshot.intervention_queue[0].pending_uploads[0] else {
            panic!("durable bundle, not a process-local path");
        };
        let bundle = attachments::store::load(&host.pool, reference)
            .await
            .unwrap();
        assert_eq!(
            bundle.as_bundle().entries[0].bytes.as_slice(),
            b"input-b attachment must survive late start and runtime replacement"
        );
        let before = receipt(host, channel, original.message_id.get())
            .await
            .unwrap();
        let deps = crate::services::discord::router::IntakeDeps {
            http: &host.api.http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &replacement,
            token: "replacement-input-b-test",
        };
        let entry = queue_dispatch::kickoff::kickoff_idle_queue_channel(
            &deps,
            &ProviderKind::Claude,
            channel,
        );
        let (outcome, submitted) = start(channel, entry).await;
        assert!(
            !outcome.started,
            "replacement cannot issue a second start permit for a lost ACK"
        );
        assert!(
            submitted.is_none(),
            "restored protected input never reaches the provider"
        );
        assert_eq!(
            receipt(host, channel, original.message_id.get()).await,
            Some(before.clone())
        );
        assert_no_retry_child(host, channel, before.0).await;
        let remaining = replacement.mailbox(channel).snapshot().await;
        assert_eq!(
            remaining.intervention_queue.len(),
            1,
            "canonical hold consumes only its old head"
        );
        assert_full_item(&remaining.intervention_queue[0], tail);
        // This exercises queue/card loading and a real new-runtime kickoff, not complete bootstrap.
        replacement.mailboxes.remove_fixture_for_test(channel);
    }

    fn run_case(test: &str, case: usize) {
        if !isolated_child(test) {
            return;
        }
        assert_eq!(std::env::var("AGENTDESK_REQUIRE_PG").as_deref(), Ok("1"));
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
            let channels = [100_000_006_008_881, 100_000_006_008_882, 100_000_006_008_883, 100_000_006_008_884].map(ChannelId::new);
            let fixture = configured_host(&[channels[case]]).await;
            let host = &fixture.host;
            for case in [case] {
                let channel = channels[case];
                let source = MessageId::new(channel.get() * 10 + 1);
                let head = MessageId::new(channel.get() * 10 + 2);
                let head_card = MessageId::new(channel.get() * 10 + 4);
                let merged_card = MessageId::new(channel.get() * 10 + 5);
                let upload = stored_upload(host, channel, head).await;
                let original = item(host, head, source, vec![upload]);
                let tail_id = MessageId::new(channel.get() * 10 + 3);
                let tail = item(host, tail_id, tail_id, Vec::new());
                // Dormant has no tail: its terminal bridge cannot launch an unprobed second turn.
                seed_queue_and_view(host, channel, &original, matches!(case, 1 | 2).then_some(&tail),
                    head_card, merged_card).await;
                let before_markers = [marker(channel, source), marker(channel, head)];
                let op_count = host.shared.turn_view_reconciler.ops().len();
                host.api.take();
                let _active = (case > 0).then(|| test_activation::activate(channel.get()));
                if matches!(case, 1 | 2) { test_activation::lose_next_ack(channel.get()); }
                let fault = async {
                    let (reached, resume) = crate::services::discord::live_bridge::handback_gap::arm(channel.get());
                    let arm = async {
                        tokio::time::timeout(Duration::from_secs(45), reached.notified())
                            .await.expect("actual begin rejection reached claim handback");
                        crate::services::discord::live_bridge::handback_gap::arm_memory_ack_on_persistence_failure(channel.get());
                        turn_orchestrator::test_support::fail_queue_saves(channel, 1);
                        resume.notify_one();
                    };
                    let (outcome, ()) = tokio::join!(kickoff(host, channel), arm);
                    outcome
                };
                let entry = async {
                    if case == 2 { fault.await } else { kickoff(host, channel).await }
                };
                let (outcome, submitted) = start(channel, entry).await;
                if matches!(case, 0 | 3) {
                    assert!(outcome.started, "successful kickoff keeps a real started result (case {case})");
                    let (prompt, release) = submitted.expect("positive actual provider input");
                    assert_eq!(prompt.session_id.as_deref(), Some(STALE));
                    assert!(prompt.prompt.contains(&original.text));
                    assert!(prompt.prompt.contains("input-b-custody.txt"));
                    let _ = release.send(());
                    if case == 0 {
                        assert_eq!(receipt(host, channel, head.get()).await, None);
                    } else {
                        assert!(receipt(host, channel, head.get()).await.is_some(), "active success commits its receipt");
                    }
                    assert!(host.shared.mailbox(channel).snapshot().await.intervention_queue.is_empty());
                    assert!(!host.shared.queued_placeholder_still_owned(channel, head, head_card));
                    assert!(!host.shared.queued_placeholder_still_owned(channel, source, merged_card));
                    let calls = host.api.take();
                    for card in [head_card, merged_card] {
                        let target = format!("/channels/{channel}/messages/{card}");
                        assert!(calls.iter().any(|call| call.starts_with("DELETE ")
                            && call.split_whitespace().nth(1).is_some_and(|path| path.ends_with(&target))),
                            "normal card settlement: {calls:?}");
                    }
                    continue;
                }
                if case == 2 {
                    assert_eq!(turn_orchestrator::test_support::queue_save_faults(channel, true), 0,
                        "the durable requeue save actually consumed its injected fault");
                }
                let snapshot = host.shared.mailbox(channel).snapshot().await;
                assert_eq!(snapshot.intervention_queue.len(), 2, "head restored ahead of independent tail");
                assert_full_item(&snapshot.intervention_queue[0], &original);
                assert_full_item(&snapshot.intervention_queue[1], &tail);
                assert!(snapshot.cancel_token.is_none());
                assert!(host.shared.queued_placeholder_still_owned(channel, head, head_card));
                assert!(host.shared.queued_placeholder_still_owned(channel, source, merged_card));
                assert_eq!([marker(channel, source), marker(channel, head)], before_markers,
                    "late rejection keeps both persisted queue markers");
                let ops = host.shared.turn_view_reconciler.ops();
                for message in [source, head] {
                    assert!(!ops[op_count..].iter().any(|op| op.target.channel_id == channel
                        && op.target.message_id == message && !op.add
                        && queue_reactions::QUEUE_PENDING_REACTION_EMOJIS.contains(&op.emoji)),
                        "marker settlement waits for actual execution: {message}");
                }
                let calls = host.api.take();
                for card in [head_card, merged_card] {
                    let target = format!("/channels/{channel}/messages/{card}");
                    assert!(!calls.iter().any(|call| call.starts_with("DELETE ")
                        && call.split_whitespace().nth(1).is_some_and(|path| path.ends_with(&target))),
                        "no queued card deletion: {calls:?}");
                }
                assert!(!outcome.started, "ACK loss is an unstarted outcome (case {case})");
                assert!(submitted.is_none(), "no provider input after an unacknowledged begin");
                assert_receipt_custody(host, channel, &original).await;
                assert_restart_custody(host, channel, &original, &tail, head_card, merged_card).await;
            }
            let dormant_rows: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM intake_outbox WHERE channel_id=$1",
            ).bind(channels[0].to_string()).fetch_one(&host.pool).await.unwrap();
            assert_eq!(dormant_rows, 0, "dormant producer writes neither receipt nor delivery");
            for channel in channels { host.shared.mailboxes.remove_fixture_for_test(channel); }
            host.pool.close().await;
        });
    }

    #[test]
    fn b_kickoff_dormant_preserves_legacy_execution_pg() {
        run_case(
            concat!(
                "services::discord::router::message_handler::intake_turn::replay_start_tests::",
                "kickoff_tests::b_kickoff_dormant_preserves_legacy_execution_pg",
            ),
            0,
        );
    }

    #[test]
    fn b_kickoff_late_start_keeps_full_custody_and_queue_view_pg() {
        run_case(
            concat!(
                "services::discord::router::message_handler::intake_turn::replay_start_tests::",
                "kickoff_tests::b_kickoff_late_start_keeps_full_custody_and_queue_view_pg",
            ),
            1,
        );
    }

    #[test]
    fn b_kickoff_durable_failure_restores_original_head_pg() {
        run_case(
            concat!(
                "services::discord::router::message_handler::intake_turn::replay_start_tests::",
                "kickoff_tests::b_kickoff_durable_failure_restores_original_head_pg",
            ),
            2,
        );
    }

    #[test]
    fn b_kickoff_active_execution_settles_head_and_merged_cards_pg() {
        run_case(
            concat!(
                "services::discord::router::message_handler::intake_turn::replay_start_tests::",
                "kickoff_tests::b_kickoff_active_execution_settles_head_and_merged_cards_pg",
            ),
            3,
        );
    }

    #[test]
    fn b_queued_finish_exposes_durable_failure_before_restore_pg() {
        if !isolated_child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::",
            "kickoff_tests::b_queued_finish_exposes_durable_failure_before_restore_pg",
        )) {
            return;
        }
        assert_eq!(std::env::var("AGENTDESK_REQUIRE_PG").as_deref(), Ok("1"));
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
            let channel = ChannelId::new(100_000_006_008_885);
            let fixture = configured_host(&[channel]).await;
            let host = &fixture.host;
            let source = MessageId::new(channel.get() * 10 + 1);
            let head = MessageId::new(channel.get() * 10 + 2);
            let head_card = MessageId::new(channel.get() * 10 + 4);
            let merged_card = MessageId::new(channel.get() * 10 + 5);
            let upload = stored_upload(host, channel, head).await;
            let original = item(host, head, source, vec![upload]);
            let tail_id = MessageId::new(channel.get() * 10 + 3);
            let tail = item(host, tail_id, tail_id, Vec::new());
            seed_queue_and_view(host, channel, &original, Some(&tail), head_card, merged_card).await;
            let _active = test_activation::activate(channel.get());
            let take = queue_dispatch::mailbox_take_next_soft_intervention(
                &host.shared, &ProviderKind::Claude, channel,
            ).await;
            assert!(take.persistence_error.is_none(), "the fault belongs to handback, not dequeue");
            let (dequeued, has_more, lease) = take.into_intervention().expect("actual dequeued head");
            assert_full_item(&dequeued, &original);
            assert!(has_more);
            let lease = lease.expect("a real queue dispatch lease");
            let deps = crate::services::discord::router::IntakeDeps {
                http: &host.api.http, cache: None, ctx_for_chained_dispatch: None,
                shared: &host.shared, token: "input-b-durable-error-test",
            };
            let admitted = crate::services::discord::router::admit_queued_intake(
                &deps, ProviderKind::Claude, channel, &dequeued, dequeued.author_id,
                "input-b-owner".into(), has_more, false, "input_b_durable_error",
                Some(lease.clone()),
            ).await;
            let crate::services::discord::router::QueuedAdmissionDisposition::Admitted(admitted) = admitted else {
                panic!("actual queue leader admission must reach its final worker entry");
            };
            test_activation::lose_next_ack(channel.get());
            let (reached, resume) = crate::services::discord::live_bridge::handback_gap::arm(channel.get());
            let arm = async {
                tokio::time::timeout(Duration::from_secs(45), reached.notified())
                    .await.expect("actual begin rejection reaches claim handback");
                crate::services::discord::live_bridge::handback_gap::arm_memory_ack_on_persistence_failure(channel.get());
                turn_orchestrator::test_support::fail_queue_saves(channel, 1);
                resume.notify_one();
            };
            let finish = async {
                let (result, ()) = tokio::join!(
                    crate::services::discord::router::finish_admitted_queued_intake(&deps, admitted),
                    arm,
                );
                result
            };
            let (result, submitted) = start(channel, finish).await;
            assert_eq!(turn_orchestrator::test_support::queue_save_faults(channel, true), 0,
                "the durable queue save actually consumed the injected fault");
            let error = result.err().expect("durable failure is Err despite a memory-only enqueue ACK");
            assert!(error.to_string().contains("replay start deferred"), "wrong failure: {error}");
            assert!(submitted.is_none(), "failed durable handback never reaches provider input");
            let after_failure = host.shared.mailbox(channel).snapshot().await;
            assert_eq!(after_failure.intervention_queue.len(), 1,
                "failed save rolls back its speculative front requeue");
            assert_full_item(&after_failure.intervention_queue[0], &tail);
            assert!(after_failure.cancel_token.is_none());
            assert_receipt_custody(host, channel, &original).await;

            let restored = queue_dispatch::mailbox_restore_dequeued_head(
                &host.shared, &ProviderKind::Claude, channel, dequeued, lease,
            ).await;
            assert!(restored.enqueued && restored.persistence_error.is_none(),
                "a subsequent real durable restore must succeed");
            let snapshot = host.shared.mailbox(channel).snapshot().await;
            assert_eq!(snapshot.intervention_queue.len(), 2);
            assert_full_item(&snapshot.intervention_queue[0], &original);
            assert_full_item(&snapshot.intervention_queue[1], &tail);
            assert_restart_custody(host, channel, &original, &tail, head_card, merged_card).await;
            host.shared.mailboxes.remove_fixture_for_test(channel);
            host.pool.close().await;
        });
    }

    // Append below kickoff_tests helpers. Production saves projection atomically with begin;
    // the throwaway PG fixture preserves an earlier projection in the actual registration commit.
    async fn install_begin_before_commit_refusal(
        host: &Host,
        channel: ChannelId,
        head: MessageId,
        cas: bool,
    ) {
        let key = format!("discord:claude:{channel}:{head}");
        let target = EffectTarget::ProviderStart {
            provider: "claude".into(),
            channel: channel.to_string(),
        }
        .key();
        let quoted = |value: &str| value.replace('\'', "''");
        let mode = if cas { "cas" } else { "storage" };
        let sql = sqlx::AssertSqlSafe(format!(
            r#"
            CREATE SEQUENCE b_input_begin_refusals;
            CREATE FUNCTION b_input_save_prior_projection() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                IF NEW.replay_only AND NEW.replay_disposition='registered_not_started'
                   AND NEW.channel_id=TG_ARGV[0] AND NEW.replay_request_key=TG_ARGV[1] THEN
                    NEW.replay_preserved := COALESCE(NEW.replay_preserved,'{{}}'::jsonb)
                        || jsonb_build_object('projection',jsonb_build_object(
                            'nonce',NEW.replay_episode_nonce,'effect_target',TG_ARGV[2],
                            'input_hash',NEW.replay_request_hash,
                            'binding',jsonb_build_object('user_msg_id',TG_ARGV[3]::BIGINT,
                                                       'fixture','saved_before_begin')));
                END IF;
                RETURN NEW;
            END $$;
            CREATE TRIGGER b_input_prior_projection BEFORE INSERT ON intake_outbox
                FOR EACH ROW EXECUTE FUNCTION b_input_save_prior_projection(
                    '{channel}','{key}','{target}','{head}');
            CREATE FUNCTION b_input_refuse_begin() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                IF OLD.channel_id=TG_ARGV[0] AND OLD.replay_request_key=TG_ARGV[1]
                   AND OLD.replay_disposition='registered_not_started'
                   AND NEW.replay_disposition='started_unclassified'
                   AND OLD.replay_episode_nonce=NEW.replay_episode_nonce
                   AND OLD.replay_preserved->'projection'->>'nonce'=OLD.replay_episode_nonce THEN
                    PERFORM nextval('b_input_begin_refusals');
                    IF TG_ARGV[2]='cas' THEN RETURN NULL; END IF;
                    RAISE EXCEPTION 'BI01-b begin-before-commit storage refusal' USING ERRCODE='P0001';
                END IF;
                RETURN NEW;
            END $$;
            CREATE TRIGGER b_input_begin_refusal BEFORE UPDATE ON intake_outbox
                FOR EACH ROW EXECUTE FUNCTION b_input_refuse_begin('{channel}','{key}','{mode}');
        "#,
            key = quoted(&key),
            target = quoted(&target)
        ));
        sqlx::raw_sql(sql).execute(&host.pool).await.unwrap();
    }

    fn begin_failure_case(test: &str, cas: bool) {
        if !isolated_child(test) {
            return;
        }
        assert_eq!(std::env::var("AGENTDESK_REQUIRE_PG").as_deref(), Ok("1"));
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(
            async {
                let channel = ChannelId::new(if cas {
                    100_000_006_008_887
                } else {
                    100_000_006_008_886
                });
                let fixture = configured_host(&[channel]).await;
                let host = &fixture.host;
                let source = MessageId::new(channel.get() * 10 + 1);
                let head = MessageId::new(channel.get() * 10 + 2);
                let tail_id = MessageId::new(channel.get() * 10 + 3);
                let head_card = MessageId::new(channel.get() * 10 + 4);
                let merged_card = MessageId::new(channel.get() * 10 + 5);
                let upload = stored_upload(host, channel, head).await;
                let original = item(host, head, source, vec![upload]);
                let tail = item(host, tail_id, tail_id, Vec::new());
                seed_queue_and_view(
                    host,
                    channel,
                    &original,
                    Some(&tail),
                    head_card,
                    merged_card,
                )
                .await;
                let before_markers = [marker(channel, source), marker(channel, head)];
                let op_count = host.shared.turn_view_reconciler.ops().len();
                host.api.take();
                install_begin_before_commit_refusal(host, channel, head, cas).await;
                let _active = test_activation::activate(channel.get());
                let (outcome, submitted) = start(channel, kickoff(host, channel)).await;
                let calls: (i64, bool) =
                    sqlx::query_as("SELECT last_value,is_called FROM b_input_begin_refusals")
                        .fetch_one(&host.pool)
                        .await
                        .unwrap();
                assert_eq!(
                    calls,
                    (1, true),
                    "actual final begin reached its pre-commit refusal exactly once"
                );
                assert!(
                    !outcome.started,
                    "a refused begin is not a successful kickoff"
                );
                assert!(
                    submitted.is_none(),
                    "no provider input before begin commit ACK"
                );
                let stored: (
                    i64,
                    String,
                    String,
                    String,
                    Vec<String>,
                    Option<String>,
                    serde_json::Value,
                    serde_json::Value,
                    String,
                ) = sqlx::query_as(
                    "SELECT id,replay_disposition,replay_episode_nonce,user_text,
                    replay_source_message_ids,reply_context,attachment_refs,replay_preserved,
                    replay_request_hash FROM intake_outbox WHERE replay_request_key=$1",
                )
                .bind(format!("discord:claude:{channel}:{head}"))
                .fetch_one(&host.pool)
                .await
                .unwrap();
                assert_eq!(
                    stored.1, "registered_not_started",
                    "the rejected begin did not commit started state"
                );
                assert!(!stored.2.is_empty());
                assert_eq!(stored.3, original.text);
                let mut sources: Vec<String> = original
                    .source_message_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                sources.sort();
                assert_eq!(stored.4, sources);
                assert_eq!(stored.5, original.reply_context);
                assert_eq!(
                    stored.6,
                    serde_json::json!(
                        original
                            .pending_uploads
                            .iter()
                            .map(Upload::history_record)
                            .collect::<Vec<_>>()
                    )
                );
                let target = EffectTarget::ProviderStart {
                    provider: "claude".into(),
                    channel: channel.to_string(),
                }
                .key();
                assert_eq!(
                    stored.7.get("projection"),
                    Some(&serde_json::json!({
                        "nonce":stored.2,"effect_target":target,"input_hash":stored.8,
                        "binding":{"user_msg_id":head.get(),"fixture":"saved_before_begin"},
                    })),
                    "the earlier projection survives failed begin replacement/rollback"
                );
                assert_no_retry_child(host, channel, stored.0).await;
                let snapshot = host.shared.mailbox(channel).snapshot().await;
                assert_eq!(snapshot.intervention_queue.len(), 2);
                assert_full_item(&snapshot.intervention_queue[0], &original);
                assert_full_item(&snapshot.intervention_queue[1], &tail);
                assert!(snapshot.cancel_token.is_none());
                let (restored, role) = turn_orchestrator::load_channel_pending_queue_checked(
                    &ProviderKind::Claude,
                    &host.shared.token_hash,
                    channel,
                )
                .expect("durable queue custody must ACK, not soft-skip its loader error");
                assert!(role.is_none());
                assert_eq!(restored.len(), 2);
                assert_full_item(&restored[0], &original);
                assert_full_item(&restored[1], &tail);
                assert!(
                    host.shared
                        .queued_placeholder_still_owned(channel, head, head_card)
                );
                assert!(
                    host.shared
                        .queued_placeholder_still_owned(channel, source, merged_card)
                );
                let cards = queued_placeholders_store::load_queued_placeholders(
                    &ProviderKind::Claude,
                    &host.shared.token_hash,
                );
                assert_eq!(cards.get(&(channel, head)), Some(&head_card));
                assert_eq!(cards.get(&(channel, source)), Some(&merged_card));
                assert_eq!(
                    [marker(channel, source), marker(channel, head)],
                    before_markers
                );
                let ops = host.shared.turn_view_reconciler.ops();
                for message in [source, head] {
                    assert!(
                        !ops[op_count..]
                            .iter()
                            .any(|op| op.target.channel_id == channel
                                && op.target.message_id == message
                                && !op.add
                                && queue_reactions::QUEUE_PENDING_REACTION_EMOJIS
                                    .contains(&op.emoji)),
                        "failed begin cannot settle a source's queued marker"
                    );
                }
                let requests = host.api.take();
                for card in [head_card, merged_card] {
                    let target = format!("/channels/{channel}/messages/{card}");
                    assert!(
                        !requests.iter().any(|request| request.starts_with("DELETE ")
                            && request
                                .split_whitespace()
                                .nth(1)
                                .is_some_and(|path| path.ends_with(&target))),
                        "failed begin preserves both queued cards: {requests:?}"
                    );
                }
                // Inspect the raw finish before any caller restore can turn an ordinary Err into custody.
                let take = queue_dispatch::mailbox_take_next_soft_intervention(
                    &host.shared,
                    &ProviderKind::Claude,
                    channel,
                )
                .await;
                assert!(
                    take.persistence_error.is_none(),
                    "the original head is durably dequeued again"
                );
                let (dequeued, has_more, lease) =
                    take.into_intervention().expect("same original head");
                assert_full_item(&dequeued, &original);
                assert!(has_more);
                let lease = lease.expect("the second dequeue has its own real dispatch lease");
                let deps = crate::services::discord::router::IntakeDeps {
                    http: &host.api.http,
                    cache: None,
                    ctx_for_chained_dispatch: None,
                    shared: &host.shared,
                    token: "input-b-begin-raw-finish",
                };
                let admitted = crate::services::discord::router::admit_queued_intake(
                    &deps,
                    ProviderKind::Claude,
                    channel,
                    &dequeued,
                    dequeued.author_id,
                    "input-b-owner".into(),
                    has_more,
                    false,
                    "input_b_begin_raw_finish",
                    Some(lease.clone()),
                )
                .await;
                let crate::services::discord::router::QueuedAdmissionDisposition::Admitted(
                    admitted,
                ) = admitted
                else {
                    panic!(
                        "registered-not-started custody must reach the same actual finish again"
                    );
                };
                let entry = crate::services::discord::router::finish_admitted_queued_intake(
                    &deps, admitted,
                );
                let (raw, submitted) = start(channel, entry).await;
                let calls: (i64, bool) =
                    sqlx::query_as("SELECT last_value,is_called FROM b_input_begin_refusals")
                        .fetch_one(&host.pool)
                        .await
                        .unwrap();
                assert_eq!(
                    calls,
                    (2, true),
                    "the same canonical receipt reached a second actual begin refusal"
                );
                assert!(
                    raw.is_ok(),
                    "a durably restored late begin refusal is Deferred, not ordinary Err: {raw:?}"
                );
                assert_eq!(
                    format!("{:?}", raw.unwrap()),
                    "Deferred",
                    "actual typed Deferred crosses the queued finish boundary"
                );
                assert!(
                    submitted.is_none(),
                    "the raw finish cannot launch a provider after refused begin"
                );
                drop(lease);
                let after_raw: (
                    i64,
                    String,
                    String,
                    String,
                    Vec<String>,
                    Option<String>,
                    serde_json::Value,
                    serde_json::Value,
                    String,
                ) = sqlx::query_as(
                    "SELECT id,replay_disposition,replay_episode_nonce,user_text,
                        replay_source_message_ids,reply_context,attachment_refs,replay_preserved,
                        replay_request_hash FROM intake_outbox WHERE replay_request_key=$1",
                )
                .bind(format!("discord:claude:{channel}:{head}"))
                .fetch_one(&host.pool)
                .await
                .unwrap();
                assert_eq!(
                    after_raw, stored,
                    "logical receipt id/nonce/projection/full input never change"
                );
                assert_no_retry_child(host, channel, stored.0).await;
                let snapshot = host.shared.mailbox(channel).snapshot().await;
                assert_eq!(snapshot.intervention_queue.len(), 2);
                assert_full_item(&snapshot.intervention_queue[0], &original);
                assert_full_item(&snapshot.intervention_queue[1], &tail);
                assert!(snapshot.cancel_token.is_none());
                let (restored, role) = turn_orchestrator::load_channel_pending_queue_checked(
                    &ProviderKind::Claude,
                    &host.shared.token_hash,
                    channel,
                )
                .expect("raw Deferred requires a real durable queue ACK");
                assert!(role.is_none());
                assert_eq!(restored.len(), 2);
                assert_full_item(&restored[0], &original);
                assert_full_item(&restored[1], &tail);
                assert!(
                    host.shared
                        .queued_placeholder_still_owned(channel, head, head_card)
                );
                assert!(
                    host.shared
                        .queued_placeholder_still_owned(channel, source, merged_card)
                );
                assert_eq!(
                    [marker(channel, source), marker(channel, head)],
                    before_markers
                );
                let requests = host.api.take();
                for card in [head_card, merged_card] {
                    let target = format!("/channels/{channel}/messages/{card}");
                    assert!(
                        !requests.iter().any(|request| request.starts_with("DELETE ")
                            && request
                                .split_whitespace()
                                .nth(1)
                                .is_some_and(|path| path.ends_with(&target))),
                        "raw Deferred keeps both queued cards: {requests:?}"
                    );
                }
                host.shared.mailboxes.remove_fixture_for_test(channel);
                host.pool.close().await;
            },
        ));
    }

    #[test]
    fn b_kickoff_begin_storage_failure_preserves_committed_projection_and_original_queue_pg() {
        begin_failure_case(
            concat!(
                "services::discord::router::message_handler::intake_turn::replay_start_tests::kickoff_tests::",
                "b_kickoff_begin_storage_failure_preserves_committed_projection_and_original_queue_pg",
            ),
            false,
        );
    }

    #[test]
    fn b_kickoff_begin_cas_failure_preserves_committed_projection_and_original_queue_pg() {
        begin_failure_case(
            concat!(
                "services::discord::router::message_handler::intake_turn::replay_start_tests::kickoff_tests::",
                "b_kickoff_begin_cas_failure_preserves_committed_projection_and_original_queue_pg",
            ),
            true,
        );
    }
}

mod worker_entry_tests {
    //! Exercises worker admission and delivery custody through the actual executor.

    use super::*;
    use crate::db::intake_outbox::{InsertPendingPayload, insert_pending};
    use crate::services::cluster::intake_worker::{
        TickOutcome, run_intake_worker_tick, test_executor,
    };
    use crate::services::discord::health::owner_runtime_for_tests;

    const WORKER: &str = "input-b-worker";
    const BODY: &str = "keep the complete worker input";

    fn child(test: &str) -> bool {
        const CHILD: &str = "ADK_INPUT_B_WORKER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stderr}\n{stdout}"
        );
        false
    }

    async fn worker_host(
        channels: &[ChannelId],
    ) -> (Host, Arc<crate::services::discord::health::HealthRegistry>) {
        let (registry, _) = owner_runtime_for_tests::registered("claude").await;
        let mut host = host(channels).await;
        crate::db::replay_disposition::tests::apply_test_mutant(&host.pool).await;
        Arc::get_mut(&mut host.shared).unwrap().health_registry = Arc::downgrade(&registry);
        host.shared
            .http
            .cached_bot_token
            .set("test-token".into())
            .unwrap();
        host.shared.settings.write().await.allowed_channel_ids =
            channels.iter().map(|id| id.get()).collect();
        registry
            .register("claude".into(), host.shared.clone())
            .await;
        for channel in channels {
            sqlx::query("INSERT INTO agents (id,name,provider,discord_channel_id) VALUES ($1,'Test','claude',$2)")
                .bind(format!("input-b-{channel}"))
                .bind(channel.to_string())
                .execute(&host.pool).await.unwrap();
        }
        (host, registry)
    }

    async fn attachment(host: &Host, channel: ChannelId, message: u64) -> serde_json::Value {
        use crate::services::cluster::attachment_transfer::{
            self as transfer, AttachmentBundleV1, AttachmentEntryV1, AttachmentMessageIdentity,
        };
        let identity = AttachmentMessageIdentity {
            provider: "claude".into(),
            channel_id: channel.to_string(),
            user_msg_id: message.to_string(),
        };
        let bytes = b"keep all attachment bytes".to_vec();
        let bundle = AttachmentBundleV1 {
            version: transfer::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![AttachmentEntryV1 {
                filename: "worker-input.txt".into(),
                sha256: transfer::attachment_sha256_hex(&bytes),
                bytes,
            }],
        };
        let validated = transfer::validate_attachment_bundle_v1(bundle, &identity).unwrap();
        let reference = transfer::store::put(&host.pool, &validated).await.unwrap();
        serde_json::json!([reference])
    }

    async fn delivery(
        host: &Host,
        channel: ChannelId,
        message: u64,
        sources: Option<Vec<String>>,
    ) -> (i64, serde_json::Value) {
        let refs = attachment(host, channel, message).await;
        let payload = InsertPendingPayload {
            target_instance_id: WORKER.into(),
            forwarded_by_instance_id: "input-b-leader".into(),
            required_labels: serde_json::json!([]),
            execution_requirements: serde_json::json!({}),
            attachment_refs: refs.clone(),
            channel_id: channel.to_string(),
            user_msg_id: message.to_string(),
            replay_source_message_ids: None,
            request_owner_id: "7".into(),
            request_owner_name: Some("owner".into()),
            user_text: BODY.into(),
            reply_context: Some("keep reply context".into()),
            has_reply_boundary: true,
            dm_hint: Some(false),
            turn_kind: "foreground".into(),
            merge_consecutive: false,
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            preserve_on_cancel: true,
            agent_id: format!("input-b-{channel}"),
            provider: "claude".into(),
            home_epoch: None,
        };
        let id = insert_pending(&host.pool, &payload, 1, None).await.unwrap();
        // Seed SQL directly to cover rows written by older delivery producers.
        sqlx::query("UPDATE intake_outbox SET replay_source_message_ids=$2 WHERE id=$1")
            .bind(id)
            .bind(sources)
            .execute(&host.pool)
            .await
            .unwrap();
        (id, refs)
    }

    async fn registered(host: &Host, channel: ChannelId, sources: &[u64]) -> i64 {
        let input = CanonicalInput {
            request_key: format!("input-b-absorbed-{channel}"),
            provider: "claude".into(),
            channel: channel.to_string(),
            sources: sources.iter().map(u64::to_string).collect(),
            original_text: "original held source".into(),
            owner_id: "7".into(),
            agent_id: format!("input-b-{channel}"),
            attachments: serde_json::json!([]),
            reply_context: None,
            provenance: "new_input",
        };
        write::register_or_reuse(&host.pool, &input, "input-b-original-owner")
            .await
            .unwrap()
            .id
    }

    async fn tick(host: &Host, channel: ChannelId, owner: &str) -> Option<SubmittedPrompt> {
        let (result, submitted) = start(
            channel,
            run_intake_worker_tick(&host.pool, &host.shared, WORKER, "claude", owner, &|| false),
        )
        .await;
        result.unwrap();
        submitted.map(|(prompt, release)| {
            if !crate::services::discord::router::checkpoint_waits_for_admission(channel) {
                let state = crate::services::discord::inflight::load_inflight_state_read_only(
                    &ProviderKind::Claude,
                    channel.get(),
                )
                .expect("actual dormant provider has its inflight payload");
                assert!(
                    state.source_message_ids.is_empty(),
                    "dormant hydration keeps the legacy source payload"
                );
            }
            let _ = release.send(());
            prompt
        })
    }

    async fn assert_custody(
        host: &Host,
        id: i64,
        sources: Option<Vec<String>>,
        refs: serde_json::Value,
    ) {
        let row: (
            String,
            bool,
            String,
            Option<Vec<String>>,
            serde_json::Value,
            Option<String>,
            i32,
        ) = sqlx::query_as(
            "SELECT status::TEXT, completed_at IS NULL, user_text, replay_source_message_ids,
                    attachment_refs, reply_context, attempt_no FROM intake_outbox WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&host.pool)
        .await
        .unwrap();
        assert!(
            ![
                "pending",
                "done",
                "failed_post_accept",
                "failed_pre_accept",
                "dispatched"
            ]
            .contains(&row.0.as_str()),
            "an unstarted replay-blocked delivery stays held: {row:?}"
        );
        assert!(
            row.1,
            "an unstarted replay-blocked delivery is not completed: {row:?}"
        );
        assert_eq!(
            (row.2, row.3, row.4, row.5, row.6),
            (
                BODY.into(),
                sources,
                refs,
                Some("keep reply context".into()),
                1
            )
        );
        let children: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id=$1")
                .bind(id)
                .fetch_one(&host.pool)
                .await
                .unwrap();
        assert_eq!(
            children, 0,
            "no retry child disguises the original delivery custody"
        );
        let channel: String =
            sqlx::query_scalar("SELECT channel_id FROM intake_outbox WHERE id=$1")
                .bind(id)
                .fetch_one(&host.pool)
                .await
                .unwrap();
        let queued = host
            .shared
            .mailbox(ChannelId::new(channel.parse().unwrap()))
            .snapshot()
            .await;
        assert!(
            queued.intervention_queue.is_empty(),
            "the original delivery is custody; no duplicate mailbox requeue"
        );
    }

    #[test]
    fn b_worker_late_absorbed_hold_keeps_original_delivery_without_marker_or_input_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_worker_late_absorbed_hold_keeps_original_delivery_without_marker_or_input_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
            let channel = ChannelId::new(6_008_801);
            let message = 6_008_811;
            let sources = Some(vec![message.to_string(), (message+1).to_string()]);
            let (host, _registry) = worker_host(&[channel]).await;
            let _rest = crate::services::discord::shared_state::test_rest::install(host.api.http.clone());
            let _active = test_activation::activate(channel.get());
            let (id, refs) = delivery(&host, channel, message, sources.clone()).await;
            let receipt = registered(&host, channel, &[message+1]).await;
            let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (hook_pool, committed) = (host.pool.clone(), changed.clone());
            let _hook = test_executor::hook(Box::new(move |checkpoint| {
                let (pool, committed) = (hook_pool.clone(), committed.clone());
                Box::pin(async move {
                    if checkpoint == test_executor::Checkpoint::Spawned {
                        sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld', replay_hold_reason='late hold', replay_preserved='{\"partial_body\":\"keep result\"}' WHERE id=$1")
                            .bind(receipt).execute(&pool).await.unwrap();
                        committed.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            }));
            assert!(tick(&host, channel, "first-owner").await.is_none(), "the absorbed held source reaches no provider");
            assert!(changed.load(std::sync::atomic::Ordering::SeqCst), "the hold commits after mark_spawned and before the real worker entry");
            let calls = host.api.take();
            assert!(!calls.iter().any(|call| call.starts_with("POST ")), "worker admission refuses before placeholders or start markers: {calls:?}");
            assert_custody(&host,id,sources.clone(),refs.clone()).await;
            let held: (String,String,serde_json::Value) = sqlx::query_as("SELECT user_text,replay_disposition,replay_preserved FROM intake_outbox WHERE id=$1")
                .bind(receipt).fetch_one(&host.pool).await.unwrap();
            assert_eq!(held, ("original held source".into(),"withheld".into(),serde_json::json!({"partial_body":"keep result"})));
            drop(_hook);
            assert!(tick(&host,channel,"replacement-owner").await.is_none(), "a restarted worker creates no additional input");
            assert_custody(&host,id,sources,refs).await;
            host.pool.close().await;
            host.db.drop().await;
        }));
    }

    #[test]
    fn b_worker_late_lookup_failure_keeps_original_delivery_without_input_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_worker_late_lookup_failure_keeps_original_delivery_without_input_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
            let channel = ChannelId::new(6_008_802);
            let message = 6_008_821;
            let sources = Some(vec![message.to_string(), (message+1).to_string()]);
            let (host, _registry) = worker_host(&[channel]).await;
            let _rest = crate::services::discord::shared_state::test_rest::install(host.api.http.clone());
            let _active = test_activation::activate(channel.get());
            let (id, refs) = delivery(&host, channel, message, sources.clone()).await;
            let hook_pool = host.pool.clone();
            let _hook = test_executor::hook(Box::new(move |checkpoint| {
                let pool = hook_pool.clone();
                Box::pin(async move {
                    if checkpoint == test_executor::Checkpoint::Spawned {
                        // The delivery transitions still work; the receipt lookup's SELECT alone fails.
                        sqlx::query("ALTER TABLE intake_outbox RENAME COLUMN replay_request_hash TO replay_request_hash_unavailable")
                            .execute(&pool).await.unwrap();
                    }
                })
            }));
            let submitted = tick(&host, channel, "lookup-owner").await;
            drop(_hook);
            sqlx::query("ALTER TABLE intake_outbox RENAME COLUMN replay_request_hash_unavailable TO replay_request_hash")
                .execute(&host.pool).await.unwrap();
            assert!(submitted.is_none(), "receipt lookup failure cannot reach provider input");
            let calls = host.api.take();
            assert!(!calls.iter().any(|call| call.starts_with("POST ")), "lookup failure is handled before placeholders or start markers: {calls:?}");
            assert_custody(&host,id,sources.clone(),refs.clone()).await;
            assert!(tick(&host,channel,"replacement-owner").await.is_none(), "a replacement worker cannot recover the unstarted delivery as fresh input");
            assert_custody(&host,id,sources,refs).await;
            host.pool.close().await;
            host.db.drop().await;
        }));
    }

    #[test]
    fn b_worker_late_begin_ack_loss_keeps_original_delivery_without_success_or_retry_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_worker_late_begin_ack_loss_keeps_original_delivery_without_success_or_retry_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(
            async {
                let channel = ChannelId::new(6_008_803);
                let message = 6_008_831;
                let sources = Some(vec![message.to_string(), (message + 1).to_string()]);
                let (host, _registry) = worker_host(&[channel]).await;
                let _rest = crate::services::discord::shared_state::test_rest::install(
                    host.api.http.clone(),
                );
                let _active = test_activation::activate(channel.get());
                let (id, refs) = delivery(&host, channel, message, sources.clone()).await;
                let _hook = test_executor::hook(Box::new(move |checkpoint| {
                    Box::pin(async move {
                        if checkpoint == test_executor::Checkpoint::Spawned {
                            test_activation::lose_next_ack(channel.get());
                        }
                    })
                }));
                assert!(
                    tick(&host, channel, "lost-ack-owner").await.is_none(),
                    "a committed begin without ACK gives no provider input"
                );
                drop(_hook);
                assert_custody(&host, id, sources.clone(), refs.clone()).await;
                let (receipt_id, disposition, _) = receipt(&host, channel, message)
                    .await
                    .expect("begin committed its receipt");
                assert_eq!(disposition, "started_unclassified");
                let receipt_sources: Vec<String> = sqlx::query_scalar(
                    "SELECT replay_source_message_ids FROM intake_outbox WHERE id=$1",
                )
                .bind(receipt_id)
                .fetch_one(&host.pool)
                .await
                .unwrap();
                assert_eq!(receipt_sources, sources.clone().unwrap());
                assert!(
                    tick(&host, channel, "replacement-owner").await.is_none(),
                    "lost ACK cannot become fresh input after worker replacement"
                );
                assert_custody(&host, id, sources, refs).await;
                host.pool.close().await;
                host.db.drop().await;
            },
        ));
    }

    #[test]
    fn b_worker_dormant_dispatch_and_active_legacy_null_sources_use_actual_provider_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_worker_dormant_dispatch_and_active_legacy_null_sources_use_actual_provider_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
            let (dormant,active) = (ChannelId::new(6_008_804),ChannelId::new(6_008_805));
            let (host,_registry) = worker_host(&[dormant,active]).await;
            let _rest = crate::services::discord::shared_state::test_rest::install(host.api.http.clone());
            let (dormant_id,_) = delivery(&host,dormant,6_008_841,Some(vec!["6008841".into(),"6008842".into()])).await;
            let prompt = tick(&host,dormant,"dormant-owner").await.expect("dormant keeps the legacy provider dispatch");
            assert_eq!(prompt.session_id.as_deref(),Some(STALE));
            let dormant_status: (String,bool) = sqlx::query_as("SELECT status::TEXT,completed_at IS NOT NULL FROM intake_outbox WHERE id=$1")
                .bind(dormant_id).fetch_one(&host.pool).await.unwrap();
            assert_eq!(dormant_status,("done".into(),true));
            let replay_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE replay_only OR replay_disposition IS NOT NULL")
                .fetch_one(&host.pool).await.unwrap();
            assert_eq!(replay_rows,0,"dormant actual dispatch makes no receipt");
            let _active = test_activation::activate(active.get());
            let (legacy_id,_) = delivery(&host,active,6_008_851,None).await;
            assert!(tick(&host,active,"old-format-owner").await.is_some(),"an old delivery row with NULL sources runs under its original message identity");
            let (receipt_id,disposition,_) = receipt(&host,active,6_008_851).await.expect("legacy active input gets a receipt");
            assert_eq!(disposition,"started_unclassified");
            let canonical: Vec<String> = sqlx::query_scalar("SELECT replay_source_message_ids FROM intake_outbox WHERE id=$1")
                .bind(receipt_id).fetch_one(&host.pool).await.unwrap();
            assert_eq!(canonical,vec!["6008851".to_string()]);
            let restored: Option<Vec<String>> = sqlx::query_scalar("SELECT replay_source_message_ids FROM intake_outbox WHERE id=$1")
                .bind(legacy_id).fetch_one(&host.pool).await.unwrap();
            assert_eq!(restored,None,"the old delivery row remains in its old storage format");
            assert!(tick(&host,active,"replacement-owner").await.is_none(),"neither dispatch is input again after a replacement worker tick");
            host.pool.close().await;
            host.db.drop().await;
        }));
    }

    #[test]
    fn b_absorbed_sources_survive_leader_forwarding_into_real_worker_claim_fence_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_absorbed_sources_survive_leader_forwarding_into_real_worker_claim_fence_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
            use crate::services::discord::router::intake_dispatch::{IntakeAdmission,IntakeOrigin,IntakeSubmission,admit_text_intake};
            let channel = ChannelId::new(6_008_806);
            let message = 6_008_861;
            let (host,_registry) = worker_host(&[channel]).await;
            let _rest = crate::services::discord::shared_state::test_rest::install(host.api.http.clone());
            let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock("ADK_INTAKE_ROUTING_MODE",std::ffi::OsStr::new("enforce"));
            let _active = test_activation::activate(channel.get());
            sqlx::query("INSERT INTO worker_nodes (instance_id,status,role,effective_role,labels,capabilities,last_heartbeat_at,started_at,updated_at) VALUES ($1,'online','worker','worker','[]','{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"],\"features\":[\"preserve_on_cancel_v1\"]}}',NOW(),NOW(),NOW())")
                .bind(WORKER).execute(&host.pool).await.unwrap();
            sqlx::query("INSERT INTO sessions (session_key,agent_id,provider,channel_id,instance_id,status,last_heartbeat) VALUES ($1,$2,'claude',$3,$4,'idle',NOW())")
                .bind(format!("claude-{channel}")).bind(format!("input-b-{channel}")).bind(channel.to_string()).bind(WORKER)
                .execute(&host.pool).await.unwrap();
            let submission = IntakeSubmission {
                provider: ProviderKind::Claude,request: request(channel,message,BODY),origin: IntakeOrigin::LiveMessage,
                preserve_on_cancel: true,has_nonportable_uploads: false,attachments: Vec::new(),preloaded_uploads: Vec::new(),voice_announcement: None,
            };
            let deps = IntakeDeps { http:&host.api.http,cache:None,ctx_for_chained_dispatch:None,shared:&host.shared,token:"test-token" };
            let IntakeAdmission::Forwarded { outbox_id,target_instance_id } = admit_text_intake(&deps,&submission).await else {
                panic!("the leader must actually INSERT a forwarded delivery before the later hold");
            };
            assert_eq!(target_instance_id,WORKER);
            let forwarded: (String,Option<Vec<String>>,serde_json::Value) = sqlx::query_as("SELECT user_text,replay_source_message_ids,attachment_refs FROM intake_outbox WHERE id=$1")
                .bind(outbox_id).fetch_one(&host.pool).await.unwrap();
            assert_eq!(forwarded,(BODY.into(),Some(vec![message.to_string(),(message+1).to_string()]),serde_json::json!([])),"leader INSERT preserves every absorbed source for the worker's SQL fence");
            let receipt = registered(&host,channel,&[message+1]).await;
            sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld',replay_hold_reason='between forward and claim' WHERE id=$1")
                .bind(receipt).execute(&host.pool).await.unwrap();
            let (result,provider) = start(channel,run_intake_worker_tick(&host.pool,&host.shared,WORKER,"claude","claim-owner",&|| false)).await;
            if let Some((_,release)) = provider { let _=release.send(()); panic!("the SQL claim fence must prevent the absorbed source from reaching a provider"); }
            assert_eq!(result.unwrap(),TickOutcome::QueueEmpty);
            let pending: (String,bool,bool,bool) = sqlx::query_as("SELECT status::TEXT,claimed_at IS NULL,accepted_at IS NULL,spawned_at IS NULL FROM intake_outbox WHERE id=$1")
                .bind(outbox_id).fetch_one(&host.pool).await.unwrap();
            assert_eq!(pending,("pending".into(),true,true,true));
            assert!(host.api.take().iter().all(|call| !call.starts_with("POST ")));
            assert!(tick(&host,channel,"replacement-owner").await.is_none());
            host.pool.close().await;
            host.db.drop().await;
        }));
    }

    #[test]
    fn b_worker_dormant_env_off_and_on_reach_same_actual_provider_without_receipts_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_worker_dormant_env_off_and_on_reach_same_actual_provider_without_receipts_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
            let channels = [ChannelId::new(6_008_807),ChannelId::new(6_008_808)];
            let (host,_registry) = worker_host(&channels).await;
            let _rest = crate::services::discord::shared_state::test_rest::install(host.api.http.clone());
            let mut outcomes = Vec::new();
            for (channel,value) in channels.into_iter().zip(["0","1"]) {
                let _env = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
                    "ADK_REPLAY_HOLD_PRODUCERS",std::ffi::OsStr::new(value),
                );
                let message = channel.get()+100;
                let sources = Some(vec![message.to_string(),(message+1).to_string()]);
                let (id,refs) = delivery(&host,channel,message,sources.clone()).await;
                let prompt = tick(&host,channel,"dormant-owner").await.expect("both env values reach the actual legacy provider dispatch");
                assert_eq!(prompt.session_id.as_deref(),Some(STALE));
                assert!(prompt.prompt.contains(BODY));
                let row: (String,bool,String,Option<Vec<String>>,serde_json::Value,i32) = sqlx::query_as(
                    "SELECT status::TEXT,completed_at IS NOT NULL,user_text,replay_source_message_ids,attachment_refs,attempt_no FROM intake_outbox WHERE id=$1",
                ).bind(id).fetch_one(&host.pool).await.unwrap();
                assert_eq!((row.2,row.3,row.4,row.5),(BODY.into(),sources,refs,1));
                outcomes.push((prompt.session_id,row.0,row.1));
                let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE replay_only OR replay_disposition IS NOT NULL")
                    .fetch_one(&host.pool).await.unwrap();
                assert_eq!(receipts,0,"environment {value} cannot connect an activation supplier at the actual worker entry");
            }
            assert_eq!(outcomes,vec![(Some(STALE.into()),"done".into(),true);2]);
            host.pool.close().await;
            host.db.drop().await;
        }));
    }

    #[test]
    fn b_worker_malformed_sql_sources_hold_claimed_custody_without_failure_or_child_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_worker_malformed_sql_sources_hold_claimed_custody_without_failure_or_child_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
            let channels = [6_008_809,6_008_810,6_008_811].map(ChannelId::new);
            let (host,_registry) = worker_host(&channels).await;
            let _rest = crate::services::discord::shared_state::test_rest::install(host.api.http.clone());
            for (channel,malformed) in channels.into_iter().zip(["not-a-snowflake","0","18446744073709551616"]) {
                let message = channel.get()+100;
                // The schema accepts these nonempty strings; the message representative is still present.
                let sources = Some(vec![message.to_string(),malformed.into()]);
                let (id,refs) = delivery(&host,channel,message,sources.clone()).await;
                let _active = test_activation::activate(channel.get());
                assert!(tick(&host,channel,"malformed-owner").await.is_none(),"{malformed}: malformed authority may not be dropped while constructing typed sources");
                let calls = host.api.take();
                assert!(calls.iter().all(|call| !call.starts_with("POST ")),"{malformed}: no core start marker or placeholder: {calls:?}");
                type ClaimedDeliveryRow = (String,Option<String>,bool,bool,bool,String,Option<Vec<String>>,serde_json::Value,i32,i32);
                let row: ClaimedDeliveryRow = sqlx::query_as(
                    "SELECT status::TEXT,claim_owner,accepted_at IS NULL,spawned_at IS NULL,completed_at IS NULL,
                            user_text,replay_source_message_ids,attachment_refs,attempt_no,retry_count FROM intake_outbox WHERE id=$1",
                ).bind(id).fetch_one(&host.pool).await.unwrap();
                assert_eq!(row,("claimed".into(),Some("malformed-owner".into()),true,true,true,BODY.into(),sources.clone(),refs.clone(),1,0));
                let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id=$1")
                    .bind(id).fetch_one(&host.pool).await.unwrap();
                assert_eq!(children,0,"an undecodable source is custody, not a normal preaccept retry budget item");
                assert!(tick(&host,channel,"replacement-owner").await.is_none());
                // This case stops before accept. The other late-start cases cover accepted/spawned custody.
            }
            host.pool.close().await;
            host.db.drop().await;
        }));
    }

    async fn waiting_for_registration(pool: &sqlx::PgPool, expected: i64) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_stat_activity
                      WHERE datname=current_database() AND wait_event_type='Lock'
                        AND wait_event='advisory' AND query LIKE '%pg_advisory_xact_lock(6008%'",
                )
                .fetch_one(pool)
                .await
                .unwrap();
                if waiting == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the actual entry must wait at the same canonical registration lock");
    }

    #[test]
    fn b_actual_leader_begin_races_spawned_worker_and_loser_keeps_delivery_custody_pg() {
        if !child(concat!(
            "services::discord::router::message_handler::intake_turn::replay_start_tests::worker_entry_tests::",
            "b_actual_leader_begin_races_spawned_worker_and_loser_keeps_delivery_custody_pg"
        )) {
            return;
        }
        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
            use crate::services::discord::router::intake_dispatch::{
                IntakeAdmission,IntakeOrigin,IntakeSubmission,admit_text_intake,finish_text_intake_admission,
            };
            use crate::services::cluster::attachment_transfer::uploads::{BundleRef,Upload};
            use super::super::super::provider_dispatch::CLAUDE_LEGACY_PROBE;

            let channel = ChannelId::new(6_008_812);
            let message = 6_008_912;
            let sources = Some(vec![message.to_string(),(message+1).to_string()]);
            let key = format!("discord:claude:{channel}:{message}");
            let (host,_registry) = worker_host(&[channel]).await;
            let _rest = crate::services::discord::shared_state::test_rest::install(host.api.http.clone());
            let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
                "ADK_INTAKE_ROUTING_MODE",std::ffi::OsStr::new("disabled"),
            );
            let _active = test_activation::activate(channel.get());
            let (id,refs) = delivery(&host,channel,message,sources.clone()).await;

            // Separate runtime claims make the database boundary prove single start, not a mailbox race.
            let mut leader = crate::services::discord::make_shared_data_for_tests_with_storage(Some(host.pool.clone()));
            Arc::get_mut(&mut leader).unwrap().api_port = host.api.port;
            crate::services::discord::host_defer_gate::tests::map_channel(&leader,channel,"").await;
            {
                let mut core = leader.core.lock().await;
                let session = core.sessions.get_mut(&channel).unwrap();
                session.channel_name = Some(format!("replay-start-{channel}"));
                session.current_path = Some(host._workspace.path().display().to_string());
                session.session_id = Some(STALE.into());
            }
            let bundle_refs: Vec<BundleRef> = serde_json::from_value(refs.clone()).unwrap();
            let mut leader_request = request(channel,message,BODY);
            leader_request.reply_context = Some("keep reply context".into());
            leader_request.has_reply_boundary = true;
            leader_request.preserve_on_cancel = true;
            let submission = IntakeSubmission {
                provider:ProviderKind::Claude,request:leader_request,origin:IntakeOrigin::LiveMessage,
                preserve_on_cancel:true,has_nonportable_uploads:false,attachments:Vec::new(),
                preloaded_uploads:bundle_refs.into_iter().map(Upload::Bundle).collect(),voice_announcement:None,
            };
            let deps = IntakeDeps { http:&host.api.http,cache:None,ctx_for_chained_dispatch:None,shared:&leader,token:"test-token" };

            let mut blocker = host.pool.begin().await.unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock(6008,hashtext($1))")
                .bind(&key).execute(&mut *blocker).await.unwrap();
            let (spawned,at_spawn) = tokio::sync::oneshot::channel();
            let (release_worker,continue_worker) = tokio::sync::oneshot::channel();
            let (mut spawned,mut continue_worker) = (Some(spawned),Some(continue_worker));
            let _hook = test_executor::hook(Box::new(move |checkpoint| {
                let pause = (checkpoint == test_executor::Checkpoint::Spawned)
                    .then(|| (spawned.take().unwrap(),continue_worker.take().unwrap()));
                Box::pin(async move {
                    if let Some((spawned,continue_worker)) = pause {
                        spawned.send(()).unwrap();
                        continue_worker.await.unwrap();
                    }
                })
            }));

            // The first probe returns before any external driver. A mutant's second dispatch hits this
            // sentinel, so even a duplicate start cannot escape to a real CLI/TUI process.
            let additional = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let duplicate = additional.clone();
            *CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((channel.get(),Box::new(move || {
                duplicate.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                Err("duplicate provider dispatch sentinel".into())
            })));
            let (capture,prompt) = tokio::sync::oneshot::channel();
            *INPUT_PROMPT_PROBE.lock().unwrap() = Some((channel.get(),capture));
            let (entered,provider_entered) = tokio::sync::oneshot::channel();
            let (release_provider,provider_release) = std::sync::mpsc::channel();
            *INPUT_EFFECT_PROBE.lock().unwrap() = Some(InputEffectProbe {
                channel:channel.get(),entered,release:provider_release,terminal_before_release:false,
            });
            let (completed,bridge_completed) = tokio::sync::oneshot::channel();
            *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE.lock().unwrap()
                = Some((channel,completed));

            let worker = Box::pin(run_intake_worker_tick(&host.pool,&host.shared,WORKER,"claude","race-worker",&|| false));
            let leader_entry = Box::pin(async {
                at_spawn.await.unwrap();
                let admitted = admit_text_intake(&deps,&submission).await;
                assert!(matches!(&admitted,IntakeAdmission::Local(_)),"the actual leader entry must admit fresh input before beginning: {admitted:?}");
                finish_text_intake_admission(&deps,admitted,submission).await
            });
            let unlock = async {
                // Leader reaches fresh registration first. Worker then enters while the receipt is
                // still absent, and both block before any committed begin or saved inflight owner.
                waiting_for_registration(&host.pool,1).await;
                release_worker.send(()).unwrap();
                waiting_for_registration(&host.pool,2).await;
                let row: (String,bool) = sqlx::query_as("SELECT status::TEXT,completed_at IS NULL FROM intake_outbox WHERE id=$1")
                    .bind(id).fetch_one(&host.pool).await.unwrap();
                assert_eq!(row,("spawned".into(),true));
                blocker.commit().await.unwrap();
            };
            let (worker_result,leader_result,()) = tokio::time::timeout(Duration::from_secs(60),async {
                tokio::join!(worker,leader_entry,unlock)
            }).await.unwrap();
            worker_result.unwrap();
            leader_result.unwrap();
            tokio::time::timeout(Duration::from_secs(10),provider_entered).await.unwrap().unwrap();
            let _submitted = tokio::time::timeout(Duration::from_secs(10),prompt).await.unwrap().unwrap();
            // Registration releases its advisory lock before the separate begin transaction. Either
            // contender can win begin; the still-live inflight row is the observed winner's identity.
            let active = crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude,channel.get())
                .expect("the provider is held before terminal output so its inflight identity is still live");
            assert!(active.intake_outbox_id().is_none() || active.intake_outbox_id() == Some(id));
            let worker_won = active.intake_outbox_id() == Some(id);
            assert_eq!(additional.load(std::sync::atomic::Ordering::SeqCst),0,"a second fresh preflight cannot issue another provider start permit");
            let stored: Vec<(String,String,Vec<String>)> = sqlx::query_as(
                "SELECT replay_disposition,replay_episode_nonce,replay_source_message_ids FROM intake_outbox WHERE replay_request_key=$1",
            ).bind(&key).fetch_all(&host.pool).await.unwrap();
            assert_eq!(stored.len(),1,"concurrent actual entries share one canonical receipt");
            assert_eq!(stored[0].0,"started_unclassified");
            assert!(!stored[0].1.is_empty());
            assert_eq!(stored[0].2,sources.clone().unwrap());
            if worker_won {
                let row: (String,bool,String,Option<Vec<String>>,serde_json::Value) = sqlx::query_as(
                    "SELECT status::TEXT,completed_at IS NOT NULL,user_text,replay_source_message_ids,attachment_refs FROM intake_outbox WHERE id=$1",
                ).bind(id).fetch_one(&host.pool).await.unwrap();
                assert_eq!(row,("done".into(),true,BODY.into(),sources.clone(),refs.clone()),"the worker winner performed the actual provider dispatch");
                let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE parent_outbox_id=$1")
                    .bind(id).fetch_one(&host.pool).await.unwrap();
                assert_eq!(children,0);
            } else {
                assert_custody(&host,id,sources.clone(),refs.clone()).await;
            }
            let _=release_provider.send(());
            tokio::time::timeout(Duration::from_secs(20),bridge_completed).await.unwrap().unwrap();
            assert_eq!(additional.load(std::sync::atomic::Ordering::SeqCst),0);
            CLAUDE_LEGACY_PROBE.lock().unwrap().take();
            drop(_hook);
            assert!(tick(&host,channel,"replacement-owner").await.is_none());
            if !worker_won { assert_custody(&host,id,sources,refs).await; }
            host.pool.close().await;
            host.db.drop().await;
        }));
    }
}

struct Host {
    api: Recorder,
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    db: TestPostgresDb,
    _workspace: tempfile::TempDir,
    _root: crate::config::TestRuntimeRootGuard,
}

async fn host(channels: &[ChannelId]) -> Host {
    host_with(channels, Arc::new(|_: &axum::http::Method, _: &str| None)).await
}

/// How the runtime API stand-in answers one request ahead of its default body.
type Answer = Arc<dyn Fn(&axum::http::Method, &str) -> Option<serde_json::Value> + Send + Sync>;

async fn host_with(channels: &[ChannelId], answer: Answer) -> Host {
    let root = crate::config::TestRuntimeRootGuard::new();
    let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
    crate::services::tui_o::channel_policy::install(&boot).unwrap();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let api = Recorder::start_with(answer).await;
    crate::services::discord::internal_api::init(api.port, None);
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    let workspace = tempfile::tempdir().unwrap();
    for &channel in channels {
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some(format!("replay-start-{channel}"));
        session.current_path = Some(workspace.path().display().to_string());
        session.session_id = Some(STALE.into());
    }
    Host {
        api,
        shared,
        pool,
        db,
        _workspace: workspace,
        _root: root,
    }
}

fn request(channel: ChannelId, message: u64, text: &str) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: MessageId::new(message),
        source_message_ids: vec![MessageId::new(message + 1)],
        busy_followup_retry_user_msg_id: MessageId::new(message),
        request_owner: UserId::new(7),
        request_owner_name: "owner".into(),
        user_text: text.into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: false,
    }
}

/// Runs `entry`; returns what the provider received and the sender that releases it, or `None`
/// when nothing reached the provider.
async fn start<T>(
    channel: ChannelId,
    entry: impl std::future::Future<Output = T>,
) -> (T, Option<(SubmittedPrompt, std::sync::mpsc::Sender<()>)>) {
    let (capture, prompt) = tokio::sync::oneshot::channel();
    *INPUT_PROMPT_PROBE.lock().unwrap() = Some((channel.get(), capture));
    let (entered, _entered) = tokio::sync::oneshot::channel();
    let (release_provider, release) = std::sync::mpsc::channel();
    *INPUT_EFFECT_PROBE.lock().unwrap() = Some(InputEffectProbe {
        channel: channel.get(),
        entered,
        release,
        terminal_before_release: true,
    });
    let output = tokio::time::timeout(Duration::from_secs(60), entry)
        .await
        .unwrap();
    let submitted = tokio::time::timeout(Duration::from_secs(3), prompt).await;
    INPUT_PROMPT_PROBE.lock().unwrap().take();
    let started = submitted.ok().and_then(Result::ok);
    if started.is_none() {
        INPUT_EFFECT_PROBE.lock().unwrap().take();
    }
    (output, started.map(|prompt| (prompt, release_provider)))
}

async fn run(host: &Host, request: IntakeRequest) -> Option<SubmittedPrompt> {
    let channel = request.channel_id;
    let entry = execute_intake_turn_core(&host.api.http, &host.shared, "", request, Vec::new());
    let (result, started) = start(channel, entry).await;
    result.unwrap();
    started.map(|(prompt, release)| {
        let _ = release.send(());
        prompt
    })
}

/// The receipt stored under the Discord request key, and its current attempt and projection.
async fn receipt(host: &Host, channel: ChannelId, message: u64) -> Option<(i64, String, String)> {
    let key = format!("discord:claude:{channel}:{message}");
    sqlx::query_as(
        "SELECT id, replay_disposition, replay_episode_nonce FROM intake_outbox
          WHERE replay_request_key = $1",
    )
    .bind(key)
    .fetch_optional(&host.pool)
    .await
    .unwrap()
}

async fn session_key(host: &Host, channel: ChannelId) -> String {
    crate::services::discord::adk_session::build_adk_session_key(
        &host.shared,
        channel,
        &ProviderKind::Claude,
        None,
    )
    .await
    .expect("session key")
}

async fn seed_session(host: &Host, key: &str, channel: &str, bound: Option<(i64, &str)>) {
    sqlx::query(
        "INSERT INTO sessions (session_key, channel_id, provider, status, claude_session_id,
                               current_replay_receipt_id, replay_episode_nonce)
         VALUES ($1, $2, 'claude', 'turn_active', $3, $4, $5)",
    )
    .bind(key)
    .bind(channel)
    .bind(STALE)
    .bind(bound.map(|(receipt, _)| receipt))
    .bind(bound.map(|(_, nonce)| nonce))
    .execute(&host.pool)
    .await
    .unwrap();
}

/// A started receipt of `key` over `sources`, its attempt and nonce, as an earlier start left it.
async fn started(
    host: &Host,
    key: String,
    channel: ChannelId,
    sources: Vec<String>,
    text: &str,
) -> (write::ExactAttempt, String) {
    let input = CanonicalInput {
        request_key: key,
        provider: "claude".into(),
        channel: channel.to_string(),
        sources,
        original_text: text.into(),
        owner_id: "7".into(),
        agent_id: String::new(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(&host.pool, &input, "node-a")
        .await
        .unwrap();
    let nonce = receipt.episode_nonce.unwrap();
    let binding = serde_json::json!({});
    let effect = EffectProjection {
        effect_target: "earlier-start",
        input_hash: "earlier",
        binding: &binding,
    };
    let from = BeginFrom::Registered { nonce: &nonce };
    let ack = write::begin(&host.pool, receipt.id, from, &effect, "incarnation-a").await;
    (ack.unwrap().into_parts().0, nonce)
}

async fn queued(host: &Host, channel: ChannelId) -> Vec<(u64, Vec<u64>)> {
    let snapshot = host.shared.mailbox(channel).snapshot().await;
    let ids = |item: &crate::services::discord::Intervention| {
        let sources = item.source_message_ids.iter().map(|id| id.get()).collect();
        (item.message_id.get(), sources)
    };
    snapshot.intervention_queue.iter().map(ids).collect()
}

// A dormant build starts as before: no receipt, the stored resume passed on. Active, the first
// input is made only after its receipt, projection and session binding committed.
#[test]
fn intake_start_begins_its_receipt_before_the_first_input_only_when_active_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "intake_start_begins_its_receipt_before_the_first_input_only_when_active_pg"
    )) {
        return;
    }
    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
        let (dormant, active) = (ChannelId::new(6_008_701), ChannelId::new(6_008_702));
        let host = host(&[dormant, active]).await;
        let submitted = run(&host, request(dormant, 6_008_711, "dormant start")).await;
        assert_eq!(submitted.unwrap().session_id.as_deref(), Some(STALE));
        assert_eq!(receipt(&host, dormant, 6_008_711).await, None);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
            .fetch_one(&host.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "a dormant start writes no replay row");

        let key = session_key(&host, active).await;
        seed_session(&host, &key, &active.to_string(), None).await;
        let _active = test_activation::activate(active.get());
        let entry = execute_intake_turn_core(
            &host.api.http,
            &host.shared,
            "",
            request(active, 6_008_721, "active start"),
            Vec::new(),
        );
        let (result, started) = start(active, entry).await;
        result.unwrap();
        let (submitted, release) = started.expect("the active start reaches the provider");
        let (id, disposition, nonce) = receipt(&host, active, 6_008_721).await.unwrap();
        assert_eq!(disposition, "started_unclassified");
        let projection: (String, String) = sqlx::query_as(
            "SELECT replay_preserved->'projection'->>'effect_target',
                    replay_preserved->'projection'->>'input_hash'
               FROM intake_outbox WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&host.pool)
        .await
        .unwrap();
        let target = EffectTarget::ProviderStart {
            provider: "claude".into(),
            channel: active.to_string(),
        };
        assert_eq!(projection, (target.key(), input_hash(&submitted.prompt)));
        let bound: (Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT current_replay_receipt_id, replay_episode_nonce FROM sessions
              WHERE session_key = $1",
        )
        .bind(&key)
        .fetch_one(&host.pool)
        .await
        .unwrap();
        assert_eq!(bound, (Some(id), Some(nonce)));
        let row = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Claude,
            active.get(),
        )
        .unwrap();
        assert_eq!(row.replay_receipt_id, Some(id));
        let token = host.shared.mailbox(active).snapshot().await.cancel_token;
        let observed = token.is_some_and(|token| token.replay_attempt().is_some());
        assert!(observed, "the started provider attempt has its live reader");
        let _ = release.send(());
        host.db.drop().await;
    });
}

// A held source is neither started nor requeued; an unacknowledged begin, a refused binding or
// another turn's row keep it queued without input, and a requeued maybe-begun request is consumed.
#[test]
fn undecided_or_held_intake_start_keeps_custody_without_input_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "undecided_or_held_intake_start_keeps_custody_without_input_pg"
    )) {
        return;
    }
    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
        let channels = [6_008_731, 6_008_732, 6_008_733, 6_008_734].map(ChannelId::new);
        let [held, lost, fenced, owned] = channels;
        let host = host(&channels).await;
        let _activated = channels.map(|channel| test_activation::activate(channel.get()));

        let sources = vec!["6008741".to_string(), "6008742".to_string()];
        started(&host, "earlier-held".into(), held, sources, "held").await;
        assert!(run(&host, request(held, 6_008_741, "held")).await.is_none());
        assert!(queued(&host, held).await.is_empty());
        let mailbox = host.shared.mailbox(held).snapshot().await;
        assert!(
            mailbox.cancel_token.is_none(),
            "the consumed start released its claim"
        );

        test_activation::lose_next_ack(lost.get());
        assert!(
            run(&host, request(lost, 6_008_751, "lost ack"))
                .await
                .is_none()
        );
        let (_, disposition, _) = receipt(&host, lost, 6_008_751).await.unwrap();
        assert_eq!(disposition, "started_unclassified");
        assert_eq!(
            queued(&host, lost).await,
            vec![(6_008_751, vec![6_008_752, 6_008_751])]
        );
        let persistence = crate::services::discord::queue_persistence_context(
            &host.shared,
            &ProviderKind::Claude,
            lost,
        );
        host.shared
            .mailbox(lost)
            .replace_queue(Vec::new(), persistence)
            .await;
        assert!(
            run(&host, request(lost, 6_008_751, "lost ack"))
                .await
                .is_none()
        );
        assert!(
            queued(&host, lost).await.is_empty(),
            "no permit is issued again"
        );

        let other = started(&host, "other".into(), fenced, vec!["1".into()], "other").await;
        let key = session_key(&host, fenced).await;
        seed_session(
            &host,
            &key,
            "elsewhere",
            Some((other.0.receipt_id(), &other.1)),
        )
        .await;
        assert!(
            run(&host, request(fenced, 6_008_761, "fenced"))
                .await
                .is_none()
        );
        let (_, disposition, _) = receipt(&host, fenced, 6_008_761).await.unwrap();
        assert_eq!(disposition, "registered_not_started");
        assert_eq!(
            queued(&host, fenced).await,
            vec![(6_008_761, vec![6_008_762, 6_008_761])]
        );

        // Another turn's inflight row still owns the channel: nothing is begun or replaced.
        let provider = ProviderKind::Claude;
        let row = InflightTurnState::new(
            provider.clone(),
            owned.get(),
            None,
            7,
            6_008_700,
            6_008_700,
            "other turn".into(),
            None,
            None,
            None,
            None,
            0,
        );
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        assert!(
            run(&host, request(owned, 6_008_765, "owned"))
                .await
                .is_none()
        );
        assert_eq!(receipt(&host, owned, 6_008_765).await, None);
        assert_eq!(
            queued(&host, owned).await,
            vec![(6_008_765, vec![6_008_766, 6_008_765])]
        );
        let kept = crate::services::discord::inflight::load_inflight_state(&provider, owned.get());
        assert_eq!(kept.map(|row| row.user_msg_id), Some(6_008_700));
        host.db.drop().await;
    });
}

// Only the exact continuation restarts a no-effect request: its next start drops the stale
// resume and moves receipt and session to the next nonce. Unacknowledged, nothing resets or runs.
#[test]
fn exact_retry_starts_only_through_its_continuation_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "exact_retry_starts_only_through_its_continuation_pg"
    )) {
        return;
    }
    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
        let channels = [6_008_771, 6_008_772].map(ChannelId::new);
        let host = host(&channels).await;
        let mut outcomes = Vec::new();
        for (channel, lose_ack) in channels.into_iter().zip([false, true]) {
            let _active = test_activation::activate(channel.get());
            let message = channel.get() + 100;
            let key = format!("discord:claude:{channel}:{message}");
            let sources = vec![message.to_string(), (message + 1).to_string()];
            let (attempt, nonce) = started(&host, key, channel, sources, "exact").await;
            let session = session_key(&host, channel).await;
            let receipt_id = attempt.receipt_id();
            seed_session(&host, &session, &channel.to_string(), Some((receipt_id, &nonce))).await;
            let mut observation = RawObservation::for_attempt(&attempt);
            for seen in [Seen::StdoutEof, Seen::Exited { waited: true }, Seen::StderrDrained] {
                observation.record(seen);
            }
            let evidence = observation.no_effect_evidence().unwrap();
            let ack = write::classify_no_effect(&host.pool, attempt, &evidence).await;
            let permit = RetryPermit::from_no_effect(ack.unwrap());
            let next = permit.next_nonce().to_string();
            if lose_ack {
                test_activation::lose_next_ack(channel.get());
            }
            let retry = ExactRetry {
                user_message_id: MessageId::new(message),
                sources: vec![MessageId::new(message + 1)],
                request_owner: UserId::new(7),
                text: "exact".into(),
                reply_context: None,
                permit,
            };
            let gateway = crate::services::discord::gateway::DiscordGateway::new(
                host.api.http.clone(),
                host.shared.clone(),
                ProviderKind::Claude,
                None,
            );
            let handoff = crate::services::discord::gateway::TurnGateway::schedule_exact_retry_with_history(
                &gateway, channel, retry,
            );
            let (handoff, started) = start(channel, handoff).await;
            let resume = host.shared.core.lock().await.sessions[&channel].session_id.clone();
            let stored: (String, String, Option<String>) = sqlx::query_as(
                "SELECT replay_disposition, replay_episode_nonce, replay_retry_of_nonce
                   FROM intake_outbox WHERE id = $1",
            )
            .bind(receipt_id)
            .fetch_one(&host.pool)
            .await
            .unwrap();
            let moved: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT claude_session_id, replay_episode_nonce FROM sessions WHERE session_key = $1",
            )
            .bind(&session)
            .fetch_one(&host.pool)
            .await
            .unwrap();
            let submitted = started.map(|(prompt, release)| {
                let _ = release.send(());
                prompt.session_id
            });
            assert_eq!(stored, ("started_unclassified".into(), next.clone(), Some(nonce)));
            assert_eq!(moved, (None, Some(next)));
            outcomes.push((handoff, submitted, resume));
        }
        assert_eq!(
            outcomes,
            vec![
                (ExactHandoff::Started, Some(None), None),
                (ExactHandoff::Refused, None, Some(STALE.into())),
            ]
        );
        host.db.drop().await;
    });
}

// A dispatch whose request is held stays with that hold: an active start carries it neither from
// its text nor from the thread's card link, while a dormant build reads nothing and keeps it.
#[test]
fn held_dispatch_is_not_inherited_by_an_active_start_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "held_dispatch_is_not_inherited_by_an_active_start_pg"
    )) {
        return;
    }
    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
        let (dormant, active) = (ChannelId::new(6_008_771), ChannelId::new(6_008_772));
        // Once set, the thread's card link answers with this dispatch, as for a card thread.
        let linked = Arc::new(std::sync::Mutex::new(None::<String>));
        let thread = linked.clone();
        let answer = Arc::new(move |_: &axum::http::Method, path: &str| {
            let lookup = path == "/api/internal/pending-dispatch-for-thread";
            let linked = thread.lock().unwrap().clone().filter(|_| lookup)?;
            Some(serde_json::json!({ "dispatch_id": linked }))
        });
        let host = host_with(&[dormant, active], answer).await;
        for channel in [dormant, active] {
            let key = format!("held-dispatch-{channel}");
            let sources = vec![channel.to_string()];
            let (attempt, _) = started(&host, key, channel, sources, "held work").await;
            let preserved = serde_json::json!({ "hold": "test" });
            write::withhold(&host.pool, &attempt, "held", &preserved)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO task_dispatches (id, status, replay_receipt_id)
                 VALUES ($1, 'dispatched', $2)",
            )
            .bind(format!("held-{channel}"))
            .bind(attempt.receipt_id())
            .execute(&host.pool)
            .await
            .unwrap();
            let key = session_key(&host, channel).await;
            seed_session(&host, &key, &channel.to_string(), None).await;
        }
        let blocked: bool = sqlx::query_scalar("SELECT replay_dispatch_blocked($1)")
            .bind(format!("held-{active}"))
            .fetch_one(&host.pool)
            .await
            .unwrap();
        assert!(blocked, "the fixture's dispatch is held");

        let text = format!("DISPATCH: held-{dormant}\nwork");
        let started = run(&host, request(dormant, 6_008_781, &text)).await;
        assert!(started.is_some(), "the dormant start runs");
        let _active = test_activation::activate(active.get());
        *linked.lock().unwrap() = Some(format!("held-{active}"));
        let text = format!("DISPATCH: held-{active}\nwork");
        let started = run(&host, request(active, 6_008_791, &text)).await;
        assert!(
            started.is_some(),
            "the active start runs without the dispatch"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let calls = host.api.take();
        let carried = |channel: ChannelId| {
            let lookup = format!("GET /api/dispatches/held-{channel} ");
            let field = format!("\"dispatch_id\":\"held-{channel}\"");
            let carried = |call: &String| call.starts_with(&lookup) || call.contains(&field);
            calls.iter().any(carried)
        };
        assert!(carried(dormant), "dormant keeps the dispatch: {calls:?}");
        assert!(!carried(active), "{calls:?}");
        // The prompt's turn records name the dispatch it carries.
        let recorded = |channel: ChannelId| {
            let query = "SELECT COUNT(*) FROM turn_lifecycle_events WHERE dispatch_id = $1";
            sqlx::query_scalar::<_, i64>(query)
                .bind(format!("held-{channel}"))
                .fetch_one(&host.pool)
        };
        assert!(
            recorded(dormant).await.unwrap() > 0,
            "dormant prompt carries it"
        );
        assert_eq!(recorded(active).await.unwrap(), 0);
        host.pool.close().await;
        host.db.drop().await;
    });
}
