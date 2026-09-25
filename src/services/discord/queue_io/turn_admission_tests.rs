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

/// #5951 C3r: an adopting claim is fenced on the episode it names, not on the
/// token it installs, and records the installed token as the started episode.
#[tokio::test]
async fn an_adopting_claim_is_fenced_on_the_named_episode() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(5_951_203);
    let (owner, released, latest) = (UserId::new(53), MessageId::new(301), MessageId::new(302));
    let exact = |msg, nonce: &str| {
        let nonce = Some(nonce.to_owned());
        let finish =
            crate::services::discord::mailbox_finish_turn_if_matches_episode_started_before;
        finish(
            &shared,
            &shared.provider,
            channel_id,
            msg,
            nonce,
            std::time::Instant::now(),
        )
    };
    let adopt = |token: &str, msg, named: &str| {
        let (kind, named) = (ActiveTurnKind::Background, Some(named.to_owned()));
        mailbox_try_start_turn_adopting(
            &shared,
            channel_id,
            episode(token),
            owner,
            msg,
            kind,
            named,
        )
    };
    let mailbox = shared.mailbox(channel_id);
    assert!(mailbox.try_start_turn(episode("e1"), owner, released).await);
    assert!(exact(released, "e1").await.removed_token.is_some());
    let refused = adopt("fresh", released, "e1").await;
    assert!(refused.refused_released_episode && !refused.started);

    assert!(mailbox.try_start_turn(episode("e2"), owner, latest).await);
    let by_id = crate::services::discord::mailbox_finish_turn_if_matches;
    let by_id = by_id(&shared, &shared.provider, channel_id, latest).await;
    assert!(by_id.removed_token.is_some());
    let adopted = adopt("fresh2", latest, "e2").await;
    assert!(adopted.started && !adopted.refused_released_episode);
    let snapshot = mailbox.snapshot().await;
    assert_eq!(snapshot.active_turn_nonce.as_deref(), Some("fresh2"));
    assert_eq!(snapshot.active_turn_kind, ActiveTurnKind::Background);

    // Releasing the installed token ends the adopted episode as well.
    assert!(exact(latest, "fresh2").await.removed_token.is_some());
    assert!(adopt("fresh3", latest, "e2").await.refused_released_episode);
}
