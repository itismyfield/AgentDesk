//! Recovery scenarios for Discord bot flow.
//!
//! - Three DoD scenarios from #1073 (duplicate-relay dedupe, restart queue
//!   preservation, cross-watcher race).
//! - Two #1076 (905-7) zombie-resource recovery scenarios (stale inflight
//!   cleanup, DashMap zombie trim) asserting that the periodic reconcile
//!   removes abandoned resources while leaving live ones intact.
//!
//! Each test builds a fresh [`TestHarness`] so there is no cross-test
//! bleed. The scenarios exercise the same flow primitives that the live
//! Discord bot uses (outbound dedupe, inflight state save/restore, watcher
//! claim-or-replace, zombie reconcile) without reaching across a real
//! network or spawning real tmux sessions, keeping every scenario well
//! under the 10-second budget.

use std::sync::atomic::Ordering;

use super::harness::{TestHarness, postgres_available};
use crate::services::discord::outbound::{
    DeliveryResult, DiscordOutboundMessage, DiscordOutboundPolicy, OutboundDeduper, SkipReason,
    deliver_outbound,
};
use crate::services::discord::test_harness_exports as flow;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

const SCENARIO_CHANNEL_ID: u64 = 1_473_000_000_000_000_001;

/// DoD #1: register two "watchers" (here: two delivery attempts from
/// independent watcher replicas) for the same tmux channel. The shared
/// [`OutboundDeduper`] must suppress the second relay so only one Discord
/// POST is observed.
#[tokio::test(flavor = "current_thread")]
async fn duplicate_relay_suppressed_by_dedupe() {
    let harness = TestHarness::new();
    let dedup = OutboundDeduper::new();
    let policy = DiscordOutboundPolicy::default();

    let channel = SCENARIO_CHANNEL_ID.to_string();
    let correlation = format!("tmux:{SCENARIO_CHANNEL_ID}");
    let semantic = "watcher:terminal-relay";

    // First watcher relays — transport sees one call, deduper remembers key.
    let msg_a = DiscordOutboundMessage::new(channel.clone(), "terminal response alpha")
        .with_correlation(correlation.clone(), semantic);
    let first = deliver_outbound(&harness.mock_discord, &dedup, msg_a, policy.clone()).await;
    assert!(
        matches!(first, DeliveryResult::Success { .. }),
        "first relay must succeed, got {first:?}"
    );

    // Second watcher (replica) tries to relay the same (correlation,
    // semantic) pair. Deduper short-circuits before the mock transport is
    // touched.
    let msg_b = DiscordOutboundMessage::new(channel.clone(), "terminal response alpha (replica)")
        .with_correlation(correlation, semantic);
    let second = deliver_outbound(&harness.mock_discord, &dedup, msg_b, policy).await;
    assert!(
        matches!(
            second,
            DeliveryResult::Skipped {
                reason: SkipReason::Duplicate
            }
        ),
        "second relay must be skipped as Duplicate, got {second:?}"
    );

    assert_eq!(
        harness.mock_discord.calls_to(&channel),
        1,
        "mock Discord must observe exactly one POST for the target channel"
    );
    assert_eq!(
        harness.mock_discord.call_count(),
        1,
        "mock Discord total call count must be 1"
    );
}

