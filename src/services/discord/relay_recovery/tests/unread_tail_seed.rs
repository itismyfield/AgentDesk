//! A live `AgentDesk-` pane, mailbox turn and aged row for the unread-tail entry tests;
//! the tail reading comes only from the real `SessionEnrichment::load`.
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
    /// A ready transcript that a watcher bound to another session leaves unattributed
    /// (UNMEASURED); its final answer is still unrelayed when `answer`.
    ForeignWatcher { answer: bool },
    /// `RowOutputMissing` as explicit background work whose live watcher this channel owns.
    OwnedBackground,
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
    pub(crate) async fn start(channel: u64, shape: UnreadTailShape) -> Option<Self> {
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
        let ready = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
        let transcript = match shape {
            UnreadTailShape::RowOutputMissing | UnreadTailShape::OwnedBackground => String::new(),
            UnreadTailShape::MeasuredBacklog => ready.to_string(),
            UnreadTailShape::ForeignWatcher { .. } => {
                format!(
                    "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ANSWER\"}}\n{ready}"
                )
            }
        };
        if !transcript.is_empty() {
            std::fs::write(&output, &transcript).expect("write transcript fixture");
        }
        let last_offset = match shape {
            UnreadTailShape::ForeignWatcher { answer: false } => transcript.len() as u64,
            _ => 0,
        };
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
            last_offset,
        );
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        if let UnreadTailShape::ForeignWatcher { .. } = shape {
            // A heartbeat-stale watcher on another pane and no row relay owner: nothing live relays.
            let foreign = format!("{tmux_session}-other");
            bind_watcher(&shared, channel, &foreign, &output, 0);
        } else if shape == UnreadTailShape::OwnedBackground {
            row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
            row.task_notification_kind =
                Some(crate::services::agent_protocol::TaskNotificationKind::Background);
            row.current_msg_len = 1; // Discord write evidence the watchdog ages
            let now = crate::services::discord::tmux_watcher_now_ms();
            bind_watcher(&shared, channel, &tmux_session, &output, now);
        } else {
            row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
        }
        inflight::save_inflight_state_create_new(&row).expect("persist row fixture");
        // The save stamps `updated_at`; age the persisted row past
        // the desync and watchdog windows directly.
        let stale_at = (chrono::Local::now() - chrono::Duration::minutes(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        edit_persisted_row(&provider, channel.get(), |row| {
            row["started_at"] = serde_json::Value::String(stale_at.clone());
            row["updated_at"] = serde_json::Value::String(stale_at);
        });

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

/// Binds a watcher handle for `channel` on `tmux_session` with the given heartbeat.
pub(crate) fn bind_watcher(
    shared: &SharedData,
    channel: ChannelId,
    tmux_session: &str,
    output: &std::path::Path,
    heartbeat_ms: i64,
) {
    shared.tmux_watchers.insert(
        channel,
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux_session.to_string(),
            output_path: output.to_string_lossy().to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(heartbeat_ms)),
        },
    );
}

/// Rewrites the persisted row's raw JSON in place and returns its path.
pub(crate) fn edit_persisted_row(
    provider: &ProviderKind,
    channel: u64,
    edit: impl FnOnce(&mut serde_json::Value),
) -> std::path::PathBuf {
    let root = inflight::inflight_runtime_root().expect("inflight runtime root");
    let row_path = inflight::inflight_state_path(&root, provider, channel);
    let raw = std::fs::read_to_string(&row_path).expect("read row fixture");
    let mut row: serde_json::Value = serde_json::from_str(&raw).expect("parse row fixture");
    edit(&mut row);
    let raw = serde_json::to_string_pretty(&row).expect("serialize row fixture");
    std::fs::write(&row_path, raw).expect("rewrite row fixture");
    row_path
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

/// The seed's current snapshot with the mailbox turn cleared, so the row alone names the episode.
async fn rowed_snapshot(
    seed: &UnreadTailSeed,
) -> crate::services::discord::health::WatcherStateSnapshot {
    let registry = &seed.registry;
    let channel = seed.channel.get();
    let mut snapshot = registry
        .snapshot_watcher_state_for_provider(&seed.provider, channel)
        .await
        .expect("fixture snapshot");
    (
        snapshot.mailbox_active_user_msg_id,
        snapshot.mailbox_active_turn_nonce,
    ) = (None, None);
    snapshot
}

/// Refusals nothing names are never folded together, and a measured backlog is never one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmeasured_tail_refusals_nothing_names_are_never_folded() {
    use super::UNREAD_TAIL_SITE_STALE_MAILBOX;
    let Some(seed) = UnreadTailSeed::start(5_996_140_001, UnreadTailShape::RowOutputMissing).await
    else {
        return;
    };
    let (provider, channel) = (&seed.provider, seed.channel.get());
    let count = || seed.refusals().len();

    let mut rowless = rowed_snapshot(&seed).await;
    rowless.inflight_birth = None;
    for session in ["AgentDesk-claude-5996-a", "AgentDesk-claude-5996-b"] {
        rowless.tmux_session = Some(session.to_string());
        let other = UNREAD_TAIL_SITE_STALE_MAILBOX;
        super::record_unmeasured_tail_refusal_for_snapshot(provider, channel, &rowless, other);
    }
    assert_eq!(count(), 2, "a row-less session change is a new refusal");

    let mut measured = rowed_snapshot(&seed).await;
    (measured.unread_bytes, measured.relay_health.unread_bytes) = (Some(64), Some(64));
    measured.mailbox_active_user_msg_id = Some(64); // an ungraded episode
    assert!(!super::stale_mailbox_idle_tail_admits(
        provider, &measured, true
    ));
    assert_eq!(count(), 2, "a measured backlog is no wedge");
}

