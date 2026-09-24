//! RecoveryKickoff claims only an empty slot; an occupied slot and its
//! finished signal stay untouched whatever the occupant's identity or state.

use super::*;

const OWNER_A: UserId = UserId::new(51);
const OWNER_B: UserId = UserId::new(52);

fn episode(nonce: Option<&str>) -> Arc<CancelToken> {
    Arc::new(CancelToken::from_persisted_turn_nonce(
        nonce.map(str::to_owned),
    ))
}

fn persistence() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "recovery-kickoff-cas-test", None)
}

fn is_activated(result: &RecoveryKickoffResult) -> bool {
    result.activated_turn()
}

async fn signal_latched(registry: &ChannelMailboxRegistry, channel_id: ChannelId) -> bool {
    let signal = registry.turn_finished(channel_id);
    tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait())
        .await
        .is_ok()
}

/// Every field an occupant's release or liveness gates read back.
#[derive(Debug, PartialEq)]
struct Occupant {
    token: usize,
    owner: Option<UserId>,
    msg: Option<MessageId>,
    nonce: Option<String>,
    kind: ActiveTurnKind,
    recovery_started_at: Option<Instant>,
    turn_started_at: Option<DateTime<Utc>>,
}

async fn occupant(handle: &ChannelMailboxHandle) -> Occupant {
    let snapshot = handle.snapshot().await;
    Occupant {
        token: snapshot
            .cancel_token
            .as_ref()
            .map_or(0, |token| Arc::as_ptr(token) as usize),
        owner: snapshot.active_request_owner,
        msg: snapshot.active_user_message_id,
        nonce: snapshot.active_turn_nonce,
        kind: snapshot.active_turn_kind,
        recovery_started_at: snapshot.recovery_started_at,
        turn_started_at: snapshot.turn_started_at,
    }
}

async fn finish_episode(
    handle: &ChannelMailboxHandle,
    msg: MessageId,
    nonce: &str,
) -> Option<Arc<CancelToken>> {
    handle
        .finish_turn_if_matches_episode_started_before(
            msg,
            Some(nonce.to_owned()),
            Instant::now(),
            persistence(),
        )
        .await
        .removed_token
}

async fn start_a(handle: &ChannelMailboxHandle, msg: u64, nonce: &str) -> Arc<CancelToken> {
    let token = episode(Some(nonce));
    assert!(
        handle
            .try_start_turn(token.clone(), OWNER_A, MessageId::new(msg))
            .await
    );
    token
}

#[tokio::test]
async fn different_episode_kickoff_leaves_occupant_and_signal_untouched() {
    for (channel, b_msg, b_nonce) in [(5_951_101, 202, "b"), (5_951_102, 101, "b")] {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(channel);
        let handle = registry.handle(channel_id);
        let token_a = start_a(&handle, 101, "a").await;
        registry.turn_finished(channel_id).mark_done();
        let before = occupant(&handle).await;

        let result = handle
            .recovery_kickoff(episode(Some(b_nonce)), OWNER_B, Some(MessageId::new(b_msg)))
            .await;

        assert_eq!(result, RecoveryKickoffResult::OccupiedDifferentEpisode);
        assert_eq!(occupant(&handle).await, before, "msg {b_msg} kickoff");
        assert!(signal_latched(&registry, channel_id).await);
        let removed = finish_episode(&handle, MessageId::new(101), "a").await;
        assert!(removed.is_some_and(|token| Arc::ptr_eq(&token, &token_a)));
    }
}

#[tokio::test]
async fn same_episode_kickoff_keeps_the_existing_token() {
    let registry = ChannelMailboxRegistry::default();
    let channel_id = ChannelId::new(5_951_103);
    let handle = registry.handle(channel_id);
    let token_a = start_a(&handle, 101, "a").await;
    let before = occupant(&handle).await;
    let candidate = episode(Some("a"));

    let result = handle
        .recovery_kickoff(candidate.clone(), OWNER_A, Some(MessageId::new(101)))
        .await;

    assert_eq!(result, RecoveryKickoffResult::AlreadyActiveSameEpisode);
    assert_eq!(occupant(&handle).await, before);
    let removed = finish_episode(&handle, MessageId::new(101), "a").await;
    assert!(removed.is_some_and(|token| Arc::ptr_eq(&token, &token_a)));
}

#[tokio::test]
async fn empty_slot_kickoff_installs_and_releases_its_own_episode() {
    let registry = ChannelMailboxRegistry::default();
    let channel_id = ChannelId::new(5_951_104);
    let handle = registry.handle(channel_id);
    registry.turn_finished(channel_id).mark_done();
    let token = episode(Some("r"));

    let result = handle
        .recovery_kickoff(token.clone(), OWNER_A, Some(MessageId::new(301)))
        .await;

    assert_eq!(result, RecoveryKickoffResult::Activated);
    let installed = occupant(&handle).await;
    assert_eq!(installed.token, Arc::as_ptr(&token) as usize);
    assert_eq!(installed.owner, Some(OWNER_A));
    assert_eq!(installed.msg, Some(MessageId::new(301)));
    assert_eq!(installed.nonce.as_deref(), Some("r"));
    assert!(installed.recovery_started_at.is_some());
    assert!(installed.turn_started_at.is_some());
    assert!(!signal_latched(&registry, channel_id).await);
    let removed = finish_episode(&handle, MessageId::new(301), "r").await;
    assert!(removed.is_some_and(|removed| Arc::ptr_eq(&removed, &token)));
    assert!(!handle.has_active_turn().await.unwrap());
}

