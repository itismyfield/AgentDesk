#![cfg(unix)]

use super::{IntakeAdmission, IntakeOrigin, IntakeSubmission, admit_text_intake};
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::codex::CodexSourceMode;
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::absent_launch_tests::{auxiliary_tests, custody_tests};
use crate::services::discord::router::TurnKind;
use crate::services::discord::router::message_handler::{IntakeDeps, IntakeRequest};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover::{self, intake_route, test_override};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::host::{self, HostParts, test_io::TestHost};
use crate::services::tui_prompt_dedupe as dedupe;
use chrono::Utc;
use futures::FutureExt;
use poise::serenity_prelude as serenity;
use std::sync::Arc;
use std::time::Duration;

impl IntakeSubmission {
    pub(in crate::services::discord) fn text_for_tests(
        provider: ProviderKind,
        request: IntakeRequest,
        origin: IntakeOrigin,
    ) -> Self {
        Self {
            provider,
            request,
            origin,
            preserve_on_cancel: false,
            has_nonportable_uploads: false,
            attachments: Vec::new(),
            preloaded_uploads: Vec::new(),
            voice_announcement: None,
        }
    }
}

impl IntakeAdmission {
    pub(in crate::services::discord) fn forwarded_for_tests(&self) -> Option<(&str, i64)> {
        match self {
            Self::Forwarded {
                target_instance_id,
                outbox_id,
            } => Some((target_instance_id, *outbox_id)),
            _ => None,
        }
    }
}

