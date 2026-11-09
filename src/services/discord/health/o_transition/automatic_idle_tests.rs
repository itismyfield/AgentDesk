use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude::MessageId;

use super::*;
use crate::services::discord::make_shared_data_for_tests;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use crate::services::turn_orchestrator::registry_purge::MailboxRefusal;

#[test]
fn automatic_idle_keeps_an_active_turn_or_reserved_dispatch() {
    assert!(snapshot_idle(&ChannelMailboxSnapshot::default()));
    let active = ChannelMailboxSnapshot {
        cancel_token: Some(Arc::new(CancelToken::new())),
        ..ChannelMailboxSnapshot::default()
    };
    assert!(!snapshot_idle(&active), "active turn must hold cleanup");
    let dispatching = ChannelMailboxSnapshot {
        pending_user_dispatch: Some(MessageId::new(7_268_301)),
        ..ChannelMailboxSnapshot::default()
    };
    assert!(
        !snapshot_idle(&dispatching),
        "reserved input must hold cleanup"
    );
}

#[test]
fn automatic_idle_keeps_queued_input_or_recovery() {
    let queued = ChannelMailboxSnapshot {
        intervention_queue: vec![ChannelMailboxRegistry::queued_for_test(7_268_302)],
        ..ChannelMailboxSnapshot::default()
    };
    assert!(!snapshot_idle(&queued), "queued input must hold cleanup");
    let recovering = ChannelMailboxSnapshot {
        recovery_started_at: Some(Instant::now()),
        ..ChannelMailboxSnapshot::default()
    };
    assert!(!snapshot_idle(&recovering), "recovery must hold cleanup");
}

#[tokio::test]
async fn automatic_idle_keeps_missing_closed_and_unanswered_mailboxes() {
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(7_268_303);
    assert!(shared.mailbox_peek(channel).is_none());
    assert!(
        !idle(&shared, channel.get()).await,
        "missing actor is unknown"
    );
    assert!(
        shared.mailbox_peek(channel).is_none(),
        "probe must not create an actor"
    );

    shared.mailboxes.insert_unreachable_for_test(channel);
    assert!(
        !idle(&shared, channel.get()).await,
        "closed actor is unknown"
    );
    shared.mailboxes.remove_fixture_for_test(channel);

    shared.mailboxes.insert_reply_dropping_for_test(channel);
    assert!(!idle(&shared, channel.get()).await, "lost reply is unknown");
    shared.mailboxes.remove_fixture_for_test(channel);
}

#[tokio::test]
async fn automatic_idle_reads_the_live_runtime_snapshot() {
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(7_268_304);
    for (snapshot, expected) in [
        (ChannelMailboxSnapshot::default(), true),
        (
            ChannelMailboxSnapshot {
                intervention_queue: vec![ChannelMailboxRegistry::queued_for_test(7_268_304)],
                ..ChannelMailboxSnapshot::default()
            },
            false,
        ),
    ] {
        shared
            .mailboxes
            .insert_idle_snapshot_for_test(channel, snapshot, MailboxRefusal::Closed);
        assert_eq!(idle(&shared, channel.get()).await, expected);
        shared.mailboxes.remove_fixture_for_test(channel);
    }
}

// Input a V2 owner holds keeps cleanup away: a waiting input, and a front an attempt carries alike.
#[tokio::test]
async fn automatic_idle_keeps_input_the_v2_owner_holds() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let seed =
        crate::services::discord::relay_recovery::tests::orphan_token_finish::v2_attempted_front;
    for (channel, waiting) in [(7_268_305, true), (7_268_306, false)] {
        let channel = ChannelId::new(channel);
        seed(&shared, &ProviderKind::Codex, channel, waiting).await;
        assert!(!idle(&shared, channel.get()).await, "waiting={waiting}");
    }
}
