//! Stale-turn reconcile tests that read inflight episodes through the ROOT lookup and warrant fixture.
use crate::domain::lifecycle::STALE_TURN_GRACE;
use crate::services::discord::relay_recovery::AxisBSite;
#[cfg(unix)]
use crate::services::engine_runtime::health_port::StaleSweepPort;
use crate::services::provider::ProviderKind;
use crate::services::stale_turn_reconciler::*;
use anyhow::Result;
use sqlx::{PgPool, Row};

/// The outer wrapper hands the production inflight probe to the shared path, so
/// the existing suites exercise the real `channel_inflight_observation` on both
/// sides of the barrier rather than an injected stand-in.
async fn reconcile_stale_turns_matching_pg<F>(
    pool: &PgPool,
    session_key: Option<&str>,
    probe: F,
) -> Result<usize>
where
    F: Fn(&str, &str) -> IndependentLiveness + Clone + Send + 'static,
{
    Ok(reconcile_stale_turns_matching_with_warrant_pg(
        pool,
        session_key,
        probe,
        channel_inflight_probe(crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name),
        None,
    )
    .await?
    .reconciled)
}

async fn allow_legacy_working_status(pool: &PgPool) {
    sqlx::query("ALTER TABLE sessions DROP CONSTRAINT sessions_status_known_check")
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_session(
    pool: &PgPool,
    session_key: &str,
    status: &str,
    active_dispatch_id: Option<&str>,
    heartbeat_age_seconds: i64,
) {
    seed_session_for_provider(
        pool,
        session_key,
        "claude",
        status,
        active_dispatch_id,
        heartbeat_age_seconds,
    )
    .await;
}

async fn seed_session_for_provider(
    pool: &PgPool,
    session_key: &str,
    provider: &str,
    status: &str,
    active_dispatch_id: Option<&str>,
    heartbeat_age_seconds: i64,
) {
    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, active_dispatch_id, last_heartbeat, session_info
         ) VALUES (
            $1, $2, $3, $4,
            NOW() - ($5::BIGINT * INTERVAL '1 second'), 'original'
         )",
    )
    .bind(session_key)
    .bind(provider)
    .bind(status)
    .bind(active_dispatch_id)
    .bind(heartbeat_age_seconds)
    .execute(pool)
    .await
    .unwrap();
}

