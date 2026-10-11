use super::super::o_seed_install_tests::{append, assert_rowless, observe, seed, until};
use super::*;
use crate::services::discord::health::legacy_supervision::{
    RetiredForTest, test_support::tree_fingerprint,
};
use crate::services::discord::task_supervisor::n4d_spawn_observation::Scope;
use crate::services::discord::task_supervisor::watcher_completion::Outcome;
use crate::services::discord::tmux::{OOnlyInstallOutcome, watch_host_of};

fn child(test: &str, env: &[(&str, &str)]) -> bool {
    isolated_in(
        "retired_host_tick_tests::inherited_fixture_tests",
        test,
        env,
    )
}

fn require_joined(scope: &Scope, joined: &[(u64, Outcome)], observed: &Guard) {
    assert_eq!(
        scope.submissions(),
        1,
        "raw installation submissions: {joined:?}"
    );
    assert_eq!(joined.len(), 1);
    assert!(
        joined
            .iter()
            .all(|(_, result)| *result == Outcome::Returned)
    );
    assert_rowless(observed);
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    assert_eq!(
        (
            key.writable_inflight_load_calls,
            key.readonly_inflight_load_calls,
            key.compatibility_backfill_attempts,
            key.legacy_prefix_restore_calls,
            key.decoded_source_chunks,
            key.watcher_initialization_observed,
            key.stream_decoder_initialization_observed,
        ),
        (
            raw.writable_inflight_load_calls,
            raw.readonly_inflight_load_calls,
            raw.compatibility_backfill_attempts,
            raw.legacy_prefix_restore_calls,
            raw.decoded_source_chunks,
            raw.watcher_initialization_observed,
            raw.stream_decoder_initialization_observed,
        ),
        "all raw entry counters remain visible: key={key:?} raw={raw:?}"
    );
    assert_eq!(key.events, raw.events);
    assert_eq!(key.parser_responses, raw.parser_responses);
    assert_eq!(key.retired_terminal_ranges, raw.retired_terminal_ranges);
    assert!(!key.decoded_chunks.is_empty());
    // Source payloads are keyed; their process-wide entry count remains raw.
    assert!(raw.decoded_chunks.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_host_snapshot_unknown_installed_normally_keeps_both_eofs() {
    if !child(
        "inherited_host_snapshot_unknown_installed_normally_keeps_both_eofs",
        &[],
    ) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (fixture, discord) = fixture_on_pg(&pool, 632_511_620).await;
    native_host_proof(&fixture);
    seed(&fixture, 0);
    seed_host_record(&pool, &fixture, Some(hosted_record(&fixture, true))).await;
    assert_eq!(
        watch_host_of(
            &fixture.shared,
            &CLAUDE,
            fixture.channel.get(),
            &fixture.session
        )
        .await,
        WatchHost::Unverified
    );
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let fingerprint = tree_fingerprint(&root);
    let observed = observe(&fixture);
    let submitted = Scope::new(&fixture.shared);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(
        &observed,
        "unknown snapshot captured by actual installer",
        |s| s.event_count("outer_eof") >= 2 && s.event_count("host_probe") >= 2,
    )
    .await;
    assert_eq!(observed.snapshot().event_count("rowless_host_probe"), 0);
    seed_host_record(&pool, &fixture, None).await;
    assert_eq!(
        watch_host_of(
            &fixture.shared,
            &CLAUDE,
            fixture.channel.get(),
            &fixture.session
        )
        .await,
        WatchHost::Legacy
    );
    require_kept_outer_and_inner(&fixture, &observed).await;
    let joined = submitted.cancel_and_join_all().await;
    require_joined(&submitted, &joined, &observed);
    assert_eq!(tree_fingerprint(&root), fingerprint);
    assert!(discord.requests_for(fixture.channel.get()).is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_death_legacy_preserves_actual_pg_cancel_tombstone() {
    if !child(
        "inherited_death_legacy_preserves_actual_pg_cancel_tombstone",
        &[],
    ) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (fixture, discord) = fixture_on_pg(&pool, 632_511_621).await;
    native_host_proof(&fixture);
    seed(&fixture, 0);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let fingerprint = tree_fingerprint(&root);
    let observed = observe(&fixture);
    let submitted = Scope::new(&fixture.shared);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(&observed, "initial live EOF before death", |s| {
        s.event_count("rowless_host_probe") > 0
    })
    .await;
    let cancel = fixture.handle().cancel.clone();
    let body = said("death fixture nonterminal body");
    append(&fixture, body.as_bytes());
    until(
        &observed,
        "actual inner collector before natural death",
        |s| s.decoded_chunks.concat() == body && s.event_count("inner_eof") > 0,
    )
    .await;
    let client = uuid::Uuid::new_v4();
    crate::db::cancel_tombstones::insert_cancel_tombstone(
        &pool,
        client,
        fixture.channel.get() as i64,
        Some(&fixture.session),
        Some(body.len() as i64),
        "fixture cancellation proof",
    )
    .await
    .unwrap();
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(t) FROM cancel_tombstones t WHERE client_id = $1")
            .bind(client)
            .fetch_one(&pool)
            .await
            .unwrap();
    fixture_pane("dead");
    std::fs::write(
        crate::services::tmux_common::session_dead_marker_path(&fixture.session),
        "dead",
    )
    .unwrap();
    let joined = submitted.join_all().await;
    require_joined(&submitted, &joined, &observed);
    assert!(
        !cancel.load(Ordering::Acquire),
        "natural death, not test cancellation"
    );
    let after: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT to_jsonb(t) FROM cancel_tombstones t WHERE client_id = $1")
            .bind(client)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(
        after,
        Some(before),
        "O death cannot consume a Legacy cancellation tombstone"
    );
    assert_eq!(tree_fingerprint(&root), fingerprint);
    assert!(discord.requests_for(fixture.channel.get()).is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_tick_existing_panel_preserves_actual_lifecycle_snapshot() {
    let name = "inherited_tick_existing_panel_preserves_actual_lifecycle_snapshot";
    if !child(
        name,
        &[
            ("AGENTDESK_STATUS_INTERVAL_SECS", "3"),
            ("AGENTDESK_SINGLE_MESSAGE_PANEL", "1"),
        ],
    ) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (mut fixture, discord) = fixture_on_pg(&pool, 632_511_622).await;
    fixture.registry.provider_entries_guard().await.clear();
    Arc::get_mut(&mut fixture.shared)
        .unwrap()
        .ui
        .status_panel_v2_enabled = true;
    fixture
        .registry
        .register("claude".into(), fixture.shared.clone())
        .await;
    seed(&fixture, 0);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let fingerprint = tree_fingerprint(&root);
    let observed = observe(&fixture);
    let submitted = Scope::new(&fixture.shared);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    until(&observed, "actual initial EOF before panel fixture", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let body = said("timed panel fixture nonterminal body");
    append(&fixture, body.as_bytes());
    until(&observed, "fresh frame cleared before panel fixture", |s| {
        s.decoded_chunks.concat() == body && s.event_count("inner_eof") > 0
    })
    .await;
    assert_eq!(observed.snapshot().event_count("streaming_tick"), 0);
    let details = serde_json::json!({
        "provider_session_id":"retired-panel-provider-id", "tmux_reused":true,
    });
    let events = &fixture.shared.ui.placeholder_live_events;
    assert!(events.set_session_panel_lifecycle_event(
        fixture.channel,
        Some("retired-panel-fixture"),
        "session_resumed",
        &details,
    ));
    // An identical snapshot returns false before mutation and does not consume its banner.
    assert!(!events.set_session_panel_lifecycle_event(
        fixture.channel,
        Some("retired-panel-fixture"),
        "session_resumed",
        &details,
    ));
    until(
        &observed,
        "actual expired tick with existing session panel",
        |s| s.decoded_chunks.concat() == body && s.event_count("streaming_tick") > 0,
    )
    .await;
    let joined = submitted.cancel_and_join_all().await;
    require_joined(&submitted, &joined, &observed);
    assert!(
        !events.set_session_panel_lifecycle_event(
            fixture.channel,
            Some("retired-panel-fixture"),
            "session_resumed",
            &details,
        ),
        "rowless streaming cannot clear the live session lifecycle panel"
    );
    assert_eq!(tree_fingerprint(&root), fingerprint);
    assert!(discord.requests_for(fixture.channel.get()).is_empty());
    pool.close().await;
    db.drop().await;
}
