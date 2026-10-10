use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::Method;
use tokio::sync::Notify;

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::{
    MockDiscord, age_file, fingerprint,
};
use crate::services::discord::inflight::{inflight_state_path, save_inflight_state};

#[derive(Default)]
struct ProbeBarrier {
    entered: Notify,
    release: Notify,
}

tokio::task_local! {
    static OWNER_PROBE: Arc<ProbeBarrier>;
}

pub(super) async fn after_owner_probe() {
    if let Ok(barrier) = OWNER_PROBE.try_with(Arc::clone) {
        barrier.entered.notify_one();
        barrier.release.notified().await;
    }
}

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
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeEAdapter);
        row.claude_e_pid = Some(u32::MAX - 1);
        row.claude_e_process_starttime = Some(1);
        row.claude_e_macos_lstart_hash = Some(1);
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_panel_rechecks_retirement_after_owner_probe() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = MockDiscord::start().await;
    for (channel, retire) in [(6_325_502_001, true), (6_325_502_002, false)] {
        let row = row(channel, true);
        let path = seed(&row, 3600);
        let before = fingerprint(&path);
        let barrier = Arc::new(ProbeBarrier::default());
        let shared = shared.clone();
        let http = discord.http.clone();
        let barrier_task = barrier.clone();
        let task = tokio::spawn(OWNER_PROBE.scope(barrier_task, async move {
            let mut tracker = StalledEditTracker::default();
            run_placeholder_sweep_pass(&http, &shared, &ProviderKind::Claude, &mut tracker).await
        }));
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
            .await
            .expect("owner probe reached");
        let _retired = retire.then(|| RetiredForTest::new("claude", channel));
        barrier.release.notify_one();
        let report = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("sweep completed")
            .unwrap();
        let deletes = discord
            .calls_for(channel)
            .iter()
            .filter(|c| c.starts_with("DELETE"))
            .count();
        assert_eq!(
            deletes,
            usize::from(!retire),
            "retirement gates inline DELETE"
        );
        assert_eq!(report.reclaimed_panels, usize::from(!retire));
        if retire {
            assert_eq!(fingerprint(&path), before, "retired row was not rewritten");
            // Remove this fixture so the control pass only holds its own probe.
            std::fs::remove_file(path).unwrap();
        } else {
            assert!(!path.exists(), "legacy panel-only row converges");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tick_retries_5xx_without_mutating_retired_rows() {
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
    let (retired, legacy) = (6_325_502_011, 6_325_502_012);
    let rpath = seed(&row(retired, false), 600);
    let lpath = seed(&row(legacy, false), 600);
    let before_r = fingerprint(&rpath);
    let before_l = fingerprint(&lpath);
    let _retired = RetiredForTest::new("claude", retired);
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
    assert!(discord.calls_for(retired).is_empty());
    assert_eq!(fingerprint(&rpath), before_r);
}

/// The sweep leaves an input-protected row unread in Discord and on disk, and re-judges
/// after its probe so a channel protected meanwhile is not edited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tick_leaves_input_protected_rows_before_and_after_probe() {
    use crate::services::discord::input_runtime::fence::{Gate, test_health};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (protected, race, legacy) = (6_325_578_001, 6_325_578_002, 6_325_578_003);
    let discord = MockDiscord::start_with(Arc::new(move |method, path| {
        if method == Method::GET && path.contains(&format!("/channels/{race}/")) {
            Gate::protect(ProviderKind::Claude, race).unwrap();
        }
        None
    }))
    .await;
    let paths = [protected, race, legacy].map(|channel| seed(&row(channel, false), 600));
    let before = paths.each_ref().map(|path| fingerprint(path));
    let gate = Gate::protect(ProviderKind::Claude, protected).unwrap();
    let _health = test_health::Clear::new(&gate);
    let mut tracker = StalledEditTracker::default();
    tick::run_placeholder_sweeper_tick(&discord.http, &shared, &ProviderKind::Claude, &mut tracker)
        .await;
    assert!(discord.calls_for(protected).is_empty());
    assert!(discord.calls_for(race).iter().all(|c| c.starts_with("GET")));
    assert_eq!(fingerprint(&paths[0]), before[0]);
    assert_eq!(fingerprint(&paths[1]), before[1]);
    let legacy_calls = discord.calls_for(legacy);
    assert!(
        legacy_calls.iter().any(|c| c.starts_with("PATCH")),
        "{legacy_calls:?}"
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
    for (channel, retire) in [(6_325_502_021, true), (6_325_502_022, false)] {
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
        let _pane =
            InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent);
        let token = busy_turn(&shared, channel_id, &name).await;
        let mut state =
            crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude, channel)
                .unwrap();
        state.turn_nonce = token.turn_nonce().map(str::to_owned);
        let path = seed(&state, 3600);
        let before = fingerprint(&path);
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let _retired = retire.then(|| RetiredForTest::new("claude", channel));
        take_requested_intents_for_test();
        let kills_before = crate::services::provider::cancel_token_cleanup::executor::tmux_kill_dispatches_for_test();
        tick::run_placeholder_sweeper_tick(
            &discord.http,
            &shared,
            &ProviderKind::Claude,
            &mut tracker,
        )
        .await;
        let intents = take_requested_intents_for_test();
        assert_eq!(token.cancelled.load(Ordering::Relaxed), !retire);
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            usize::from(retire)
        );
        if retire {
            assert_eq!(fingerprint(&path), before);
            assert!(discord.calls_for(channel).is_empty());
            assert!(intents.is_empty());
            std::fs::remove_file(path).unwrap();
        } else {
            assert!(!path.exists(), "dead-pane row reclaimed");
            assert!(intents.contains(&TmuxCleanupIntent::CleanupSession));
            assert!(crate::services::provider::cancel_token_cleanup::executor::tmux_kill_dispatches_for_test() > kills_before);
            assert!(
                discord
                    .calls_for(channel)
                    .iter()
                    .any(|c| c.starts_with("PATCH"))
            );
        }
    }
    pool.close().await;
    db.drop().await;
}
