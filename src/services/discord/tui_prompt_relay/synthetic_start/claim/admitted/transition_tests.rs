use super::*;
use crate::services::discord::inflight;
use crate::services::discord::tui_prompt_relay::synthetic_start::claim::{
    SyntheticClaimIdentity, SyntheticClaimPreparation,
};
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};
use tokio::sync::OwnedMutexGuard;

struct Fixture {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    logical: String,
    output: PathBuf,
    anchor: MessageId,
    lease: ExternalInputRelayLease,
}

#[tokio::test(flavor = "current_thread")]
async fn a_nonce_pinned_tmux_input_keeps_the_non_herdr_production_claim_available() {
    use dedupe::hosted_source::tests::{CHANNEL, Fixture as SourceFixture, LOGICAL, N1};
    let source_fixture = SourceFixture::new();
    let source = source_fixture.source(N1, "native-tmux-input");
    crate::services::tmux_common::host_marker::record_tmux_host_marker(LOGICAL);
    dedupe::register_tmux_runtime_binding(
        LOGICAL,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: source.path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(source.session_id.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let mut lease = ExternalInputRelayLease::unassigned(Some(CHANNEL));
    lease.turn_id = Some("native-tmux-input-turn".into());
    lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
    lease.hosted_source = Some(
        dedupe::hosted_source::capture("claude", LOGICAL, CHANNEL, &source.path, None)
            .expect("a normal tmux execution also has durable source nonce history"),
    );
    let mut fixture = Fixture {
        shared: crate::services::discord::make_shared_data_for_tests(),
        provider: ProviderKind::Claude,
        channel: ChannelId::new(CHANNEL),
        logical: LOGICAL.into(),
        output: source.path,
        anchor: MessageId::new(CHANNEL + 1),
        lease: dedupe::record_external_input_turn_lease("claude", LOGICAL, lease),
    };
    let original_pin = fixture.lease.hosted_source.clone().unwrap();
    let mut old_pin = serde_json::to_value(&original_pin).unwrap();
    old_pin
        .as_object_mut()
        .unwrap()
        .remove("hosted_admission_required");
    let marker = crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind");
    std::fs::write(&marker, b"herdr").unwrap();
    let hosted_pin =
        dedupe::hosted_source::capture("claude", LOGICAL, CHANNEL, &fixture.output, None).unwrap();
    std::fs::remove_file(&marker).unwrap();
    for pin in [hosted_pin, serde_json::from_value(old_pin).unwrap()] {
        fixture.lease.hosted_source = Some(pin);
        assert!(
            !claim_tui_direct_synthetic_turn_prepared(fixture.prepared(None))
                .await
                .claimed,
            "original hosted or unknown host evidence must survive a missing current marker"
        );
        fixture.assert_quiet().await;
    }
    fixture.lease.hosted_source = Some(original_pin);
    crate::services::tmux_common::host_marker::record_tmux_host_marker(LOGICAL);
    let claim = claim_tui_direct_synthetic_turn_prepared(fixture.prepared(None)).await;
    assert!(
        claim.claimed,
        "a source nonce alone must not require a Herdr endpoint"
    );
    let row = inflight::load_inflight_state_read_only(&fixture.provider, CHANNEL).unwrap();
    assert_eq!(row.user_msg_id, fixture.anchor.get());
    assert!(row.hosted_execution_nonce.is_none());
}

impl Fixture {
    fn new(root: &Path, id: u64) -> Self {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(id);
        let logical = format!("borrowed-transition-claim-{id}");
        let output = root.join(format!("{id}.jsonl"));
        std::fs::write(&output, b"").unwrap();
        dedupe::register_tmux_runtime_binding(
            &logical,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: output.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let mut lease = ExternalInputRelayLease::unassigned(Some(id));
        lease.turn_id = Some(format!("original-external-turn-{id}"));
        lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
        let lease = dedupe::record_external_input_turn_lease("claude", &logical, lease);
        Self {
            shared,
            provider: ProviderKind::Claude,
            channel,
            logical,
            output,
            anchor: MessageId::new(id + 1),
            lease,
        }
    }

    fn prepared<'a>(
        &'a self,
        transition: Option<&'a OwnedMutexGuard<()>>,
    ) -> SyntheticClaimPreparation<'a> {
        SyntheticClaimPreparation {
            identity: SyntheticClaimIdentity {
                shared: &self.shared,
                provider: &self.provider,
                channel_id: self.channel,
                tmux_session_name: &self.logical,
                prompt_text: "an already submitted input",
                anchor_message_id: self.anchor,
                lease: &self.lease,
                register_deferred_start: false,
                native_turn: None,
                transition,
            },
            output_path: Some(self.output.clone()),
            start_offset: 0,
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            relay_owner_kind: RelayOwnerKind::None,
        }
    }

    async fn assert_quiet(&self) {
        let actor = crate::services::discord::mailbox_snapshot(&self.shared, self.channel).await;
        assert!(actor.cancel_token.is_none());
        assert!(actor.active_user_message_id.is_none());
        assert!(
            inflight::load_inflight_state_read_only(&self.provider, self.channel.get()).is_none()
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_claim_borrows_the_exact_held_transition_and_leaves_it_with_the_caller() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let fixture = Fixture::new(root.path(), 7_186_702);
    let held = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    let claim = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        claim_tui_direct_synthetic_turn_prepared(fixture.prepared(Some(&held))),
    )
    .await
    .expect("borrowing the caller's lease must not wait on its own mutex");
    assert!(
        claim.claimed,
        "a valid borrowed transition keeps the legacy production claim available"
    );
    let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel).await;
    let row =
        inflight::load_inflight_state_read_only(&fixture.provider, fixture.channel.get()).unwrap();
    assert!(actor.cancel_token.is_some());
    assert_eq!(actor.active_user_message_id, Some(fixture.anchor));
    assert_eq!(row.user_msg_id, fixture.anchor.get());
    assert_eq!(row.turn_nonce, actor.active_turn_nonce);
    assert!(
        fixture
            .shared
            .session_transition_lock(fixture.channel)
            .try_lock_owned()
            .is_err(),
        "finishing a borrowed claim must not unlock the caller's guard"
    );
    drop(held);
    assert!(
        fixture
            .shared
            .session_transition_lock(fixture.channel)
            .try_lock_owned()
            .is_ok()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_claim_rejects_a_guard_for_another_channel_runtime_or_mutex() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    for (index, foreign_kind) in ["channel", "runtime", "mutex"].into_iter().enumerate() {
        let fixture = Fixture::new(root.path(), 7_186_712 + index as u64 * 10);
        let foreign_runtime = crate::services::discord::make_shared_data_for_tests();
        let other = match foreign_kind {
            "channel" => fixture
                .shared
                .session_transition_lock(ChannelId::new(fixture.channel.get() + 2)),
            "runtime" => foreign_runtime.session_transition_lock(fixture.channel),
            _ => Arc::new(tokio::sync::Mutex::new(())),
        };
        let foreign = other.clone().lock_owned().await;
        assert!(
            fixture
                .shared
                .session_transition_lock(fixture.channel)
                .try_lock_owned()
                .is_ok(),
            "the actual target mutex starts free"
        );
        let claim =
            claim_tui_direct_synthetic_turn_prepared(fixture.prepared(Some(&foreign))).await;
        assert!(
            !claim.claimed,
            "a foreign {foreign_kind} guard granted actor admission"
        );
        fixture.assert_quiet().await;
        assert!(
            other.clone().try_lock_owned().is_err(),
            "the failed borrowed claim released the foreign caller's guard"
        );
        assert!(
            fixture
                .shared
                .session_transition_lock(fixture.channel)
                .try_lock_owned()
                .is_ok(),
            "a rejected guard must not retain the target transition"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_existing_actor_readoption_preserves_the_original_actor_and_saved_row() {
    use crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff;
    use std::sync::atomic::Ordering;

    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let fixture = Fixture::new(root.path(), 7_186_752);
    let held = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    assert!(
        claim_tui_direct_synthetic_turn_prepared(fixture.prepared(Some(&held)))
            .await
            .claimed
    );
    let before = crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel).await;
    let original = before.cancel_token.clone().unwrap();
    let row =
        inflight::load_inflight_state_read_only(&fixture.provider, fixture.channel.get()).unwrap();
    let row_bytes = serde_json::to_vec(&row).unwrap();
    let active_count = fixture.shared.restart.global_active.load(Ordering::SeqCst);
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *bridge_handoff::ADMISSION_PAUSE.lock().unwrap() =
        Some((fixture.channel.get(), entered.clone(), resume.clone()));
    let change = async {
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .expect("the real readoption reached its post-admission checkpoint");
        let current =
            crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel).await;
        assert!(
            current
                .cancel_token
                .as_ref()
                .is_some_and(|actor| Arc::ptr_eq(actor, &original)),
            "readoption must still refer to the original allocation"
        );
        dedupe::install_herdr_execution(&fixture.logical, "a-newly-observed-execution");
        dedupe::admit_herdr_execution(&fixture.logical, "a-newly-observed-execution");
        resume.notify_one();
    };
    let (retry, ()) = tokio::join!(
        claim_tui_direct_synthetic_turn_prepared(fixture.prepared(Some(&held))),
        change,
    );
    assert!(
        !retry.claimed,
        "the changed host trace must refuse the late save"
    );
    let after = crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel).await;
    assert!(
        after
            .cancel_token
            .as_ref()
            .is_some_and(|actor| Arc::ptr_eq(actor, &original)),
        "failure cleanup must not release an actor that this attempt did not create"
    );
    assert_eq!(after.active_user_message_id, before.active_user_message_id);
    assert_eq!(after.active_turn_nonce, before.active_turn_nonce);
    assert!(!original.cancelled.load(Ordering::SeqCst));
    assert_eq!(
        fixture.shared.restart.global_active.load(Ordering::SeqCst),
        active_count
    );
    let after_row =
        inflight::load_inflight_state_read_only(&fixture.provider, fixture.channel.get()).unwrap();
    assert_eq!(serde_json::to_vec(&after_row).unwrap(), row_bytes);
    assert!(
        fixture
            .shared
            .session_transition_lock(fixture.channel)
            .try_lock_owned()
            .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn old_unpinned_known_tmux_pending_still_claims_its_captured_source() {
    use crate::services::discord::tui_direct_pending_start as pending;
    use crate::services::discord::tui_prompt_relay::synthetic_start;
    use crate::services::session_host::HostKind;
    use crate::services::tmux_common::host_marker::{self, HostKindMarker};
    use std::sync::Mutex;

    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    for marked in [false, true] {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_186_682 + u64::from(marked));
        let logical = if marked {
            "legacy-unpinned-known-tmux-7186"
        } else {
            "legacy-unpinned-markerless-tmux-7186"
        };
        let path = root.path().join("known-tmux-original-source.jsonl");
        std::fs::write(&path, b"").unwrap();
        if marked {
            host_marker::record_tmux_host_marker(logical);
        }
        assert_eq!(
            host_marker::read_host_kind_marker(logical),
            if marked {
                HostKindMarker::Known(HostKind::Tmux)
            } else {
                HostKindMarker::Absent
            }
        );
        assert!(crate::config::session_hosts::herdr_endpoint(channel.get()).is_none());
        assert!(!dedupe::herdr_execution_listed(logical));
        dedupe::register_tmux_runtime_binding(
            logical,
            dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("known-native-tmux-source".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        shared.tmux_watchers.insert(
            channel,
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: logical.into(),
                output_path: path.display().to_string(),
                paused: Arc::new(false.into()),
                resume_offset: Arc::new(Mutex::new(None)),
                cancel: Arc::new(false.into()),
                pause_epoch: Arc::new(0.into()),
                turn_delivered: Arc::new(false.into()),
                last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
            },
        );
        let old: pending::TuiDirectPendingStart = serde_json::from_value(serde_json::json!({
            "provider": "claude", "channel_id": channel.get(), "tmux_session_name": logical,
            "prompt_text": "known tmux input", "anchor_message_id": 7_186_683,
            "lease_relay_owner": "tmux_watcher", "lease_runtime_kind": "claude_tui",
            "lease_turn_id": "known-tmux-original-turn", "lease_session_key": null,
            "generation": shared.restart.current_generation, "created_at_ms": 0, "observed_at_ms": 0,
            "captured_source": [path.display().to_string(), 0]
        })).unwrap();
        assert!(old.hosted_source.is_none());
        assert!(!old.hosted_source_checked);
        assert!(old.captured_source.is_some());
        let claim = synthetic_start::pending_start_claim_fn();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), claim(&shared, &old))
                .await
                .expect("the known non-Herdr production claim completed"),
            "a non-Herdr None input was held by the legacy Herdr origin policy"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.user_msg_id, old.anchor_message_id);
        assert_eq!(saved.output_path.as_deref(), path.to_str());
        assert_eq!(saved.external_turn_id, old.lease_turn_id);
        assert!(saved.hosted_record_id.is_none());
        assert!(saved.hosted_execution_nonce.is_none());
    }
}
