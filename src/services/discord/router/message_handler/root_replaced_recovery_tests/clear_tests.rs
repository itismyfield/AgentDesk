use super::*;
use crate::services::discord::commands::control::{
    ClearReport, SoftClearNotifyMode, clear_channel_session_state_with_session_key,
};
use crate::services::turn_orchestrator::{
    ClaimObservation, QueuePersistenceContext, load_channel_pending_dispatch_marker_checked,
    save_channel_pending_dispatch_marker,
};

fn context(case: &Case) -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, &case.shared.token_hash, None)
}

fn clear(case: &Case) -> ClearReport {
    let admitted =
        crate::services::tui_prompt_dedupe::herdr_claim_admission(case.fx.logical(), true).is_ok();
    if admitted {
        // Model the process-wide launch listing on this test's thread-local fast-path flag.
        crate::services::tui_prompt_dedupe::admit_herdr_execution(case.fx.logical(), &case.nonce);
    }
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let _registry = case.fx.rig.registry_as_on_this_thread(
        &endpoint.execution_node,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let key = super::super::super::session_key(&case.fx.owner);
    let http = Arc::new(poise::serenity_prelude::Http::new(""));
    case.fx
        .rt
        .block_on(clear_channel_session_state_with_session_key(
            &http,
            &case.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
            "!clear",
            SoftClearNotifyMode::Suppress,
            Some(&key),
        ))
        .expect("the public clear completes")
}

fn waiting(case: &Case) -> (Intervention, ClaimObservation) {
    let queued = queued_input();
    let channel = ChannelId::new(CHANNEL);
    let observation = case.fx.rt.block_on(async {
        let mailbox = case.shared.mailbox(channel);
        let observed = mailbox.snapshot().await.claim_observation;
        assert!(
            mailbox
                .enqueue(queued.clone(), context(case))
                .await
                .enqueued
        );
        assert_eq!(mailbox.snapshot().await.intervention_queue.len(), 1);
        observed
    });
    let mut marker = queued.clone();
    marker.message_id = MessageId::new(queued.message_id.get() + 100);
    marker.source_message_ids = vec![marker.message_id];
    save_channel_pending_dispatch_marker(
        &ProviderKind::Claude,
        &case.shared.token_hash,
        channel,
        &marker,
        None,
    )
    .unwrap();
    assert!(
        load_channel_pending_dispatch_marker_checked(
            &ProviderKind::Claude,
            &case.shared.token_hash,
            channel
        )
        .unwrap()
        .is_some()
    );
    crate::services::discord::catch_up::retry_state::arm_catch_up_retry_for_tests(
        &case.shared,
        channel,
        queued.message_id.get(),
    );
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &ProviderKind::Claude,
        &case.shared.token_hash,
        channel,
    );
    assert_eq!(
        disk.len(),
        1,
        "the discarded queue existed durably before clear"
    );
    (queued, observation)
}

fn discarded(case: &Case, queued: Intervention, observed: ClaimObservation, report: &ClearReport) {
    assert!(
        report.reply().contains("대기 메시지 2건을 비웠어요."),
        "{}",
        report.reply()
    );
    discarded_state(case, queued, observed);
}

fn discarded_state(case: &Case, queued: Intervention, observed: ClaimObservation) {
    let channel = ChannelId::new(CHANNEL);
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &ProviderKind::Claude,
        &case.shared.token_hash,
        channel,
    );
    assert!(disk.is_empty(), "the old queue is absent on disk");
    assert!(
        load_channel_pending_dispatch_marker_checked(
            &ProviderKind::Claude,
            &case.shared.token_hash,
            channel
        )
        .unwrap()
        .is_none()
    );
    assert!(!case.shared.catch_up_retry_pending.contains_key(&channel));
    case.fx.rt.block_on(async {
        let snapshot = case.shared.mailbox(channel).snapshot().await;
        assert!(snapshot.intervention_queue.is_empty());
        assert!(snapshot.pending_user_dispatch.is_none());
        let taken = tokio::time::timeout(
            Duration::from_secs(15),
            crate::services::discord::mailbox_take_next_automatic_intervention(
                &case.shared,
                &ProviderKind::Claude,
                channel,
            ),
        )
        .await
        .unwrap();
        assert!(taken.persistence_error.is_none());
        assert!(
            taken.intervention.is_none(),
            "actual dequeue cannot dispatch the discarded Q"
        );
        let late = case
            .shared
            .mailbox(channel)
            .enqueue_observed(queued, context(case), Some(observed))
            .await;
        assert!(
            !late.enqueued,
            "the pre-clear observer cannot restore the discarded Q"
        );
        assert!(
            case.shared
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
    });
}

