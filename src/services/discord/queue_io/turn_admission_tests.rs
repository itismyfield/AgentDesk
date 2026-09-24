//! Only an `Activated` recovery kickoff may clear the channel's `recovery_done` latch.

use super::*;
use crate::services::turn_orchestrator::RecoveryKickoffResult;

fn episode(nonce: &str) -> Arc<CancelToken> {
    Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        nonce.to_owned(),
    )))
}

async fn latched(shared: &SharedData, channel_id: ChannelId) -> bool {
    let signal = shared.mailboxes.recovery_done(channel_id);
    tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait())
        .await
        .is_ok()
}

#[tokio::test]
async fn only_an_activated_kickoff_resets_the_recovery_done_latch() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(5_951_201);
    let occupant = episode("a");
    assert!(
        shared
            .mailbox(channel_id)
            .try_start_turn(occupant, UserId::new(51), MessageId::new(101))
            .await
    );
    shared.mailboxes.recovery_done(channel_id).mark_done();

    let refused = mailbox_recovery_kickoff(
        &shared,
        channel_id,
        episode("b"),
        UserId::new(52),
        Some(MessageId::new(202)),
    )
    .await;

    assert_eq!(refused, RecoveryKickoffResult::OccupiedDifferentEpisode);
    assert!(
        latched(&shared, channel_id).await,
        "a refused kickoff reset the live recovery's recovery_done latch"
    );

    assert!(
        shared
            .mailbox(channel_id)
            .hard_stop()
            .await
            .removed_token
            .is_some()
    );
    let activated = mailbox_recovery_kickoff(
        &shared,
        channel_id,
        episode("b"),
        UserId::new(52),
        Some(MessageId::new(202)),
    )
    .await;

    assert_eq!(activated, RecoveryKickoffResult::Activated);
    assert!(!latched(&shared, channel_id).await);
}
