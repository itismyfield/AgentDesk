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

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_non_herdr_fence_survives_an_unknown_exact_lookup_without_bypassing_the_actor_pin() {
    use super::super::super::admission_fence::AdmissionFence;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, load_hosted_execution_pg,
    };
    use dedupe::hosted_source::tests::Fixture as SourceFixture;

    let source_fixture = SourceFixture::new();
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgresql://test:test@127.0.0.1:1/not_connected")
        .unwrap();
    pool.close().await;
    let mut fixture = Fixture::new(source_fixture.root.path(), 7_186_781);
    fixture.lease.session_key = Some("closed-pool-exact-session".into());
    Arc::get_mut(&mut fixture.shared).unwrap().pg_pool = Some(pool.clone());
    crate::services::tmux_common::host_marker::record_tmux_host_marker(&fixture.logical);
    assert_eq!(
        dedupe::hosted_source::herdr_evidence(fixture.channel.get(), &fixture.logical, None),
        Ok(false)
    );
    assert!(matches!(
        load_hosted_execution_pg(
            &pool,
            HostedLookupKey::SessionKey(fixture.lease.session_key.as_deref().unwrap()),
        )
        .await,
        HostedLookup::Unknown(why) if why.to_ascii_lowercase().contains("closed")
    ));
    let log_path = source_fixture.root.path().join("nonherdr-unknown.log");
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(std::fs::File::create(&log_path).unwrap())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _logs = tracing::subscriber::set_default(subscriber);
    let fence = AdmissionFence::capture(
        &fixture.shared,
        &fixture.provider,
        fixture.channel,
        &fixture.logical,
        &fixture.lease,
        None,
    )
    .await
    .unwrap_or_else(|error| {
        panic!("the non-Herdr production fence refused its lookup outage: {error}")
    });
    assert!(fence.hosted_identity().is_none());
    fence.revalidate().await.unwrap();
    drop(fence);
    assert!(matches!(
        bridge_handoff::capture_session_pin(&fixture.shared, fixture.lease.session_key.as_deref())
            .await,
        Err(why) if why.to_ascii_lowercase().contains("closed")
    ));
    let claim = claim_tui_direct_synthetic_turn_prepared(fixture.prepared(None)).await;
    assert!(
        !claim.claimed,
        "a hosted lookup fallback must not bypass the existing unavailable PG actor pin"
    );
    fixture.assert_quiet().await;
    assert!(
        std::fs::read_to_string(log_path)
            .unwrap()
            .contains("synthetic_nonherdr_hosted_lookup_unknown")
    );

    let strict_channel = 7_186_782;
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("closed-pool-herdr-node"),
        &[(strict_channel, "closed-pool-herdr-node")],
    );
    let mut strict = Fixture::new(source_fixture.root.path(), strict_channel);
    strict.lease.session_key = fixture.lease.session_key.clone();
    Arc::get_mut(&mut strict.shared).unwrap().pg_pool = Some(pool);
    assert!(crate::config::session_hosts::herdr_endpoint(strict_channel).is_some());
    assert!(
        !claim_tui_direct_synthetic_turn_prepared(strict.prepared(None))
            .await
            .claimed,
        "an unavailable exact lookup must still refuse an affirmative Herdr input"
    );
    strict.assert_quiet().await;
}

#[cfg(unix)]
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
async fn a_markerless_endpoint_still_refuses_an_unpinned_prepared_claim() {
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let channel = 7_186_772;
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("endpoint-evidence-node"),
        &[(channel, "endpoint-evidence-node")],
    );
    let fixture = Fixture::new(root.path(), channel);
    assert!(crate::config::session_hosts::herdr_endpoint(channel).is_some());
    assert_eq!(
        read_host_kind_marker(&fixture.logical),
        HostKindMarker::Absent
    );
    assert!(!dedupe::herdr_execution_listed(&fixture.logical));
    assert!(fixture.lease.hosted_source.is_none());
    let claim = claim_tui_direct_synthetic_turn_prepared(fixture.prepared(None)).await;
    assert!(
        !claim.claimed,
        "a configured Herdr endpoint still requires its original hosted source without a marker"
    );
    fixture.assert_quiet().await;
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

#[cfg(unix)]
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

impl Fixture {
    /// The row a lost allocation left behind for this anchor: no live actor, no witness.
    fn seed_persisted_episode(&self, nonce: Option<&str>) -> InflightTurnState {
        let mut row = build_tui_direct_synthetic_inflight_state(
            self.provider.clone(),
            self.channel,
            self.anchor,
            None,
            "an already submitted input",
            &self.logical,
            Some(self.output.as_path()),
            0,
            &self.lease,
            RelayOwnerKind::None,
        );
        row.turn_nonce = nonce.map(str::to_owned);
        inflight::save_inflight_state(&row).unwrap();
        inflight::load_inflight_state_read_only(&self.provider, self.channel.get()).unwrap()
    }

    fn row(&self) -> InflightTurnState {
        inflight::load_inflight_state_read_only(&self.provider, self.channel.get()).unwrap()
    }
}