fn recover_after_clear(case: &Case, original: &HostedRecord, effects: &[Value]) {
    assert_eq!(
        &case.fx.record(),
        original,
        "clear preserves N1 before observations recover"
    );
    // This overwrites even the unreadable process reply and removes the new-provider observation.
    case.fx.rig.restore_as_new_shell(&context_of(&case.nonce));
    case.fx.rig.exists(SHELL, false);
    case.fx.rig.exists(PROVIDER, false);
    case.fx.rig.show_panes(&[PANE]);
    assert!(
        crate::services::tui_prompt_dedupe::herdr_execution_listed(case.fx.logical()),
        "the process already lists the replaced execution before its background pass"
    );
    let passes = crate::services::discord::recovery_engine::herdr_reader::PASSES.with(|v| v.get());
    case.pass();
    assert!(
        crate::services::discord::recovery_engine::herdr_reader::PASSES.with(|v| v.get()) > passes
    );
    let HostedRecord::Known(mut expected) = original.clone() else {
        panic!("Bound N1");
    };
    expected.state = HostedState::Retired;
    assert_eq!(
        case.fx.record(),
        HostedRecord::Known(expected),
        "background recovery changes only N1 state after observations normalize"
    );
    assert!(
        recovery::CAS_ATTEMPTS.with(|count| count.get() > 0),
        "the real background pass proved and attempted the captured execution retirement"
    );
    assert_eq!(case.cleanups(), 1);
    assert!(!case.hold_path().exists());
    assert_eq!(case.effects(), effects);
}

#[test]
fn rr_public_clear_with_termination_disabled_retires_and_reports_empty_queue_pg() {
    let case = Case::launched("rr-clear-disabled");
    assert!(
        crate::services::tui_prompt_dedupe::herdr_claim_admission(case.fx.logical(), true).is_ok(),
        "the real launch still admits N1 before clear observes its replacement"
    );
    let _intake = IntakeGate::configure(&case);
    assert!(!crate::services::discord::herdr_terminate::terminate_enabled());
    let original = case.fx.record();
    let effects = case.effects();
    let report = clear(&case);
    assert!(
        report.reply().contains("대기 메시지 0건을 비웠어요."),
        "{}",
        report.reply()
    );
    assert!(
        !report.reply().contains("정리하지 못했어요"),
        "{}",
        report.reply()
    );
    assert_eq!(
        case.fx.row(),
        Some(HostedState::Retired),
        "public clear must retire the proven replaced-root execution even when termination is disabled"
    );
    let (HostedRecord::Known(before), HostedRecord::Known(after)) = (original, case.fx.record())
    else {
        panic!("known execution");
    };
    assert_eq!(before.execution_nonce, after.execution_nonce);
    assert_eq!(
        before.expected, after.expected,
        "clear never overwrites launch evidence"
    );
    assert_eq!(case.cleanups(), 1);
    assert!(!case.hold_path().exists());
    assert_eq!(
        case.effects(),
        effects,
        "public recovery clear has no pane mutation"
    );
    crate::services::claude::herdr_turn::hold(&case.nonce).unwrap();
    assert!(
        case.hold_path().exists(),
        "the same Retired N1 needs cleanup again"
    );
    let repeated = clear(&case);
    assert!(
        !repeated.reply().contains("정리하지 못했어요"),
        "{}",
        repeated.reply()
    );
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert!(
        !case.hold_path().exists(),
        "repeated Retired clear durably clears the same N1 hold"
    );
    assert_eq!(
        case.effects(),
        effects,
        "a repeated Retired clear has no managed fallback"
    );
}

