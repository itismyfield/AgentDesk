#![cfg(unix)]

use super::{IntakeAdmission, IntakeOrigin, IntakeSubmission, admit_text_intake};
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::codex::CodexSourceMode;
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
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
use poise::serenity_prelude as serenity;
use std::sync::Arc;
use std::time::Duration;

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

#[derive(Clone, Copy)]
enum Case {
    Admission,
    Progress,
    Worker(WorkerCase),
}

#[derive(Clone, Copy)]
enum WorkerCase {
    Ready,
    PaneAppeared,
    FinalPaneAppeared,
    ClaimChanged,
}

async fn admission_case(case: Case) {
    let fixture =
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
    fixture.fake.mark_missing();
    crate::services::platform::tmux::invalidate_availability_cache();
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);

    let observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
            .expect("the initial placement has no inflight row");
    assert!(
        dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &observation).is_some(),
        "the existing B1 absence predicate must accept this fixture"
    );
    assert!(!dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));

    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
        .unwrap()
        .unwrap();
    store
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
            gate,
            readiness: Arc::clone(&readiness),
        },
    );
    assert_eq!(
        hosts.len(),
        1,
        "the selected channel must start its real host"
    );
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
    let admission = admit_text_intake(&deps, &input()).await;
    if let Case::Worker(case) = case {
        worker_case(&fixture, case).await;
    }
    assert!(io.posts.to(CANARY_CHANNEL).is_empty());
    hosts.pop().unwrap().stop_and_join().await.unwrap();
    if let Some((_guard, sibling, external)) = progress {
        sibling.await.unwrap();
        let (probe_thread, advanced_before_release) = external.join().unwrap();
        assert_ne!(
            probe_thread, async_thread,
            "the sync probe must leave the async task"
        );
        assert!(
            advanced_before_release,
            "another task must progress before the external thread releases the sync probe"
        );
    }
    assert!(
        matches!(admission, IntakeAdmission::Local(_)),
        "an existing false-resumed O actor with exact B1 absence must admit input: {admission:?}"
    );
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
    let id = insert_pending(pool, &payload, 2, None).await.unwrap();
    let before: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('text', user_text, 'sources', replay_source_message_ids,
         'message', user_msg_id, 'owner', request_owner_id, 'attempt', attempt_no,
         'parent', parent_outbox_id) FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, releasing) = tokio::sync::oneshot::channel();
    let mut barrier = Some((entered, releasing));
    let _checkpoint = test_executor::hook(Box::new(move |point| {
        let expected = if matches!(case, WorkerCase::FinalPaneAppeared) {
            test_executor::Checkpoint::BeforeFinalO
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
    match case {
        WorkerCase::Ready => {
            assert_eq!(outcome, intake_worker::TickOutcome::Processed);
            assert_eq!(
                status, "done",
                "both preaccept checks must admit: {error:?}"
            );
            assert_eq!(recorder.channels(), [CANARY_CHANNEL]);
        }
        WorkerCase::PaneAppeared | WorkerCase::FinalPaneAppeared => {
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
        assert!(
            recorder.channels().is_empty(),
            "the provider must not execute"
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
