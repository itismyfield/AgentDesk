use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::Method;

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::legacy_supervision::test_support::{
    MockDiscord, age_file, fingerprint,
};
use crate::services::discord::inflight::{inflight_state_path, save_inflight_state};

fn row(channel: u64, panel: bool) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        1,
        0,
        if panel { 0 } else { channel + 1 },
        "retirement fixture".into(),
        None,
        None,
        None,
        None,
        0,
    );
    if panel {
        row.status_message_id = Some(channel + 2);
        row.runtime_kind = Some(RuntimeHandoffKind::LegacyTmuxWrapper);
        row.tmux_session_name = Some(format!("u45-dead-owner-{channel}"));
    }
    row
}

fn seed(row: &InflightTurnState, age: i64) -> std::path::PathBuf {
    save_inflight_state(row).unwrap();
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
    age_file(&path, age);
    path
}

/// A dead-owner panel-only row loses its panel inline and converges.
#[cfg(unix)]
async fn inline_panel_reclaims_legacy_panel_only_row(
    pool: &sqlx::PgPool,
    shared: &Arc<SharedData>,
) {
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, channel_key, seed as seed_host,
    };
    let discord = MockDiscord::start().await;
    let channel = 6_325_502_002;
    let row = row(channel, true);
    let name = row.tmux_session_name.as_deref().unwrap();
    seed_host(
        pool,
        &channel_key(shared, name),
        name,
        channel,
        Stored::Legacy,
    )
    .await;
    let _host = crate::services::session_host::test_support::InjectedLivenessGuard::set(
        crate::services::session_host::HostSessionRef::tmux(name),
        crate::services::session_host::HostLiveness::DeadOrAbsent,
    );
    let path = seed(&row, 3600);
    let mut tracker = StalledEditTracker::default();
    let report =
        run_placeholder_sweep_pass(&discord.http, shared, &ProviderKind::Claude, &mut tracker)
            .await;
    let deletes = discord
        .calls_for(channel)
        .iter()
        .filter(|c| c.starts_with("DELETE"))
        .count();
    assert_eq!(deletes, 1, "the inline orphan panel is deleted");
    assert_eq!(report.reclaimed_panels, 1);
    assert!(!path.exists(), "legacy panel-only row converges");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tick_retries_5xx_and_then_edits_the_legacy_placeholder() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let unavailable = Arc::new(AtomicBool::new(true));
    let answer = unavailable.clone();
    let discord = MockDiscord::start_with(Arc::new(move |method, _| {
        (method == Method::GET && answer.load(Ordering::Relaxed)).then(|| {
            (
                503,
                serde_json::json!({"message": "retry later", "code": 0}),
            )
        })
    }))
    .await;
    let legacy = 6_325_502_012;
    let lpath = seed(&row(legacy, false), 600);
    let before_l = fingerprint(&lpath);
    let mut tracker = StalledEditTracker::default();
    tick::run_placeholder_sweeper_tick(&discord.http, &shared, &ProviderKind::Claude, &mut tracker)
        .await;
    assert_eq!(
        fingerprint(&lpath),
        before_l,
        "5xx leaves the legacy row retryable"
    );
    assert!(
        discord
            .calls_for(legacy)
            .iter()
            .all(|c| c.starts_with("GET"))
    );
    unavailable.store(false, Ordering::Relaxed);
    tick::run_placeholder_sweeper_tick(&discord.http, &shared, &ProviderKind::Claude, &mut tracker)
        .await;
    assert!(
        discord
            .calls_for(legacy)
            .iter()
            .any(|c| c.starts_with("PATCH")),
        "successful retry edits the placeholder"
    );
}

#[tokio::test]
async fn tick_dead_pane_reclaims_only_legacy_turn_and_requests_tmux_kill_pg() {
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, busy_turn, channel_key, seed as seed_host, shared_on,
    };
    use crate::services::provider::cancel_token_cleanup::executor::{
        TmuxCleanupIntent, take_requested_intents_for_test,
    };
    use crate::services::session_host::test_support::InjectedLivenessGuard;
    use crate::services::session_host::{HostLiveness, HostSessionRef};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let discord = MockDiscord::start().await;
    let mut tracker = StalledEditTracker::default();
    let channel = 6_325_502_022;
    let channel_id = serenity::ChannelId::new(channel);
    let name = format!("n4a-p2-dead-{channel}");
    seed_host(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel,
        Stored::Legacy,
    )
    .await;
    let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent);
    let token = busy_turn(&shared, channel_id, &name).await;
    let mut state =
        crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude, channel)
            .unwrap();
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    let path = seed(&state, 3600);
    shared.restart.global_active.store(1, Ordering::Relaxed);
    take_requested_intents_for_test();
    let kills_before =
        crate::services::provider::cancel_token_cleanup::executor::tmux_kill_dispatches_for_test();
    tick::run_placeholder_sweeper_tick(&discord.http, &shared, &ProviderKind::Claude, &mut tracker)
        .await;
    let intents = take_requested_intents_for_test();
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(!path.exists(), "dead-pane row reclaimed");
    assert!(intents.contains(&TmuxCleanupIntent::CleanupSession));
    assert!(
        crate::services::provider::cancel_token_cleanup::executor::tmux_kill_dispatches_for_test()
            > kills_before
    );
    assert!(
        discord
            .calls_for(channel)
            .iter()
            .any(|c| c.starts_with("PATCH"))
    );
    #[cfg(unix)]
    inline_panel_reclaims_legacy_panel_only_row(&pool, &shared).await;
    pool.close().await;
    db.drop().await;
}
