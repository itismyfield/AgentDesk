use super::*;
use std::sync::atomic::Ordering;

struct PauseReset;

impl Drop for PauseReset {
    fn drop(&mut self) {
        synthetic_start::bridge_handoff::ADMISSION_PAUSE
            .lock()
            .unwrap()
            .take();
        synthetic_start::bridge_handoff::PREPARE_PAUSE
            .lock()
            .unwrap()
            .take();
    }
}

fn inflight_bytes(fixture: &Fixture) -> Vec<u8> {
    let root = inflight::inflight_runtime_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, fixture.channel.get());
    std::fs::read(path).unwrap()
}

async fn raw_claim(
    fixture: &Fixture,
    record: &pending::TuiDirectPendingStart,
    lease: &ExternalInputRelayLease,
    owner: ExternalInputRelayOwner,
) -> bool {
    let (claim, _) = synthetic_start::claim_tui_direct_synthetic_turn_inner::<false>(
        &fixture.shared,
        &ProviderKind::Claude,
        fixture.channel,
        &fixture.logical,
        (
            &record.prompt_text,
            MessageId::new(record.anchor_message_id),
        ),
        lease,
        (
            record.captured_source.as_ref(),
            record.native_turn_id.as_deref(),
        ),
    )
    .await;
    assert_eq!(claim.relay_owner, owner);
    claim.claimed
}

async fn entered(notify: &tokio::sync::Notify, stage: &str) {
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .unwrap_or_else(|_| panic!("the valid production path did not reach {stage}"));
}

fn assert_n1_admitted(fixture: &Fixture) {
    let admitted = dedupe::herdr_claim_admission(&fixture.logical, true)
        .unwrap()
        .unwrap();
    assert_eq!(
        admitted.get(&fixture.logical),
        Some(&(N1.to_owned(), true)),
        "the race changes the hosted row, not the admission precondition"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn persisted_adoption_refresh_rechecks_hosted_identity_at_its_actual_save_pg() {
    Box::pin(refresh_case(false, 7_186_802)).await;
    Box::pin(refresh_case(true, 7_186_812)).await;
}

async fn refresh_case(replace: bool, id: u64) {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(id);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let fixture = Fixture::new(root.path(), channel).await;
    let original = fixture.pending(N1, &fixture.source, id + 1);
    let lease = fixture.publish_pending_lease(&original);
    assert!(
        raw_claim(
            &fixture,
            &original,
            &lease,
            ExternalInputRelayOwner::TmuxWatcher
        )
        .await
    );
    let before = inflight_bytes(&fixture);
    let row = inflight::load_inflight_state_read_only(&ProviderKind::Claude, id).unwrap();
    assert_eq!(row.hosted_execution_nonce.as_deref(), Some(N1));
    assert_eq!(row.external_turn_id, lease.turn_id);
    let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, channel)
        .await
        .cancel_token
        .unwrap();
    let active_count = fixture.shared.restart.global_active.load(Ordering::Acquire);
    let _reset = PauseReset;
    let outer = Arc::new(tokio::sync::Notify::new());
    let outer_resume = Arc::new(tokio::sync::Notify::new());
    let final_stage = Arc::new(tokio::sync::Notify::new());
    let final_resume = Arc::new(tokio::sync::Notify::new());
    *synthetic_start::bridge_handoff::ADMISSION_PAUSE
        .lock()
        .unwrap() = Some((id, outer.clone(), outer_resume.clone()));
    let change = async {
        entered(&outer, "persisted actor admission").await;
        // Preparation already consumed this slot; only refresh's final stage can take it now.
        *synthetic_start::bridge_handoff::PREPARE_PAUSE
            .lock()
            .unwrap() = Some((id, final_stage.clone(), final_resume.clone()));
        outer_resume.notify_one();
        entered(&final_stage, "the actual PersistedAdoption refresh save").await;
        let current = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
        assert!(Arc::ptr_eq(current.cancel_token.as_ref().unwrap(), &actor));
        assert_eq!(
            current.active_user_message_id,
            Some(MessageId::new(original.anchor_message_id))
        );
        assert_eq!(inflight_bytes(&fixture), before);
        assert!(
            fixture
                .shared
                .session_transition_lock(channel)
                .try_lock_owned()
                .is_err()
        );
        assert_n1_admitted(&fixture);
        let expected = if replace {
            serde_json::to_value(fixture.replace_execution(HostedState::Bound, N2).await).unwrap()
        } else {
            fixture.hosted_json().await
        };
        assert_eq!(fixture.hosted_json().await, expected);
        final_resume.notify_one();
        expected
    };
    let (claimed, expected) = tokio::join!(
        raw_claim(
            &fixture,
            &original,
            &lease,
            ExternalInputRelayOwner::TmuxWatcher
        ),
        change
    );
    assert_eq!(
        claimed, !replace,
        "a persisted refresh must refuse the changed hosted identity"
    );
    let current = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
    assert!(Arc::ptr_eq(current.cancel_token.as_ref().unwrap(), &actor));
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::Acquire),
        active_count
    );
    if replace {
        assert_eq!(
            inflight_bytes(&fixture),
            before,
            "a refused refresh must preserve the original row"
        );
    }
    assert_eq!(fixture.hosted_json().await, expected);
    assert_n1_admitted(&fixture);
    fixture.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn dormant_resume_rechecks_hosted_identity_after_actual_actor_readmission_pg() {
    Box::pin(dormant_case(false, 7_186_822)).await;
    Box::pin(dormant_case(true, 7_186_832)).await;
}

