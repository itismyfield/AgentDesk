//! A live Claude original and the watchdog's watcher respawn take the same slot gate.
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::idle_relay_absence::{RoutableUnwatchedSession, observe_routable_unwatched_session};
use super::*;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::CancelToken;

/// Pauses one channel's admitted respawn before its snapshot.
pub(crate) mod respawn_gap {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use poise::serenity_prelude::ChannelId;
    use tokio::sync::Notify;

    type Gap = (Arc<Notify>, Arc<Notify>);
    static GAPS: Mutex<Option<HashMap<u64, Gap>>> = Mutex::new(None);

    /// Returns (reached, resume) for the channel's next admitted respawn.
    pub(crate) fn arm(channel: u64) -> Gap {
        let gap = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let mut gaps = GAPS.lock().unwrap_or_else(|e| e.into_inner());
        gaps.get_or_insert_with(HashMap::new)
            .insert(channel, gap.clone());
        gap
    }

    pub(crate) async fn pause(channel: ChannelId) {
        let gap = {
            let mut gaps = GAPS.lock().unwrap_or_else(|e| e.into_inner());
            gaps.as_mut().and_then(|gaps| gaps.remove(&channel.get()))
        };
        if let Some((reached, resume)) = gap {
            reached.notify_one();
            resume.notified().await;
        }
    }
}

const PROVIDER: ProviderKind = ProviderKind::Claude;

/// Runtime root and a `tmux` that reports every pane alive and lists no sessions.
struct Fixture {
    _path: crate::config::TestEnvVarGuard,
    _root_env: crate::config::TestEnvVarGuard,
    _tmux_dir: tempfile::TempDir,
    root: tempfile::TempDir,
    registry: HealthRegistry,
    shared: Arc<SharedData>,
    channel: ChannelId,
    tmux: String,
}

impl Fixture {
    async fn new(channel: u64) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let root_env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let tmux_dir = tempfile::tempdir().unwrap();
        let script = tmux_dir.path().join("tmux");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in list-panes) echo 0 ;; esac; exit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(
            tmux_dir.path(),
        );
        let channel = ChannelId::new(channel);
        clear_watcher_absence(&PROVIDER, channel);
        RESPAWN_TEST_COUNTS.insert(channel.get(), [0, 0]);
        let shared = discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry
            .register(PROVIDER.as_str().to_string(), shared.clone())
            .await;
        Self {
            _path: path,
            _root_env: root_env,
            _tmux_dir: tmux_dir,
            root,
            registry,
            shared,
            tmux: format!("AgentDesk-claude-routine-{}", channel.get()),
            channel,
        }
    }

    fn row(&self, user_msg_id: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            PROVIDER,
            self.channel.get(),
            None,
            7,
            user_msg_id,
            user_msg_id + 1,
            "routine prompt".into(),
            None,
            Some(self.tmux.clone()),
            None,
            None,
            0,
        );
        state.turn_nonce = Some(format!("nonce-{user_msg_id}"));
        state
    }

    /// The mailbox claim a headless start takes before it registers.
    async fn claim(&self, user_msg_id: u64) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(&self.tmux);
        assert!(
            discord::mailbox_try_start_turn(
                &self.shared,
                self.channel,
                token.clone(),
                UserId::new(7),
                MessageId::new(user_msg_id),
            )
            .await
        );
        token
    }

    async fn sweep(&self, now: i64) {
        sweep_and_retry_absences(
            &self.registry,
            &PROVIDER,
            std::slice::from_ref(&self.shared),
            &std::collections::HashSet::new(),
            now,
        )
        .await;
    }

    fn arm_idle_absence(&self, now: i64) {
        observe_routable_unwatched_session(
            &PROVIDER,
            &RoutableUnwatchedSession {
                channel_id: self.channel,
                tmux_session: self.tmux.clone(),
            },
            now,
        );
    }

    fn counts(&self) -> [usize; 2] {
        *RESPAWN_TEST_COUNTS.get(&self.channel.get()).unwrap()
    }

    fn failed_attempts(&self) -> Option<u32> {
        WATCHER_ABSENCE
            .get(&WatcherAbsenceKey::new(&PROVIDER, self.channel))
            .map(|state| state.failed_attempts)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        clear_watcher_absence(&PROVIDER, self.channel);
        RESPAWN_TEST_COUNTS.remove(&self.channel.get());
    }
}

