//! Residual home state cannot replace ordinary gateway ownership or host selection.

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::channel_home;
use crate::services::provider::ProviderKind;
use crate::services::turn_host::{self, TurnHost};

#[tokio::test(flavor = "current_thread")]
async fn residual_home_row_does_not_change_writer_or_turn_host_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[]);
    let _channels = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = CHANNEL.to_string();
    let poison_epoch = i64::MAX - 7;
    sqlx::query(
        "INSERT INTO o_channel_homes
          (channel_id, provider, state, holder, target, epoch, renewed_at, detail)
         VALUES ($1, 'codex', 'worker', 'stale-remote', NULL, $2,
                 NOW() - INTERVAL '1 year', 'poison: ignore this home authority')",
    )
    .bind(&channel)
    .bind(poison_epoch)
    .execute(&pool)
    .await
    .unwrap();
    let read = || async {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT row_to_json(h)::jsonb FROM o_channel_homes h WHERE channel_id=$1",
        )
        .bind(&channel)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = read().await;
    assert_eq!(before["epoch"], poison_epoch);
    assert_eq!(before["holder"], "stale-remote");
    assert_eq!(before["provider"], "codex");
    let home = channel_home::register_for_test(CHANNEL, None);
    assert_eq!(home.refusal(), Some(channel_home::HomeRefusal::NotHeld));
    let harness = Harness::new();
    let mut writer = harness.writer();
    harness.gate.acquired();
    let posted = writer
        .deliver(&piece("poison-home", "ignored-home-row"))
        .await;
    let refusal =
        turn_host::refusal_before_turn(Some(&pool), &ProviderKind::Claude, CHANNEL, || async {
            None
        })
        .await;
    let host = turn_host::for_turn(Some(&pool), &ProviderKind::Claude, CHANNEL, None).await;
    assert_eq!(
        (
            posted == Step::Done,
            refusal.is_none(),
            matches!(host, TurnHost::Tmux)
        ),
        (true, true, true),
        "writer and TurnHost ignore residual home authority",
    );
    assert_eq!(harness.port.posts(), ["ignored-home-row"]);
    assert_eq!(read().await, before, "residual home row is untouched");
    channel_home::unregister(&channel);
    pool.close().await;
    db.drop().await;
}