/// Re-adopting a persisted episode under a freshly admitted actor moves the row to that actor's
/// nonce, from a nonced or a legacy row, and keeps every other birth axis.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn synthetic_readoption_nonce_transition_7219() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    for (id, persisted) in [(7_219_701, Some("lost-allocation-n1")), (7_219_711, None)] {
        let fixture = Fixture::new(root.path(), id);
        let before = fixture.seed_persisted_episode(persisted);
        let claim = claim_tui_direct_synthetic_turn_prepared(fixture.prepared(None)).await;
        assert!(claim.claimed, "re-adoption of {persisted:?} was refused");
        let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel)
            .await
            .cancel_token
            .expect("the admitted actor holds the slot");
        let after = fixture.row();
        assert!(actor.turn_nonce().is_some());
        assert_eq!(after.turn_nonce.as_deref(), actor.turn_nonce());
        assert_ne!(after.turn_nonce, before.turn_nonce);
        assert_eq!(
            (after.user_msg_id, &after.started_at, after.born_generation),
            (
                before.user_msg_id,
                &before.started_at,
                before.born_generation
            )
        );
        assert_eq!(after.request_owner_user_id, before.request_owner_user_id);
    }
}

#[derive(Clone, Copy, Debug)]
enum FenceRace {
    SameEpisodeAdvanced,
    SuccessorEpisode,
    SameNonceOtherAllocation,
}

/// A change that lands while the transition waits on its last fence refuses the N2 save: the
/// row keeps whatever the racing writer left, and the claim's own fresh actor is released.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn synthetic_readoption_replaced_episode_7219() {
    use crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff;
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let races = [
        FenceRace::SameEpisodeAdvanced,
        FenceRace::SuccessorEpisode,
        FenceRace::SameNonceOtherAllocation,
    ];
    for (index, race) in races.into_iter().enumerate() {
        let fixture = Fixture::new(root.path(), 7_219_721 + 10 * index as u64);
        let persisted = fixture.seed_persisted_episode(Some("lost-allocation-n1"));
        let pause = || {
            let (entered, resume) = (
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tokio::sync::Notify::new()),
            );
            *bridge_handoff::PREPARE_PAUSE.lock().unwrap() =
                Some((fixture.channel.get(), entered.clone(), resume.clone()));
            (entered, resume)
        };
        let (prepared, resume_prepared) = pause();
        let racer = async {
            let wait = std::time::Duration::from_secs(5);
            tokio::time::timeout(wait, prepared.notified())
                .await
                .unwrap();
            let (refreshing, resume_refresh) = pause();
            resume_prepared.notify_one();
            tokio::time::timeout(wait, refreshing.notified())
                .await
                .expect("the re-adoption reached its last fence");
            let mut impostor = None;
            match race {
                FenceRace::SameEpisodeAdvanced => {
                    let mut advanced = persisted.clone();
                    advanced.full_response = "the same episode moved on".into();
                    inflight::save_inflight_state(&advanced).unwrap();
                }
                FenceRace::SuccessorEpisode => {
                    let mut successor = persisted.clone();
                    successor.turn_nonce = Some("successor-episode".into());
                    successor.born_generation += 1;
                    inflight::save_inflight_state(&successor).unwrap();
                }
                FenceRace::SameNonceOtherAllocation => {
                    let admitted = crate::services::discord::mailbox_snapshot(
                        &fixture.shared,
                        fixture.channel,
                    )
                    .await
                    .cancel_token
                    .unwrap();
                    let other = Arc::new(CancelToken::from_persisted_turn_nonce(
                        admitted.turn_nonce().map(str::to_owned),
                    ));
                    crate::services::discord::mailbox_finish_turn(
                        &fixture.shared,
                        &fixture.provider,
                        fixture.channel,
                    )
                    .await;
                    assert!(
                        crate::services::discord::mailbox_try_start_turn(
                            &fixture.shared,
                            fixture.channel,
                            other.clone(),
                            serenity::UserId::new(1),
                            fixture.anchor,
                        )
                        .await
                    );
                    impostor = Some(other);
                }
            }
            let raced = serde_json::to_vec(&fixture.row()).unwrap();
            resume_refresh.notify_one();
            (raced, impostor)
        };
        let (claim, (raced, impostor)) = tokio::join!(
            claim_tui_direct_synthetic_turn_prepared(fixture.prepared(None)),
            racer
        );
        assert!(!claim.claimed, "{race:?}: the late N2 save was accepted");
        assert_eq!(
            serde_json::to_vec(&fixture.row()).unwrap(),
            raced,
            "{race:?}: the racing row was overwritten"
        );
        let slot = crate::services::discord::mailbox_snapshot(&fixture.shared, fixture.channel)
            .await
            .cancel_token;
        match impostor {
            Some(other) => assert!(
                slot.is_some_and(|held| Arc::ptr_eq(&held, &other)),
                "{race:?}: the other allocation lost its slot"
            ),
            None => assert!(
                slot.is_none(),
                "{race:?}: the refused fresh actor kept the slot"
            ),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_tui_claim_clears_the_recorded_idle_card_pg() {
    use crate::services::discord::session_idle_cleanup::recap_cleanup_tests::{
        assert_cleared, discord, seed_for_channel,
    };
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "tui_claim_recap",
        "TUI admitted claim legacy card wiring",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let mut fixture = Fixture::new(root.path(), 7_274_203);
    Arc::get_mut(&mut fixture.shared).unwrap().pg_pool = Some(pool.clone());
    seed_for_channel(&pool, 4242, fixture.channel.get()).await;
    let (http, deletes, server) = discord("📦 응답 완료 · 이어서 질문 가능", true).await;
    let _http = crate::services::discord::shared_state::test_rest::install(http);
    let claim = claim_tui_direct_synthetic_turn_prepared(fixture.prepared(None)).await;
    assert!(
        claim.claimed,
        "the TUI fixture must reach the admitted production claim"
    );
    assert_cleared(&pool, &deletes, "the admitted TUI claim").await;
    server.abort();
    pool.close().await;
    db.drop().await;
}