async fn dormant_case(replace: bool, id: u64) {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = ChannelId::new(id);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
    let mut fixture = Fixture::new(root.path(), channel).await;
    fixture.remove_watcher_and_prepare_poll_owner();
    let original = fixture.pending(N1, &fixture.source, id + 1);
    let mut lease = fixture.publish_pending_lease(&original);
    lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
    let lease = dedupe::record_external_input_turn_lease("claude", &fixture.logical, lease);
    assert!(
        raw_claim(
            &fixture,
            &original,
            &lease,
            ExternalInputRelayOwner::BridgeAdapter
        )
        .await
    );
    let row = inflight::load_inflight_state_read_only(&ProviderKind::Claude, id).unwrap();
    assert_eq!(
        row.effective_relay_owner_kind(),
        inflight::RelayOwnerKind::None
    );
    assert_eq!(row.hosted_execution_nonce.as_deref(), Some(N1));
    assert_eq!(row.response_sent_offset, 0);
    assert!(row.full_response.is_empty());
    assert!(!row.terminal_delivery_committed);
    let restarted = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
        fixture.pool.clone(),
    ));
    assert_eq!(restarted.token_hash, fixture.shared.token_hash);
    drop(std::mem::replace(&mut fixture.shared, restarted));
    assert!(
        crate::services::discord::mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .is_none()
    );
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::Acquire),
        0
    );
    let before = inflight_bytes(&fixture);
    let _reset = PauseReset;
    let final_stage = Arc::new(tokio::sync::Notify::new());
    let final_resume = Arc::new(tokio::sync::Notify::new());
    *synthetic_start::bridge_handoff::PREPARE_PAUSE
        .lock()
        .unwrap() = Some((id, final_stage.clone(), final_resume.clone()));
    let change = async {
        entered(
            &final_stage,
            "dormant's final check after fresh actor readmission",
        )
        .await;
        let current = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
        let actor = current.cancel_token.unwrap();
        assert_eq!(
            current.active_user_message_id,
            Some(MessageId::new(row.user_msg_id))
        );
        assert_eq!(actor.turn_nonce(), row.turn_nonce.as_deref());
        assert_eq!(
            fixture.shared.restart.global_active.load(Ordering::Acquire),
            1
        );
        assert!(
            fixture
                .shared
                .session_transition_lock(channel)
                .try_lock_owned()
                .is_err()
        );
        assert_eq!(inflight_bytes(&fixture), before);
        assert_n1_admitted(&fixture);
        let expected = if replace {
            serde_json::to_value(fixture.replace_execution(HostedState::Bound, N2).await).unwrap()
        } else {
            fixture.hosted_json().await
        };
        assert_eq!(fixture.hosted_json().await, expected);
        final_resume.notify_one();
        (expected, actor)
    };
    let (resumed, (expected, actor)) = tokio::join!(
        synthetic_start::bridge_handoff::resume_unpublished(
            &fixture.shared,
            &row,
            &fixture.source.path
        ),
        change
    );
    assert_eq!(
        resumed.is_some(),
        !replace,
        "dormant must revalidate after actor readmission"
    );
    let current = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
    if replace {
        assert!(
            current.cancel_token.is_none(),
            "only the newly readmitted actor is rolled back"
        );
        assert_eq!(
            fixture.shared.restart.global_active.load(Ordering::Acquire),
            0
        );
    } else {
        assert!(Arc::ptr_eq(current.cancel_token.as_ref().unwrap(), &actor));
        assert_eq!(
            fixture.shared.restart.global_active.load(Ordering::Acquire),
            1
        );
        assert_eq!(resumed.unwrap().hosted_source, lease.hosted_source);
    }
    assert_eq!(
        inflight_bytes(&fixture),
        before,
        "dormant keeps its original durable obligation"
    );
    assert_eq!(fixture.hosted_json().await, expected);
    assert_n1_admitted(&fixture);
    fixture.close().await;
}
