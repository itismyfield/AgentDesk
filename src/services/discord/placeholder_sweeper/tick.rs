//! The sweeper task loop and the tick it runs: the row pass, then the durable drains.

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::{
    INITIAL_DELAY_SECS, SWEEP_HEARTBEAT_INTERVAL_SWEEPS, SWEEP_INTERVAL_SECS, SharedData,
    StalledEditTracker, SweepPassReport, run_placeholder_sweep_pass,
};
use crate::services::provider::ProviderKind;

fn should_log_sweep_report(report: SweepPassReport, sweeps_since_heartbeat: u64) -> bool {
    report.stalled > 0
        || report.abandoned > 0
        || report.reclaimed_panels > 0
        || sweeps_since_heartbeat >= SWEEP_HEARTBEAT_INTERVAL_SWEEPS
}

/// Spawn the long-lived background task that runs the stall sweeper at the
/// configured interval until the runtime exits. Should be called once per
/// provider during dcserver bootstrap.
pub(in crate::services::discord) fn spawn_placeholder_sweeper(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
) {
    tokio::spawn(async move {
        let mut stalled_tracker = StalledEditTracker::default();
        let mut sweeps_since_heartbeat = 0u64;
        tokio::time::sleep(tokio::time::Duration::from_secs(INITIAL_DELAY_SECS)).await;
        loop {
            let tick =
                run_placeholder_sweeper_tick(&http, &shared, &provider, &mut stalled_tracker).await;
            sweeps_since_heartbeat = sweeps_since_heartbeat.saturating_add(1);
            if should_log_sweep_report(tick.pass, sweeps_since_heartbeat) || tick.drained_any() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧹 placeholder sweeper ({}): scanned={} stalled={} abandoned={} reclaimed_panels={} drained_orphans={} drained_abort_markers={} drained_abandon_requests={} swept_busy_retry_bindings={} swept_orphan_anchors={}",
                    provider.as_str(),
                    tick.pass.scanned,
                    tick.pass.stalled,
                    tick.pass.abandoned,
                    tick.pass.reclaimed_panels,
                    tick.drained_orphans,
                    tick.drained_abort_markers,
                    tick.drained_abandon_requests,
                    tick.swept_busy_retry_bindings,
                    tick.swept_orphan_anchors
                );
                sweeps_since_heartbeat = 0;
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
        }
    });
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TickReport {
    pass: SweepPassReport,
    drained_orphans: usize,
    drained_abort_markers: usize,
    swept_orphan_anchors: usize,
    drained_abandon_requests: usize,
    swept_busy_retry_bindings: usize,
}

impl TickReport {
    fn drained_any(&self) -> bool {
        self.drained_orphans > 0
            || self.drained_abort_markers > 0
            || self.drained_abandon_requests > 0
            || self.swept_busy_retry_bindings > 0
            || self.swept_orphan_anchors > 0
    }
}

