//! A failed carried-turn receipt survives the automatic empty-orphan cleaner.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::o_stall_tests::watcher;
use super::*;
use crate::db::turns::TerminalOutcome;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::tui_o::writer::actor::{Owing, Undelivered};
use crate::services::tui_o::writer::host::OwedView;

const PROVIDER: ProviderKind = ProviderKind::Claude;

struct Fixture {
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    registry: HealthRegistry,
    channel: ChannelId,
    row_path: PathBuf,
    user_msg_id: u64,
    o_task: tokio::task::JoinHandle<()>,
    _keep: Vec<Box<dyn std::any::Any>>,
}

impl Fixture {
    async fn new(channel: u64, turn_mode: bool, origins: bool) -> Self {
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let source_root = tempfile::tempdir().unwrap();
        let binding = discord::health::BindingRoot::enter(source_root.path());
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = 6;
        let path = source_root.path().join(format!("{channel}.jsonl"));
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"receipt-input\",\"message\":{\"content\":\"input\"}}\n",
                "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n"
            ),
        )
        .unwrap();
        let session = format!("AgentDesk-claude-7807-receipt-{channel}");
        let ready = discord::health::ReadyPane::mark(&session);
        let live = crate::services::tmux_diagnostics::PaneLivenessOverrideGuard::set(
            &session,
            crate::services::platform::tmux::PaneLiveness::Live,
        );
        let confirmation = discord::health::bind_turn_mode_transcript(
            &shared, &PROVIDER, channel, &session, &path,
        );
        let confirmation = turn_mode.then_some(confirmation);
        let handle = watcher(&session, &path);
        handle
            .last_heartbeat_ts_ms
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
        shared.tmux_watchers.insert(ChannelId::new(channel), handle);
        let owned = crate::services::tui_o::cutover::test_override::force_channels(&[(
            channel,
            RuntimeHandoffKind::ClaudeTui,
        )]);

        let user_msg_id = channel + 100;
        let mut row = InflightTurnState::new(
            PROVIDER,
            channel,
            None,
            7,
            user_msg_id,
            user_msg_id + 1,
            "input".into(),
            Some(format!("receipt-session-{channel}")),
            Some(session),
            Some(path.display().to_string()),
            None,
            0,
        );
        row.born_generation = 5;
        row.turn_nonce = Some(format!("receipt-nonce-{channel}"));
        if origins {
            row.input_origin_ids = vec![format!("receipt-origin-{channel}")];
        }
        inflight::save_inflight_state(&row).unwrap();
        let row_path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &PROVIDER,
            channel,
        );
        discord::health::settled_reason(&shared, &PROVIDER, channel).await;

        let owing = Owing::default();
        crate::services::tui_o::writer::host::process_readiness().track_owed_for_test(
            channel,
            OwedView {
                published: owing.published.subscribe(),
                demand: owing.demand.clone(),
            },
        );
        let o_task = tokio::spawn(async move {
            loop {
                if owing.demand.wanted() {
                    owing.published.send_replace(Some(Undelivered {
                        asked: owing.demand.asked(),
                        captured: vec![(path.clone(), std::fs::metadata(&path).unwrap().len())],
                        ..Undelivered::default()
                    }));
                }
                tokio::task::yield_now().await;
            }
        });
        let registry = HealthRegistry::new();
        registry
            .register(PROVIDER.as_str().to_string(), shared.clone())
            .await;
        Self {
            shared,
            pool,
            registry,
            channel: ChannelId::new(channel),
            row_path,
            user_msg_id,
            o_task,
            _keep: vec![
                Box::new(db),
                Box::new(owned),
                Box::new(confirmation),
                Box::new(live),
                Box::new(ready),
                Box::new(binding),
                Box::new(source_root),
                Box::new(runtime),
            ],
        }
    }

    fn row(&self) -> Option<InflightTurnState> {
        inflight::load_inflight_state_read_only(&PROVIDER, self.channel.get())
    }

    fn age_row(&self) -> InflightTurnState {
        let mut row = self.row().expect("the failed receipt keeps its row");
        row.updated_at = (chrono::Local::now()
            - chrono::Duration::seconds(STALL_WATCHDOG_THRESHOLD_SECS as i64 + 60))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
        std::fs::write(&self.row_path, serde_json::to_vec(&row).unwrap()).unwrap();
        let aged = inflight::parse_updated_at_unix(&row.updated_at).unwrap();
        filetime::set_file_mtime(&self.row_path, filetime::FileTime::from_unix_time(aged, 0))
            .unwrap();
        row
    }

    async fn assert_empty_orphan_candidate(&self) {
        let snapshot = self
            .registry
            .snapshot_watcher_state_for_shared(&PROVIDER, self.shared.clone(), self.channel.get())
            .await
            .expect("the live watcher exposes this channel");
        assert!(inflight_completed_stale_leak_detected(
            snapshot.attached,
            snapshot.desynced,
            snapshot.inflight_state_present,
            snapshot.mailbox_active_user_msg_id,
            snapshot.inflight_updated_at.as_deref(),
            snapshot.tmux_session_alive,
            chrono::Utc::now().timestamp(),
            STALL_WATCHDOG_THRESHOLD_SECS,
        ));
        let row = self.row().unwrap();
        assert!(
            row.restart_mode.is_none(),
            "a restart guard cannot mask the cleaner"
        );
        assert!(row.managed_submission.is_none());
        assert!(row.full_response.is_empty());
        assert_eq!(row.response_sent_offset, 0);
        assert!(!row.terminal_delivery_committed);
        assert!(completed_stale_no_answer_orphan_should_clean(
            row.terminal_delivery_committed,
            row.user_msg_id,
        ));
        let mailbox = discord::mailbox_snapshot(&self.shared, self.channel).await;
        assert!(mailbox.cancel_token.is_none());
        assert!(mailbox.active_user_message_id.is_none());
    }

    async fn pass(&self) {
        run_stall_watchdog_pass(&self.registry, &PROVIDER).await;
    }

    fn archive_count(&self) -> usize {
        let prefix = format!("{}.json.turn-mode-carried-turn", self.channel.get());
        std::fs::read_dir(inflight::inflight_runtime_root().unwrap().join("archive"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                    .count()
            })
            .unwrap_or(0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.o_task.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_carried_receipt_survives_stale_orphan_health_and_settles_once_pg() {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    for (channel, origins) in [(7_807_701, false), (7_807_702, true)] {
        let fx = Fixture::new(channel, true, origins).await;
        let options = fx.pool.connect_options().as_ref().clone();
        assert!(fx.row().unwrap().terminal_receipt.is_none());
        fx.pool.close().await;
        fx.pass().await;
        let marked = fx.row().expect("a failed PG commit keeps the carried row");
        let stamp = marked
            .terminal_receipt
            .expect("the health pass stamped its failed commit");
        assert_eq!(stamp.outcome, TerminalOutcome::Completed);
        assert_eq!(fx.archive_count(), 0);

        let aged = fx.age_row();
        fx.assert_empty_orphan_candidate().await;
        fx.pass().await;
        let kept = fx
            .row()
            .expect("the empty-orphan cleaner must preserve a pending receipt");
        assert_eq!(kept.terminal_receipt, Some(stamp));
        assert_eq!(
            kept.updated_at, aged.updated_at,
            "retrying a stamp must not refresh its age"
        );
        assert_eq!(kept.save_generation, aged.save_generation);
        assert_eq!(fx.archive_count(), 0);

        let recovered_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let recovered =
            discord::make_shared_data_for_tests_with_storage(Some(recovered_pool.clone()));
        assert!(discord::receipt_settle::settle_marked_row(&recovered, &PROVIDER, channel).await);
        assert!(fx.row().is_none());
        let turn_id = format!("discord:{channel}:{}", fx.user_msg_id);
        let terminal: (String, chrono::DateTime<chrono::Utc>, Vec<String>) = sqlx::query_as(
            "SELECT terminal_outcome, terminal_confirmed_at, consumed_origin_ids FROM turns WHERE turn_id=$1",
        )
        .bind(&turn_id)
        .fetch_one(&recovered_pool)
        .await
        .unwrap();
        assert_eq!(terminal.0, "completed");
        assert_eq!(
            terminal.1.timestamp_micros(),
            stamp.confirmed_at.timestamp_micros()
        );
        assert_eq!(terminal.2, aged.input_origin_ids);
        assert!(!discord::receipt_settle::settle_marked_row(&recovered, &PROVIDER, channel).await);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE turn_id=$1")
            .bind(&turn_id)
            .fetch_one(&recovered_pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "the retained receipt settles exactly once");
        recovered_pool.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unmarked_empty_orphan_still_clears_on_the_stale_health_pass_pg() {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    let fx = Fixture::new(7_807_703, false, false).await;
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        fx.channel.get()
    ));
    let aged = fx.age_row();
    assert!(aged.terminal_receipt.is_none());
    fx.assert_empty_orphan_candidate().await;
    fx.pass().await;
    assert!(fx.row().is_none(), "the ordinary empty orphan still clears");
    assert_eq!(
        fx.archive_count(),
        0,
        "the carried closer did not remove the control"
    );
}