/// The fresh-routine case: no watcher, the original reads the transcript itself.
#[tokio::test]
async fn a_registered_claude_original_keeps_the_watchdog_tick_from_respawning() {
    let _absence = lock_watcher_absence_for_test().await;
    let fx = Fixture::new(5_707_001).await;
    let token = fx.claim(5_707_101).await;
    let row = fx.row(5_707_101);
    let original =
        discord::live_bridge::register_without_requeue(&fx.shared, &PROVIDER, &row, &token)
            .await
            .unwrap();
    discord::inflight::save_inflight_state_create_new(&row).unwrap();
    let t0 = chrono::Utc::now().timestamp();

    fx.sweep(t0).await;
    assert_eq!(
        fx.counts(),
        [0, 0],
        "no respawn may run while the original lives"
    );
    assert_eq!(
        fx.failed_attempts(),
        None,
        "the relay-work sweep must not arm"
    );

    // The routing-registry sweep arms without the relay-work evidence; the retry retires it.
    fx.arm_idle_absence(t0 + 30);
    assert_eq!(fx.failed_attempts(), Some(0));
    assert_eq!(
        retry_pending_watcher_respawns(
            &fx.registry,
            &PROVIDER,
            std::slice::from_ref(&fx.shared),
            t0 + 30
        )
        .await,
        0
    );
    assert_eq!(fx.counts(), [0, 0]);
    assert_eq!(
        fx.failed_attempts(),
        None,
        "a live original retires the absence"
    );
    let durable = discord::inflight::load_inflight_state(&PROVIDER, fx.channel.get()).unwrap();
    assert_eq!(
        durable.effective_relay_owner_kind(),
        discord::inflight::RelayOwnerKind::None,
        "the original keeps relay ownership"
    );
    drop(original);
}

/// Producer and bridge share one registration; the reader ends on its first send after the bridge dies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bridge_panic_keeps_the_original_live_until_its_real_reader_ends() {
    let _absence = lock_watcher_absence_for_test().await;
    let fx = Fixture::new(5_707_002).await;
    let token = fx.claim(5_707_102).await;
    let row = fx.row(5_707_102);
    let original =
        discord::live_bridge::register_without_requeue(&fx.shared, &PROVIDER, &row, &token)
            .await
            .unwrap()
            .unwrap();
    discord::inflight::save_inflight_state_create_new(&row).unwrap();
    let transcript = fx.root.path().join("transcript.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let producer_registration = original.clone();
    let path = transcript.display().to_string();
    let producer = tokio::task::spawn_blocking(move || {
        let _original_registration = producer_registration;
        crate::services::session_backend::read_output_file_until_result_with_harvest(
            &path,
            0,
            tx,
            None,
            crate::services::provider::SessionProbe::new(|| true, || false),
        )
        .map(|(result, _)| result)
        .map_err(|failure| failure.error)
    });
    let bridge_registration =
        discord::live_bridge::retain_original(&PROVIDER, fx.channel.get(), &token)
            .expect("the bridge shares the start's registration");
    drop(original);
    let bridge = discord::task_supervisor::spawn_observed("fixture_bridge", async move {
        let _registration = bridge_registration;
        let _rx = rx;
        panic!("fixture bridge panic");
    });
    bridge.await.unwrap();
    let t0 = chrono::Utc::now().timestamp();

    assert!(discord::live_bridge::is_live(&PROVIDER, fx.channel.get()));
    fx.sweep(t0).await;
    assert_eq!(
        fx.counts(),
        [0, 0],
        "the producer still holds the registration"
    );

    let line = serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "late output"}]},
    });
    std::fs::write(&transcript, format!("{line}\n")).unwrap();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), producer)
        .await
        .expect("the reader must end once its bridge is gone")
        .unwrap()
        .unwrap();
    assert!(matches!(
        ended,
        crate::services::provider::ReadOutputResult::Cancelled { .. }
    ));
    assert!(!discord::live_bridge::is_live(&PROVIDER, fx.channel.get()));

    fx.sweep(t0 + 30).await;
    assert_eq!(fx.counts()[0], 1, "the next tick must drive one respawn");
}

