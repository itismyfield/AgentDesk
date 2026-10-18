//! Compatibility keeps the original queued input parked until a strictly later clear exists.

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueueIdentity};

const ORIGINAL_MESSAGE: u64 = 1_561_159_600_206_516_285;

fn original(message: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(message),
        queued_generation: 322,
        source_message_ids: vec![MessageId::new(message)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "original after-clear input".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn enqueue(shared: &Arc<SharedData>, item: Intervention) -> ChannelMailboxSnapshot {
    let channel = ChannelId::new(CANARY_CHANNEL);
    let message = item.message_id;
    let persistence =
        super::super::queue_persistence_context(shared, &ProviderKind::Codex, channel);
    let result = shared.mailbox(channel).enqueue(item, persistence).await;
    assert!(
        result.enqueued && result.persistence_error.is_none(),
        "message={message} enqueued={} refusal={:?} persistence_error={:?}",
        result.enqueued,
        result.refusal_reason,
        result.persistence_error
    );
    shared.mailbox(channel).snapshot().await
}

fn legacy_ended(fixture: &CanaryFixture) -> chrono::DateTime<chrono::Utc> {
    fixture.fake.mark_missing();
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
    binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .committed_at
}

async fn boundary(pool: &sqlx::PgPool, at: chrono::DateTime<chrono::Utc>) {
    sqlx::query(
        "INSERT INTO channel_session_clear_boundaries (channel_id, cleared_at, clear_generation)
         VALUES ($1, $2, 1)
         ON CONFLICT (channel_id) DO UPDATE SET cleared_at = EXCLUDED.cleared_at",
    )
    .bind(CANARY_CHANNEL.to_string())
    .bind(at)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_compat_keeps_original_queue_and_retirement_is_idempotent_pg() {
    let fixture = CanaryFixture::new_with_audit_prefix(120).await;
    assert_eq!(
        binding_events::codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .unwrap()
            .seq,
        121
    );
    let at = legacy_ended(&fixture);
    boundary(
        fixture.shared.pg_pool.as_ref().unwrap(),
        at + chrono::Duration::seconds(1),
    )
    .await;
    let snapshot = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
    let identities: Vec<_> = snapshot
        .intervention_queue
        .iter()
        .map(QueueIdentity::of)
        .collect();
    let history = std::fs::read(fixture.history()).unwrap();
    let channel = ChannelId::new(CANARY_CHANNEL);
    for _ in 0..2 {
        assert!(admit(&fixture.shared, &ProviderKind::Codex, channel, &snapshot).await);
        let retained = fixture.shared.mailbox(channel).snapshot().await;
        assert_eq!(
            retained
                .intervention_queue
                .iter()
                .map(QueueIdentity::of)
                .collect::<Vec<_>>(),
            identities
        );
        assert_eq!(
            retained.intervention_queue[0].text,
            "original after-clear input"
        );
        assert!(retained.pending_user_dispatch.is_none());
        assert!(retained.cancel_token.is_none());
    }
    assert!(dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(dedupe::codex_verified_requires_proof(CANARY_TMUX));
    assert!(dedupe::codex_verified_input_blocked(CANARY_TMUX));
    assert_eq!(
        std::fs::read(fixture.history()).unwrap(),
        history,
        "retirement leaves ownership history intact"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_compat_boot_kickoff_launches_the_original_input_once_pg() {
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(concat!(
        module_path!(),
        "::legacy_compat_boot_kickoff_launches_the_original_input_once_pg"
    )) {
        return;
    }
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::queue_dispatch::codex_clear_compat::tests::",
        "legacy_compat_boot_kickoff_launches_the_original_input_once_pg"
    )) {
        return;
    }
    let mut fixture = CanaryFixture::new_with_audit_prefix(120).await;
    assert!(
        !crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
            CANARY_CHANNEL,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
        )
        .expect("the fixture installs an explicit empty O boot list")
    );
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        |method, path| {
            (method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{CANARY_CHANNEL}")))
            .then(|| {
                serde_json::json!({
                    "id": CANARY_CHANNEL.to_string(), "type": 0,
                    "name": "adk-codex-tui-e2e", "guild_id": "42000",
                    "position": 0, "permission_overwrites": [], "nsfw": false,
                    "parent_id": null
                })
            })
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    let at = legacy_ended(&fixture);
    let channel = ChannelId::new(CANARY_CHANNEL);
    let pool = fixture.shared.pg_pool.as_ref().unwrap();
    boundary(pool, at + chrono::Duration::microseconds(1)).await;
    let key = crate::services::discord::adk_session::build_namespaced_session_key(
        &fixture.shared.token_hash,
        &ProviderKind::Codex,
        CANARY_TMUX,
    );
    crate::services::discord::adk_session::clear_provider_session_id_pg(pool, &key)
        .await
        .unwrap();
    fixture
        .shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .unwrap()
        .session_id = None;
    let mut saved = original(ORIGINAL_MESSAGE);
    saved
        .source_message_ids
        .push(MessageId::new(ORIGINAL_MESSAGE + 1));
    let identity = QueueIdentity::of(&saved);
    crate::services::turn_orchestrator::save_channel_queue(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        channel,
        &[saved],
        None,
    )
    .unwrap();
    assert!(
        fixture
            .shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
    let stale_cards =
        crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
            &api.http,
            &fixture.shared,
            &ProviderKind::Codex,
        )
        .await;
    assert!(stale_cards.is_empty());
    let snapshot = fixture.shared.mailbox(channel).snapshot().await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(
        QueueIdentity::of(&snapshot.intervention_queue[0]),
        identity,
        "boot restore preserves the original ID, generation, and ordered source IDs"
    );
    assert_eq!(
        snapshot.intervention_queue[0].text,
        "original after-clear input"
    );
    assert_eq!(
        binding_events::codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .unwrap()
            .seq,
        121
    );
    let deps = crate::services::discord::router::IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "e51-isolated-test-bot",
    };
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            super::super::kickoff::kickoff_idle_queues_with_deps(&deps, &ProviderKind::Codex)
        )
        .await
        .expect("restored queue did not finish preparation"),
        1
    );
    let turn_guard = fixture.turn_failure_guard().await;
    let dispatch = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Codex,
        CANARY_CHANNEL,
    )
    .expect("the launched provider must retain the original request anchor");
    assert_eq!(dispatch.user_msg_id, ORIGINAL_MESSAGE);
    let new = fixture.publish_new_hook().await;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let state = fixture.shared.mailbox(channel).snapshot().await;
            if state.cancel_token.is_none() && state.pending_user_dispatch.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the verified restored turn did not complete its terminal");
    assert_ne!(new.execution_nonce, fixture.context.execution_nonce);
    assert_eq!(new.launch_mode, "fresh");
    assert!(new.expected_native_session_id.is_none());
    let proof = binding_events::codex::read_ownership(&new).unwrap();
    assert!(proof.pending.is_empty() && !proof.conflicted);
    assert_eq!(
        proof.verified.unwrap().source.session_id,
        crate::services::discord::commands::control::codex_clear_relaunch_tests::NEW_NATIVE_ID
    );
    let argv: Vec<String> = serde_json::from_slice(
        &std::fs::read(fixture.root.path().join("fresh-argv.json")).unwrap(),
    )
    .unwrap();
    assert!(!argv.iter().any(|arg| arg == "resume"));
    assert!(
        argv.iter()
            .any(|arg| arg.contains("original after-clear input"))
    );
    let retained = fixture.shared.mailbox(channel).snapshot().await;
    assert!(retained.intervention_queue.is_empty());
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    assert_eq!(
        super::super::kickoff::kickoff_idle_queues_with_deps(&deps, &ProviderKind::Codex).await,
        0
    );
    assert_eq!(
        fixture
            .fake
            .take_calls()
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1
    );
    let sent = api.take();
    let bodies = sent
        .iter()
        .filter(|call| {
            let mut parts = call.splitn(3, ' ');
            let method = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();
            let body = parts.next().unwrap_or_default();
            matches!(method, "POST" | "PATCH")
                && path.contains(&format!("/channels/{CANARY_CHANNEL}/messages"))
                && serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|value| value["content"].as_str().map(str::to_owned))
                    .is_some_and(|content| content.contains("clear relaunch completed"))
        })
        .count();
    assert_eq!(
        bodies, 1,
        "the restored verified turn sends one completed response: {sent:?}"
    );
    turn_guard.finish();
}

