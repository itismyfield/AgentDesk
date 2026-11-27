//! Force-kill host-guard tests that seed DL teardown rows and ROOT runtime watchers.
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, runtime, runtime_state, seed, stop_recorded, turn_kept,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::*;
use poise::serenity_prelude::ChannelId;

// A force-kill the registry can key reads the stored rows before its tombstone, stop
// or kill; a missing row keeps main's name-only path, any other trace keeps the turn.
#[tokio::test]
async fn force_kill_stops_a_turn_only_after_the_host_guard_admits_it_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (shared, registry) = runtime(&pool).await;
    for (n, stored) in Stored::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_059_800 + n as u64);
        let name = format!("AgentDesk-claude-p4a-kill-{n}");
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel.get(),
            stored,
        )
        .await;
        let token = busy_turn(&shared, channel, &name).await;
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: name.clone(),
        };
        let result = force_kill_turn(Some(&registry), &target, "p4a host guard", "p4a").await;
        let admitted = matches!(stored, Stored::Legacy | Stored::Missing);
        assert_eq!(
            result.lifecycle_path != HOST_GUARD_KEPT_PATH,
            admitted,
            "{stored:?}"
        );
        assert_eq!(
            !turn_kept(&shared, channel, &token).await,
            admitted,
            "{stored:?}"
        );
        assert_eq!(stop_recorded(channel), admitted, "{stored:?}");
    }
    pool.close().await;
    db.drop().await;
}

// A kill runs only on the session its verdict approved: a runtime that took up another
// session between the verdict and the kill is left as it is.
#[tokio::test]
async fn force_kill_leaves_a_runtime_that_moved_on_after_its_verdict_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (shared, registry) = runtime(&pool).await;
    let channel = ChannelId::new(1_479_671_301_387_059_900);
    let name = "AgentDesk-claude-p4r-moved-a";
    let key = channel_key(&shared, name);
    seed(&pool, &key, name, channel.get(), Stored::Legacy).await;
    let token = busy_turn(&shared, channel, name).await;
    let target = TurnLifecycleTarget {
        provider: Some(ProviderKind::Claude),
        channel_id: Some(channel),
        tmux_name: name.to_string(),
    };
    let verdict = force_kill_verdict(Some(&registry), &target, None).await;
    assert!(!verdict.kept(), "the legacy session is admitted");
    let moved = "AgentDesk-claude-p4r-moved-b";
    crate::services::discord::register_resume_watcher_for_tests(&shared, channel, moved);
    let before = runtime_state(&shared, channel).await;
    let kill = force_kill_turn_with_verdict(Some(&registry), verdict, "moved on", "p4r");
    assert_eq!(kill.await.lifecycle_path, HOST_GUARD_KEPT_PATH);
    assert_eq!(runtime_state(&shared, channel).await, before);
    assert!(turn_kept(&shared, channel, &token).await);
    assert!(!stop_recorded(channel), "no stop is recorded");
    pool.close().await;
    db.drop().await;
}

// A kill carries out its verdict: a marker that turns Herdr after the verdict does not make
// the cleanup judge the host again, while a cleanup with no verdict still reads it.
#[test]
fn force_kill_follows_its_verdict_when_the_marker_changes_after_it_pg() {
    use crate::services::provider::cancel_token_cleanup::executor::{
        CleanupRequest, TmuxCleanupIntent, tmux_kill_dispatches_for_test,
        with_executor_dispatch_seam,
    };
    // The runtime root (env lock) comes before the dispatch seam, as in the stop host tests.
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut executor = tokio::runtime::Builder::new_current_thread();
    let executor = executor.enable_all().build().unwrap();
    with_executor_dispatch_seam(|| {
        executor.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let (shared, registry) = runtime(&pool).await;
            let channel = ChannelId::new(1_479_671_301_387_059_950);
            let name = "AgentDesk-claude-p4r-marker-after";
            let key = channel_key(&shared, name);
            seed(&pool, &key, name, channel.get(), Stored::Legacy).await;
            busy_turn(&shared, channel, name).await;
            let target = TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: name.to_string(),
            };
            let verdict = force_kill_verdict(Some(&registry), &target, None).await;
            assert!(!verdict.kept(), "the legacy session is admitted");
            let marker = crate::services::tmux_common::session_temp_path(name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(&marker, "herdr").unwrap();
            let kills = tmux_kill_dispatches_for_test();
            let kill = force_kill_turn_with_verdict(Some(&registry), verdict, "marker", "p4r");
            assert!(!kill.await.host_guard_kept());
            let dispatched = tmux_kill_dispatches_for_test() - kills;
            assert_eq!(
                dispatched, 1,
                "the approved session is killed once, by its cleanup"
            );
            let other = crate::services::provider::CancelToken::new();
            other.bind_unmanaged_session_name(name);
            let outcome = other.request_cleanup(CleanupRequest {
                cancel_source: "no verdict".to_string(),
                intent: TmuxCleanupIntent::CleanupSession,
                termination_reason: Some("p4r"),
                hard_stop_target: None,
            });
            assert!(
                outcome.host_refused,
                "a cleanup with no verdict still reads the marker"
            );
            assert_eq!(tmux_kill_dispatches_for_test() - kills, 1);
            pool.close().await;
            db.drop().await;
        })
    });
}