/// One sweeper tick: the row pass, then the durable-record drains in fixed order.
async fn run_placeholder_sweeper_tick(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    stalled_tracker: &mut StalledEditTracker,
) -> TickReport {
    let pass = run_placeholder_sweep_pass(http, shared, provider, stalled_tracker).await;
    // #3003: retry any durably-queued orphan status-panel deletes whose
    // inline reclaim failed transiently (and whose inflight row is gone, so
    // there is no per-turn handle left). Independent of inflight lifecycle.
    let drained_orphans =
        super::super::status_panel_orphan_store::drain(http, shared, provider, &shared.token_hash)
            .await;
    // #3296: reconcile durable aborted-anchor markers — retry the ✅ for
    // markers a terminal commit already covered, and apply the TTL'd
    // `⏳ → ⚠` fallback for anchors nothing ever covered (held while a
    // live inflight for the session may still cover them). The sweeper
    // owns this reclaim so an aborted anchor always converges (#3282).
    let drained_abort_markers =
        super::super::tui_direct_abort_marker::sweep_expired(shared, provider).await;
    // #4278 orphan-`⏳` sweep (mechanism: turn_view_reconciler::orphan_sweep).
    let swept_orphan_anchors =
        super::super::turn_view_reconciler::sweep_orphan_tui_anchor_reactions(shared, provider)
            .await;
    // #3859: finalize placeholders stranded by a failure-path inflight
    // eviction (turn-task Drop / heartbeat-gap sweeper). Each durable
    // abandon-request is edited to its terminal "중단됨" card BY MESSAGE
    // ID — decoupled from the inflight lifecycle, so a re-adopt (new row
    // + new placeholder) never collides with it.
    let drained_abandon_requests =
        super::super::abandon_request_store::drain(http, shared, provider, &shared.token_hash)
            .await;
    // #4888: cleanup guards and process crashes can leave a retry binding
    // without a surviving turn to clear it. Bound those durable sidecars
    // independently of the normal terminal clear path.
    let swept_busy_retry_bindings = super::super::busy_followup_retry_store::sweep_expired();
    TickReport {
        pass,
        drained_orphans,
        drained_abort_markers,
        swept_orphan_anchors,
        drained_abandon_requests,
        swept_busy_retry_bindings,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use axum::http::Method;

    use super::{StalledEditTracker, run_placeholder_sweeper_tick};
    use crate::config::TestEnvVarGuard;
    use crate::services::discord::abandon_request_store::{
        self, AbandonEpisodeIdentity, AbandonRecord,
    };
    use crate::services::discord::health::legacy_supervision::RetiredForTest;
    use crate::services::discord::health::legacy_supervision::test_support::{
        Answer, MockDiscord, RetireLater, age_file, message_json, seed_backfill_row,
        tree_fingerprint,
    };
    use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};
    use crate::services::discord::{SharedData, runtime_store, status_panel_orphan_store};
    use crate::services::provider::ProviderKind;

    const PROVIDER: ProviderKind = ProviderKind::Codex;
    const GONE: u64 = 3;
    const DELIVERED: u64 = 2;

    /// Answers abandon probes by message id: `..3` gone, `..2` delivered, else placeholder.
    fn probe_answer() -> Answer {
        Arc::new(|method: &Method, path: &str| {
            if method != Method::GET || !path.contains("/messages/") {
                return None;
            }
            let id: u64 = path.rsplit('/').next()?.parse().ok()?;
            match id % 10 {
                GONE => Some((
                    404,
                    serde_json::json!({"message": "Unknown Message", "code": 10008}),
                )),
                DELIVERED => Some((
                    200,
                    message_json(id, 1, 1, "real answer", "2026-10-02T00:00:00+00:00"),
                )),
                _ => None,
            }
        })
    }

    fn placeholder_row(channel: u64, msg: u64) -> InflightTurnState {
        InflightTurnState::new(
            PROVIDER,
            channel,
            None,
            1,
            msg - 1,
            msg,
            "prompt".into(),
            None,
            None,
            None,
            None,
            0,
        )
    }

    /// Files of every durable store that name `channel`, with bytes and mtime.
    fn channel_records(
        root: &Path,
        channel: u64,
    ) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        let needle = channel.to_string();
        tree_fingerprint(root)
            .into_iter()
            .filter(|(path, _)| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(&needle)
            })
            .collect()
    }

    fn write_corrupt_abort_marker(channel: u64) -> PathBuf {
        let root = runtime_store::tui_direct_abort_marker_root().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("codex_{channel}_0.json"));
        let marker = serde_json::json!({
            "provider": "codex", "channel_id": channel, "anchor_message_id": 0,
            "tmux_session_name": "AgentDesk-codex-n4a", "aborted_at_ms": 1,
        });
        std::fs::write(&path, marker.to_string()).unwrap();
        age_file(&path, 3_600);
        path
    }

    fn abandon(msg_id: u64, episode_user_msg: u64) -> AbandonRecord {
        AbandonRecord {
            msg_id,
            started_at: "2026-10-01 00:00:00".into(),
            current_tool_line: None,
            terminal_status: Default::default(),
            episode: AbandonEpisodeIdentity {
                user_msg_id: episode_user_msg,
                started_at: "2026-10-01 00:00:00".into(),
                ..Default::default()
            },
        }
    }

    /// Seeds the row-pass, orphan-panel, abort-marker and abandon fixtures on one channel
    /// group (`base + 1..=6`); returns the channels touched.
    fn seed_group(shared: &SharedData, base: u64) -> [u64; 5] {
        let token = shared.token_hash.as_str();
        // Row pass: a stalled placeholder row.
        let row_channel = base + 1;
        let row = placeholder_row(row_channel, row_channel * 10 + 1);
        save_inflight_state(&row).unwrap();
        let root = runtime_store::discord_inflight_root().unwrap();
        age_file(
            &crate::services::discord::inflight::inflight_state_path(&root, &PROVIDER, row_channel),
            600,
        );
        // Orphan panels: a pending bind and a stranded delete.
        let panel_channel = base + 2;
        status_panel_orphan_store::enqueue_pending_bind(
            &PROVIDER,
            token,
            panel_channel,
            panel_channel * 10 + 1,
            None,
        );
        status_panel_orphan_store::enqueue(&PROVIDER, token, panel_channel, panel_channel * 10 + 4);
        // Abort marker store: a corrupt marker.
        let marker_channel = base + 3;
        write_corrupt_abort_marker(marker_channel);
        // Rowless abandon requests: placeholder, already delivered, gone.
        let abandon_channel = base + 4;
        for suffix in [1, DELIVERED, GONE] {
            abandon_request_store::enqueue(
                &PROVIDER,
                token,
                abandon_channel,
                abandon(abandon_channel * 10 + suffix, 77),
            )
            .unwrap();
        }
        // Ownership change: a newer turn's row anchors the record's message.
        let owner_channel = base + 5;
        let msg = owner_channel * 10 + 1;
        let mut newer = placeholder_row(owner_channel, msg);
        newer.full_response = "streamed".into();
        seed_backfill_row(&newer);
        abandon_request_store::enqueue(&PROVIDER, token, owner_channel, abandon(msg, 77)).unwrap();
        for dir in ["discord_status_panel_orphans", "discord_abandon_requests"] {
            for entry in walk(&runtime_store::runtime_root().unwrap().join(dir)) {
                age_file(&entry, 3_600);
            }
        }
        [
            row_channel,
            panel_channel,
            marker_channel,
            abandon_channel,
            owner_channel,
        ]
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        tree_fingerprint(dir).into_keys().collect()
    }

    /// Two ticks over a retired and a Legacy channel group: the retired group's records keep
    /// bytes, existence and mtime with zero Discord calls, while the Legacy group converges.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retired_channels_skip_every_tick_effect_while_legacy_channels_converge() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let discord = MockDiscord::start_with(probe_answer()).await;
        let retired = seed_group(&shared, 6_325_400_100);
        let legacy = seed_group(&shared, 6_325_400_200);
        let _marks: Vec<_> = retired
            .iter()
            .map(|c| RetiredForTest::new("codex", *c))
            .collect();
        let runtime = runtime_store::runtime_root().unwrap();
        let before: Vec<_> = retired
            .iter()
            .map(|c| channel_records(&runtime, *c))
            .collect();
        assert!(before.iter().all(|records| !records.is_empty()));

        let mut tracker = StalledEditTracker::default();
        for _ in 0..2 {
            run_placeholder_sweeper_tick(&discord.http, &shared, &PROVIDER, &mut tracker).await;
        }

        for (channel, before) in retired.iter().zip(&before) {
            assert_eq!(
                discord.calls_for(*channel),
                Vec::<String>::new(),
                "retired {channel}"
            );
            let after = channel_records(&runtime, *channel);
            assert!(
                &after == before,
                "retired {channel} records: {:?}",
                after.keys()
            );
        }
        let [row, panel, marker, abandon_ch, owner] = legacy;
        let calls = discord.calls_for(row);
        assert!(
            calls.iter().any(|c| c.starts_with("PATCH")),
            "stalled edit: {calls:?}"
        );
        assert_eq!(
            discord.calls_for(panel),
            vec![format!(
                "DELETE /api/v10/channels/{panel}/messages/{}",
                panel * 10 + 4
            )]
        );
        let pending_bind = std::fs::read_to_string(
            runtime
                .join("discord_status_panel_orphans/codex")
                .join(&shared.token_hash)
                .join(format!("{panel}.json")),
        )
        .unwrap();
        assert!(
            pending_bind.contains("\"pending_bind_drain_cycles\": 2"),
            "{pending_bind}"
        );
        assert!(
            channel_records(&runtime, marker).is_empty(),
            "corrupt marker deleted"
        );
        assert!(
            channel_records(&runtime, abandon_ch).is_empty(),
            "abandon records consumed"
        );
        assert!(
            discord
                .calls_for(abandon_ch)
                .iter()
                .any(|c| c.starts_with("PATCH"))
        );
        assert!(
            discord.calls_for(owner).is_empty(),
            "ownership change drops without Discord"
        );
        let owner_records = channel_records(&runtime, owner);
        assert!(
            owner_records
                .keys()
                .all(|path| !path.starts_with(runtime.join("discord_abandon_requests"))),
            "the record dropped for its newer owner: {:?}",
            owner_records.keys()
        );
    }

    /// A channel retired while a probe awaits Discord is neither edited nor consumed: the
    /// row pass and the abandon drain both re-judge after their probe.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick_rejudges_after_each_probe_await() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let (row_channel, abandon_channel) = (6_325_400_301u64, 6_325_400_302u64);
        let retire = RetireLater::default();
        let on_probe = (retire.clone(), RetireLater::default());
        let discord = MockDiscord::start_with(Arc::new(move |method: &Method, path: &str| {
            if method == Method::GET && path.contains(&format!("/channels/{row_channel}/")) {
                on_probe.0.retire("codex", row_channel);
            }
            if method == Method::GET && path.contains(&format!("/channels/{abandon_channel}/")) {
                on_probe.1.retire("codex", abandon_channel);
            }
            None
        }))
        .await;
        save_inflight_state(&placeholder_row(row_channel, row_channel * 10 + 1)).unwrap();
        let inflight = runtime_store::discord_inflight_root().unwrap();
        age_file(
            &crate::services::discord::inflight::inflight_state_path(
                &inflight,
                &PROVIDER,
                row_channel,
            ),
            600,
        );
        for suffix in [1, 5] {
            abandon_request_store::enqueue(
                &PROVIDER,
                &shared.token_hash,
                abandon_channel,
                abandon(abandon_channel * 10 + suffix, 77),
            )
            .unwrap();
        }
        let runtime = runtime_store::runtime_root().unwrap();
        for path in walk(&runtime.join("discord_abandon_requests")) {
            age_file(&path, 3_600);
        }
        let before = [
            channel_records(&runtime, row_channel),
            channel_records(&runtime, abandon_channel),
        ];

        let mut tracker = StalledEditTracker::default();
        run_placeholder_sweeper_tick(&discord.http, &shared, &PROVIDER, &mut tracker).await;

        let probe = |c: u64| format!("GET /api/v10/channels/{c}/messages/{}", c * 10 + 1);
        for (channel, before) in [row_channel, abandon_channel].into_iter().zip(&before) {
            assert_eq!(discord.calls_for(channel), vec![probe(channel)]);
            assert!(
                &channel_records(&runtime, channel) == before,
                "{channel} records kept"
            );
        }
    }
}