#[tokio::test(flavor = "current_thread")]
async fn compat_leaves_a_live_verified_incarnation_to_existing_readiness_pg() {
    let fixture = CanaryFixture::new().await;
    let snapshot = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
    assert!(
        admit(
            &fixture.shared,
            &ProviderKind::Codex,
            ChannelId::new(CANARY_CHANNEL),
            &snapshot
        )
        .await
    );
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(!dedupe::codex_verified_input_blocked(CANARY_TMUX));
}

#[tokio::test(flavor = "current_thread")]
async fn compat_historical_pin_with_a_live_or_unreadable_pane_parks_c2_pg() {
    for failed_probe in [false, true] {
        let fixture = CanaryFixture::new().await;
        crate::services::session_backend::remove_process_session(CANARY_TMUX);
        crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
        if failed_probe {
            fixture.fake.fail_probes();
        }
        let snapshot = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
        let channel = ChannelId::new(CANARY_CHANNEL);
        assert!(!admit(&fixture.shared, &ProviderKind::Codex, channel, &snapshot).await);
        let parked = fixture.shared.queue_park_ledger.project(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
            &snapshot,
        );
        assert_eq!(parked.reason.as_deref(), Some("codex_clear_compat:C2"));
        assert_eq!(
            snapshot.intervention_queue[0].message_id.get(),
            ORIGINAL_MESSAGE
        );
        assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn compat_pinned_disabled_or_herdr_target_parks_c1_but_legacy_passes_pg() {
    for condition in ["disabled", "herdr"] {
        for pinned in [true, false] {
            let fixture = CanaryFixture::new().await;
            legacy_ended(&fixture);
            if !pinned {
                std::fs::write(fixture.history(), b"").unwrap();
            }
            let _hosts = (condition == "herdr").then(|| {
                crate::config::session_hosts::force_for_test(
                    Some("test-local"),
                    &[(CANARY_CHANNEL, "test-local")],
                )
            });
            if condition == "disabled" {
                crate::services::codex::SOURCE_MODE_TEST.with(|mode| {
                    mode.set(Some(crate::services::codex::CodexSourceMode::Legacy));
                });
            }
            assert_eq!(dedupe::codex_verified_requires_proof(CANARY_TMUX), pinned);
            let channel = ChannelId::new(CANARY_CHANNEL);
            let snapshot = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
            assert_eq!(
                admit(&fixture.shared, &ProviderKind::Codex, channel, &snapshot).await,
                !pinned,
                "condition={condition} pinned={pinned}"
            );
            let projection = fixture.shared.queue_park_ledger.project(
                &fixture.shared,
                &ProviderKind::Codex,
                channel,
                &snapshot,
            );
            if pinned {
                assert_eq!(projection.owner, Some("codex_clear_compat"));
                assert_eq!(projection.reason.as_deref(), Some("codex_clear_compat:C1"));
            } else {
                assert_ne!(projection.owner, Some("codex_clear_compat"));
            }
            assert_eq!(
                fixture
                    .shared
                    .mailbox(channel)
                    .snapshot()
                    .await
                    .intervention_queue
                    .len(),
                1
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn compat_rejects_wrong_target_unreadable_history_and_corrupt_watermark_pg() {
    for condition in ["target", "history", "pending", "watermark"] {
        let fixture = CanaryFixture::new().await;
        if condition == "pending" {
            use crate::services::claude_tui::hook_server::adoption_retry::DurableKind;
            use crate::services::claude_tui::hook_server::observation_ingress::{
                IngressOutcome, observe_binding_hook,
            };
            use crate::services::tui_prompt_dedupe::binding_context::{
                BINDING_HEADER, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
            };
            let pending_id = "019e660d-4859-7522-9cee-8ba7c4e7c745";
            let envelope = HookBindingEnvelope {
                context: CapturedContext::Captured(fixture.context.clone()),
                observed: ObservedHookProcess::default(),
            };
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
            let published = chrono::Utc::now() + chrono::Duration::seconds(2);
            headers.insert(
                crate::services::claude_tui::hook_server::relay_receipts::RELAY_PUBLISHED_AT_HEADER,
                published.to_rfc3339().parse().unwrap(),
            );
            assert_eq!(
                observe_binding_hook(
                    "codex",
                    "session-start",
                    Some(pending_id),
                    Some(pending_id),
                    &serde_json::json!({"session_id": pending_id, "source": "startup"}),
                    &headers
                ),
                IngressOutcome::Durable(DurableKind::Pending)
            );
            let fold = binding_events::codex::read_ownership(&fixture.context).unwrap();
            assert!(fold.verified.is_some() && fold.pending.len() == 1 && fold.conflicted);
        }
        let at = legacy_ended(&fixture);
        boundary(
            fixture.shared.pg_pool.as_ref().unwrap(),
            at + chrono::Duration::seconds(1),
        )
        .await;
        let channel = ChannelId::new(CANARY_CHANNEL);
        let reason = match condition {
            "target" => {
                fixture
                    .shared
                    .core
                    .lock()
                    .await
                    .sessions
                    .get_mut(&channel)
                    .unwrap()
                    .channel_name = Some("another-channel".into());
                "codex_clear_compat:C1"
            }
            "history" => {
                std::fs::write(fixture.history(), b"malformed ownership record\n").unwrap();
                "codex_clear_compat:C3"
            }
            "pending" => "codex_clear_compat:C3",
            "watermark" => {
                let path = fixture
                    .root
                    .path()
                    .join("runtime/codex_clear_retired")
                    .join(format!("{CANARY_CHANNEL}.json"));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"malformed retirement\n").unwrap();
                "codex_clear_compat:C5"
            }
            _ => unreachable!(),
        };
        let snapshot = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
        let history = std::fs::read(fixture.history()).unwrap();
        assert!(
            !admit(&fixture.shared, &ProviderKind::Codex, channel, &snapshot).await,
            "condition={condition}"
        );
        let projected = fixture.shared.queue_park_ledger.project(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
            &snapshot,
        );
        assert_eq!(
            projected.reason.as_deref(),
            Some(reason),
            "condition={condition}"
        );
        assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
        assert_eq!(
            std::fs::read(fixture.history()).unwrap(),
            history,
            "a rejected compatibility observation leaves ownership history intact"
        );
        let retained = fixture.shared.mailbox(channel).snapshot().await;
        assert_eq!(retained.intervention_queue.len(), 1);
        assert_eq!(
            QueueIdentity::of(&retained.intervention_queue[0]),
            QueueIdentity::of(&snapshot.intervention_queue[0])
        );
        assert_eq!(
            retained.intervention_queue[0].text,
            "original after-clear input"
        );
        assert!(retained.pending_user_dispatch.is_none() && retained.cancel_token.is_none());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn compat_failure_is_cached_until_new_input_clear_or_runtime_pg() {
    {
        let fixture = CanaryFixture::new().await;
        let at = legacy_ended(&fixture);
        let pool = fixture.shared.pg_pool.as_ref().unwrap();
        boundary(pool, at).await;
        let stale = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
        let channel = ChannelId::new(CANARY_CHANNEL);
        assert!(!admit(&fixture.shared, &ProviderKind::Codex, channel, &stale).await);
        let mut input = original(ORIGINAL_MESSAGE + 1);
        input.text = "second input establishes the current failure cache".into();
        let fresh = enqueue(&fixture.shared, input).await;
        assert!(!admit(&fixture.shared, &ProviderKind::Codex, channel, &fresh).await);
        boundary(pool, at + chrono::Duration::seconds(1)).await;
        assert!(
            !admit(&fixture.shared, &ProviderKind::Codex, channel, &stale).await,
            "a delayed old snapshot cannot reset the current queue's one-shot failure"
        );
        assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
        let retained = fixture.shared.mailbox(channel).snapshot().await;
        assert_eq!(
            retained
                .intervention_queue
                .iter()
                .map(QueueIdentity::of)
                .collect::<Vec<_>>(),
            fresh
                .intervention_queue
                .iter()
                .map(QueueIdentity::of)
                .collect::<Vec<_>>()
        );
        let projection = fixture.shared.queue_park_ledger.project(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
            &retained,
        );
        assert_eq!(projection.reason.as_deref(), Some("codex_clear_compat:C4"));
    }
    for trigger in ["new_input", "clear", "runtime"] {
        eprintln!("E51_COMPAT_RESET trigger={trigger} stage=initial_enqueue");
        let fixture = CanaryFixture::new().await;
        let at = legacy_ended(&fixture);
        let pool = fixture.shared.pg_pool.as_ref().unwrap();
        boundary(pool, at).await;
        let snapshot = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
        let channel = ChannelId::new(CANARY_CHANNEL);
        assert!(!admit(&fixture.shared, &ProviderKind::Codex, channel, &snapshot).await);
        let parked = fixture.shared.queue_park_ledger.project(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
            &snapshot,
        );
        assert_eq!(parked.reason.as_deref(), Some("codex_clear_compat:C4"));
        assert_eq!(parked.owner, Some("codex_clear_compat"));
        boundary(pool, at + chrono::Duration::seconds(1)).await;
        assert!(
            !admit(&fixture.shared, &ProviderKind::Codex, channel, &snapshot).await,
            "a later database change alone must not re-evaluate the same parked input"
        );
        assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
        let (runtime, fresh) = match trigger {
            "new_input" => {
                let mut input = original(ORIGINAL_MESSAGE + 1);
                input.text = "new input retries the parked compatibility check".into();
                let fresh = enqueue(&fixture.shared, input).await;
                assert_eq!(fresh.intervention_queue.len(), 2);
                assert_eq!(
                    fresh.intervention_queue[0].message_id.get(),
                    ORIGINAL_MESSAGE
                );
                (fixture.shared.clone(), fresh)
            }
            "clear" => {
                reset(&fixture.shared, channel);
                (fixture.shared.clone(), snapshot.clone())
            }
            "runtime" => {
                let replacement = crate::services::discord::make_shared_data_for_tests_with_storage(
                    Some(pool.clone()),
                );
                let session = fixture.shared.core.lock().await.sessions[&channel].clone();
                replacement
                    .core
                    .lock()
                    .await
                    .sessions
                    .insert(channel, session);
                let persistence = super::super::queue_persistence_context(
                    &replacement,
                    &ProviderKind::Codex,
                    channel,
                );
                let hydrated = replacement
                    .mailbox(channel)
                    .hydrate_pending_queue_from_disk(persistence)
                    .await;
                assert_eq!(
                    (hydrated.absorbed, hydrated.persistence_error),
                    (1, None),
                    "the new runtime restores the original without enqueueing a duplicate"
                );
                let restored = replacement.mailbox(channel).snapshot().await;
                assert_eq!(
                    QueueIdentity::of(&restored.intervention_queue[0]),
                    QueueIdentity::of(&snapshot.intervention_queue[0])
                );
                (replacement, restored)
            }
            _ => unreachable!(),
        };
        assert!(
            admit(&runtime, &ProviderKind::Codex, channel, &fresh).await,
            "reset={trigger}"
        );
        assert!(dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
        assert_eq!(
            snapshot.intervention_queue[0].message_id.get(),
            ORIGINAL_MESSAGE
        );
        assert_eq!(snapshot.intervention_queue[0].queued_generation, 322);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn compat_clear_reset_during_boundary_read_cannot_publish_retirement_pg() {
    let fixture = CanaryFixture::new().await;
    let at = legacy_ended(&fixture);
    let pool = fixture.shared.pg_pool.as_ref().unwrap();
    boundary(pool, at + chrono::Duration::seconds(1)).await;
    let snapshot = enqueue(&fixture.shared, original(ORIGINAL_MESSAGE)).await;
    let channel = ChannelId::new(CANARY_CHANNEL);
    let mut blocked = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE channel_session_clear_boundaries IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocked)
        .await
        .unwrap();
    let provider = ProviderKind::Codex;
    let attempt = admit(&fixture.shared, &provider, channel, &snapshot);
    tokio::pin!(attempt);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut attempt)
            .await
            .is_err()
    );
    reset(&fixture.shared, channel);
    blocked.rollback().await.unwrap();
    assert!(
        !attempt.await,
        "the clear superseded this in-flight observation"
    );
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(
        admit(&fixture.shared, &ProviderKind::Codex, channel, &snapshot).await,
        "the superseded observation must not cache a permanent failure"
    );
}

#[tokio::test]
async fn compat_boundary_requires_a_strict_later_nonnull_row_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let recorded_at = chrono::DateTime::parse_from_rfc3339("2026-10-17T23:27:34.032898Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        !boundary_after(&pool, recorded_at).await,
        "a missing row cannot retire proof"
    );
    sqlx::query(
        "INSERT INTO channel_session_clear_boundaries (channel_id, cleared_at, clear_generation)
         VALUES ($1, $2, 1)",
    )
    .bind(CANARY_CHANNEL.to_string())
    .bind(recorded_at)
    .execute(&pool)
    .await
    .unwrap();
    for (offset, permitted) in [(-1, false), (0, false), (1, true)] {
        sqlx::query(
            "UPDATE channel_session_clear_boundaries SET cleared_at = $2 WHERE channel_id = $1",
        )
        .bind(CANARY_CHANNEL.to_string())
        .bind(recorded_at + chrono::Duration::microseconds(offset))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            boundary_after(&pool, recorded_at).await,
            permitted,
            "offset={offset}"
        );
    }
    // A legacy or damaged nullable row must fail closed rather than decode into permission.
    sqlx::query(
        "ALTER TABLE channel_session_clear_boundaries ALTER COLUMN cleared_at DROP NOT NULL",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE channel_session_clear_boundaries SET cleared_at = NULL WHERE channel_id = $1",
    )
    .bind(CANARY_CHANNEL.to_string())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !boundary_after(&pool, recorded_at).await,
        "NULL is not a later clear"
    );
    pool.close().await;
    assert!(
        !boundary_after(&pool, recorded_at).await,
        "a query failure cannot retire proof"
    );
    database.drop().await;
}
