//! Production reader recovery over real Herdr and PostgreSQL.
use super::*;
use crate::services::discord::recovery_engine::herdr_reader;
use crate::services::session_host::herdr_terminate_real_tests as real;
use real::missing_marker_rig::{CHANNEL, Case, TOKEN, child};
impl Case {
    pub(crate) fn pass(&self) {
        let _enter = self.rt.enter();
        let before = self.observation();
        herdr_reader::reconnect_restarted_herdr_with_shared(&self.shared, &ProviderKind::Claude);
        eprintln!(
            "MM_RECOVERY {:?} counts={:?}",
            super::result_for_test(&before),
            herdr_reader::reconnect_counts()
        );
    }
}
#[test]
fn mm_orphan_retires_with_another_live_provider_pg() {
    if !child(
        "mm_orphan_retires_with_another_live_provider_pg",
        module_path!(),
    ) {
        return;
    }
    let mut case = Case::new();
    let cwd = case.real.work("same-cwd");
    let other = case.rt.block_on(real::launch(
        &mut case.real,
        &case.pool,
        TOKEN,
        CHANNEL + 1,
        "AgentDesk-claude-mm-other",
        cwd,
    ));
    let other_record = case.rt.block_on(real::stored(&case.pool, &other.key));
    case.ended();
    case.lose_marker();
    case.rt.block_on(
        crate::services::discord::herdr_terminate::test_queue::queue_original(
            &case.shared,
            ChannelId::new(CHANNEL),
        ),
    );
    let before = case.record();
    let panes = case.real.panes();
    case.pass();
    let mut retired = before.clone();
    retired.state = HostedState::Retired;
    assert_eq!(case.record(), retired);
    assert_eq!(
        case.rt.block_on(real::stored(&case.pool, &other.key)),
        other_record
    );
    assert!(real::alive(&other.provider));
    assert_eq!(case.real.panes(), panes);
    assert_eq!(
        case.rt.block_on(
            crate::services::discord::herdr_terminate::test_queue::queued_texts(
                &case.shared,
                ChannelId::new(CHANNEL)
            )
        ),
        vec![crate::services::discord::herdr_terminate::test_queue::ORIGINAL]
    );
    assert!(!crate::services::claude::herdr_turn::input_held(&case.execution.nonce).unwrap());
}

#[test]
fn mm_exact_alive_lone_shell_keeps_pg() {
    if !child("mm_exact_alive_lone_shell_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.signal(libc::SIGSTOP);
    case.shell();
    case.lose_marker();
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
    assert!(real::alive(&case.execution.provider));
    case.signal(libc::SIGCONT);
}

#[test]
fn mm_unverified_foreground_keeps_pg() {
    if !child("mm_unverified_foreground_keeps_pg", module_path!()) {
        return;
    }
    let mut case = Case::new();
    case.ended();
    // A second execution without this nonce's context occupies the original pane.
    case.occupy_unverified();
    case.lose_marker();
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
    case.real.recorded.retain(|p| real::alive(p));
}

#[test]
fn mm_unreadable_recorded_pid_keeps_pg() {
    if !child("mm_unreadable_recorded_pid_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    let mut record = case.record();
    record.expected.as_mut().unwrap().provider_process.pid = u32::MAX;
    case.store(&record);
    case.lose_marker();
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
}

#[test]
fn mm_final_mailbox_change_keeps_pg() {
    if !child("mm_final_mailbox_change_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    let (row, hold) = (case.observation(), case.hold_bytes());
    let shared = case.shared.clone();
    AFTER_FINAL_PROOF.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                shared
                    .mailbox(ChannelId::new(CHANNEL))
                    .restore_active_turn(
                        Arc::new(CancelToken::new()),
                        serenity::model::id::UserId::new(78_780_032),
                        serenity::model::id::MessageId::new(78_780_033),
                    )
                    .await;
            })
        }))
    });
    case.pass();
    assert!(
        AFTER_FINAL_PROOF.with(|slot| slot.borrow().is_none()),
        "final proof boundary reached"
    );
    case.assert_kept(&row, &hold);
    assert!(
        case.rt
            .block_on(case.shared.mailbox(ChannelId::new(CHANNEL)).snapshot())
            .cancel_token
            .is_some()
    );
}

#[test]
fn mm_final_inflight_change_keeps_pg() {
    if !child("mm_final_inflight_change_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    let (row, hold) = (case.observation(), case.hold_bytes());
    AFTER_FINAL_PROOF.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                let row = crate::services::discord::inflight::InflightTurnState::new(
                    ProviderKind::Claude,
                    CHANNEL,
                    None,
                    7,
                    31,
                    32,
                    "new input after proof".into(),
                    None,
                    None,
                    None,
                    None,
                    0,
                );
                crate::services::discord::inflight::save_inflight_state(&row).unwrap();
            })
        }))
    });
    case.pass();
    assert!(
        AFTER_FINAL_PROOF.with(|slot| slot.borrow().is_none()),
        "final proof boundary reached"
    );
    case.assert_kept(&row, &hold);
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            CHANNEL
        )
        .is_some()
    );
}