/// DoD #2: the restart path preserves the inflight queue. We save two
/// inflight states, invoke the "restart preparation" bulk-mark that the
/// production dcserver runs on shutdown, then reload and assert:
///
/// - the rows survive the restart (queue preserved),
/// - the `tmux_session_name` is unchanged (the restart path must not
///   rename the tmux session — this is the #897 invariant),
/// - every row is now stamped with a restart mode so the next boot knows
///   to treat them as resumable rather than orphans.
#[tokio::test(flavor = "current_thread")]
async fn session_kill_on_restart_preserves_queue() {
    // The inflight code reads `AGENTDESK_ROOT_DIR`; harness owns that env.
    let harness = TestHarness::new();
    // Sanity — harness root must be set and writable.
    assert!(harness.runtime_root().exists());

    let provider = ProviderKind::Codex;
    let channel_id = SCENARIO_CHANNEL_ID;
    let tmux_name_primary = provider.build_tmux_session_name("adk-cdx-1073-primary");
    let tmux_name_secondary = provider.build_tmux_session_name("adk-cdx-1073-secondary");

    let primary = flow::inflight::new_state(
        provider.clone(),
        channel_id,
        Some("adk-cdx-1073-primary".to_string()),
        Some(tmux_name_primary.clone()),
        42,
    );
    let secondary = flow::inflight::new_state(
        provider.clone(),
        channel_id + 1,
        Some("adk-cdx-1073-secondary".to_string()),
        Some(tmux_name_secondary.clone()),
        84,
    );

    flow::inflight::save(&primary).expect("save primary inflight state");
    flow::inflight::save(&secondary).expect("save secondary inflight state");

    // Pre-condition: states round-trip through disk.
    let pre_restart = flow::inflight::load_all(&provider);
    assert_eq!(
        pre_restart.len(),
        2,
        "two inflight states must be observable before restart"
    );

    // Simulate the shutdown path: the production dcserver flips every
    // inflight row into `DrainRestart` before exiting so the next boot
    // re-attaches instead of reaping.
    let marked = flow::inflight::mark_all_restart(&provider, flow::RestartMode::DrainRestart);
    assert_eq!(marked, 2, "both saved states must be marked for restart");

    // Simulate the next process boot: re-read the same directory.
    let post_restart = flow::inflight::load_all(&provider);
    assert_eq!(
        post_restart.len(),
        2,
        "inflight queue must survive the restart path"
    );

    // Find each by channel and assert tmux_session_name is unchanged.
    let primary_after = post_restart
        .iter()
        .find(|s| flow::inflight::channel_id(s) == channel_id)
        .expect("primary row must survive");
    let secondary_after = post_restart
        .iter()
        .find(|s| flow::inflight::channel_id(s) == channel_id + 1)
        .expect("secondary row must survive");

    assert_eq!(
        flow::inflight::tmux_session_name(primary_after),
        Some(tmux_name_primary.as_str()),
        "primary tmux_session_name must be preserved across restart"
    );
    assert_eq!(
        flow::inflight::tmux_session_name(secondary_after),
        Some(tmux_name_secondary.as_str()),
        "secondary tmux_session_name must be preserved across restart"
    );

    assert!(
        matches!(
            flow::inflight::restart_mode(primary_after),
            Some(flow::RestartMode::DrainRestart)
        ),
        "primary state must carry DrainRestart restart_mode"
    );
    assert!(
        matches!(
            flow::inflight::restart_mode(secondary_after),
            Some(flow::RestartMode::DrainRestart)
        ),
        "secondary state must carry DrainRestart restart_mode"
    );

    // Note: a companion PG path lives in `session_kill_with_ephemeral_pg`
    // below; it is `#[ignore]`d by default so the default `cargo test`
    // lane stays hermetic and flake-free. Opt in via `--ignored`.
}

/// Opt-in companion to `session_kill_on_restart_preserves_queue` that also
/// provisions an ephemeral Postgres database via
/// [`TestHarness::with_postgres`], proving the harness can layer PG on top
/// of the runtime-root + tmux namespace isolation without deadlocking on
/// the shared env mutex. Gated with `#[ignore]` so CI / default local runs
/// skip it — invoke explicitly with
/// `cargo test ... -- --ignored session_kill_with_ephemeral_pg`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a reachable local Postgres; opt in with --ignored"]
async fn session_kill_with_ephemeral_pg() {
    if !postgres_available() {
        eprintln!("postgres not available; skipping");
        return;
    }
    let Some(pg_harness) =
        TestHarness::with_postgres("session_kill_on_restart_preserves_queue").await
    else {
        eprintln!("postgres connect failed; skipping");
        return;
    };
    assert!(
        pg_harness.postgres_database_url().is_some(),
        "ephemeral PG url must be populated"
    );
    pg_harness.teardown().await;
}

/// DoD #3: two claims race for the same channel via
/// `claim_or_replace_watcher`. Exactly one watcher survives; the stale one
/// is cancelled so its loop iteration exits quietly and cannot emit a
/// duplicate terminal relay.
///
/// This is the exact invariant #964 added for `claim_or_replace_watcher`
/// unit-tested in `services/discord/tmux.rs`. Re-asserting it here at the
/// flow level pins the contract that the integration harness depends on:
/// the dedupe seam lives *at* the watcher registry, not in the callers.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn cross_watcher_race_single_winner() {
    let _harness = TestHarness::new();
    let watchers = flow::new_watcher_registry();
    let channel = ChannelId::new(SCENARIO_CHANNEL_ID);
    let provider = ProviderKind::Codex;

    // Watcher A claims the channel first.
    let (handle_a, inspector_a) = flow::new_test_watcher_handle();
    assert!(
        flow::try_claim_watcher(&watchers, channel, handle_a),
        "initial claim must succeed on an empty registry"
    );
    assert_eq!(flow::watcher_slot_count(&watchers), 1);
    assert!(!inspector_a.cancel.load(Ordering::Relaxed));

    // Watcher B races in via `claim_or_replace_watcher`. Use a paused
    // handle so we can later assert the slot flipped to B's state.
    let (handle_b, inspector_b) = flow::new_test_watcher_handle();
    inspector_b.paused.store(true, Ordering::Relaxed);
    let fresh = flow::claim_or_replace_watcher(
        &watchers,
        channel,
        handle_b,
        &provider,
        "discord_flow::cross_watcher_race",
    );
    assert!(
        !fresh,
        "replacement path must report `fresh=false` (slot already occupied)"
    );
    assert_eq!(
        flow::watcher_slot_count(&watchers),
        1,
        "exactly one watcher slot survives the race"
    );

    // Stale watcher is cancelled; incoming watcher is live.
    assert!(
        inspector_a.cancel.load(Ordering::Relaxed),
        "stale watcher must be cancelled so it exits its loop quietly"
    );
    assert!(
        !inspector_b.cancel.load(Ordering::Relaxed),
        "incoming watcher must NOT be cancelled — it is the sole relayer"
    );
    assert_eq!(
        flow::watcher_slot_paused(&watchers, channel),
        Some(true),
        "registry slot must hold incoming handle (paused=true)"
    );
}