#[test]
fn rr_public_clear_missing_holds_execution_but_discards_waiting_queue_pg() {
    let case = Case::launched("rr-clear-missing");
    let _intake = IntakeGate::configure(&case);
    let mut live = (*crate::config_live_reload::current().unwrap()).clone();
    live.runtime.herdr_terminate_enabled = Some(false);
    crate::config_live_reload::install(live);
    let original = case.fx.record();
    let effects = case.effects();
    waiting(&case);
    case.fx.rig.show_panes(&[]);
    let report = clear(&case);
    assert!(
        report
            .reply()
            .contains("이전 세션 프로세스의 종료를 확인하지 못했어요."),
        "{}",
        report.reply()
    );
    assert_eq!(
        case.fx.record(),
        original,
        "disabled legacy termination leaves the original execution unchanged"
    );
    assert_eq!(case.cleanups(), 0);
    assert!(case.hold_path().exists());
    assert_eq!(case.effects(), effects);
    legacy_discarded(&case, &report);
}

#[test]
fn rr_public_clear_unproven_provider_or_read_failure_keeps_n1_and_discards_q_pg() {
    for (change, tag) in [
        (Change::ProviderAlive, "rr-clear-provider"),
        (Change::NewProvider, "rr-clear-new-provider"),
        (Change::ReadFailed, "rr-clear-unreadable"),
    ] {
        std::thread::spawn(move || {
            let case = Case::launched(tag);
            let _intake = IntakeGate::configure(&case);
            let original = case.fx.record();
            let effects = case.effects();
            let (queued, observed) = waiting(&case);
            if matches!(change, Change::NewProvider | Change::ReadFailed) {
                case.fx.rig.exists(PROVIDER, true);
                case.pass();
                assert!(ATTACHES.with(|count| count.get() > 0));
                assert_eq!(
                    case.fx.record(),
                    original,
                    "the real pass identified RootReplaced but kept the live provider Held"
                );
            }
            match change {
                Change::ProviderAlive => case.fx.rig.exists(PROVIDER, true),
                Change::NewProvider => {
                    let next = case.fx.rig.context("fedcba9876543210fedcba9876543210");
                    case.fx.rig.run_provider(&next, true);
                    case.fx.rig.exists(PROVIDER, true);
                    case.fx.rig.foreground(&[PROVIDER]);
                }
                Change::ReadFailed => {
                    case.fx.rig.exists(PROVIDER, false);
                    case.fx.rig.answer(
                        "pane.process_info",
                        json!({"type":"pane_process_info", "process_info":{"pane_id":PANE}}),
                    );
                }
                _ => unreachable!(),
            }
            let report = clear(&case);
            let expected = if matches!(change, Change::ProviderAlive) {
                "이전 Herdr 실행은 정리하지 못했어요."
            } else {
                "이전 세션 프로세스의 종료를 확인하지 못했어요."
            };
            assert!(report.reply().contains(expected), "{}", report.reply());
            assert_eq!(case.fx.record(), original);
            assert_eq!(case.cleanups(), 0);
            assert!(case.hold_path().exists());
            assert_eq!(case.effects(), effects);
            recover_after_clear(&case, &original, &effects);
            if matches!(change, Change::ProviderAlive) {
                discarded(&case, queued, observed, &report);
            } else {
                legacy_discarded(&case, &report);
            }
        })
        .join()
        .unwrap();
    }
}