fn input() -> IntakeSubmission {
    let message = serenity::MessageId::new(CANARY_CHANNEL + 7163);
    IntakeSubmission {
        provider: ProviderKind::Codex,
        request: IntakeRequest {
            intake_outbox_id: None,
            channel_id: serenity::ChannelId::new(CANARY_CHANNEL),
            user_msg_id: message,
            source_message_ids: vec![message],
            busy_followup_retry_user_msg_id: message,
            request_owner: serenity::UserId::new(7163),
            request_owner_name: "initial-fresh-owner".into(),
            user_text: "preserved first input before the writer resumes".into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: TurnKind::Foreground,
            preserve_on_cancel: false,
        },
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: false,
        has_nonportable_uploads: false,
        attachments: Vec::new(),
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn historical_verified_empty_store_admits_input_before_source_resume_pg() {
    admission_case(Case::Admission).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_probe_does_not_block_the_current_thread_runtime_pg() {
    admission_case(Case::Progress).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_worker_discovers_and_checks_a_claimed_input_pg() {
    admission_case(Case::Worker(WorkerCase::Ready)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_worker_rechecks_absence_after_claim_pg() {
    admission_case(Case::Worker(WorkerCase::PaneAppeared)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_worker_rechecks_absence_before_final_accept_pg() {
    admission_case(Case::Worker(WorkerCase::FinalPaneAppeared)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_worker_preserves_a_claim_taken_by_another_owner_pg() {
    admission_case(Case::Worker(WorkerCase::ClaimChanged)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_worker_holds_an_unproved_successor_after_claim_pg() {
    admission_case(Case::Worker(WorkerCase::UnprovedFirst)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_worker_holds_an_unproved_successor_before_final_accept_pg() {
    admission_case(Case::Worker(WorkerCase::UnprovedFinal)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_intake_requires_exact_absence_and_a_live_owned_writer_pg() {
    for refusal in [
        Refusal::PanePresent,
        Refusal::ProbeFailed,
        Refusal::ForeignNonce,
        Refusal::BlankNonce,
        Refusal::ForeignOwnership,
        Refusal::NullOwnership,
        Refusal::MarkerIo,
        Refusal::InflightRow,
        Refusal::WrongProvider,
        Refusal::Lost,
        Refusal::Uncertain,
        Refusal::SubmissionMissing,
        Refusal::Stopping,
        Refusal::HostMissing,
        Refusal::UnprovedSuccessor,
        Refusal::CustodyPending,
        Refusal::CustodyUnreadable,
        Refusal::LegacyReceiptCustody,
    ] {
        admission_case(Case::Negative(refusal)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn initial_fresh_reason_observation_preserves_directory_modes_pg() {
    admission_case(Case::PureReason).await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_clear_reason_observation_preserves_directory_modes_pg() {
    admission_case(Case::PureClearReason).await;
}

#[tokio::test(flavor = "current_thread")]
async fn retained_cursor_without_checkpoint_keeps_the_actual_host_pre_live_pg() {
    admission_case(Case::Metadata(Metadata::Missing)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn retained_cursor_with_unreadable_checkpoint_keeps_the_actual_host_pre_live_pg() {
    admission_case(Case::Metadata(Metadata::Unreadable)).await;
}

#[derive(Clone, Copy)]
enum Case {
    Admission,
    Progress,
    Worker(WorkerCase),
    Negative(Refusal),
    PureReason,
    PureClearReason,
    Metadata(Metadata),
}

#[derive(Clone, Copy)]
enum Metadata {
    Missing,
    Unreadable,
}

#[derive(Clone, Copy, Debug)]
enum Refusal {
    PanePresent,
    ProbeFailed,
    ForeignNonce,
    BlankNonce,
    ForeignOwnership,
    NullOwnership,
    MarkerIo,
    InflightRow,
    WrongProvider,
    Lost,
    Uncertain,
    SubmissionMissing,
    Stopping,
    HostMissing,
    UnprovedSuccessor,
    CustodyPending,
    CustodyUnreadable,
    LegacyReceiptCustody,
}

#[derive(Clone, Copy)]
enum WorkerCase {
    Ready,
    PaneAppeared,
    FinalPaneAppeared,
    ClaimChanged,
    UnprovedFirst,
    UnprovedFinal,
}

async fn admission_case(case: Case) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1b-input-host")).await;
    let _mode = fixture.scoped_test_env("ADK_INTAKE_ROUTING_MODE", "disabled".as_ref());
    let _writer = fixture.scoped_test_env(test_override::CHILD_ENV, "1".as_ref());
    let channels = serde_json::to_string(&[(CANARY_CHANNEL, CodexTui)]).unwrap();
    let _channels = fixture.scoped_test_env(test_override::CHANNELS_ENV, channels.as_ref());
    let _selected = test_override::force_channels(&[(CANARY_CHANNEL, CodexTui)]);
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let _cli = fixture.cold_cli();
    assert!(crate::services::codex::successor_hooks_ready());
    fixture.fake.enable_fixture_launch();
    let custody = matches!(
        case,
        Case::Negative(Refusal::CustodyPending | Refusal::LegacyReceiptCustody)
    );
    if custody {
        auxiliary_tests::target_missing(&fixture, false);
        let observed =
            crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
                .unwrap();
        assert!(dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observed).is_some());
        let (local, range) = custody_tests::admitted_terminal(&fixture, true).await;
        custody_tests::retain_after_successor(&fixture, &local, &range).await;
    }
    auxiliary_tests::target_missing(&fixture, true);
    crate::services::platform::tmux::invalidate_availability_cache();

    let observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
            .expect("the initial placement has no inflight row");
    if !custody {
        assert!(
            dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation).is_some(),
            "the existing B1 absence predicate must accept this fixture"
        );
    }
    assert!(!dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));

    if matches!(
        case,
        Case::PureClearReason | Case::Negative(Refusal::LegacyReceiptCustody)
    ) {
        use crate::services::discord::commands::control::{
            SoftClearNotifyMode, clear_channel_session_state,
        };
        let http = Arc::new(serenity::Http::new("Bot initial-fresh-test"));
        let cleared = clear_channel_session_state(
            &http,
            &fixture.shared,
            &ProviderKind::Codex,
            serenity::ChannelId::new(CANARY_CHANNEL),
            "!clear",
            SoftClearNotifyMode::Suppress,
        )
        .await;
        cleared.expect("the actual clear rail must succeed");
        assert!(dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
        if matches!(case, Case::Negative(Refusal::LegacyReceiptCustody)) {
            let path = crate::config::runtime_root()
                .unwrap()
                .join("runtime/codex_clear_retired")
                .join(format!("{CANARY_CHANNEL}.json"));
            let mut mark: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            mark["origin"] = serde_json::json!("legacy_clear_boundary");
            std::fs::write(path, serde_json::to_vec(&mark).unwrap()).unwrap();
        }
    }
    if matches!(case, Case::PureReason | Case::PureClearReason) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            crate::services::tmux_common::persistent_sessions_dir().unwrap(),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        let before = directory_snapshot(fixture.root.path());
        for _ in 0..3 {
            let reason = dedupe::clear_relaunch::input_reason_for_channel(CANARY_CHANNEL)
                .expect("a genuine initial fresh reason is required");
            let name = format!("{reason:?}");
            assert!(name.starts_with(if matches!(case, Case::PureClearReason) {
                "ExplicitClear("
            } else {
                "ExactAbsence("
            }));
            assert!(!dedupe::clear_relaunch::successor_unproved_for_channel(
                CANARY_CHANNEL
            ));
        }
        assert_eq!(
            directory_snapshot(fixture.root.path()),
            before,
            "reason observation may not create files, change modes, or advance file sizes"
        );
    }

    let preserved = if matches!(case, Case::Negative(_) | Case::Metadata(_)) {
        let mut held =
            auxiliary_tests::instruction(CANARY_CHANNEL + 7165, "restored payload held intact");
        held.source_message_queued_generations = vec![
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                held.message_id,
                held.queued_generation,
            ),
        ];
        held.source_text_segments = vec![
            crate::services::turn_orchestrator::SourceMessageTextSegment::new(
                held.message_id,
                held.text.clone(),
            ),
        ];
        held.reply_context = Some("preserved restored reply context".into());
        held.has_reply_boundary = true;
        let identities = auxiliary_tests::restore_instructions(&mut fixture, vec![held]).await;
        Some((
            identities,
            queue_bytes(&fixture),
            queue_payload(&fixture).await,
        ))
    } else {
        None
    };

    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[CANARY_CHANNEL], Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: Vec::new(),
                initial_anchor: 0,
                build_digest: "initial-fresh-intake".into(),
                at: Utc::now(),
            })
        })
        .unwrap();
    let metadata = if let Case::Metadata(kind) = case {
        let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
        let cursor = channel.attach_source(&fixture.old_source).unwrap();
        assert!(channel.binding_checkpoint().unwrap().is_none());
        if matches!(kind, Metadata::Unreadable) {
            let path = fixture
                .root
                .path()
                .join("o_store")
                .join(CANARY_CHANNEL.to_string())
                .join(crate::services::tui_o::store::rotation::CHECKPOINT_FILE);
            std::fs::write(path, "{torn checkpoint").unwrap();
            assert!(channel.binding_checkpoint().is_err());
        }
        Some((
            cursor,
            std::fs::read(&fixture.old_source.path).unwrap(),
            crate::services::codex::actual_submit_tests::source_io::Watch::start(
                &fixture.old_source.path,
            ),
        ))
    } else {
        None
    };
    let io = TestHost::new([(CANARY_CHANNEL, fixture.old_source.clone())]);
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let readiness = host::process_readiness();
    let mut hosts = host::start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(fixture.root.path().to_owned()),
            gate: Arc::clone(&gate),
            readiness: Arc::clone(&readiness),
        },
    );
    assert_eq!(
        hosts.len(),
        1,
        "the selected channel must start its real host"
    );
    let run = std::panic::AssertUnwindSafe(async {
    if let Some((cursor, bytes, old_io)) = metadata {
        tokio::time::timeout(Duration::from_secs(5), async {
            while hosts[0].live() && readiness.submission(CANARY_CHANNEL).is_none() {
                tokio::task::yield_now().await;
            }
        }).await.expect("the real host must either refuse metadata or publish its actor");
        let pre_live = readiness.submission(CANARY_CHANNEL).is_none();
        let finished = !hosts[0].live();
        fixture.fake.take_calls();
        let _ = crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
        let http = Arc::new(serenity::Http::new("Bot initial-fresh-test"));
        let deps = IntakeDeps { http: &http, cache: None, ctx_for_chained_dispatch: None,
            shared: &fixture.shared, token: "Bot initial-fresh-test" };
        let admission = admit_text_intake(&deps, &input()).await;
        hosts.pop().unwrap().stop_and_join().await.unwrap();
        assert!(matches!(admission, IntakeAdmission::Blocked { .. }),
            "invalid recovered metadata may not grant initial input: {admission:?}");
        assert!(finished && pre_live, "the actual metadata guard must finish before publishing Live");
        assert!(readiness.submission(CANARY_CHANNEL).is_none());
        assert!(io.posts.to(CANARY_CHANNEL).is_empty());
        let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
        assert_eq!(channel.cursor(&fixture.old_source), Some(&cursor));
        assert_eq!(std::fs::read(&fixture.old_source.path).unwrap(), bytes);
        assert_eq!(old_io.stats(), Default::default());
        let (identities, raw, payload) = preserved.unwrap();
        assert_eq!(auxiliary_tests::queue_identities(&fixture).await, identities);
        assert_eq!(queue_bytes(&fixture), raw);
        assert_eq!(queue_payload(&fixture).await, payload);
        assert!(crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX).is_empty());
        assert!(!fixture.fake.take_calls().iter().any(|call| {
            ["new-session", "kill-session", "send-keys", "paste-buffer"].iter().any(|effect| call.starts_with(effect))
        }));
        return;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while readiness.submission(CANARY_CHANNEL).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the store and port must reach a real live actor");
    assert!(readiness.is_hosted(CANARY_CHANNEL));
    assert!(!readiness.is_ready(CANARY_CHANNEL));
    assert!(!readiness.accepts(CANARY_CHANNEL));
    assert!(matches!(
        intake_route::route_for_placement("codex", CANARY_CHANNEL),
        intake_route::IntakeRoute::Hold(_)
    ));

    let http = Arc::new(serenity::Http::new("Bot initial-fresh-test"));
    let deps = IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "Bot initial-fresh-test",
    };
    let mut submission = input();
    let mut stop = None;
    let mut row = None;
    if let Case::Negative(refusal) = case {
        use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
        let nonce_path =
            crate::services::tmux_common::session_temp_path(CANARY_TMUX, "spawn_nonce");
        let ownership_path = crate::services::tmux_common::session_temp_path(
            CANARY_TMUX,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        );
        assert!(host::initial_fresh_refusal(CANARY_CHANNEL).is_none());
        if !custody {
            assert!(dedupe::clear_relaunch::input_reason_for_channel(CANARY_CHANNEL).is_some());
        }
        match refusal {
            Refusal::PanePresent => fixture.fake.mark_present(),
            Refusal::ProbeFailed => fixture.fake.fail_probes(),
            Refusal::ForeignNonce => std::fs::write(&nonce_path, uuid::Uuid::new_v4().simple().to_string()).unwrap(),
            Refusal::BlankNonce => std::fs::write(&nonce_path, "").unwrap(),
            Refusal::ForeignOwnership => std::fs::write(&ownership_path, serde_json::to_vec(&serde_json::json!({
                "codex_ownership": {"execution_nonce":uuid::Uuid::new_v4().simple().to_string(), "proof_seq":1,"dev":1,"ino":1}
            })).unwrap()).unwrap(),
            Refusal::NullOwnership => std::fs::write(&ownership_path, br#"{"codex_ownership":null}"#).unwrap(),
            Refusal::MarkerIo => std::fs::create_dir(&ownership_path).unwrap(),
            Refusal::InflightRow => {
                let token = Arc::new(crate::services::discord::CancelToken::new());
                let path = crate::services::discord::save_actual_episode_for_tests(
                    CANARY_CHANNEL, CANARY_TMUX, CANARY_CHANNEL + 7180, &token,
                );
                row = Some((path.clone(), std::fs::read(path).unwrap(), token));
                assert!(crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX).is_none());
            }
            Refusal::WrongProvider => submission.provider = ProviderKind::Claude,
            Refusal::Lost => {
                gate.lost();
            }
            Refusal::Uncertain => {
                gate.uncertain();
            }
            Refusal::SubmissionMissing => {
                readiness.drop_submission_for_tests(CANARY_CHANNEL);
            }
            Refusal::Stopping => {
                stop = Some(hosts.pop().unwrap().stop_and_join());
            }
            Refusal::HostMissing => {
                hosts.pop().unwrap().stop_and_join().await.unwrap();
            }
            Refusal::UnprovedSuccessor => {
                let mut context = fixture.context.clone();
                context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
                context.created_at = Utc::now() + chrono::Duration::seconds(2);
                let prepared = PreparedIncarnation::create(context).unwrap();
                crate::services::discord::stamp_spawn_markers(CANARY_TMUX, Some(&prepared)).unwrap();
                assert!(dedupe::clear_relaunch::successor_unproved_for_channel(CANARY_CHANNEL));
                assert_eq!(crate::services::platform::tmux::session_presence(CANARY_TMUX),
                    crate::services::platform::tmux::SessionPresence::Missing);
            }
            Refusal::CustodyPending | Refusal::LegacyReceiptCustody => {
                assert!(crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX).is_some());
                assert_eq!(dedupe::clear_relaunch::input_hold_reason_for_channel(CANARY_CHANNEL), "absence_custody_pending");
            }
            Refusal::CustodyUnreadable => {
                let root = fixture.root.path().join("runtime/discord_terminal_delivery_custody");
                std::fs::create_dir_all(&root).unwrap();
                std::fs::write(root.join("malformed.json"), "{torn").unwrap();
                assert_eq!(dedupe::clear_relaunch::input_hold_reason_for_channel(CANARY_CHANNEL), "absence_custody_pending");
            }
        }
    }
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(
        &fixture.old_source.path,
    );
    fixture.fake.take_calls();
    let _ = crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    let async_thread = std::thread::current().id();
    let progress = matches!(case, Case::Progress).then(|| {
        let progressed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (entered, entering) = std::sync::mpsc::channel();
        let (scheduled, scheduling) = tokio::sync::oneshot::channel();
        let (release, releasing) = std::sync::mpsc::channel();
        let guard = intake_route::install_probe(intake_route::InputProbe {
            channel: CANARY_CHANNEL,
            entered,
            scheduled,
            release: releasing,
        });
        let flag = Arc::clone(&progressed);
        let sibling = tokio::spawn(async move {
            scheduling.await.expect("the actual O input probe must run");
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let external = std::thread::spawn(move || {
            let thread = entering
                .recv_timeout(Duration::from_secs(5))
                .expect("the input probe must enter");
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !progressed.load(std::sync::atomic::Ordering::SeqCst)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            let advanced = progressed.load(std::sync::atomic::Ordering::SeqCst);
            release.send(()).unwrap();
            (thread, advanced)
        });
        (guard, sibling, external)
    });
    let live_refusal = host::initial_fresh_refusal(CANARY_CHANNEL);
    let admission = admit_text_intake(&deps, &submission).await;
    if let Case::Worker(case) = case {
        worker_case(&fixture, case).await;
    }
    assert!(io.posts.to(CANARY_CHANNEL).is_empty());
    if let Some(host) = hosts.pop() {
        host.stop_and_join().await.unwrap();
    }
    if let Some(stop) = stop {
        stop.await.unwrap();
    }
    if let Case::Negative(refusal) = case {
        assert!(
            matches!(admission, IntakeAdmission::Blocked { .. }),
            "{refusal:?}: a false-resumed actor must hold actual input: {admission:?}"
        );
        let expected = match refusal {
            Refusal::Lost | Refusal::Uncertain => Some("ownership_not_owned"),
            Refusal::SubmissionMissing => Some("writer_sources_unready"),
            Refusal::Stopping => Some("stopping"),
            Refusal::HostMissing => Some("host_missing"),
            _ => None,
        };
        if let Some(expected) = expected {
            assert_eq!(live_refusal, Some(expected), "{refusal:?}");
        }
        let (identities, raw, payload) = preserved.unwrap();
        assert_eq!(
            auxiliary_tests::queue_identities(&fixture).await,
            identities,
            "{refusal:?}"
        );
        assert_eq!(
            queue_bytes(&fixture),
            raw,
            "{refusal:?}: persisted queue changed"
        );
        assert_eq!(
            queue_payload(&fixture).await,
            payload,
            "{refusal:?}: restored payload changed"
        );
        let stats = old_io.stats();
        assert_eq!(
            (stats.read_calls, stats.read_bytes, stats.advance_commits),
            (0, 0, 0),
            "{refusal:?}: intake may not consume the historical body"
        );
        assert!(
            crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX)
                .is_empty()
        );
        assert!(
            !fixture.fake.take_calls().iter().any(|call| {
                ["new-session", "kill-session", "send-keys", "paste-buffer"]
                    .iter()
                    .any(|effect| call.starts_with(effect))
            }),
            "{refusal:?}: provider effects must remain zero"
        );
        assert!(
            !fixture.root.path().join("fresh-argv").exists(),
            "{refusal:?}"
        );
        if let Some((path, bytes, _token)) = row {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        return;
    }
    if let Some((_guard, sibling, external)) = progress {
        sibling.await.unwrap();
        let (probe_thread, advanced_before_release) = external.join().unwrap();
        assert!(
            advanced_before_release,
            "another task must progress before the external thread releases the sync probe"
        );
        assert_ne!(
            probe_thread, async_thread,
            "the sync probe must leave the async task"
        );
    }
    assert!(
        matches!(admission, IntakeAdmission::Local(_)),
        "an existing false-resumed O actor with exact B1 absence must admit input: {admission:?}"
    );
    }).catch_unwind().await;
    for host in hosts {
        let _ = host.stop_and_join().await;
    }
    if let Err(panic) = run {
        std::panic::resume_unwind(panic);
    }
}

fn queue_bytes(fixture: &CanaryFixture) -> Vec<u8> {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("codex")
        .join(&fixture.shared.token_hash)
        .join(format!("{CANARY_CHANNEL}.json"));
    std::fs::read(path).unwrap()
}

async fn queue_payload(fixture: &CanaryFixture) -> Vec<(std::time::Instant, serde_json::Value)> {
    fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .intervention_queue
        .iter()
        .map(|item| {
            (
                item.created_at,
                serde_json::json!({
                    "author":item.author_id.get(), "bot":item.author_is_bot,
                    "message":item.message_id.get(), "generation":item.queued_generation,
                    "sources":item.source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
                    "source_generations":format!("{:?}",item.source_message_queued_generations),
                    "source_text":format!("{:?}",item.source_text_segments),
                    "text":item.text,"mode":format!("{:?}",item.mode),
                    "reply":item.reply_context,"reply_boundary":item.has_reply_boundary,
                    "merge":item.merge_consecutive,"uploads":item.pending_uploads,
                    "voice":item.voice_announcement,
                }),
            )
        })
        .collect()
}

fn directory_snapshot(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, (u32, u64)> {
    use std::os::unix::fs::PermissionsExt;
    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        entries: &mut std::collections::BTreeMap<std::path::PathBuf, (u32, u64)>,
    ) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        entries.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            (metadata.permissions().mode(), metadata.len()),
        );
        if metadata.is_dir() {
            for child in std::fs::read_dir(path).unwrap() {
                visit(root, &child.unwrap().path(), entries);
            }
        }
    }
    let mut entries = std::collections::BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

async fn worker_case(fixture: &CanaryFixture, case: WorkerCase) {
    use crate::db::intake_outbox::{InsertPendingPayload, insert_pending};
    use crate::services::cluster::intake_worker::{self, test_executor};

    let pool = fixture.shared.pg_pool.as_ref().unwrap();
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    let mut worker = crate::services::discord::make_shared_data_for_tests();
    {
        let shared = Arc::get_mut(&mut worker).unwrap();
        shared.provider = ProviderKind::Codex;
        shared.health_registry = Arc::downgrade(&registry);
    }
    {
        let mut settings = worker.settings.write().await;
        settings.provider = ProviderKind::Codex;
        settings.allowed_channel_ids.extend([CANARY_CHANNEL]);
    }
    worker
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    registry.register("codex".into(), Arc::clone(&worker)).await;
    let runtime = crate::services::discord::health::resolve_intake_worker_runtime(
        &worker,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .await
    .expect("the real worker resolver must select the registered Codex channel owner");
    assert!(Arc::ptr_eq(&runtime.shared, &worker));
    assert_eq!(runtime.token, "test-token");
    sqlx::query(
        "INSERT INTO agents (id, name, provider) VALUES ('b1b-input-worker', 'Worker', 'codex')",
    )
    .execute(pool)
    .await
    .unwrap();
    let payload = InsertPendingPayload {
        target_instance_id: "b1b-worker".into(),
        forwarded_by_instance_id: "b1b-gateway".into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: CANARY_CHANNEL.to_string(),
        user_msg_id: (CANARY_CHANNEL + 7164).to_string(),
        replay_source_message_ids: None,
        request_owner_id: "7163".into(),
        request_owner_name: Some("initial-fresh-owner".into()),
        user_text: "restored worker input remains in its exact claim".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "b1b-input-worker".into(),
        provider: "codex".into(),
        home_epoch: None,
    };
    let parent = insert_pending(pool, &payload, 1, None).await.unwrap();
    let claimed = crate::db::intake_outbox::claim_pending_for_target(
        pool,
        "b1b-worker",
        "codex",
        "parent-fixture",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, parent);
    assert!(
        crate::db::intake_outbox::mark_failed_pre_accept(
            pool,
            parent,
            "parent-fixture",
            "retry fixture",
        )
        .await
        .unwrap()
    );
    let id = insert_pending(pool, &payload, 2, Some(parent))
        .await
        .unwrap();
    let before: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('text', user_text, 'sources', replay_source_message_ids,
         'message', user_msg_id, 'owner', request_owner_id, 'attempt', attempt_no,
         'parent', parent_outbox_id) FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(before["attempt"], 2);
    assert_eq!(before["parent"], parent);
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, releasing) = tokio::sync::oneshot::channel();
    let mut barrier = Some((entered, releasing));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let checkpoints = Arc::clone(&seen);
    let _checkpoint = test_executor::hook(Box::new(move |point| {
        checkpoints.lock().unwrap().push(point);
        let expected = if matches!(
            case,
            WorkerCase::FinalPaneAppeared | WorkerCase::UnprovedFinal
        ) {
            test_executor::Checkpoint::PreAccept
        } else {
            test_executor::Checkpoint::AfterClaim
        };
        let barrier = (point == expected).then(|| barrier.take()).flatten();
        Box::pin(async move {
            if let Some((entered, release)) = barrier {
                let _ = entered.send(());
                release.await.unwrap();
            }
        })
    }));
    let recorder = test_executor::record();
    let cancelled = || false;
    let (finished, finishing) = tokio::sync::oneshot::channel();
    let tick = async {
        let result = intake_worker::run_intake_worker_tick(
            pool,
            &worker,
            "b1b-worker",
            "codex",
            "b1b-claim",
            &cancelled,
        )
        .await;
        let _ = finished.send(());
        result
    };
    let intervention = async {
        tokio::select! {
            entered = entering => entered.expect("the claimed row must reach its checkpoint"),
            _ = finishing => return,
        }
        match case {
            WorkerCase::Ready => {}
            WorkerCase::PaneAppeared | WorkerCase::FinalPaneAppeared => fixture.fake.mark_present(),
            WorkerCase::UnprovedFirst | WorkerCase::UnprovedFinal => {
                let mut context = fixture.context.clone();
                context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
                context.created_at = Utc::now() + chrono::Duration::seconds(2);
                let prepared = crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(context).unwrap();
                crate::services::discord::stamp_spawn_markers(CANARY_TMUX, Some(&prepared))
                    .unwrap();
                assert!(dedupe::clear_relaunch::successor_unproved_for_channel(
                    CANARY_CHANNEL
                ));
            }
            WorkerCase::ClaimChanged => {
                sqlx::query("UPDATE intake_outbox SET claim_owner='other-claim' WHERE id=$1")
                    .bind(id)
                    .execute(pool)
                    .await
                    .unwrap();
            }
        }
        release.send(()).unwrap();
    };
    let (outcome, ()) = tokio::join!(tick, intervention);
    let outcome = outcome.unwrap();
    let (status, owner, error): (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT status, claim_owner, last_error FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    let transitions: (bool, bool) = sqlx::query_as(
        "SELECT accepted_at IS NOT NULL, spawned_at IS NOT NULL FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    let checkpoints = seen.lock().unwrap().clone();
    match case {
        WorkerCase::Ready => {
            assert_eq!(outcome, intake_worker::TickOutcome::Processed);
            assert_eq!(
                status, "done",
                "both preaccept checks must admit: {error:?}"
            );
            assert_eq!(recorder.channels(), [CANARY_CHANNEL]);
            assert_eq!(transitions, (true, true));
            assert_eq!(
                checkpoints
                    .iter()
                    .filter(|point| **point == test_executor::Checkpoint::PreAccept)
                    .count(),
                2,
                "both final O and accept boundaries must run"
            );
            for checkpoint in [
                test_executor::Checkpoint::AfterClaim,
                test_executor::Checkpoint::PreAccept,
                test_executor::Checkpoint::Spawned,
                test_executor::Checkpoint::FinalDb,
                test_executor::Checkpoint::FinalDbDone,
            ] {
                assert!(
                    checkpoints.contains(&checkpoint),
                    "actual tick missed {checkpoint:?}: {checkpoints:?}"
                );
            }
        }
        WorkerCase::PaneAppeared
        | WorkerCase::FinalPaneAppeared
        | WorkerCase::UnprovedFirst
        | WorkerCase::UnprovedFinal => {
            assert_eq!(outcome, intake_worker::TickOutcome::Held);
            assert_eq!(status, "pending");
            assert!(owner.is_none());
        }
        WorkerCase::ClaimChanged => {
            assert_eq!(outcome, intake_worker::TickOutcome::Held);
            assert_eq!(status, "claimed");
            assert_eq!(owner.as_deref(), Some("other-claim"));
        }
    }
    if !matches!(case, WorkerCase::Ready) {
        assert_eq!(transitions, (false, false));
        assert!(
            recorder.channels().is_empty(),
            "the provider must not execute"
        );
    }
    if matches!(
        case,
        WorkerCase::FinalPaneAppeared | WorkerCase::UnprovedFinal
    ) {
        assert!(
            checkpoints.contains(&test_executor::Checkpoint::PreAccept),
            "the race must reach the real final O check: {checkpoints:?}"
        );
    }
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('text', user_text, 'sources', replay_source_message_ids,
         'message', user_msg_id, 'owner', request_owner_id, 'attempt', attempt_no,
         'parent', parent_outbox_id) FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        after, before,
        "claim checks must preserve the worker payload"
    );
}
