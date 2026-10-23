use super::*;
use crate::services::discord::turn_view_reconciler::{
    TurnViewDelivery, TurnViewIdentity, TurnViewOwner, TurnViewReconciler, TurnViewTarget,
};
use poise::serenity_prelude::MessageId;

fn target(channel: u64, message: u64) -> TurnViewTarget {
    TurnViewTarget::intake_user_message(ChannelId::new(channel), MessageId::new(message))
}

async fn inject(shared: &Arc<SharedData>, channel: u64, message: u64) {
    shared
        .turn_view_reconciler
        .note_busy_injected(
            shared,
            target(channel, message),
            TurnViewIdentity::Test("inject"),
        )
        .await;
}

async fn settled(shared: &SharedData, message: u64) {
    for (emoji, add, assertion) in [
        (
            '⏳',
            false,
            "idle must remove each immediate injection hourglass",
        ),
        (
            '✅',
            true,
            "idle must add a completion checkmark to each immediate injection",
        ),
    ] {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if shared.turn_view_reconciler.ops().iter().any(|op| {
                    op.target.message_id.get() == message && op.emoji == emoji && op.add == add
                }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect(assertion);
    }
}

fn supervisor(shared: &Arc<SharedData>, channels: &[u64]) -> (Arc<Fake>, Supervisor<Fake>) {
    let fake = Fake::new(channels, BUSY);
    *lock(&fake.live_mailbox) = Some(shared.clone());
    let mut supervisor = Supervisor::new(fake.clone());
    for &channel in channels {
        supervisor.presence(channel);
    }
    (fake, supervisor)
}

#[tokio::test(flavor = "current_thread")]
async fn idle_completes_only_its_injections_and_preserves_anchor_and_queue_lifecycles() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (a, b) = (7_269_001, 7_269_002);
    let ids = [
        1514500851761287311,
        1514500851761287312,
        1514500851761287313,
    ];
    let anchor = target(a, 1514500851761287321);
    let queued = target(a, 1514500851761287322);
    let queued_running = target(a, 1514500851761287323);
    let synthetic = TurnViewTarget::tui_direct_bot_anchor(
        ChannelId::new(a),
        MessageId::new(1514500851761287324),
    );
    let reconciler = &shared.turn_view_reconciler;
    let owner = TurnViewOwner::new(1, "ordinary-turn");
    for item in [anchor, synthetic] {
        assert!(
            reconciler
                .note_turn_started(
                    &shared,
                    item,
                    owner.clone(),
                    TurnViewIdentity::Test("ordinary"),
                    "fixture",
                )
                .await
        );
    }
    for item in [queued, queued_running] {
        assert!(
            reconciler
                .note_queue_marker_added(
                    &shared,
                    item,
                    owner.clone(),
                    TurnViewIdentity::Test("ordinary"),
                    '📬',
                    "fixture",
                )
                .await
        );
    }
    assert!(
        reconciler
            .note_turn_started(
                &shared,
                queued_running,
                owner.clone(),
                TurnViewIdentity::Test("ordinary"),
                "fixture_dispatch",
            )
            .await
    );
    for (channel, message) in [(a, ids[0]), (a, ids[1]), (b, ids[2])] {
        inject(&shared, channel, message).await;
    }
    let start = reconciler.ops().len();
    let (fake, supervisor) = supervisor(&shared, &[a, b]);
    supervisor.tick(Instant::now()).await;
    fake.set(a, UNKNOWN);
    supervisor.tick(Instant::now()).await;
    assert_eq!(
        reconciler.ops().len(),
        start,
        "busy and unknown preserve injection marks"
    );
    fake.set(a, IDLE);
    supervisor.tick(Instant::now()).await;
    settled(&shared, ids[0]).await;
    settled(&shared, ids[1]).await;
    let mut changed: Vec<_> = reconciler.ops()[start..]
        .iter()
        .map(|op| {
            (
                op.target.channel_id.get(),
                op.target.message_id.get(),
                op.emoji,
                op.add,
            )
        })
        .collect();
    changed.sort_unstable();
    let mut expected: Vec<_> = ids[..2]
        .iter()
        .flat_map(|&id| [(a, id, '⏳', false), (a, id, '✅', true)])
        .collect();
    expected.sort_unstable();
    assert_eq!(
        changed, expected,
        "idle must touch only this channel's listed message ids"
    );
    assert!(
        reconciler
            .note_turn_completed(
                &shared,
                anchor,
                owner,
                TurnViewIdentity::Test("ordinary"),
                "fixture_complete",
            )
            .await
    );
    assert!(
        reconciler
            .ops()
            .iter()
            .any(|op| op.target == anchor && op.emoji == '✅' && op.add)
    );
    let before_b = reconciler.ops().len();
    fake.set(b, IDLE);
    supervisor.tick(Instant::now()).await;
    settled(&shared, ids[2]).await;
    assert_eq!(
        reconciler.ops().len(),
        before_b + 2,
        "the other channel retains its own list"
    );
    assert!(
        reconciler
            .ops()
            .iter()
            .filter(|op| op.target.message_id.get() == ids[2])
            .all(|op| op.target.channel_id.get() == b),
        "another channel's injection must complete in its own channel"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_injection_mark_added_after_idle_publication_finishes_on_the_next_existing_observation()
{
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = 7_269_003;
    let message = 1514500851761287331;
    let (fake, supervisor) = supervisor(&shared, &[channel]);
    fake.set(channel, IDLE);
    supervisor.tick(Instant::now()).await;
    inject(&shared, channel, message).await;
    assert_eq!(shared.turn_view_reconciler.ops().len(), 1);
    supervisor.tick(Instant::now()).await;
    settled(&shared, message).await;
    assert_eq!(shared.turn_view_reconciler.ops().len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn idle_retries_each_failed_reaction_once_then_discards_the_list() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().turn_view_reconciler =
        TurnViewReconciler::with_test_deliveries(vec![
            TurnViewDelivery::Failed,
            TurnViewDelivery::Failed,
            TurnViewDelivery::Delivered,
            TurnViewDelivery::Failed,
            TurnViewDelivery::Failed,
        ]);
    let channel = 7_269_004;
    let message = 1514500851761287341;
    inject(&shared, channel, message).await;
    let (fake, supervisor) = supervisor(&shared, &[channel]);
    fake.set(channel, IDLE);
    supervisor.tick(Instant::now()).await;
    settled(&shared, message).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while shared.turn_view_reconciler.ops().len() < 5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("each failed reaction gets its second and final attempt");
    let ops = shared.turn_view_reconciler.ops();
    assert_eq!(
        ops.iter().map(|op| (op.emoji, op.add)).collect::<Vec<_>>(),
        [
            ('⏳', true),
            ('⏳', false),
            ('⏳', false),
            ('✅', true),
            ('✅', true)
        ]
    );
    supervisor.tick(Instant::now()).await;
    tokio::task::yield_now().await;
    assert_eq!(
        shared.turn_view_reconciler.ops(),
        ops,
        "no further retries after two attempts"
    );
}
