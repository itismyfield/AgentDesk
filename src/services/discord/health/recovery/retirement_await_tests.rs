use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::ChannelId;
use tokio::sync::Notify;

use crate::config::TestEnvVarGuard;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::{
    fingerprint, seed_backfill_row,
};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

#[derive(Clone)]
struct ReturnBarrier {
    channel: u64,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

static RETURN_BARRIER: Mutex<Option<ReturnBarrier>> = Mutex::new(None);

pub(super) async fn hold_reattach_return(channel: u64) {
    let barrier = RETURN_BARRIER.lock().unwrap().clone();
    if let Some(barrier) = barrier.filter(|barrier| barrier.channel == channel) {
        barrier.entered.notify_one();
        barrier.release.notified().await;
    }
}

struct BarrierGuard;
impl Drop for BarrierGuard {
    fn drop(&mut self) {
        *RETURN_BARRIER.lock().unwrap() = None;
    }
}

async fn reattach_case(retire: bool, channel: u64) {
    let temp = tempfile::tempdir().unwrap();
    let _env =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
    let provider = ProviderKind::Codex;
    let registry = HealthRegistry::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let session = format!("n4a-p2-stall-test-{channel}");
    let output = temp.path().join(format!("{channel}.jsonl"));
    std::fs::write(&output, "").unwrap();
    let output = output.to_string_lossy().to_string();
    let state = InflightTurnState::new(
        provider.clone(),
        channel,
        None,
        1,
        channel * 10,
        channel * 10 + 1,
        "prompt".into(),
        None,
        Some(session.clone()),
        Some(output.clone()),
        None,
        0,
    );
    let row = seed_backfill_row(&state);
    let initial = fingerprint(&row);
    let cancelled = Arc::new(AtomicBool::new(false));
    shared.tmux_watchers.insert(
        ChannelId::new(channel),
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: session,
            output_path: output,
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: cancelled.clone(),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(chrono::Utc::now().timestamp_millis())),
        },
    );
    let barrier = ReturnBarrier {
        channel,
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    *RETURN_BARRIER.lock().unwrap() = Some(barrier.clone());
    let _barrier_guard = BarrierGuard;
    let mut retired = None;
    let mut after_first_load = None;
    let work = super::run_stall_watchdog_pass(&registry, &provider);
    let transition = async {
        barrier.entered.notified().await;
        assert_ne!(fingerprint(&row), initial, "the first loader already ran");
        // Re-arm backfill while reattach is suspended to expose the later writing loader.
        seed_backfill_row(&state);
        after_first_load = fingerprint(&row);
        if retire {
            retired = Some(RetiredForTest::new(provider.as_str(), channel));
        }
        barrier.release.notify_one();
    };
    let (cleaned, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(work, transition)
    })
    .await
    .expect("reattach return barrier");
    assert_eq!(cleaned, 0);
    assert!(!cancelled.load(Ordering::Relaxed));
    assert!(shared.tmux_watchers.contains_key(&ChannelId::new(channel)));
    assert!(after_first_load.is_some());
    if retire {
        assert_eq!(fingerprint(&row), after_first_load);
    } else {
        assert_ne!(
            fingerprint(&row),
            after_first_load,
            "legacy backfill still runs"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stall_watchdog_rechecks_retirement_after_reattach() {
    let _absence = super::watcher_respawn::lock_watcher_absence_for_test().await;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    reattach_case(true, 6_325_411_001).await;
    reattach_case(false, 6_325_411_002).await;
}
