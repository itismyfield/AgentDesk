use crate::services::discord::SharedData;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::Method;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::test_support::{MockDiscord, age_file, fingerprint};
use crate::services::discord::inflight::{inflight_state_path, save_inflight_state};
use crate::services::discord::placeholder_sweeper::*;

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

#[cfg(unix)]
#[path = "rebind_resume_tests.rs"]
mod rebind_resume;

mod create_monotonic_host_tests {
    use crate::services::discord::inflight::RelayOwnerKind;
    use crate::services::discord::inflight::is_synthetic_create_state;
    use crate::services::discord::inflight::save_store::create_monotonic_observer::{
        fixture::{SERIAL, state, write_bytes},
        test_seams,
    };
    use crate::services::discord::inflight::save_store::{
        save_inflight_state_create_new_in_root, save_inflight_state_if_absent_in_root,
    };
    use crate::services::provider::ProviderKind;
    #[test]
    fn create_paths_observe_real_but_exclude_all_synthetic_shapes() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, b'a', 512);
        let real = state(54_900_003, &output, 256);
        test_seams::clear(ProviderKind::Codex, real.channel_id);
        save_inflight_state_create_new_in_root(temp.path(), &real).unwrap();
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, real.channel_id),
            Some(256)
        );

        let adopted = crate::services::discord::recovery_engine::manual_rebind::build_external_adopted_inflight_state(
        state(54_900_011, &output, 128),
    );
        assert!(is_synthetic_create_state(&adopted));
        test_seams::clear(ProviderKind::Codex, adopted.channel_id);
        save_inflight_state_create_new_in_root(temp.path(), &adopted).unwrap();
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, adopted.channel_id),
            None
        );

        let lease = crate::services::tui_prompt_dedupe::ExternalInputRelayLease::unassigned(Some(
            54_900_006,
        ));
        let tui = crate::services::discord::tui_prompt_relay::synthetic_start::build_tui_direct_synthetic_inflight_state(
        ProviderKind::Codex,
        crate::services::discord::ChannelId::new(54_900_006),
        crate::services::discord::MessageId::new(54_900_106),
        None,
        "observer prompt",
        "AgentDesk-codex-observer-5490",
        Some(&output),
        128,
        &lease,
        RelayOwnerKind::Watcher,
    );
        assert!(is_synthetic_create_state(&tui));
        test_seams::clear(ProviderKind::Codex, tui.channel_id);
        assert!(save_inflight_state_if_absent_in_root(temp.path(), &tui).unwrap());
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, tui.channel_id),
            None
        );

        #[cfg(unix)]
        {
            let monitor = crate::services::discord::tmux::build_monitor_triggered_inflight_state(
                state(54_900_004, &output, 128),
            );
            assert!(is_synthetic_create_state(&monitor));
            test_seams::clear(ProviderKind::Codex, monitor.channel_id);
            save_inflight_state_create_new_in_root(temp.path(), &monitor).unwrap();
            assert_eq!(
                test_seams::offset(ProviderKind::Codex, monitor.channel_id),
                None
            );

            let watcher = crate::services::discord::tmux::tmux_watcher::liveness::build_watcher_reacquire_inflight_state(
            state(54_900_005, &output, 128),
        );
            assert!(is_synthetic_create_state(&watcher));
            test_seams::clear(ProviderKind::Codex, watcher.channel_id);
            assert!(save_inflight_state_if_absent_in_root(temp.path(), &watcher).unwrap());
            assert_eq!(
                test_seams::offset(ProviderKind::Codex, watcher.channel_id),
                None
            );
        }

        let real_if_absent = state(54_900_007, &output, 192);
        assert!(!is_synthetic_create_state(&real));
        assert!(!is_synthetic_create_state(&real_if_absent));
        assert!(save_inflight_state_if_absent_in_root(temp.path(), &real_if_absent).unwrap());
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, real_if_absent.channel_id),
            Some(192)
        );
    }
}

mod stall_recovery_synthetic_carry_forward_reclaim_tests {
    use tempfile::TempDir;

