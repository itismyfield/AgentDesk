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
