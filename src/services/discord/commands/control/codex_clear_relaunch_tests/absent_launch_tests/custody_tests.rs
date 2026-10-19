//! An admitted terminal keeps launch closed until its durable custody settles.

use super::super::CanaryFixture;
use super::auxiliary_tests::{
    instruction, kickoff, publish_named_source, queue_identities, recorder, restore_instructions,
    target_missing, terminal, wait_completion, wait_context_after, wait_idle, warm_probe,
};
use super::*;
use crate::services::agent_protocol::{
    CapturedTuiTerminalSource, NativeTerminalKind, RuntimeHandoffKind, StreamMessage,
};
use crate::services::discord::inflight::{
    self, CodexRange, GuardedClearOutcome, InflightTurnIdentity, InflightTurnState,
};
use crate::services::discord::turn_bridge::custody_test_adapter;
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

const BODY: &str = "B1 captured terminal answer retained before its successor disappeared";
const FIRST: &str = "B1 restored FIFO first instruction";
const SECOND: &str = "B1 restored FIFO second instruction";

async fn admitted_terminal(
    fixture: &CanaryFixture,
    empty_body: bool,
) -> (InflightTurnState, CodexRange) {
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    let path = fixture.old_source.path.canonicalize().unwrap();
    terminal(&path, "b1-custody-incumbent", BODY);
    let metadata = fs::metadata(&path).unwrap();
    let response = if empty_body { "" } else { BODY };
    let actor = Arc::new(CancelToken::new());
    let mut local = InflightTurnState::new(
        ProviderKind::Codex,
        CANARY_CHANNEL,
        Some("adk-codex-tui-e2e".into()),
        1,
        CANARY_CHANNEL + 501,
        CANARY_CHANNEL + 502,
        "captured custody source".into(),
        Some(NATIVE_ID.into()),
        Some(CANARY_TMUX.into()),
        Some(path.display().to_string()),
        None,
        0,
    );
    local.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    local.turn_nonce = actor.turn_nonce().map(str::to_owned);
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &fixture.shared,
            channel,
            actor.clone(),
            serenity::UserId::new(local.request_owner_user_id),
            serenity::MessageId::new(local.user_msg_id),
        )
        .await
    );
    inflight::save_inflight_state(&local).unwrap();
    let expected = InflightTurnIdentity::from_state(&local);
    let frame = StreamMessage::CodexTuiTerminalDone {
        result: response.into(),
        session_id: Some(NATIVE_ID.into()),
        rollout_path: path.display().to_string(),
        tmux_session_name: CANARY_TMUX.into(),
        turn_nonce: actor.turn_nonce().unwrap().into(),
        source_start: 0,
        complete_record_end: metadata.len(),
        captured_source: Some(CapturedTuiTerminalSource {
            generation_mtime_ns:
                crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(
                    CANARY_TMUX,
                ),
            source_file_dev: metadata.dev(),
            source_file_ino: metadata.ino(),
            actor: Arc::downgrade(&actor),
        }),
        kind: NativeTerminalKind::Completed,
    };
    let mut baseline = local.clone();
    let (_, range, terminal, _) = local
        .admit_tui_terminal_frame(
            &mut baseline,
            &expected,
            true,
            (&fixture.shared, &actor),
            response,
            frame,
        )
        .await
        .expect("the actual captured terminal admission must succeed");
    assert!(terminal);
    let range = range.expect("the actual terminal must retain an admitted source range");
    assert_eq!(range.result, response);
    assert_eq!(baseline.full_response, response);
    assert_eq!(
        range.source_file_identity,
        Some((metadata.dev(), metadata.ino()))
    );
    let released = crate::services::discord::mailbox_finish_turn_if_matches(
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        serenity::MessageId::new(local.user_msg_id),
    )
    .await;
    assert!(released.removed_token.is_some());
    (baseline, range)
}

async fn retain_after_successor(
    fixture: &CanaryFixture,
    local: &InflightTurnState,
    range: &CodexRange,
) -> String {
    let mut successor = local.clone();
    successor.provider = "codex".into();
    successor.user_msg_id += 10;
    successor.turn_nonce = Some(uuid::Uuid::new_v4().to_string());
    successor.turn_start_offset = Some(local.last_offset);
    successor.full_response.clear();
    successor.tui_terminal_source_file_identity = None;
    successor.tui_terminal_generation_mtime_ns = None;
    inflight::save_inflight_state(&successor).unwrap();
    let key = custody_test_adapter::retain(local, range)
        .await
        .expect("the production terminal serializer must durably retain the captured body");
    assert_eq!(
        inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &ProviderKind::Codex,
            CANARY_CHANNEL,
            &InflightTurnIdentity::from_state(&successor),
            successor.turn_nonce.as_deref(),
        ),
        GuardedClearOutcome::Cleared,
        "only the exact successor row may be removed",
    );
    assert!(
        inflight::load_inflight_state_read_only_result(&ProviderKind::Codex, CANARY_CHANNEL)
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .shared
            .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    key
}

