//! A Codex observation that turn mode was confirmed under, whether it waited on the start lock or
//! began before the takeover, starts no Legacy turn: no pending start and no inflight row.

use super::*;
use crate::services::discord::tui_direct_pending_start::{channel_lock, load_all};
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::turn_mode::{self, TestConfirmation};

const CHANNEL: u64 = 940_000_000_005_101;

fn adopted_and_selected(adoption: Adoption) -> test_override::ChannelsGuard {
    let config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": CHANNEL.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [CHANNEL]}, "turn": {"channels": [CHANNEL]}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config).unwrap();
    test_override::force_boot(boot.adopted(adoption))
}

/// A prompt observed while the takeover turn holds the mailbox, so its start would be deferred.
async fn observed(
    shared: &Arc<SharedData>,
) -> (
    ObservedTuiPrompt,
    RelayObservedPromptInjectionDecision,
    ExternalInputRelayLease,
) {
    let (channel_id, tmux) = (ChannelId::new(CHANNEL), "AgentDesk-codex-takeover-race");
    let running = MessageId::new(940_000_000_005_201);
    let started = crate::services::discord::mailbox_try_start_turn(
        shared.as_ref(),
        channel_id,
        Arc::new(crate::services::provider::CancelToken::new()),
        serenity::UserId::new(42),
        running,
    );
    assert!(started.await, "the takeover turn holds the mailbox");
    let prompt = ObservedTuiPrompt {
        provider: "codex".into(),
        tmux_session_name: tmux.into(),
        prompt: "next".into(),
        source_event_id: None,
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        hook_prompt_id: None,
        row_prompt_id: None,
        native_turn_id: None,
        steer_echo: false,
    };
    let decision = relay_observed_prompt_injected_prompt_decision(&prompt.prompt);
    assert!(decision.starts_external_turn_lifecycle());
    let lease = ExternalInputRelayLease {
        channel_id: Some(CHANNEL),
        turn_id: Some(format!("external:codex:{CHANNEL}:{tmux}:test")),
        session_key: Some(format!("session:{tmux}")),
        relay_owner: ExternalInputRelayOwner::TmuxWatcher,
        runtime_kind: Some(RuntimeHandoffKind::CodexTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    (prompt, decision, lease)
}

fn assert_no_legacy_start() {
    let pending = load_all()
        .into_iter()
        .any(|record| record.channel_id == CHANNEL);
    assert!(!pending, "no pending start is persisted after confirmation");
    let present =
        crate::services::discord::tui_direct_pending_start::pending_synthetic_start_present(
            "codex", CHANNEL,
        );
    assert!(!present);
    let row =
        crate::services::discord::inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL);
    assert!(row.is_none(), "no synthetic inflight row is written");
}

/// The takeover turn still runs, so the observation would defer its start to a worker; it waits
/// on the lock the confirmation holds and, once turn mode is confirmed, stands down.
#[tokio::test(flavor = "current_thread")]
async fn a_codex_observation_resumed_after_confirmation_persists_no_start() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let _boot = adopted_and_selected(Adoption::Committed);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (prompt, decision, mut lease) = observed(&shared).await;
    let lock = channel_lock("codex", CHANNEL);
    let confirming = lock.try_lock().unwrap();
    let anchor = MessageId::new(940_000_000_005_301);
    let channel_id = ChannelId::new(CHANNEL);
    let wiring = wire_tui_direct_synthetic_turn_start(
        &shared, "codex", channel_id, &prompt, anchor, false, &decision, &mut lease,
    );
    tokio::pin!(wiring);
    for _ in 0..8 {
        assert!(
            futures::poll!(&mut wiring).is_pending(),
            "it waits on the lock"
        );
        tokio::task::yield_now().await;
    }
    assert!(turn_mode::confirm(CHANNEL));
    let _confirmed = TestConfirmation::confirmed(CHANNEL);
    drop(confirming);

    assert!(wiring.await, "the observer stands down without a tail");
    assert_no_legacy_start();
}

/// An observation begun before the takeover committed reads its prior turn while the takeover
/// commits and turn mode is confirmed; under the start lock it then stands down.
#[tokio::test(flavor = "current_thread")]
async fn a_codex_observation_begun_before_the_takeover_persists_no_start_after_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let deferred = adopted_and_selected(Adoption::Deferred);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (prompt, decision, mut lease) = observed(&shared).await;
    assert!(crate::services::tui_o::cutover::codex_turn_awaited(CHANNEL).is_none());
    let anchor = MessageId::new(940_000_000_005_302);
    let channel_id = ChannelId::new(CHANNEL);
    let wiring = wire_tui_direct_synthetic_turn_start(
        &shared, "codex", channel_id, &prompt, anchor, false, &decision, &mut lease,
    );
    tokio::pin!(wiring);
    assert!(
        futures::poll!(&mut wiring).is_pending(),
        "it reads the prior turn"
    );
    drop(deferred);
    let _committed = adopted_and_selected(Adoption::Committed);
    assert!(turn_mode::confirm(CHANNEL));
    let _confirmed = TestConfirmation::confirmed(CHANNEL);

    assert!(wiring.await, "the observer stands down without a tail");
    assert_no_legacy_start();
}

/// An observation of a channel not yet awaited still persists only under the start lock: while a
/// confirmation holds it the takeover commits and turn mode is confirmed, so it stands down.
#[tokio::test(flavor = "current_thread")]
async fn a_codex_observation_not_yet_awaited_waits_on_the_start_lock_before_persisting() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let deferred = adopted_and_selected(Adoption::Deferred);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (prompt, decision, mut lease) = observed(&shared).await;
    let lock = channel_lock("codex", CHANNEL);
    let confirming = lock.try_lock().unwrap();
    let anchor = MessageId::new(940_000_000_005_303);
    let channel_id = ChannelId::new(CHANNEL);
    let wiring = wire_tui_direct_synthetic_turn_start(
        &shared, "codex", channel_id, &prompt, anchor, false, &decision, &mut lease,
    );
    tokio::pin!(wiring);
    for _ in 0..8 {
        assert!(
            futures::poll!(&mut wiring).is_pending(),
            "it waits on the lock"
        );
        tokio::task::yield_now().await;
    }
    drop(deferred);
    let _committed = adopted_and_selected(Adoption::Committed);
    assert!(turn_mode::confirm(CHANNEL));
    let _confirmed = TestConfirmation::confirmed(CHANNEL);
    drop(confirming);

    assert!(wiring.await, "the observer stands down without a tail");
    assert_no_legacy_start();
}

/// A Codex channel boot never selected for turn mode defers its start without the start lock a
/// waiting worker may hold, so the observer does not stall behind that worker.
#[tokio::test(flavor = "current_thread")]
async fn an_unselected_codex_observation_defers_without_waiting_on_the_start_lock() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (prompt, decision, mut lease) = observed(&shared).await;
    let lock = channel_lock("codex", CHANNEL);
    let _worker = lock.try_lock().unwrap();
    let anchor = MessageId::new(940_000_000_005_304);
    let wiring = wire_tui_direct_synthetic_turn_start(
        &shared,
        "codex",
        ChannelId::new(CHANNEL),
        &prompt,
        anchor,
        false,
        &decision,
        &mut lease,
    );
    let deferred = tokio::time::timeout(std::time::Duration::from_secs(5), wiring).await;
    assert!(deferred.expect("the observer is not held by the lock"));
    assert!(
        load_all()
            .into_iter()
            .any(|record| record.channel_id == CHANNEL)
    );
}