async fn load_state(pool: &PgPool, session_key: &str) -> (String, Option<String>) {
    let row = sqlx::query("SELECT status, session_info FROM sessions WHERE session_key = $1")
        .bind(session_key)
        .fetch_one(pool)
        .await
        .unwrap();
    (
        row.try_get("status").unwrap(),
        row.try_get("session_info").unwrap(),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn transport_unknown_warrant_vetoes_the_destructive_apply_and_reachable_releases_it_pg() {
    // Canonical authority order is E -> P: the fixture retains E before
    // TestPostgresDb acquires P. Teardown below reverses that order.
    let fixture =
        crate::services::discord::health::StaleSweepWarrantFixture::transport_unknown(5_464_101)
            .await
            .unwrap();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let key = fixture.session_key();
    seed_session(
        &pool,
        key,
        "turn_active",
        None,
        STALE_TURN_GRACE.as_secs() as i64 + 60,
    )
    .await;

    let liveness_probe = |_: &str, _: &str| IndependentLiveness::NoPane;
    let inflight_probe = |_: &str, _: &str| InflightObservation::Unknown;
    let automatic_warrant = Some((
        Some(fixture.registry() as &dyn StaleSweepPort<AxisBSite>),
        AxisBSite::BootReconcileSweep,
    ));
    let vetoed = reconcile_stale_turns_matching_with_warrant_pg(
        &pool,
        Some(key),
        liveness_probe,
        inflight_probe,
        automatic_warrant,
    )
    .await
    .unwrap();
    assert_eq!(
        vetoed.reconciled, 0,
        "axis-B Deny must stop the destructive apply"
    );
    assert!(
        !vetoed.precondition_changed,
        "warrant veto must not be reported as a precondition change"
    );
    assert_eq!(
        load_state(&pool, key).await,
        ("turn_active".to_string(), Some("original".to_string())),
        "the row must survive untouched"
    );

    fixture.flip_to_reachable().unwrap();
    let released = reconcile_stale_turns_matching_with_warrant_pg(
        &pool,
        Some(key),
        liveness_probe,
        inflight_probe,
        automatic_warrant,
    )
    .await
    .unwrap();
    assert_eq!(
        released.reconciled, 1,
        "the same candidate must release once the axis-B verdict turns Reachable"
    );
    assert_eq!(load_state(&pool, key).await.0, "idle");

    pool.close().await;
    pg_db.drop().await;
    drop(fixture);
}

#[tokio::test]
async fn stale_busy_candidates_reconcile_only_after_terminal_liveness_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    allow_legacy_working_status(&pool).await;
    let stale_age = STALE_TURN_GRACE.as_secs() as i64 + 60;

    seed_session(&pool, "host:stale-turn", "turn_active", None, stale_age).await;
    seed_session(
        &pool,
        "host:stale-working",
        "working",
        Some("  "),
        stale_age,
    )
    .await;
    seed_session(
        &pool,
        "host:live-dispatch",
        "turn_active",
        Some("dispatch-live"),
        stale_age,
    )
    .await;
    seed_session(&pool, "host:live-heartbeat", "turn_active", None, 30).await;

    assert_eq!(
        reconcile_stale_turns_matching_pg(&pool, None, |_, _| { IndependentLiveness::NoPane })
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        load_state(&pool, "host:stale-working").await,
        (
            "idle".to_string(),
            Some(
                "reconciled stale working (no dispatch, stale heartbeat, terminal tmux)"
                    .to_string()
            )
        )
    );
    assert_eq!(
        load_state(&pool, "host:live-dispatch").await,
        ("turn_active".to_string(), Some("original".to_string()))
    );
    assert_eq!(
        load_state(&pool, "host:live-heartbeat").await,
        ("turn_active".to_string(), Some("original".to_string()))
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn automatic_sweep_abstains_without_registry_but_operator_stays_outside_warrant_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let stale_age = STALE_TURN_GRACE.as_secs() as i64 + 60;
    let automatic = "host:AgentDesk-claude-5464001";
    let operator = "host:AgentDesk-claude-5464002";
    seed_session(&pool, automatic, "turn_active", None, stale_age).await;
    seed_session(&pool, operator, "turn_active", None, stale_age).await;

    assert_eq!(
        reconcile_stale_turns_matching_with_warrant_pg(
            &pool,
            Some(automatic),
            |_, _| IndependentLiveness::NoPane,
            |_, _| InflightObservation::Unknown,
            Some((None, AxisBSite::BootReconcileSweep)),
        )
        .await
        .unwrap()
        .reconciled,
        1,
        "missing boot registry operand must preserve structural eligibility"
    );
    assert_eq!(
        reconcile_stale_turns_matching_pg(&pool, Some(operator), |_, _| {
            IndependentLiveness::NoPane
        })
        .await
        .unwrap(),
        1,
        "operator keyed reconciliation remains outside the automatic warrant"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn preserved_live_tmux_evidence_keeps_stale_row_busy_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let stale_age = STALE_TURN_GRACE.as_secs() as i64 + 60;
    seed_session(&pool, "host:preserved-live", "turn_active", None, stale_age).await;

    assert_eq!(
        reconcile_stale_turns_matching_pg(&pool, None, |_, _| {
            IndependentLiveness::LiveOrAmbiguous
        })
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        load_state(&pool, "host:preserved-live").await,
        ("turn_active".to_string(), Some("original".to_string()))
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn probe_failure_spinner_and_provider_mismatch_preserve_rows_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let stale_age = STALE_TURN_GRACE.as_secs() as i64 + 60;
    for key in [
        "host:probe-failed",
        "host:spinner",
        "host:provider-mismatch",
    ] {
        seed_session(&pool, key, "turn_active", None, stale_age).await;
    }

    assert_eq!(
        reconcile_stale_turns_matching_pg(&pool, Some("host:probe-failed"), |_, _| {
            IndependentLiveness::LiveOrAmbiguous
        })
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        reconcile_stale_turns_matching_pg(&pool, Some("host:spinner"), |_, _| {
            IndependentLiveness::LiveOrAmbiguous
        })
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        reconcile_stale_turns_matching_pg(&pool, Some("host:provider-mismatch"), |_, _| {
            IndependentLiveness::RemoteOrInvalid
        },)
        .await
        .unwrap(),
        0
    );
    for key in [
        "host:probe-failed",
        "host:spinner",
        "host:provider-mismatch",
    ] {
        assert_eq!(
            load_state(&pool, key).await,
            ("turn_active".to_string(), Some("original".to_string()))
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

/// The last production hop: an unreadable provider directory must map to
/// `Unknown`, and the operator-only destructive qualification must abstain.
#[tokio::test]
async fn unreadable_inflight_provider_maps_unknown_and_preserves_busy_session_pg() {
    let tmp = tempfile::tempdir().unwrap();
    let _root_guard = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
    let root = tmp.path().join("runtime").join("discord_inflight");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(ProviderKind::Claude.as_str()), b"not a directory").unwrap();

    let tmux_name = ProviderKind::Claude.build_tmux_session_name("5464999");
    let session_key = format!(
        "{}:{tmux_name}",
        crate::services::platform::hostname_short()
    );
    assert_eq!(
        channel_inflight_observation(&session_key, "claude", crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name),
        InflightObservation::Unknown,
        "the production session-key mapping must retain Unprobeable as Unknown"
    );

    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_session(&pool, &session_key, "turn_active", None, 30).await;
    assert_eq!(
        reconcile_stale_turn_by_key_with_probes_pg(
            &pool,
            &session_key,
            |_, _| IndependentLiveness::ReadyForInput,
            channel_inflight_probe(crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name),
        )
        .await
        .unwrap(),
        SessionReconcileOutcome::Unchanged,
        "an unreadable inflight store must not authorize IdleWithoutInflight"
    );
    assert_eq!(
        load_state(&pool, &session_key).await,
        ("turn_active".to_string(), Some("original".to_string()))
    );

    pool.close().await;
    pg_db.drop().await;
}

/// The unattended sweeps must NOT inherit the widened qualification: they
/// still require an expired heartbeat, so a fresh busy row is untouched even
/// when every local probe looks terminal.
#[tokio::test]
async fn periodic_sweep_pg_still_requires_an_expired_heartbeat() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_session(&pool, "host:sweep-fresh", "turn_active", None, 30).await;

    assert_eq!(
        reconcile_stale_turns_matching_pg(&pool, None, |_, _| IndependentLiveness::NoPane)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        load_state(&pool, "host:sweep-fresh").await,
        ("turn_active".to_string(), Some("original".to_string()))
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn keyed_unchanged_outcome_keeps_live_row_turn_active_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let stale_age = STALE_TURN_GRACE.as_secs() as i64 + 60;
    seed_session(
        &pool,
        "remote-host:live-turn",
        "turn_active",
        None,
        stale_age,
    )
    .await;

    assert_eq!(
        reconcile_stale_turn_by_key_pg(&pool, "remote-host:live-turn", crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name)
            .await
            .unwrap(),
        SessionReconcileOutcome::Unchanged
    );
    assert_eq!(
        load_state(&pool, "remote-host:live-turn").await,
        ("turn_active".to_string(), Some("original".to_string()))
    );
    assert_eq!(
        reconcile_stale_turn_by_key_pg(&pool, "missing", crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name)
            .await
            .unwrap(),
        SessionReconcileOutcome::NotFound
    );

    pool.close().await;
    pg_db.drop().await;
}