#[test]
fn rr_public_clear_preserves_original_actor_and_inflight_while_discarding_q_pg() {
    for (inflight, tag) in [(false, "rr-clear-active"), (true, "rr-clear-inflight")] {
        std::thread::spawn(move || {
            let case = Case::launched(tag);
            let _intake = IntakeGate::configure(&case);
            let original = case.fx.record();
            let effects = case.effects();
            let active = Arc::new(CancelToken::new());
            let path = crate::services::discord::inflight::inflight_state_path(
                &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
                &ProviderKind::Claude,
                CHANNEL,
            );
            let bytes = if inflight {
                let row = crate::services::discord::inflight::InflightTurnState::new(
                    ProviderKind::Claude,
                    CHANNEL,
                    None,
                    7_186_901,
                    7_186_902,
                    7_186_903,
                    "original execution".into(),
                    Some("original-native".into()),
                    Some(case.fx.logical().into()),
                    None,
                    None,
                    0,
                );
                crate::services::discord::inflight::save_inflight_state(&row).unwrap();
                Some(std::fs::read(&path).unwrap())
            } else {
                assert!(
                    case.fx
                        .rt
                        .block_on(crate::services::discord::mailbox_try_start_turn(
                            &case.shared,
                            ChannelId::new(CHANNEL),
                            active.clone(),
                            UserId::new(7_186_901),
                            MessageId::new(7_186_902)
                        ))
                );
                crate::services::discord::increment_global_active(
                    &case.shared,
                    "clear_negative_fixture",
                );
                None
            };
            let (queued, observed) = waiting(&case);
            let report = clear(&case);
            assert!(
                report
                    .reply()
                    .contains("이전 Herdr 실행은 정리하지 못했어요."),
                "{}",
                report.reply()
            );
            assert_eq!(case.fx.record(), original);
            assert_eq!(case.cleanups(), 0);
            assert!(case.hold_path().exists());
            assert_eq!(case.effects(), effects);
            if let Some(bytes) = bytes {
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    bytes,
                    "clear keeps original inflight bytes"
                );
            } else {
                let state = case
                    .fx
                    .rt
                    .block_on(crate::services::discord::mailbox_snapshot(
                        &case.shared,
                        ChannelId::new(CHANNEL),
                    ));
                assert!(
                    state
                        .cancel_token
                        .as_ref()
                        .is_some_and(|token| Arc::ptr_eq(token, &active))
                );
                assert_eq!(
                    state.active_user_message_id,
                    Some(MessageId::new(7_186_902))
                );
                assert!(!active.cancelled.load(Ordering::SeqCst));
            }
            if inflight {
                assert_eq!(
                    crate::services::discord::inflight::clear_inflight_state_if_matches(
                        &ProviderKind::Claude,
                        CHANNEL,
                        7_186_902
                    ),
                    crate::services::discord::inflight::GuardedClearOutcome::Cleared
                );
                assert!(
                    !path.exists(),
                    "the original inflight settles through its exact owner guard"
                );
            } else {
                let finished = case.fx.rt.block_on(
                    case.shared
                        .mailbox(ChannelId::new(CHANNEL))
                        .finish_turn(context(&case)),
                );
                assert!(
                    finished
                        .removed_token
                        .as_ref()
                        .is_some_and(|token| Arc::ptr_eq(token, &active)),
                    "normal settlement releases exactly the preserved original actor"
                );
                assert!(crate::services::discord::saturating_decrement_global_active(&case.shared));
            }
            recover_after_clear(&case, &original, &effects);
            discarded(&case, queued, observed, &report);
        })
        .join()
        .unwrap();
    }
}