#[tokio::test]
async fn cancelled_occupant_is_not_replaced_until_its_exact_finish() {
    let registry = ChannelMailboxRegistry::default();
    let channel_id = ChannelId::new(5_951_105);
    let handle = registry.handle(channel_id);
    let token_a = start_a(&handle, 101, "a").await;
    token_a.publish_cancel("recovery-kickoff-cas-test".to_string());
    let before = occupant(&handle).await;
    let token_b = episode(Some("b"));

    let result = handle
        .recovery_kickoff(token_b.clone(), OWNER_B, Some(MessageId::new(202)))
        .await;

    assert_eq!(result, RecoveryKickoffResult::OccupiedCancelled);
    assert_eq!(occupant(&handle).await, before);
    let removed = finish_episode(&handle, MessageId::new(101), "a").await;
    assert!(removed.is_some_and(|token| Arc::ptr_eq(&token, &token_a)));
    let retry = handle
        .recovery_kickoff(token_b.clone(), OWNER_B, Some(MessageId::new(202)))
        .await;
    assert_eq!(retry, RecoveryKickoffResult::Activated);
    assert_eq!(
        occupant(&handle).await.token,
        Arc::as_ptr(&token_b) as usize
    );
}

#[tokio::test]
async fn refused_kickoffs_never_reset_the_finished_signal() {
    for (channel, msg, nonce, cancel, expected) in [
        (
            5_951_106,
            202,
            "b",
            false,
            RecoveryKickoffResult::OccupiedDifferentEpisode,
        ),
        (
            5_951_107,
            101,
            "a",
            false,
            RecoveryKickoffResult::AlreadyActiveSameEpisode,
        ),
        (
            5_951_108,
            202,
            "b",
            true,
            RecoveryKickoffResult::OccupiedCancelled,
        ),
    ] {
        let registry = ChannelMailboxRegistry::default();
        let channel_id = ChannelId::new(channel);
        let handle = registry.handle(channel_id);
        let token_a = start_a(&handle, 101, "a").await;
        if cancel {
            token_a.publish_cancel("recovery-kickoff-cas-test".to_string());
        }
        registry.turn_finished(channel_id).mark_done();

        let result = handle
            .recovery_kickoff(episode(Some(nonce)), OWNER_B, Some(MessageId::new(msg)))
            .await;

        assert_eq!(result, expected);
        assert!(
            signal_latched(&registry, channel_id).await,
            "channel {channel}: refused kickoff reset the finished signal"
        );
    }
}

#[tokio::test]
async fn id_zero_episodes_are_distinguished_only_by_an_exact_nonce() {
    let registry = ChannelMailboxRegistry::default();
    let channel_id = ChannelId::new(5_951_109);
    let handle = registry.handle(channel_id);
    let token_n1 = episode(Some("n1"));
    assert!(is_activated(
        &handle
            .recovery_kickoff(token_n1.clone(), OWNER_A, None)
            .await
    ));
    let before = occupant(&handle).await;

    for (candidate, expected) in [
        (Some("n2"), RecoveryKickoffResult::OccupiedDifferentEpisode),
        (Some("n1"), RecoveryKickoffResult::AlreadyActiveSameEpisode),
        (None, RecoveryKickoffResult::OccupiedDifferentEpisode),
    ] {
        let result = handle
            .recovery_kickoff(episode(candidate), OWNER_B, None)
            .await;
        assert_eq!(result, expected, "candidate nonce {candidate:?}");
        assert_eq!(occupant(&handle).await, before, "candidate {candidate:?}");
    }
    assert!(handle.hard_stop().await.removed_token.is_some());

    let legacy = episode(None);
    assert!(is_activated(
        &handle.recovery_kickoff(legacy.clone(), OWNER_A, None).await
    ));
    let before = occupant(&handle).await;
    let result = handle.recovery_kickoff(episode(None), OWNER_B, None).await;
    assert_eq!(result, RecoveryKickoffResult::OccupiedDifferentEpisode);
    assert_eq!(occupant(&handle).await, before);
}

#[tokio::test]
async fn try_start_then_kickoff_preserves_the_try_start_claim() {
    let registry = ChannelMailboxRegistry::default();
    let channel_id = ChannelId::new(5_951_110);
    let handle = registry.handle(channel_id);
    let token_b = start_a(&handle, 202, "b").await;
    let before = occupant(&handle).await;

    let result = handle
        .recovery_kickoff(episode(Some("a")), OWNER_B, Some(MessageId::new(101)))
        .await;

    assert_eq!(result, RecoveryKickoffResult::OccupiedDifferentEpisode);
    assert_eq!(occupant(&handle).await, before);
    let removed = finish_episode(&handle, MessageId::new(202), "b").await;
    assert!(removed.is_some_and(|token| Arc::ptr_eq(&token, &token_b)));
}

#[tokio::test]
async fn kickoff_then_try_start_preserves_the_recovery_claim() {
    let registry = ChannelMailboxRegistry::default();
    let channel_id = ChannelId::new(5_951_111);
    let handle = registry.handle(channel_id);
    let token_a = episode(Some("a"));
    assert!(is_activated(
        &handle
            .recovery_kickoff(token_a.clone(), OWNER_A, Some(MessageId::new(101)))
            .await
    ));
    let before = occupant(&handle).await;

    assert!(
        !handle
            .try_start_turn(episode(Some("b")), OWNER_B, MessageId::new(202))
            .await
    );

    assert_eq!(occupant(&handle).await, before);
    let removed = finish_episode(&handle, MessageId::new(101), "a").await;
    assert!(removed.is_some_and(|token| Arc::ptr_eq(&token, &token_a)));
}