/// A retained snapshot never keys the row that replaced its turn: two births, two records.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retained_snapshot_never_keys_a_later_birth() {
    let Some(seed) = UnreadTailSeed::start(5_996_140_002, UnreadTailShape::RowOutputMissing).await
    else {
        return;
    };
    let (provider, channel) = (&seed.provider, seed.channel.get());
    let record = |snapshot: &crate::services::discord::health::WatcherStateSnapshot| {
        let site = super::UNREAD_TAIL_SITE_STALE_MAILBOX;
        super::record_unmeasured_tail_refusal_for_snapshot(provider, channel, snapshot, site)
    };
    let retained = rowed_snapshot(&seed).await;
    edit_persisted_row(provider, channel, |row| row["turn_nonce"] = "B".into());
    record(&retained);
    record(&rowed_snapshot(&seed).await);
    assert_eq!(seed.refusals().len(), 2, "{:?}", seed.refusals());
}

/// The seed's current manual decision, with its mailbox turn cleared as in [`rowed_snapshot`].
async fn rowed_decision(seed: &UnreadTailSeed) -> super::RelayRecoveryDecision {
    let snapshot = rowed_snapshot(seed).await;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (health, stall) = (&snapshot.relay_health, snapshot.relay_stall_state);
    let mut decision = super::plan_relay_recovery(health, stall, now_ms);
    decision.affected.pin_snapshot_turn(&snapshot);
    decision.affected.mailbox_active_user_msg_id = None;
    decision
}

/// A retained manual decision never keys the row that replaced its turn: two births, two
/// records; the decision carrying the replacing birth keys it once.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retained_decision_never_keys_a_later_birth() {
    let shape = UnreadTailShape::ForeignWatcher { answer: false };
    let Some(seed) = UnreadTailSeed::start(5_996_140_004, shape).await else {
        return;
    };
    let provider = &seed.provider;
    let admits = |decision: &super::RelayRecoveryDecision| {
        super::reattach_idle_clear_tail_admits(provider, decision, &seed.tmux_session)
    };
    let retained = rowed_decision(&seed).await;
    assert_eq!(retained.evidence.unread_bytes, None, "{retained:?}");
    edit_persisted_row(provider, seed.channel.get(), |row| {
        row["turn_nonce"] = "B".into()
    });
    assert!(!admits(&retained));
    assert!(!admits(&rowed_decision(&seed).await));
    assert_eq!(seed.refusals().len(), 2, "{:?}", seed.refusals());
    assert!(!admits(&rowed_decision(&seed).await));
    assert_eq!(
        seed.refusals().len(),
        2,
        "one birth decided twice is recorded once"
    );
}