#[test]
fn rr_public_clear_nonce_replacement_keeps_successor_and_discards_only_q_pg() {
    let case = Case::launched("rr-clear-nonce");
    let _intake = IntakeGate::configure(&case);
    let effects = case.effects();
    let (queued, observed) = waiting(&case);
    let HostedRecord::Known(mut next) = case.fx.record() else {
        panic!("Bound N1");
    };
    next.execution_nonce = "rr-clear-successor-n2".into();
    next.source_ref.execution_nonce = next.execution_nonce.clone();
    next.expected.as_mut().unwrap().binding_nonce = next.execution_nonce.clone();
    let expected = next.clone();
    let pool = case.fx.pool.clone();
    let key = super::super::super::session_key(&case.fx.owner);
    let reached = Arc::new(AtomicBool::new(false));
    let ran = reached.clone();
    recovery::BEFORE_CAS.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                ran.store(true, Ordering::SeqCst);
                canonical_fixture::set_raw(&pool, &key, Some(serde_json::to_value(next).unwrap()))
                    .await;
            })
        }))
    });
    let report = clear(&case);
    assert!(
        reached.load(Ordering::SeqCst),
        "clear reached the actual fenced recovery stage"
    );
    assert!(
        report
            .reply()
            .contains("이전 Herdr 실행은 정리하지 못했어요."),
        "{}",
        report.reply()
    );
    assert_eq!(case.fx.record(), HostedRecord::Known(expected.clone()));
    assert_eq!(case.cleanups(), 0);
    assert!(case.hold_path().exists());
    assert_eq!(case.effects(), effects);
    case.fx.rig.show_panes(&[]);
    case.pass();
    assert_eq!(
        case.fx.record(),
        HostedRecord::Known(expected),
        "the later pass cannot treat the successor's initial Missing as N1 RootReplaced"
    );
    assert_eq!(case.cleanups(), 0);
    assert!(case.hold_path().exists());
    assert_eq!(case.effects(), effects);
    discarded(&case, queued, observed, &report);
}

#[test]
fn rr_public_clear_failed_queue_publish_reports_partial_even_after_retirement_pg() {
    use crate::services::turn_orchestrator::test_support::{fail_queue_saves, queue_save_faults};
    let case = Case::launched("rr-clear-partial");
    let _intake = IntakeGate::configure(&case);
    let effects = case.effects();
    let (queued, observed) = waiting(&case);
    fail_queue_saves(ChannelId::new(CHANNEL), 2);
    let report = clear(&case);
    assert_eq!(
        queue_save_faults(ChannelId::new(CHANNEL), true),
        0,
        "both real queue publish attempts ran"
    );
    let reply = report.reply();
    assert!(
        reply.contains("대기 메시지 수를 확인하지 못했어요."),
        "{reply}"
    );
    assert!(reply.contains("일부 정리를 완료하지 못했어요."), "{reply}");
    assert!(reply.contains("이전 Herdr 실행을 정리했어요."), "{reply}");
    assert!(
        !reply.contains("세션을 초기화했어요."),
        "partial persistence cannot be reported as full session initialization"
    );
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.cleanups(), 1);
    assert!(!case.hold_path().exists());
    case.pass();
    assert_eq!(case.cleanups(), 1);
    assert_eq!(case.effects(), effects);
    discarded_state(&case, queued, observed);
}

