//! #5996 P-L2a: a production-shaped idle-tmux channel for the entry tests of
//! the three decision sites that read `unread_bytes` as destructive permission
//! (manual reattach idle-clear, stale-mailbox repair route, explicit-background
//! watchdog). A live `AgentDesk-` tmux pane, a mailbox turn and a persisted row
//! aged past every staleness window; the tail reading comes only from the real
//! `SessionEnrichment::load`, never from a synthetic snapshot.
#![cfg_attr(not(unix), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::SharedTestEnvLockGuard;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::{SharedData, inflight};
use crate::services::provider::{CancelToken, ProviderKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnreadTailShape {
    /// The row names a transcript that does not exist, so no capture offset is
    /// read and the tail is UNMEASURED (`tail_not_measured`).
    RowOutputMissing,
    /// The row's transcript holds bytes no relay frontier covers: a MEASURED
    /// backlog, which refuses the clear without being a wedge.
    MeasuredBacklog,
}

pub(crate) struct UnreadTailSeed {
    pub(crate) registry: Arc<HealthRegistry>,
    pub(crate) provider: ProviderKind,
    pub(crate) channel: ChannelId,
    pub(crate) tmux_session: String,
    token: Arc<CancelToken>,
    _env: TestEnvVarGuard,
    _root: tempfile::TempDir,
    _lock: SharedTestEnvLockGuard,
}

impl UnreadTailSeed {
    /// `None` when tmux is unavailable: the caller skips and gives NO VERDICT.
    /// `explicit_background_owner` makes the row explicit background work whose
    /// watcher binding is owned by this channel (the watchdog site's shape);
    /// otherwise no watcher is bound, which is what reads the pane as orphaned.
    pub(crate) async fn start(
        channel: u64,
        shape: UnreadTailShape,
        explicit_background_owner: bool,
    ) -> Option<Self> {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        if !crate::services::platform::tmux::is_available() {
            eprintln!("skipping #5996 unread-tail entry fixture: tmux unavailable");
            return None;
        }
        let root = tempfile::tempdir().expect("runtime root");
        let env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(channel);
        let registry = Arc::new(HealthRegistry::new());
        let shared: Arc<SharedData> = crate::services::discord::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let tmux_session = format!("AgentDesk-claude-5996-l2a-{}-{channel}", std::process::id());
        let _ = crate::services::platform::tmux::kill_session(&tmux_session, "#5996 seed reset");
        assert!(
            crate::services::platform::tmux::create_session(&tmux_session, None, "sleep 120")
                .expect("start tmux fixture")
                .status
                .success(),
            "the idle-tmux sites need a live pane"
        );

        let output = root.path().join(format!("unread-tail-{channel}.jsonl"));
        if shape == UnreadTailShape::MeasuredBacklog {
            std::fs::write(
                &output,
                "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n",
            )
            .expect("write transcript fixture");
        }
        let user_msg = MessageId::new(channel.get() + 1);
        let token = Arc::new(CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(7),
                user_msg,
            )
            .await,
            "the seed's mailbox turn must start"
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg.get(),
            user_msg.get() + 1,
            "#5996 unread-tail entry fixture".to_string(),
            None,
            Some(tmux_session.clone()),
            Some(output.to_string_lossy().to_string()),
            None,
            0,
        );
        row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        if explicit_background_owner {
            row.task_notification_kind =
                Some(crate::services::agent_protocol::TaskNotificationKind::Background);
            // Discord write evidence, so the row's `updated_at` is the outbound
            // activity the watchdog ages.
            row.current_msg_len = 1;
            shared.tmux_watchers.insert(
                channel,
                crate::services::discord::TmuxWatcherHandle {
                    tmux_session_name: tmux_session.clone(),
                    output_path: output.to_string_lossy().to_string(),
                    paused: Arc::new(AtomicBool::new(false)),
                    resume_offset: Arc::new(std::sync::Mutex::new(None)),
                    cancel: Arc::new(AtomicBool::new(false)),
                    pause_epoch: Arc::new(AtomicU64::new(0)),
                    turn_delivered: Arc::new(AtomicBool::new(false)),
                    last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                        crate::services::discord::tmux_watcher_now_ms(),
                    )),
                },
            );
        }
        inflight::save_inflight_state(&row).expect("persist row fixture");
        // `save_inflight_state` stamps `updated_at`; age the persisted row past
        // the desync and watchdog windows directly.
        let stale_at = (chrono::Local::now() - chrono::Duration::minutes(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let row_path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().expect("inflight runtime root"),
            &provider,
            channel.get(),
        );
        let mut persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&row_path).expect("read row fixture"))
                .expect("parse row fixture");
        persisted["started_at"] = serde_json::Value::String(stale_at.clone());
        persisted["updated_at"] = serde_json::Value::String(stale_at);
        std::fs::write(
            &row_path,
            serde_json::to_string_pretty(&persisted).expect("serialize row fixture"),
        )
        .expect("age row fixture");

        Some(Self {
            registry,
            provider,
            channel,
            tmux_session,
            token,
            _env: env,
            _root: root,
            _lock: lock,
        })
    }

    /// Whether the seeded turn survived: its token uncancelled and its row kept.
    pub(crate) fn turn_kept(&self) -> bool {
        !self.token.cancelled.load(Ordering::Relaxed)
            && inflight::load_inflight_state(&self.provider, self.channel.get()).is_some()
    }

    /// The I20 unread-tail refusals recorded for this channel, as `details`.
    pub(crate) fn refusals(&self) -> Vec<serde_json::Value> {
        unmeasured_tail_refusals(self.channel.get())
    }
}

impl Drop for UnreadTailSeed {
    fn drop(&mut self) {
        let _ = crate::services::platform::tmux::kill_session(
            &self.tmux_session,
            "#5996 seed teardown",
        );
    }
}

/// The I20 unread-tail refusal `details` recorded for `channel`, oldest first.
pub(crate) fn unmeasured_tail_refusals(channel: u64) -> Vec<serde_json::Value> {
    crate::services::observability::events::recent(500)
        .into_iter()
        .filter(|event| event.event_type == "invariant_violation")
        .filter(|event| event.channel_id == Some(channel))
        .filter(|event| {
            event.payload["invariant"]
                == crate::services::observability::LIVE_TURN_PROVEN_BY_PROGRESS_INVARIANT
        })
        .map(|event| event.payload["details"].clone())
        .filter(|details| details.get("site").is_some())
        .collect()
}