    use crate::config::set_agentdesk_root_for_test;
    use crate::services::discord::inflight::{
        InflightTurnIdentity, RelayOwnerKind, load_inflight_states_for_test,
        refresh_inflight_last_offset_if_matches_identity_in_root, save_inflight_state_in_root,
        synthetic_carry_forward_fixture_tests::{build_synth_3358, monotonic_3358_test_mutex},
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn synthetic_carry_forward_keeps_reclaim_monotonic_3358() {
        let _serialized = monotonic_3358_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // FIX: birth carried up to the committed frontier → re-claim is forward/
        // equal, ZERO invariant violations, offsets end at the frontier.
        let temp = TempDir::new().unwrap();
        // The refresh path records inflight-invariant observability via the
        // PROCESS-WIDE runtime root (#3293 guard) — pin it to the tempdir so a
        // standalone/parallel run never resolves the live release root.
        let _env_reset = set_agentdesk_root_for_test(temp.path());
        let relay_last_offset: u64 = 2_821_677;
        let committed_frontier: u64 = 2_838_484;
        // Drive the ACTUAL production carry-forward helper (not an inline copy) so
        // this green test honestly tracks the production wiring — if the helper
        // regressed, `carried` would no longer reach the frontier and the
        // monotonicity assertions below would fail. The frontier is `Some(..)`
        // because the watcher advanced it WITHIN this same wrapper generation (the
        // claim choke-point validates that before clamping — #3358 round 2).
        let carried =
            crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
                relay_last_offset,
                Some(committed_frontier),
            );
        assert_eq!(
            carried, committed_frontier,
            "carry-forward must lift birth to the frontier"
        );

        // #4361: build the synthetic turn identity ONCE and derive BOTH the
        // persisted row and the re-claim from that single birth. `InflightTurnState::
        // new` stamps `started_at = now_string()` at 1-SECOND resolution, and
        // `started_at` is part of `InflightTurnIdentity` (the refresh's same-turn
        // gate). Two independent `build_synth_3358` calls that straddle a wall-clock
        // second — common under parallel/loaded CI scheduling — mint DIFFERENT
        // `started_at` values, so the identity-gated refresh rejects the re-claim as
        // a *different* identity and the "forward/equal — accepted" assertion below
        // panics. That is the #4361 parallel-only flake (green in isolation because
        // both `new()` calls land in the same second). One shared birth keeps the
        // re-claim honestly same-turn and clock-independent.
        let synth = build_synth_3358(carried);
        let mut on_disk = synth.clone();
        on_disk.full_response = "X".repeat(20_000);
        on_disk.response_sent_offset = 18_000;
        on_disk.last_offset = committed_frontier;
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        // Carried-birth re-claim: turn_start_offset == last_offset == frontier,
        // rso == 0. The rso 0 is NOT a regression because the identity key matches
        // (same turn) and `response_sent_offset_monotonic` only flags within-turn
        // backward moves AFTER bytes were sent — here the re-claim's last_offset
        // equals the on-disk frontier (forward/equal) and rso reset is the
        // documented fresh-claim seed. Assert last_offset never regresses below
        // the committed frontier.
        let carried_reseed = synth;
        // #4361 clock-skew guard: the persisted row and the re-claim MUST be the
        // SAME turn identity. If a future edit regresses to two independent
        // `now_string()` stamps, this catches the desync deterministically instead
        // of leaking it back to CI as a parallel flake.
        assert_eq!(
            on_disk.started_at, carried_reseed.started_at,
            "same-turn re-claim must share one started_at (#4361 clock-skew guard)"
        );
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Drive the enforcing watermark path: a carried-birth refresh writes
            // last_offset == committed_frontier — forward/equal, accepted.
            refresh_inflight_last_offset_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Claude,
                321,
                &InflightTurnIdentity::from_state(&carried_reseed),
                carried_reseed.turn_start_offset,
                "/tmp/out.jsonl",
                Some(carried_reseed.current_msg_id),
                committed_frontier,
                RelayOwnerKind::Watcher,
            )
        }));
        assert!(res.is_ok(), "carried-birth refresh must not panic");
        assert!(
            res.unwrap(),
            "carried-birth watermark write is forward/equal — accepted"
        );
        let loaded = load_inflight_states_for_test(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].last_offset, committed_frontier,
            "offsets end at the committed frontier, never regressed"
        );
    }
}

mod stall_recovery_synthetic_carry_forward_tests {
    #[test]
    fn synthetic_carry_forward_skipped_on_generation_mismatch_3358() {
        // #3358 round 2 — Finding 1 guard, at the production-helper level.
        //
        // After a tmux wrapper RESTART the output stream legitimately resets to
        // offset 0. The committed watermark from the PREVIOUS generation is stale;
        // the claim choke-point detects the generation mismatch and passes `None`
        // for the committed frontier. The helper MUST then fall back to the
        // synthetic's own (lagging) birth offset — NOT lift it over the stale
        // watermark, which would treat future bytes below that watermark as
        // already delivered (CONTENT SKIP, strictly worse than the original
        // ERROR-only bug). On HEAD (helper took a bare `u64` and always clamped)
        // there was no way to express "stale → do not clamp", so this guard
        // failed.
        let relay_last_offset: u64 = 100; // fresh post-restart birth (lagging).
        let stale_frontier: u64 = 2_838_484; // pre-restart, NUMERICALLY higher.

        // Generation mismatch → `None`: NO clamp, born at its own cursor.
        let birth =
            crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
                relay_last_offset,
                None,
            );
        assert_eq!(
            birth, relay_last_offset,
            "a stale (different-generation) watermark must NOT clamp the new synthetic forward"
        );