/// A respawn admitted first keeps a starting original off disk until the respawn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawn_admitted_first_holds_the_original_start_until_it_ends() {
    for recovery_mints_a_row in [false, true] {
        let _absence = lock_watcher_absence_for_test().await;
        let fx = Fixture::new(5_707_003 + u64::from(recovery_mints_a_row)).await;
        let t0 = chrono::Utc::now().timestamp();
        fx.arm_idle_absence(t0);
        let (reached, resume) = respawn_gap::arm(fx.channel.get());
        let retry = retry_pending_watcher_respawns(
            &fx.registry,
            &PROVIDER,
            std::slice::from_ref(&fx.shared),
            t0,
        );
        let start = tokio::sync::Notify::new();
        let original = async {
            start.notified().await;
            let token = fx.claim(5_707_103).await;
            let row = fx.row(5_707_103);
            let registration =
                discord::live_bridge::register_without_requeue(&fx.shared, &PROVIDER, &row, &token)
                    .await;
            if registration.is_ok() {
                discord::inflight::save_inflight_state_create_new(&row).unwrap();
            }
            registration
        };
        let gap = async {
            reached.notified().await;
            start.notify_one();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert!(
                discord::inflight::load_inflight_state(&PROVIDER, fx.channel.get()).is_none(),
                "the waiting original must not have created its row"
            );
            assert!(!discord::live_bridge::is_live(&PROVIDER, fx.channel.get()));
            if recovery_mints_a_row {
                discord::inflight::save_inflight_state(&fx.row(5_707_200)).unwrap();
            }
            resume.notify_one();
        };
        let (attempted, registration, ()) = tokio::join!(retry, original, gap);
        assert_eq!(attempted, 1);
        let durable = discord::inflight::load_inflight_state(&PROVIDER, fx.channel.get()).unwrap();
        if recovery_mints_a_row {
            assert!(
                matches!(registration, Err(false)),
                "a changed episode defers the start"
            );
            assert_eq!(durable.user_msg_id, 5_707_200);
            assert_eq!(fx.counts(), [1, 1]);
        } else {
            let registration = registration.unwrap();
            assert!(
                registration.is_some(),
                "the start resumes once the respawn ends"
            );
            assert_eq!(fx.counts(), [1, 0], "the respawn found no row to pin");
            assert_eq!(durable.user_msg_id, 5_707_103);
            assert_eq!(
                durable.effective_relay_owner_kind(),
                discord::inflight::RelayOwnerKind::None
            );
            assert!(discord::live_bridge::is_live(&PROVIDER, fx.channel.get()));
        }
    }
}

/// Another recovery holding the slot is not a live original: the entry keeps its budget.
#[tokio::test]
async fn a_concurrent_respawn_leaves_the_absence_and_its_budget_alone() {
    let _absence = lock_watcher_absence_for_test().await;
    let fx = Fixture::new(5_707_005).await;
    let t0 = chrono::Utc::now().timestamp();
    fx.arm_idle_absence(t0);
    WATCHER_ABSENCE
        .get_mut(&WatcherAbsenceKey::new(&PROVIDER, fx.channel))
        .unwrap()
        .failed_attempts = 2;
    let held = discord::live_bridge::try_respawn_recovery(&PROVIDER, fx.channel.get()).unwrap();
    assert!(held.is_guarded());
    let runtimes = std::slice::from_ref(&fx.shared);

    assert_eq!(
        retry_pending_watcher_respawns(&fx.registry, &PROVIDER, runtimes, t0).await,
        0
    );
    assert_eq!(fx.counts(), [0, 0]);
    assert_eq!(
        fx.failed_attempts(),
        Some(2),
        "contention is not a live original"
    );

    drop(held);
    assert_eq!(
        retry_pending_watcher_respawns(&fx.registry, &PROVIDER, runtimes, t0).await,
        1
    );
    assert_eq!(fx.failed_attempts(), Some(3));
}