/// A snapshot keys a re-read row only by the birth it carried: repeated passes over one
/// birth record once, and an unidentifiable birth never keys the identifiable one after it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_keys_only_the_birth_it_carried() {
    let Some(seed) = UnreadTailSeed::start(5_996_140_003, UnreadTailShape::RowOutputMissing).await
    else {
        return;
    };
    let (provider, channel) = (&seed.provider, seed.channel.get());
    let record = |snapshot: &crate::services::discord::health::WatcherStateSnapshot| {
        let site = super::UNREAD_TAIL_SITE_WATCHDOG_EXPLICIT_BACKGROUND;
        super::record_unmeasured_tail_refusal_for_snapshot(provider, channel, snapshot, site)
    };
    record(&rowed_snapshot(&seed).await);
    record(&rowed_snapshot(&seed).await);
    assert_eq!(
        seed.refusals().len(),
        1,
        "one birth across passes is recorded once"
    );

    edit_persisted_row(provider, channel, |row| {
        (row["user_msg_id"], row["turn_nonce"]) = (0.into(), serde_json::Value::Null);
        row.as_object_mut().unwrap().remove("turn_start_offset");
    });
    let unidentifiable = rowed_snapshot(&seed).await;
    edit_persisted_row(provider, channel, |row| row["turn_nonce"] = "B".into());
    record(&unidentifiable);
    record(&rowed_snapshot(&seed).await);
    assert_eq!(seed.refusals().len(), 3, "{:?}", seed.refusals());

    edit_persisted_row(provider, channel, |row| {
        (row["turn_nonce"], row["turn_start_offset"]) = (serde_json::Value::Null, 7.into());
    });
    let at_seven = rowed_snapshot(&seed).await;
    edit_persisted_row(provider, channel, |row| row["turn_start_offset"] = 9.into());
    record(&at_seven);
    record(&rowed_snapshot(&seed).await);
    assert_eq!(
        seed.refusals().len(),
        5,
        "a nonce-less id-0 birth is told apart by offset"
    );
}

/// A row read with the birth its observation carried is graded once per site by that
/// birth, however its tmux name reads; an unnameable row is never folded into itself.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmeasured_tail_episodes_are_graded_by_the_carried_birth() {
    use super::{UNREAD_TAIL_SITE_MANUAL_REATTACH, UNREAD_TAIL_SITE_STALE_MAILBOX};
    let Some(seed) = UnreadTailSeed::start(5_996_140_005, UnreadTailShape::RowOutputMissing).await
    else {
        return;
    };
    let (provider, channel) = (&seed.provider, seed.channel.get());
    let site = UNREAD_TAIL_SITE_MANUAL_REATTACH;
    let count = || seed.refusals().len();
    for row_site in [site, site, UNREAD_TAIL_SITE_STALE_MAILBOX] {
        let snapshot = rowed_snapshot(&seed).await;
        super::record_unmeasured_tail_refusal_for_snapshot(provider, channel, &snapshot, row_site);
    }
    assert_eq!(count(), 2, "one birth episode is graded once per site");

    let record_row = || async {
        let snapshot = rowed_snapshot(&seed).await;
        super::record_unmeasured_tail_refusal_for_snapshot(provider, channel, &snapshot, site);
    };
    edit_persisted_row(provider, channel, |row| {
        row["tmux_session_name"] = "renamed".into()
    });
    record_row().await;
    assert_eq!(count(), 2, "a learned tmux name keeps the birth episode");
    let nonce = inflight::load_inflight_state_read_only(provider, channel)
        .unwrap()
        .turn_nonce;
    for turn_nonce in ["reborn".into(), serde_json::json!(nonce)] {
        edit_persisted_row(provider, channel, |row| row["turn_nonce"] = turn_nonce);
        record_row().await;
    }
    assert_eq!(
        count(),
        3,
        "a new birth is its own episode; the first is still graded"
    );

    edit_persisted_row(provider, channel, |row| {
        (row["user_msg_id"], row["turn_nonce"]) = (0.into(), serde_json::Value::Null);
        row.as_object_mut().unwrap().remove("turn_start_offset");
    });
    record_row().await;
    record_row().await;
    assert_eq!(count(), 5, "an unnameable row is never folded into itself");
}

/// Mailbox turns A, B, A on one site: every call refuses, but A's grading outlives B's.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_returning_mailbox_turn_keeps_its_prior_grading() {
    let Some(seed) = UnreadTailSeed::start(5_996_140_006, UnreadTailShape::RowOutputMissing).await
    else {
        return;
    };
    let mut snapshot = rowed_snapshot(&seed).await;
    for (turn, expected) in [("A", 1), ("B", 2), ("A", 2)] {
        snapshot.mailbox_active_turn_nonce = Some(turn.to_string());
        assert!(!super::stale_mailbox_idle_tail_admits(
            &seed.provider,
            &snapshot,
            true
        ));
        assert_eq!(seed.refusals().len(), expected, "after turn {turn}");
    }
}