#[test]
fn mm_final_current_change_keeps_successor_pg() {
    if !child("mm_final_current_change_keeps_successor_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    let mut next = case.record();
    next.execution_nonce = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
    next.expected.as_mut().unwrap().binding_nonce = next.execution_nonce.clone();
    next.source_ref.execution_nonce = next.execution_nonce.clone();
    let (pool, key, successor) = (case.pool.clone(), case.execution.key.clone(), next.clone());
    AFTER_FINAL_PROOF.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE session_key=$2")
                    .bind(serde_json::to_value(successor).unwrap())
                    .bind(key)
                    .execute(&pool)
                    .await
                    .unwrap();
            })
        }))
    });
    let hold = case.hold_bytes();
    case.pass();
    assert!(
        AFTER_FINAL_PROOF.with(|slot| slot.borrow().is_none()),
        "final proof boundary reached"
    );
    assert_eq!(case.record(), next);
    assert_eq!(CAS_ATTEMPTS.with(std::cell::Cell::get), 0);
    assert_eq!(case.hold_bytes(), hold);
}

#[test]
fn mm_active_mailbox_keeps_pg() {
    if !child("mm_active_mailbox_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    case.rt.block_on(async {
        case.shared
            .mailbox(ChannelId::new(CHANNEL))
            .restore_active_turn(
                Arc::new(CancelToken::new()),
                serenity::model::id::UserId::new(31),
                serenity::model::id::MessageId::new(32),
            )
            .await;
    });
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
}

#[test]
fn mm_inflight_keeps_pg() {
    if !child("mm_inflight_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    let row = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        CHANNEL,
        None,
        7,
        31,
        32,
        "active input".into(),
        None,
        None,
        None,
        None,
        0,
    );
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
}

#[test]
fn mm_initial_root_replaced_is_outside_candidate_pg() {
    if !child(
        "mm_initial_root_replaced_is_outside_candidate_pg",
        module_path!(),
    ) {
        return;
    }
    let case = Case::new();
    case.ended();
    let mut record = case.record();
    record.expected.as_mut().unwrap().root.start = "linux-ticks:1".into();
    case.store(&record);
    case.lose_marker();
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
}

#[test]
fn mm_malformed_marker_keeps_pg() {
    if !child("mm_malformed_marker_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    std::fs::write(case.marker(), "unknown-host").unwrap();
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
}

#[test]
fn mm_unreadable_marker_keeps_pg() {
    if !child("mm_unreadable_marker_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    std::fs::create_dir(case.marker()).unwrap();
    let (row, hold) = (case.observation(), case.hold_bytes());
    case.pass();
    case.assert_kept(&row, &hold);
}

#[test]
fn mm_final_new_dispatch_keeps_pg() {
    if !child("mm_final_new_dispatch_keeps_pg", module_path!()) {
        return;
    }
    let case = Case::new();
    case.ended();
    case.lose_marker();
    let (row, hold) = (case.observation(), case.hold_bytes());
    let shared = case.shared.clone();
    let taken = Arc::new(std::sync::Mutex::new(None));
    let keep = taken.clone();
    AFTER_FINAL_PROOF.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                super::super::test_queue::queue_original(&shared, ChannelId::new(CHANNEL)).await;
                let selection = shared
                    .mailbox(ChannelId::new(CHANNEL))
                    .take_next_soft(crate::services::discord::queue_persistence_context(
                        &shared,
                        &ProviderKind::Claude,
                        ChannelId::new(CHANNEL),
                    ))
                    .await;
                assert!(selection.intervention.is_some() && selection.dispatch_lease.is_some());
                *keep.lock().unwrap() = Some(selection);
            })
        }))
    });
    case.pass();
    assert!(AFTER_FINAL_PROOF.with(|slot| slot.borrow().is_none()));
    case.assert_kept(&row, &hold);
    assert!(
        case.rt
            .block_on(async {
                case.shared
                    .mailbox(ChannelId::new(CHANNEL))
                    .snapshot()
                    .await
            })
            .pending_user_dispatch
            .is_some()
    );
}

#[test]
fn mm_replaced_provider_stamp_retires_old_identity_pg() {
    if !child(
        "mm_replaced_provider_stamp_retires_old_identity_pg",
        module_path!(),
    ) {
        return;
    }
    let case = Case::new();
    case.signal(libc::SIGSTOP);
    case.shell();
    // A stale recorded start models PID reuse; the OS reads the current process unchanged.
    let mut record = case.record();
    record.expected.as_mut().unwrap().provider_process.start = "linux-ticks:1".into();
    case.store(&record);
    case.lose_marker();
    case.pass();
    record.state = HostedState::Retired;
    assert_eq!(case.record(), record);
    assert!(real::alive(&case.execution.provider));
    case.signal(libc::SIGCONT);
}