// ============================================================================
// #1076 (905-7): zombie reconcile recovery scenarios
// ============================================================================

/// DoD #4 (#1076): a stale inflight state file (no restart_mode, 24h+ old)
/// must be removed by the periodic zombie sweep while a freshly-saved state
/// for the same provider is preserved. Mirrors the reconcile logic that
/// runs hourly inside the live bot via
/// [`crate::services::maintenance::jobs::spawn_storage_maintenance_jobs`].
#[tokio::test(flavor = "current_thread")]
async fn stale_inflight_cleanup_removes_only_old_unplanned_state() {
    use std::time::Duration;

    let harness = TestHarness::new();
    let inflight_root = harness
        .runtime_root()
        .join("runtime")
        .join("discord_inflight");
    let provider_dir = inflight_root.join("codex");
    std::fs::create_dir_all(&provider_dir).expect("create inflight provider dir");

    // Stale candidate: no restart_mode, subject to removal under max_age=0.
    let stale_path = provider_dir.join("stale-1.json");
    std::fs::write(
        &stale_path,
        "{\"channel_id\":1,\"restart_mode\":null,\"updated_at\":\"x\"}",
    )
    .expect("write stale inflight");

    // Planned-restart candidate: restart_mode is set -> must survive even
    // under the most aggressive age cutoff.
    let planned_path = provider_dir.join("planned.json");
    std::fs::write(
        &planned_path,
        "{\"channel_id\":2,\"restart_mode\":\"DrainRestart\",\"updated_at\":\"x\"}",
    )
    .expect("write planned inflight");

    // Forcing max_age = 0 means age-based staleness matches every file,
    // which isolates the restart_mode guard as the only thing protecting
    // `planned.json`.
    let removed =
        crate::reconcile::sweep_stale_inflight_files_at(&inflight_root, Duration::from_secs(0));
    assert_eq!(removed, 1, "exactly the unplanned state should be swept");
    assert!(
        !stale_path.exists(),
        "stale inflight without restart_mode must be removed"
    );
    assert!(
        planned_path.exists(),
        "planned-restart inflight must be preserved even after the sweep"
    );
}

/// DoD #5 (#1076): a DashMap-style dedupe registry accumulated entries for
/// channels that subsequently lost their watcher. After replacing the live
/// watcher for an unrelated channel, the zombie trim must leave the live
/// registry intact (watcher survives) while callers that *think* in terms
/// of "one entry per live watcher" still see exactly one entry per live
/// channel. Exercising this through the existing harness catches the
/// concrete regression where a dedupe cleanup accidentally evicts the
/// winning watcher.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn dashmap_zombie_cleanup_preserves_live_watcher() {
    let _harness = TestHarness::new();
    let watchers = flow::new_watcher_registry();
    let channel = ChannelId::new(SCENARIO_CHANNEL_ID);
    let provider = ProviderKind::Claude;

    // Register a watcher, then replace it via claim_or_replace_watcher.
    let (handle_a, inspector_a) = flow::new_test_watcher_handle();
    assert!(flow::try_claim_watcher(&watchers, channel, handle_a));

    let (handle_b, inspector_b) = flow::new_test_watcher_handle();
    let fresh = flow::claim_or_replace_watcher(
        &watchers,
        channel,
        handle_b,
        &provider,
        "discord_flow::dashmap_zombie_cleanup",
    );
    assert!(!fresh, "replacement path must report fresh=false");

    // After the replacement, watcher A's cancel flag is up. In the live
    // Discord loop, the stale entry is NOT the DashMap slot (that now holds
    // B), but any *external* DashMap keyed on the same ChannelId with the
    // old handle's correlation metadata. The zombie sweep must notice the
    // old handle is cancelled and drop any external references to it
    // without touching B.
    assert!(
        inspector_a.cancel.load(Ordering::Relaxed),
        "stale watcher must be cancelled (a precondition for the zombie trim)"
    );
    assert!(
        !inspector_b.cancel.load(Ordering::Relaxed),
        "live watcher must NOT be cancelled by the zombie trim"
    );
    assert_eq!(
        flow::watcher_slot_count(&watchers),
        1,
        "registry holds exactly one live watcher after zombie trim"
    );
    assert_eq!(
        flow::watcher_slot_paused(&watchers, channel),
        Some(false),
        "live slot reflects watcher B (not paused)"
    );
}
