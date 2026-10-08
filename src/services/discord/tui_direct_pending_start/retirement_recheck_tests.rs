use super::*;
use crate::services::discord::{self as discord, health::legacy_supervision::RetiredForTest};
use crate::services::provider::{CancelToken, ProviderKind};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

#[derive(Default)]
struct Barrier {
    entered: Notify,
    resume: Notify,
}

type BarrierKey = (&'static str, u64);
static BARRIERS: LazyLock<Mutex<HashMap<BarrierKey, Arc<Barrier>>>> = LazyLock::new(Mutex::default);

pub(in crate::services::discord) async fn pause(site: &'static str, channel: u64) {
    let barrier = BARRIERS.lock().unwrap().get(&(site, channel)).cloned();
    if let Some(barrier) = barrier {
        barrier.entered.notify_one();
        barrier.resume.notified().await;
    }
}

struct InstalledBarrier(BarrierKey);
impl Drop for InstalledBarrier {
    fn drop(&mut self) {
        BARRIERS.lock().unwrap().remove(&self.0);
    }
}

async fn check_reclaim(site: &'static str, channel_id: u64, retire: bool, pending_start: bool) {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(channel_id);
    let shared = discord::make_shared_data_for_tests();
    let registry = discord::health::HealthRegistry::new();
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    let committed = site == "leaked_row_complete_after_mailbox";
    let output = temp.path().join("captured.jsonl");
    std::fs::write(
        &output,
        if committed {
            r#"{"type":"result","result":"delivered","session_id":"s"}"#
        } else {
            r#"{"type":"system","subtype":"init","session_id":"s"}"#
        },
    )
    .unwrap();
    let token = Arc::new(CancelToken::new());
    let user_msg = channel_id + 100;
    assert!(
        discord::mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(1),
            MessageId::new(user_msg),
        )
        .await
    );
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let mut row = discord::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        1,
        user_msg,
        user_msg + 1,
        "stale foreign".to_string(),
        None,
        Some("n4a-p2-reclaim-fixture".to_string()),
        Some(output.to_string_lossy().to_string()),
        None,
        0,
    );
    let stale = (chrono::Local::now()
        - chrono::Duration::seconds(STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS + 1))
    .format("%Y-%m-%d %H:%M:%S")
    .to_string();
    row.started_at = stale.clone();
    row.updated_at = stale;
    row.set_relay_owner_kind(discord::inflight::RelayOwnerKind::Watcher);
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    row.last_offset = std::fs::metadata(&output).unwrap().len();
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    row.save_generation = 1;
    if committed {
        row.terminal_delivery_committed = true;
        row.full_response = "delivered".to_string();
        row.response_sent_offset = row.full_response.len();
    }
    let root = discord::runtime_store::discord_inflight_root().unwrap();
    let path = discord::inflight::inflight_state_path(&root, &provider, channel_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&row).unwrap()).unwrap();
    if site == "leaked_row_after_runtime" {
        discord::health::legacy_supervision::test_support::seed_backfill_row(&row);
    }
    let before = discord::health::legacy_supervision::test_support::fingerprint(&path);
    let record = TuiDirectPendingStart {
        provider: "claude".to_string(),
        channel_id,
        tmux_session_name: "n4a-p2-reclaim-fixture".to_string(),
        prompt_text: String::new(),
        anchor_message_id: u64::MAX,
        lease_relay_owner: String::new(),
        lease_runtime_kind: None,
        lease_turn_id: None,
        lease_session_key: None,
        generation: shared.restart.current_generation,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: None,
        native_turn_id: None,
    };
    let barrier = Arc::new(Barrier::default());
    BARRIERS
        .lock()
        .unwrap()
        .insert((site, channel_id), barrier.clone());
    let _installed = InstalledBarrier((site, channel_id));
    let operation = async {
        if pending_start {
            demote_stale_foreign_inflight_if_current(&shared, &record).await
        } else {
            discord::relay_recovery::leaked_row_sweep::sweep_leaked_inflight_rows(
                &registry, &provider,
            )
            .await
                == 1
        }
    };
    let controller = async {
        barrier.entered.notified().await;
        let retired = retire.then(|| RetiredForTest::new("claude", channel_id));
        barrier.resume.notify_one();
        retired
    };
    let (applied, _retired) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(operation, controller)
    })
    .await
    .expect("reclaim must reach the held await and finish");
    if site == "leaked_row_after_runtime" {
        // Backfill advances the durable generation, so the old snapshot cannot reclaim it yet.
        assert!(!applied);
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        let after = discord::health::legacy_supervision::test_support::fingerprint(&path);
        if retire {
            assert_eq!(after, before, "retirement must stop the writing loader");
        } else {
            assert_ne!(after, before, "the Legacy loader must persist its backfill");
            let mut written: discord::inflight::InflightTurnState =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(written.finalizer_turn_id, user_msg);
            assert!(written.save_generation > row.save_generation);
            assert_ne!(written.updated_at, row.updated_at);
            // Model expiry of the backfill's renewed grace period without a wall-clock wait.
            written.updated_at = row.updated_at.clone();
            std::fs::write(&path, serde_json::to_vec_pretty(&written).unwrap()).unwrap();
            drop(_installed);
            assert_eq!(
                discord::relay_recovery::leaked_row_sweep::sweep_leaked_inflight_rows(
                    &registry, &provider,
                )
                .await,
                1,
            );
            assert!(!path.exists());
            assert!(token.cancelled.load(Ordering::Relaxed));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
            assert!(
                discord::mailbox_snapshot(&shared, channel)
                    .await
                    .cancel_token
                    .is_none()
            );
        }
        return;
    }
    let should_apply = !retire || pending_start;
    assert_eq!(
        applied, should_apply,
        "{site}, retire={retire}, pending={pending_start}"
    );
    assert_eq!(token.cancelled.load(Ordering::Relaxed), should_apply);
    assert_eq!(
        shared.restart.global_active.load(Ordering::Relaxed),
        usize::from(!should_apply)
    );
    assert_eq!(
        discord::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_none(),
        should_apply
    );
    if should_apply {
        assert!(!path.exists(), "actual finalizer must reclaim the row");
    } else {
        assert_eq!(
            discord::health::legacy_supervision::test_support::fingerprint(&path),
            before
        );
    }
}

fn run_matrix(site: &'static str, channel: u64, pending_control: bool) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            check_reclaim(site, channel, true, false).await;
            check_reclaim(site, channel + 1, false, false).await;
            if pending_control {
                check_reclaim(site, channel + 2, true, true).await;
            }
        });
}

#[test]
fn retirement_after_runtime_lookup_preserves_writing_loader() {
    run_matrix("leaked_row_after_runtime", 6_325_512_001, false);
}

#[test]
fn retirement_after_cancel_mailbox_preserves_real_reclaim_and_pending_start() {
    run_matrix("leaked_row_cancel_after_mailbox", 6_325_512_011, true);
}

#[test]
fn retirement_after_complete_mailbox_preserves_real_reclaim_and_pending_start() {
    run_matrix("leaked_row_complete_after_mailbox", 6_325_512_021, true);
}
