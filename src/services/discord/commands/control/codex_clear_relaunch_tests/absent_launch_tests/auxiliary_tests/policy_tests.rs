//! Entry refusals, rollback policy and post-tail errors retain their existing authority.

use super::super::super::CanaryFixture;
use super::super::negative_tests::{payload_snapshot, persisted_queue_bytes};
use super::*;

#[tokio::test(flavor = "current_thread")]
async fn unproved_foreign_inventory_never_kicks_restored_head_pg() {
    if !isolated(concat!(
        module_path!(),
        "::unproved_foreign_inventory_never_kicks_restored_head_pg"
    )) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    target_missing(&fixture, true);
    fixture
        .fake
        .add_unwatched_inventory("AgentDesk-codex-foreign-unproved-owner");
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    fixture.shared.core.lock().await.sessions.remove(&channel);
    let identities =
        restore_instructions(&mut fixture, vec![instruction(CANARY_CHANNEL + 111, Q1)]).await;
    let queue_before = persisted_queue_bytes(&fixture);
    let payload_before = payload_snapshot(
        &fixture
            .shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue,
    );
    let history = fs::read(fixture.history()).unwrap();
    assert_eq!(
        settled_owner_observation(
            "B1_UNPROVED_FOREIGN owner_epoch_retries",
            || {
                activity::tests::unbound_verdict_for_tests(
                    &fixture.shared,
                    &ProviderKind::Codex,
                    channel,
                )
            },
            owner_epoch_error,
        )
        .await
        .unwrap(),
        "Hold(\"owner_unresolved\")"
    );
    let absence_observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
            .expect("the queue guard fixture must have a read-only absent inflight observation");
    assert!(
        dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &absence_observation).is_some(),
        "the queue guard fixture must have exact absence launch evidence"
    );
    assert!(
        !dedupe::clear_relaunch::relaunch_open(CANARY_TMUX),
        "absence alone must not satisfy the explicit-clear queue exception"
    );
    let public_activity = settled_owner_observation(
        "B1_UNPROVED_FOREIGN superseded_retries",
        || activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel),
        |sample| sample.reason == "superseded",
    )
    .await;
    assert_eq!(public_activity.activity, activity::Activity::Unknown);
    assert_eq!(public_activity.reason, "session_identity_unresolved");
    fixture.fake.take_calls();
    let taken = crate::services::discord::queue_dispatch::mailbox_take_next_automatic_intervention(
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
    )
    .await;
    assert!(taken.persistence_error.is_none(), "{taken:?}");
    assert!(
        taken.into_intervention().is_none(),
        "absence must not grant queue admission past unproved foreign ownership"
    );
    assert!(!kickoff(&fixture, &api).await);
    assert_eq!(queue_identities(&fixture).await, identities);
    let snapshot = fixture.shared.mailbox(channel).snapshot().await;
    assert_eq!(
        payload_snapshot(&snapshot.intervention_queue),
        payload_before
    );
    assert_eq!(persisted_queue_bytes(&fixture), queue_before);
    assert!(snapshot.cancel_token.is_none() && snapshot.pending_user_dispatch.is_none());
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        channel,
    )
    .unwrap();
    assert_eq!(
        disk.iter().map(QueueIdentity::of).collect::<Vec<_>>(),
        identities
    );
    assert_eq!(fs::read(fixture.history()).unwrap(), history);
    assert!(fixture.fake.take_calls().iter().all(|call| {
        ["new-session", "kill-session", "send-keys", "paste-buffer"]
            .iter()
            .all(|effect| !call.starts_with(effect))
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn ambient_shadow_absence_pins_verified_but_explicit_clear_restores_shadow_pg() {
    if !isolated(concat!(
        module_path!(),
        "::ambient_shadow_absence_pins_verified_but_explicit_clear_restores_shadow_pg"
    )) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Shadow, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    target_missing(&fixture, true);
    restore_instructions(&mut fixture, vec![instruction(CANARY_CHANNEL + 121, Q1)]).await;
    let (hold_tx, hold_rx) = mpsc::channel();
    *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(hold_tx);
    let _hold_reset = HoldHookReset;
    fixture.fake.take_calls();
    assert!(kickoff(&fixture, &api).await);
    let first = fixture.turn_failure_guard().await;
    let n2 = tokio::select! {
        context = wait_context_after(&fixture, &fixture.context.execution_nonce) => Some(context),
        _ = async {
            loop {
                if hold_rx.try_recv().is_ok() { break; }
                if fixture.shared.mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
                    .snapshot().await.cancel_token.is_none() { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => None,
    };
    if n2.is_none() {
        first.token.cancelled.store(true, Ordering::SeqCst);
        wait_idle(&fixture).await;
    }
    let calls = fixture.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session"))
            .count(),
        1,
        "absence successor must launch before rollback policy is consumed; {calls:?}"
    );
    let n2 = n2.expect("an admission hold cannot satisfy a fresh successor");
    assert_eq!(
        n2.source_policy.as_deref(),
        Some("verified"),
        "absence must outrank ambient rollback"
    );
    publish_named(&fixture, &n2, NEW_NATIVE_ID);
    wait_completion(&first.token).await;
    wait_idle(&fixture).await;
    first.finish();
    wait_recovery_custody_settled(&fixture, &n2, false).await;
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(
        fixture
            .fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("kill-session"))
    );
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Codex, &fixture.shared.token_hash, None);
    fixture
        .shared
        .mailbox(channel)
        .replace_queue(
            vec![instruction(
                CANARY_CHANNEL + 122,
                "discarded by the explicit clear",
            )],
            persistence,
        )
        .await;
    let cleared = clear_channel_session_state(
        &api.http,
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        "!clear",
    )
    .await
    .unwrap();
    assert_no_retirement_warning(&cleared);
    assert!(queue_identities(&fixture).await.is_empty());
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        channel,
    )
    .unwrap();
    assert!(
        disk.is_empty(),
        "explicit clear still discards the waiting queue"
    );
    assert!(dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    recovery_diagnostic(&fixture, &n2, false, "shadow_after_explicit_clear", None).await;
    let cli = fixture.root.path().join("codex-fixture");
    fs::write(
        &cli,
        fs::read_to_string(&cli)
            .unwrap()
            .replace(NEW_NATIVE_ID, THIRD_NATIVE_ID),
    )
    .unwrap();
    restore_instructions(&mut fixture, vec![instruction(CANARY_CHANNEL + 123, Q3)]).await;
    fixture.fake.take_calls();
    let started = kickoff(&fixture, &api).await;
    if !started {
        recovery_diagnostic(&fixture, &n2, false, "shadow_clear_kickoff_refused", None).await;
    }
    assert!(started, "explicit clear permits the next restored input");
    let next = fixture.turn_failure_guard().await;
    let n3 = wait_context_after(&fixture, &n2.execution_nonce).await;
    assert_eq!(n3.launch_mode, "fresh");
    assert_eq!(
        n3.source_policy.as_deref(),
        Some("shadow"),
        "explicit clear takes precedence over historical absence policy"
    );
    let n3_path = n3
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{THIRD_NATIVE_ID}.jsonl"));
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(n3.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let _ = observe_binding_hook(
        "codex",
        "session-start",
        Some(THIRD_NATIVE_ID),
        Some(THIRD_NATIVE_ID),
        &json!({"session_id":THIRD_NATIVE_ID,"transcript_path":n3_path,"source":"startup"}),
        &headers,
    );
    // Explicit clear restores the existing Shadow consumer path for this launch.
    assert!(dedupe::codex_verified_source_allowed(
        CANARY_TMUX,
        n3_path.to_str().unwrap(),
        Some(THIRD_NATIVE_ID),
    ));
    terminal(
        &n3_path,
        "shadow-after-explicit-clear",
        "shadow relaunch completed",
    );
    wait_completion(&next.token).await;
    wait_idle(&fixture).await;
    next.finish();
    let calls = fixture.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1
    );
    assert!(calls.iter().all(|call| !call.starts_with("kill-session")));
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert!(
        argv.iter()
            .all(|arg| !arg.contains("discarded by the explicit clear"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn post_tail_error_preserves_the_new_live_incarnation_pg() {
    if !isolated(concat!(
        module_path!(),
        "::post_tail_error_preserves_the_new_live_incarnation_pg"
    )) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    target_missing(&fixture, true);
    restore_instructions(&mut fixture, vec![instruction(CANARY_CHANNEL + 131, Q1)]).await;
    let _setup = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(|_| {
            crate::services::codex::actual_submit_tests::fail_tail_once();
            Box::new(())
        }),
    );
    let (hold_tx, hold_rx) = mpsc::channel();
    *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(hold_tx);
    let _reset = HoldHookReset;
    fixture.fake.take_calls();
    assert!(kickoff(&fixture, &api).await);
    let guard = fixture.turn_failure_guard().await;
    let new = wait_context_after(&fixture, &fixture.context.execution_nonce).await;
    publish_named(&fixture, &new, NEW_NATIVE_ID);
    let held = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(tmux) = hold_rx.try_recv() {
                break tmux;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let calls = fixture.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        calls.iter().all(|call| !call.starts_with("kill-session")),
        "{calls:?}"
    );
    assert!(crate::services::tmux_diagnostics::tmux_session_has_live_pane(CANARY_TMUX));
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        SpawnNonceMarker::Known(new.execution_nonce)
    );
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(!dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &fixture.old_source
    ));
    guard.token.cancelled.store(true, Ordering::SeqCst);
    wait_idle(&fixture).await;
    assert_eq!(
        held.expect("a new verified tail error must reach the real preserving hold"),
        CANARY_TMUX
    );
    // Intentional cancellation disarms no completion assertion; its guard still owns cleanup.
    drop(guard);
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_launch_policy_remains_outside_the_absence_exception_pg() {
    if !isolated(concat!(
        module_path!(),
        "::ordinary_launch_policy_remains_outside_the_absence_exception_pg"
    )) {
        return;
    }
    let _fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let channel = CANARY_CHANNEL + 500;
    let tmux = "AgentDesk-codex-ordinary-b1-regression";
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        channel
    ));
    assert_eq!(
        crate::services::codex_tui::canary::launch_policy(tmux, Some(channel)),
        Ok("legacy")
    );
    assert!(!dedupe::codex_verified_requires_proof(tmux));
    assert!(!dedupe::clear_relaunch::launch_blocked(tmux));
    assert!(!crate::services::codex_tui::verified_hold::existing_incarnation(tmux));
    // BASE is already GREEN: the ordinary production launcher cannot create a verified pin.
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_ordinary_verified_pin_does_not_receive_canary_launch_authority_pg() {
    if !isolated(concat!(
        module_path!(),
        "::synthetic_ordinary_verified_pin_does_not_receive_canary_launch_authority_pg"
    )) {
        return;
    }
    let fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let channel = CANARY_CHANNEL + 501;
    let mut context = fixture.context.clone();
    context.channel_id = Some(channel);
    context.tmux_session = "AgentDesk-codex-ordinary-b1-synthetic".into();
    context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    context.created_at = chrono::Utc::now();
    let prepared = PreparedIncarnation::create(context.clone()).unwrap();
    crate::services::discord::stamp_spawn_markers(&context.tmux_session, Some(&prepared)).unwrap();
    dedupe::register_tmux_channel(&context.tmux_session, channel);
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{THIRD_NATIVE_ID}.jsonl"));
    let at = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    fs::write(&path, format!("{}\n", json!({"type":"session_meta","timestamp":at,"payload":{
        "id":THIRD_NATIVE_ID,"timestamp":at,"cwd":fixture.root.path(),"source":"cli","originator":"codex_cli_rs"}}))).unwrap();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    assert_eq!(
        observe_binding_hook(
            "codex",
            "session-start",
            Some(THIRD_NATIVE_ID),
            Some(THIRD_NATIVE_ID),
            &json!({"session_id":THIRD_NATIVE_ID,"transcript_path":path,"source":"startup","prompt":PROMPT}),
            &headers
        ),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    target_missing(&fixture, false);
    assert!(dedupe::evict_dead_tmux_mirror(&context.tmux_session));
    assert!(dedupe::codex_verified_requires_proof(&context.tmux_session));
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        channel
    ));
    let body = fs::read(&path).unwrap();
    fixture.fake.take_calls();
    assert!(dedupe::clear_relaunch::launch_blocked(
        &context.tmux_session
    ));
    assert!(crate::services::codex_tui::verified_hold::existing_incarnation(&context.tmux_session));
    let token = Arc::new(CancelToken::new());
    token.cancelled.store(true, Ordering::SeqCst);
    assert!(
        crate::services::codex_tui::verified_hold::preflight(&context.tmux_session, Some(&token))
            .is_some()
    );
    assert!(!dedupe::clear_relaunch::relaunch_open(
        &context.tmux_session
    ));
    assert_eq!(fs::read(path).unwrap(), body);
    assert_eq!(
        dedupe::binding_context::execution_context("codex", &context.execution_nonce).unwrap(),
        context
    );
    assert!(
        fixture
            .fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("kill-session") && !call.starts_with("new-session"))
    );
    // Synthetic preservation is also BASE GREEN; canary positive tests supply the new-feature RED.
}