        // Same generation → `Some(..)`: clamp DOES carry the frontier forward.
        let same_gen_birth =
            crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
                relay_last_offset,
                Some(stale_frontier),
            );
        assert_eq!(
            same_gen_birth, stale_frontier,
            "a same-generation committed frontier must still carry forward (the #3358 fix)"
        );
    }
}

mod placeholder_sweeper_tick_tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use axum::http::Method;

    use crate::config::TestEnvVarGuard;
    use crate::services::discord::abandon_request_store::{
        self, AbandonEpisodeIdentity, AbandonRecord,
    };
    use crate::services::discord::health::test_support::{
        Answer, MockDiscord, age_file, message_json, seed_backfill_row, tree_fingerprint,
    };
    use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};
    use crate::services::discord::placeholder_sweeper::{
        StalledEditTracker, tick::run_placeholder_sweeper_tick,
    };
    use crate::services::discord::{SharedData, runtime_store};
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

    /// Seeds the row-pass, abort-marker and abandon fixtures on one channel group
    /// (`base + 1..=5`); returns the channels touched.
    fn seed_group(shared: &SharedData, base: u64) -> [u64; 4] {
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
        for entry in walk(
            &runtime_store::runtime_root()
                .unwrap()
                .join("discord_abandon_requests"),
        ) {
            age_file(&entry, 3_600);
        }
        [row_channel, marker_channel, abandon_channel, owner_channel]
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        tree_fingerprint(dir).into_keys().collect()
    }

    /// Two ticks over one Legacy channel group: every durable store converges, and each
    /// Discord effect lands once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick_converges_every_legacy_store() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let discord = MockDiscord::start_with(probe_answer()).await;
        let [row, marker, abandon_ch, owner] = seed_group(&shared, 6_325_400_200);
        let runtime = runtime_store::runtime_root().unwrap();

        let mut tracker = StalledEditTracker::default();
        for _ in 0..2 {
            run_placeholder_sweeper_tick(&discord.http, &shared, &PROVIDER, &mut tracker).await;
        }

        let calls = discord.calls_for(row);
        assert!(
            calls.iter().any(|c| c.starts_with("PATCH")),
            "stalled edit: {calls:?}"
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

    /// A record replaced while its probe awaits Discord is neither edited nor consumed: the
    /// abandon drain re-checks its queued record after the probe.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandon_drain_rejudges_its_record_after_the_probe_await() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = 6_325_400_302u64;
        let msg = channel * 10 + 1;
        let token = shared.token_hash.clone();
        // A newer episode takes the same message while the drain's probe is in flight.
        let discord = MockDiscord::start_with(Arc::new(move |method: &Method, path: &str| {
            if method == Method::GET && path.contains(&format!("/channels/{channel}/")) {
                abandon_request_store::enqueue(&PROVIDER, &token, channel, abandon(msg, 78))
                    .unwrap();
            }
            None
        }))
        .await;
        abandon_request_store::enqueue(&PROVIDER, &shared.token_hash, channel, abandon(msg, 77))
            .unwrap();
        let runtime = runtime_store::runtime_root().unwrap();
        for path in walk(&runtime.join("discord_abandon_requests")) {
            age_file(&path, 3_600);
        }

        let mut tracker = StalledEditTracker::default();
        run_placeholder_sweeper_tick(&discord.http, &shared, &PROVIDER, &mut tracker).await;

        assert_eq!(
            discord.calls_for(channel),
            vec![format!("GET /api/v10/channels/{channel}/messages/{msg}")]
        );
        let queued = abandon_request_store::load_pending(&PROVIDER, &shared.token_hash);
        assert_eq!(queued.len(), 1, "{queued:?}");
        assert_eq!(
            (
                queued[0].0,
                queued[0].1.msg_id,
                queued[0].1.episode.user_msg_id
            ),
            (channel, msg, 78),
            "the replacement survives untouched"
        );
    }
}
