//! The stale-owner reclaim, reached through the claim entry rather than the
//! helper: a finished owner must not keep a new TUI-direct turn out.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ::serenity::model::id::{MessageId, UserId};
use poise::serenity_prelude::ChannelId;

use crate::services::discord::inflight::{self, InflightTurnState, TurnSource};
use crate::services::discord::mailbox_snapshot;
use crate::services::provider::ProviderKind;
use crate::services::tui_prompt_dedupe::ExternalInputRelayLease;

const REAL_OWNER: u64 = 5_997_007;

fn readopted_row(channel_id: ChannelId, user_msg_id: MessageId, tmux: &str) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel_id.get(),
        None,
        REAL_OWNER,
        user_msg_id.get(),
        user_msg_id.get(),
        "real user turn spanning a dcserver restart".to_string(),
        Some("session-5997".to_string()),
        Some(tmux.to_string()),
        Some("/tmp/agentdesk-5997.jsonl".to_string()),
        None,
        0,
    );
    state.turn_source = TurnSource::ExternalInput;
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state
}

/// A re-adopted owner whose terminal delivery committed and whose row was
/// cleared still holds the mailbox; the next TUI-direct claim must free it and start.
#[tokio::test(flavor = "current_thread")]
async fn claim_entry_reclaims_a_finished_readopted_owner_and_starts_the_new_turn() {
    let root = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let provider = ProviderKind::Claude;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(5_997_301);
    let tmux = "AgentDesk-claude-5997-claim-entry";
    let finished_id = MessageId::new(5_997_401);
    let next_id = MessageId::new(5_997_501);

    let row = readopted_row(channel_id, finished_id, tmux);
    inflight::save_inflight_state(&row).expect("seed pre-restart inflight");
    assert!(
        crate::services::discord::recovery::reregister_active_turn_from_inflight(&shared, &row)
            .await,
        "restart must re-adopt the live turn"
    );
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let finished_token = mailbox_snapshot(&shared, channel_id)
        .await
        .cancel_token
        .expect("re-adopted mailbox holds a cancel token");
    shared.mark_readopted_mailbox_owner_finished(
        &provider,
        channel_id.get(),
        REAL_OWNER,
        finished_id.get(),
    );
    assert!(inflight::clear_inflight_state(&provider, channel_id.get()));
    shared
        .mailbox(channel_id)
        .age_active_turn_for_test(std::time::Duration::from_secs(
            super::STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS as u64 + 1,
        ))
        .await;

    let mut lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
    lease.session_key = Some("session-5997-next".to_string());
    lease.turn_id = Some("external:claude:5997-next".to_string());
    let claim = super::super::claim_tui_direct_synthetic_turn(
        &shared, &provider, channel_id, tmux, "continue", next_id, &lease,
    )
    .await;

    assert!(
        finished_token.cancelled.load(Ordering::Relaxed),
        "the claim entry must retire the finished owner it found on the mailbox"
    );
    assert!(
        claim.claimed,
        "the new TUI-direct turn must start once the owner is freed"
    );
    let snapshot = mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(snapshot.active_user_message_id, Some(next_id));
    assert_eq!(
        snapshot.active_request_owner,
        Some(UserId::new(super::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID))
    );
    assert!(!Arc::ptr_eq(
        snapshot.cancel_token.as_ref().expect("new actor"),
        &finished_token
    ));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    let persisted = inflight::load_inflight_state(&provider, channel_id.get())
        .expect("the new synthetic turn persists its row");
    assert_eq!(persisted.user_msg_id, next_id.get());
}
