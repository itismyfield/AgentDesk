//! The drifted same-episode row retires through the production keyed host gate, too.

use poise::serenity_prelude::ChannelId;

use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, runtime, seed,
};
use crate::services::discord::inflight::{load_inflight_state, save_inflight_state};
use crate::services::provider::ProviderKind;

#[tokio::test]
async fn keyed_host_gate_heal_retires_a_drifted_same_episode_row_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (shared, _registry) = runtime(&pool).await;
    let channel = 6_484_301;
    let token = super::super::tests::seed_busy_channel(&shared, channel).await;
    let mut state = load_inflight_state(&ProviderKind::Claude, channel).unwrap();
    let name = state.tmux_session_name.clone().unwrap();
    seed(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel,
        Stored::Legacy,
    )
    .await;
    // Synthetic drift: same nonce, finalizer id moved off the mailbox key.
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    state.user_msg_id = channel * 10 + 1;
    save_inflight_state(&state).unwrap();
    shared
        .restart
        .global_active
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let dead =
        |_: String| -> futures::future::BoxFuture<'static, bool> { Box::pin(async { false }) };
    let healed = super::super::heal_stale_busy_mailbox_with_probe(
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(channel),
        "intake_keyed_drift_test",
        false,
        &dead,
        &super::super::host_guard::keyed_host_gate,
    )
    .await;
    assert!(healed);
    assert!(load_inflight_state(&ProviderKind::Claude, channel).is_none());
    let next = poise::serenity_prelude::MessageId::new(channel * 10 + 2);
    assert!(!super::admit_next_turn(&shared, ChannelId::new(channel), next).await);
    pool.close().await;
    db.drop().await;
}