fn custody_path(fixture: &CanaryFixture, key: &str) -> PathBuf {
    fixture
        .root
        .path()
        .join("runtime/discord_terminal_delivery_custody")
        .join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
}

fn assert_effects_absent(calls: &[String]) {
    assert!(
        calls.iter().all(|call| {
            !["new-session", "kill-session", "send-keys", "paste-buffer"]
                .iter()
                .any(|effect| call.starts_with(effect))
        }),
        "custody refusal produced an effect: {calls:?}",
    );
    assert!(
        crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX)
            .is_empty(),
        "custody refusal may not clean up the captured target",
    );
}

async fn readiness(fixture: &CanaryFixture) -> bool {
    crate::services::discord::router::hosted_tui_promote_readiness_blocked(
        &fixture.shared,
        &ProviderKind::Codex,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .await
}

async fn held_queue(
    fixture: &CanaryFixture,
    api: &crate::services::discord::admin_host_guard::tests::Recorder,
    identities: &[QueueIdentity],
) {
    assert!(
        readiness(fixture).await,
        "durable custody must close real readiness"
    );
    fixture.fake.take_calls();
    let _ = crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    assert!(
        !kickoff(fixture, api).await,
        "a pending custody may not take a queued head"
    );
    assert_eq!(queue_identities(fixture).await, identities);
    assert_effects_absent(&fixture.fake.take_calls());
    assert!(!fixture.root.path().join("fresh-context").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_custody_holds_until_real_resume_then_preserves_fifo_pg() {
    if !isolated(concat!(
        module_path!(),
        "::admitted_custody_holds_until_real_resume_then_preserves_fifo_pg"
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
    let (local, range) = admitted_terminal(&fixture, true).await;
    let key = retain_after_successor(&fixture, &local, &range).await;
    let retained = fs::read(custody_path(&fixture, &key)).unwrap();
    let record: serde_json::Value = serde_json::from_slice(&retained).unwrap();
    assert_eq!(record["payload"]["full_response"], "");
    assert_eq!(record["payload"]["local"]["full_response"], "");
    assert_eq!(record["payload"]["admitted"]["result"], "");
    assert!(record["payload"]["delivery_body"].is_null());
    assert!(
        record["payload"]["empty_recovery_notice"]
            .as_str()
            .is_some_and(|notice| !notice.is_empty())
    );
    target_missing(&fixture, false);
    let identities = restore_instructions(
        &mut fixture,
        vec![
            instruction(CANARY_CHANNEL + 511, FIRST),
            instruction(CANARY_CHANNEL + 512, SECOND),
        ],
    )
    .await;
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(
        &fixture.old_source.path,
    );
    assert!(
        !dedupe::clear_relaunch::launch_blocked(CANARY_TMUX),
        "a valid old source alone must not account for the custody readiness hold"
    );
    held_queue(&fixture, &api, &identities).await;
    assert_eq!(fs::read(custody_path(&fixture, &key)).unwrap(), retained);
    assert_eq!(old_io.stats(), Default::default());
    api.take();
    assert_eq!(
        custody_test_adapter::resume(&fixture.shared, &api.http)
            .await
            .unwrap(),
        1
    );
    let delivered = api.take();
    assert_eq!(
        delivered
            .iter()
            .filter(|call| call.starts_with("POST ")
                && call.contains("/messages")
                && call.contains(BODY))
            .count(),
        1,
        "the real custody adapter must publish its admitted body once: {delivered:?}"
    );
    assert!(
        !custody_path(&fixture, &key).exists(),
        "only the real successful adapter may settle custody"
    );
    // The bounded recovery reader is outside this oracle; the POST proves its source-only body.
    let after_resume = old_io.stats();
    assert_eq!(after_resume.advance_commits, 0);
    assert!(!readiness(&fixture).await);
    assert!(kickoff(&fixture, &api).await);
    let first = fixture.turn_failure_guard().await;
    let fresh = wait_context_after(&fixture, &fixture.context.execution_nonce).await;
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert_eq!(argv.iter().filter(|arg| arg.contains(FIRST)).count(), 1);
    assert!(
        argv.iter()
            .all(|arg| !arg.contains(SECOND) && arg != "resume" && arg != NATIVE_ID)
    );
    assert_eq!(queue_identities(&fixture).await, identities[1..]);
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (_setup, entered, release, boundary) = warm_probe(&fresh, accepted.clone(), calls.clone());
    let path = publish_named_source(&fixture, &fresh, NEW_NATIVE_ID);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if inflight::load_inflight_state_read_only_result(&ProviderKind::Codex, CANARY_CHANNEL)
                .unwrap()
                .is_some_and(|row| {
                    row.turn_nonce.as_deref() == first.token.turn_nonce()
                        && row.session_id.as_deref() == Some(NEW_NATIVE_ID)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fresh Q1 Init must reach its actual row before terminal");
    terminal(&path, "new-clear-turn", "clear relaunch completed");
    wait_completion(&first.token).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = fixture
                .shared
                .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
                .snapshot()
                .await;
            let native = fixture
                .shared
                .core
                .lock()
                .await
                .sessions
                .get(&serenity::ChannelId::new(CANARY_CHANNEL))
                .and_then(|session| session.session_id.clone());
            if snapshot
                .cancel_token
                .as_ref()
                .is_none_or(|token| !Arc::ptr_eq(token, &first.token))
                && native.as_deref() == Some(NEW_NATIVE_ID)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Q1 completion must release its actor and publish its selector");
    first.finish();
    activity::tests::companion_attach_log_watcher(
        &fixture.shared,
        CANARY_CHANNEL,
        CANARY_TMUX,
        &path,
    );
    assert_eq!(
        activity::tests::companion_settled_activity(&fixture.shared, CANARY_CHANNEL)
            .await
            .activity,
        Activity::Idle
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(prompt) = entered.try_recv() {
                assert!(prompt.contains(SECOND));
                break;
            }
            kickoff(&fixture, &api).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("settled custody must allow actual warm FIFO Q2");
    let second = fixture.turn_failure_guard().await;
    release.send(()).unwrap();
    wait_completion(&second.token).await;
    wait_idle(&fixture).await;
    second.finish();
    assert!(
        boundary.try_recv().is_err(),
        "the actual verified warm driver may not park"
    );
    assert_eq!(accepted.lock().unwrap().len(), 1);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "keys:Enter")
            .count(),
        1
    );
    assert!(queue_identities(&fixture).await.is_empty());
    assert_eq!(
        fixture
            .fake
            .take_calls()
            .iter()
            .filter(|call| call.starts_with("new-session"))
            .count(),
        1
    );
    assert_eq!(
        old_io.stats(),
        after_resume,
        "B1 launch must add no old source read or cursor advance after normal recovery"
    );
}

pub(in crate::services::discord) async fn unreadable_retained_custody_case(test: &str) {
    if !isolated(test) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _cli = fixture.cold_cli();
    let (local, range) = admitted_terminal(&fixture, false).await;
    let key = retain_after_successor(&fixture, &local, &range).await;
    let path = custody_path(&fixture, &key);
    fs::write(&path, b"unreadable retained terminal record").unwrap();
    target_missing(&fixture, false);
    let identities =
        restore_instructions(&mut fixture, vec![instruction(CANARY_CHANNEL + 521, FIRST)]).await;
    held_queue(&fixture, &api, &identities).await;
    assert_eq!(
        fs::read(path).unwrap(),
        b"unreadable retained terminal record"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unrelated_custody_does_not_close_captured_absence_pg() {
    if !isolated(concat!(
        module_path!(),
        "::unrelated_custody_does_not_close_captured_absence_pg"
    )) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _cli = fixture.cold_cli();
    let (local, range) = admitted_terminal(&fixture, false).await;
    let mut other = local.clone();
    other.provider = "claude".into();
    let mut other_range = range.clone();
    other_range.source.provider = "claude".into();
    let key = retain_after_successor(&fixture, &other, &other_range).await;
    let retained = fs::read(custody_path(&fixture, &key)).unwrap();
    target_missing(&fixture, false);
    let identities =
        restore_instructions(&mut fixture, vec![instruction(CANARY_CHANNEL + 531, FIRST)]).await;
    assert!(
        !readiness(&fixture).await,
        "coherent other-provider custody is irrelevant to this captured Codex target"
    );
    assert_eq!(queue_identities(&fixture).await, identities);
    assert_eq!(fs::read(custody_path(&fixture, &key)).unwrap(), retained);
    let mut other_channel = local.clone();
    other_channel.channel_id = CANARY_CHANNEL + 900;
    other_channel.watcher_owner_channel_id = Some(other_channel.channel_id);
    other_channel.logical_channel_id = Some(other_channel.channel_id);
    let mut channel_range = range.clone();
    channel_range.source.offset_authority_channel_id = other_channel.channel_id;
    channel_range.source.delivery_channel_id = other_channel.channel_id;
    custody_test_adapter::retain(&other_channel, &channel_range)
        .await
        .unwrap();
    assert!(
        !readiness(&fixture).await,
        "coherent other-channel custody is irrelevant"
    );
    let mut other_tmux = local.clone();
    other_tmux.tmux_session_name = Some("custody-other-local-target".into());
    let mut tmux_range = range.clone();
    tmux_range
        .identity
        .tmux_session_name
        .clone_from(&other_tmux.tmux_session_name);
    tmux_range.source.tmux_session_name = "custody-other-local-target".into();
    custody_test_adapter::retain(&other_tmux, &tmux_range)
        .await
        .unwrap();
    assert!(
        !readiness(&fixture).await,
        "coherent other-tmux custody is irrelevant even when the native file is shared"
    );
    assert_effects_absent(&fixture.fake.take_calls());
}

#[tokio::test(flavor = "current_thread")]
async fn partially_conflicting_custody_keeps_captured_absence_held_pg() {
    if !isolated(concat!(
        module_path!(),
        "::partially_conflicting_custody_keeps_captured_absence_held_pg"
    )) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _cli = fixture.cold_cli();
    let (local, range) = admitted_terminal(&fixture, false).await;
    let mut conflicting = range.clone();
    conflicting.source_file_identity = Some((
        range.source_file_identity.unwrap().0,
        range.source_file_identity.unwrap().1 + 1,
    ));
    let key = retain_after_successor(&fixture, &local, &conflicting).await;
    let retained = fs::read(custody_path(&fixture, &key)).unwrap();
    target_missing(&fixture, false);
    let identities =
        restore_instructions(&mut fixture, vec![instruction(CANARY_CHANNEL + 541, FIRST)]).await;
    held_queue(&fixture, &api, &identities).await;
    assert_eq!(fs::read(custody_path(&fixture, &key)).unwrap(), retained);
}

#[tokio::test(flavor = "current_thread")]
async fn custody_appearing_after_readiness_holds_before_cleanup_pg() {
    if !isolated(concat!(
        module_path!(),
        "::custody_appearing_after_readiness_holds_before_cleanup_pg"
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
    let (local, range) = admitted_terminal(&fixture, false).await;
    let mut successor = local.clone();
    successor.user_msg_id += 10;
    successor.turn_nonce = Some(uuid::Uuid::new_v4().to_string());
    successor.tui_terminal_source_file_identity = None;
    successor.tui_terminal_generation_mtime_ns = None;
    inflight::save_inflight_state(&successor).unwrap();
    assert_eq!(
        inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &ProviderKind::Codex,
            CANARY_CHANNEL,
            &InflightTurnIdentity::from_state(&successor),
            successor.turn_nonce.as_deref()
        ),
        GuardedClearOutcome::Cleared
    );
    target_missing(&fixture, false);
    let identities = restore_instructions(
        &mut fixture,
        vec![
            instruction(CANARY_CHANNEL + 551, FIRST),
            instruction(CANARY_CHANNEL + 552, SECOND),
        ],
    )
    .await;
    assert!(!readiness(&fixture).await);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let _setup = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |_| {
            crate::services::codex::actual_submit_tests::before_pane_effect(Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the test must release its exact boundary");
            }));
            Box::new(())
        }),
    );
    let (hold_tx, hold_rx) = mpsc::channel();
    *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold_tx);
    let _hold_reset = HoldHookReset;
    fixture.fake.take_calls();
    let _ = crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(
        &fixture.old_source.path,
    );
    assert!(kickoff(&fixture, &api).await);
    let guard = fixture.turn_failure_guard().await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                hold_rx.try_recv().is_err(),
                "early hold did not exercise the late custody check"
            );
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the actual provider must reach its precleanup boundary");
    let key = custody_test_adapter::retain(&local, &range).await.unwrap();
    let retained = fs::read(custody_path(&fixture, &key)).unwrap();
    release_tx.send(()).unwrap();
    let mut calls = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            calls.extend(fixture.fake.take_calls());
            assert_effects_absent(&calls);
            if let Ok(held) = hold_rx.try_recv() {
                assert_eq!(held, CANARY_TMUX);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("late custody must preserve a provider hold");
    let snapshot = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &guard.token))
    );
    assert_eq!(
        snapshot
            .intervention_queue
            .iter()
            .map(QueueIdentity::of)
            .collect::<Vec<_>>(),
        identities[1..]
    );
    let row = inflight::load_inflight_state_read_only_result(&ProviderKind::Codex, CANARY_CHANNEL)
        .unwrap()
        .unwrap();
    assert_eq!(row.turn_nonce.as_deref(), guard.token.turn_nonce());
    assert!(row.user_text.contains(FIRST));
    assert_eq!(fs::read(custody_path(&fixture, &key)).unwrap(), retained);
    assert_eq!(old_io.stats(), Default::default());
    guard.token.cancelled.store(true, Ordering::SeqCst);
    wait_idle(&fixture).await;
    calls.extend(fixture.fake.take_calls());
    assert_effects_absent(&calls);
    drop(guard);
}