#[test]
fn rr_public_clear_selects_old_originless_json_and_preserves_modern_pending_pg() {
    use crate::services::discord::tui_direct_pending_start::{
        self as pending, TuiDirectPendingStart,
    };
    let case = Case::launched("rr-clear-old-json");
    let _intake = IntakeGate::configure(&case);
    let effects = case.effects();
    let original = case.fx.record();
    case.fx.rig.exists(PROVIDER, true);
    let old = TuiDirectPendingStart {
        provider: "claude".into(),
        channel_id: CHANNEL,
        tmux_session_name: case.fx.logical().into(),
        prompt_text: "old held synthetic input".into(),
        anchor_message_id: 7_186_951,
        lease_relay_owner: "bridge_adapter".into(),
        captured_source: Some((
            case.fx
                .cwd
                .path()
                .join("old-held.jsonl")
                .display()
                .to_string(),
            17,
        )),
        ..Default::default()
    };
    let modern = TuiDirectPendingStart {
        anchor_message_id: old.anchor_message_id + 1,
        hosted_source_checked: true,
        ..old.clone()
    };
    let store = crate::services::discord::runtime_store::tui_direct_pending_start_root().unwrap();
    assert!(store.starts_with(crate::config::runtime_root().unwrap()));
    std::fs::create_dir_all(&store).unwrap();
    let path = |record: &TuiDirectPendingStart| {
        store.join(format!(
            "{}_{}_{}.json",
            record.provider, record.channel_id, record.anchor_message_id
        ))
    };
    let mut raw = serde_json::to_value(&old).unwrap();
    raw.as_object_mut().unwrap().remove("hosted_source_checked");
    raw.as_object_mut().unwrap().remove("hosted_source");
    std::fs::write(path(&old), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
    let modern_bytes = serde_json::to_vec_pretty(&modern).unwrap();
    std::fs::write(path(&modern), &modern_bytes).unwrap();
    let loaded = pending::load_all();
    assert!(
        loaded
            .iter()
            .any(|record| record.anchor_message_id == old.anchor_message_id
                && !record.hosted_source_checked
                && record.hosted_source.is_none()),
        "actual old field-absent JSON reached the production decoder"
    );
    assert!(loaded.iter().any(
        |record| record.anchor_message_id == modern.anchor_message_id
            && record.hosted_source_checked
    ));
    let inflight = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        CHANNEL,
        None,
        7_186_901,
        0,
        7_186_903,
        "unknown original anchor".into(),
        Some("original-native".into()),
        Some(case.fx.logical().into()),
        None,
        None,
        0,
    );
    crate::services::discord::inflight::save_inflight_state(&inflight).unwrap();
    let inflight_path = crate::services::discord::inflight::inflight_state_path(
        &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        CHANNEL,
    );
    let inflight_bytes = std::fs::read(&inflight_path).unwrap();
    let unknown = clear(&case);
    assert!(
        unknown
            .reply()
            .contains("대기 메시지 수를 확인하지 못했어요."),
        "{}",
        unknown.reply()
    );
    assert!(
        path(&old).exists(),
        "an unknown active anchor cannot authorize legacy JSON discard"
    );
    assert_eq!(std::fs::read(&inflight_path).unwrap(), inflight_bytes);
    assert_eq!(std::fs::read(path(&modern)).unwrap(), modern_bytes);
    assert_eq!(case.fx.record(), original);
    assert_eq!(case.effects(), effects);
    std::fs::remove_file(inflight_path).unwrap();
    let report = clear(&case);
    let reply = report.reply();
    assert!(reply.contains("대기 메시지 1건을 비웠어요."), "{reply}");
    assert!(
        reply.contains("이전 Herdr 실행은 정리하지 못했어요."),
        "{reply}"
    );
    assert!(
        !path(&old).exists(),
        "the public clear selected the original originless waiting JSON"
    );
    assert_eq!(
        std::fs::read(path(&modern)).unwrap(),
        modern_bytes,
        "the same clear preserves the modern pending record byte-for-byte"
    );
    assert!(
        pending::persist(&old).is_err(),
        "a late original pending writer cannot resurrect the discarded old JSON"
    );
    assert_eq!(case.fx.record(), original);
    assert_eq!(case.cleanups(), 0);
    assert!(case.hold_path().exists());
    assert_eq!(case.effects(), effects);
}

fn legacy_discarded(case: &Case, report: &ClearReport) {
    assert!(
        report.reply().contains("대기 메시지 2건을 비웠어요."),
        "{}",
        report.reply()
    );
    let channel = ChannelId::new(CHANNEL);
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &ProviderKind::Claude,
        &case.shared.token_hash,
        channel,
    );
    assert!(disk.is_empty());
    assert!(
        load_channel_pending_dispatch_marker_checked(
            &ProviderKind::Claude,
            &case.shared.token_hash,
            channel
        )
        .unwrap()
        .is_none()
    );
    assert!(!case.shared.catch_up_retry_pending.contains_key(&channel));
    case.fx.rt.block_on(async {
        assert!(
            case.shared
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
        let taken = tokio::time::timeout(
            Duration::from_secs(15),
            crate::services::discord::mailbox_take_next_automatic_intervention(
                &case.shared,
                &ProviderKind::Claude,
                channel,
            ),
        )
        .await
        .unwrap();
        assert!(taken.persistence_error.is_none());
        assert!(
            taken.intervention.is_none(),
            "legacy clear discarded the selected waiting input"
        );
    });
}
