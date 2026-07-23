use anyhow::{Result, anyhow};
use sqlx::PgPool;
use std::time::Duration;

use crate::services::discord::session_identity::SessionIdentity;
use crate::services::provider::ProviderKind;

/// A live turn refreshes its heartbeat roughly once per minute. Five minutes
/// leaves enough margin for transient database or scheduler delays while still
/// bounding how long a stale busy state can block mailbox injection.
pub(crate) const STALE_TURN_GRACE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionReconcileOutcome {
    Reconciled,
    Unchanged,
    NotFound,
}

#[derive(Debug, sqlx::FromRow)]
struct StaleTurnCandidate {
    session_key: String,
    provider: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndependentLiveness {
    NoPane,
    ReadyForInput,
    LiveOrAmbiguous,
    RemoteOrInvalid,
}

/// Reconcile every stale busy session that independent tmux evidence confirms
/// is no longer running a turn.
///
/// A stale database heartbeat is only a candidate signal: it can also mean the
/// database was unavailable while a preserved tmux turn kept running. Each
/// candidate is therefore checked against the local tmux pane before the final
/// guarded update. A live or ambiguous pane fails closed and remains busy.
pub(crate) async fn reconcile_stale_turns_pg(pool: &PgPool) -> Result<usize> {
    reconcile_stale_turns_matching_pg(pool, None, independent_tmux_liveness).await
}

/// Reconcile one session for the operator API without weakening the liveness
/// gates used by startup and periodic sweeps.
pub(crate) async fn reconcile_stale_turn_by_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<SessionReconcileOutcome> {
    let reconciled =
        reconcile_stale_turns_matching_pg(pool, Some(session_key), independent_tmux_liveness)
            .await?;
    if reconciled > 0 {
        return Ok(SessionReconcileOutcome::Reconciled);
    }

    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM sessions WHERE session_key = $1)",
    )
    .bind(session_key)
    .fetch_one(pool)
    .await
    .map_err(|error| anyhow!("check stale-turn reconcile session existence: {error}"))?;

    Ok(if exists {
        SessionReconcileOutcome::Unchanged
    } else {
        SessionReconcileOutcome::NotFound
    })
}

async fn reconcile_stale_turns_matching_pg<F>(
    pool: &PgPool,
    session_key: Option<&str>,
    probe: F,
) -> Result<usize>
where
    F: Fn(&str, &str) -> IndependentLiveness + Copy + Send + 'static,
{
    let candidates = load_stale_turn_candidates_pg(pool, session_key).await?;
    let mut reconciled = 0;

    for candidate in candidates {
        let key = candidate.session_key.clone();
        let provider = candidate.provider.clone();
        let liveness = tokio::task::spawn_blocking(move || probe(&key, &provider))
            .await
            .unwrap_or(IndependentLiveness::LiveOrAmbiguous);

        if !matches!(
            liveness,
            IndependentLiveness::NoPane | IndependentLiveness::ReadyForInput
        ) {
            tracing::info!(
                target: "reconcile",
                session_key = %candidate.session_key,
                ?liveness,
                "preserved stale busy session because independent liveness was not terminal"
            );
            continue;
        }

        reconciled += reconcile_candidate_pg(pool, &candidate.session_key).await?;
    }

    if reconciled > 0 {
        tracing::warn!(
            target: "reconcile",
            reconciled,
            session_key = session_key.unwrap_or("*"),
            grace_seconds = STALE_TURN_GRACE.as_secs(),
            "reconciled stale busy sessions with terminal tmux evidence"
        );
    }
    Ok(reconciled)
}

async fn load_stale_turn_candidates_pg(
    pool: &PgPool,
    session_key: Option<&str>,
) -> Result<Vec<StaleTurnCandidate>> {
    sqlx::query_as::<_, StaleTurnCandidate>(
        "SELECT session_key, COALESCE(provider, 'claude') AS provider
           FROM sessions
          WHERE status IN ('turn_active', 'working')
            AND COALESCE(BTRIM(active_dispatch_id), '') = ''
            AND last_heartbeat < NOW() - ($1::BIGINT * INTERVAL '1 second')
            AND ($2::TEXT IS NULL OR session_key = $2)",
    )
    .bind(STALE_TURN_GRACE.as_secs() as i64)
    .bind(session_key)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("load stale busy session candidates: {error}"))
}

fn independent_tmux_liveness(session_key: &str, provider: &str) -> IndependentLiveness {
    let Some(identity) = SessionIdentity::parse(session_key) else {
        return IndependentLiveness::RemoteOrInvalid;
    };
    if identity.host != crate::services::platform::hostname_short() {
        return IndependentLiveness::RemoteOrInvalid;
    }
    let Some(db_provider) = ProviderKind::from_str(provider) else {
        return IndependentLiveness::RemoteOrInvalid;
    };
    let Some((tmux_provider, _)) = identity.provider_and_channel() else {
        return IndependentLiveness::RemoteOrInvalid;
    };
    if tmux_provider != db_provider
        || identity
            .provider_from_key
            .as_deref()
            .is_some_and(|key_provider| key_provider != db_provider.as_str())
    {
        return IndependentLiveness::RemoteOrInvalid;
    }

    let runtime_kind =
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&identity.tmux_name);
    let output_path =
        crate::services::tmux_common::resolve_session_temp_path(&identity.tmux_name, "jsonl");
    match crate::services::tmux_turn_liveness::independent_tmux_readiness(
        &identity.tmux_name,
        &db_provider,
        runtime_kind,
        output_path.as_deref().map(std::path::Path::new),
        None,
    ) {
        crate::services::tmux_turn_liveness::IndependentTmuxReadiness::Missing => {
            IndependentLiveness::NoPane
        }
        crate::services::tmux_turn_liveness::IndependentTmuxReadiness::ReadyForInput => {
            IndependentLiveness::ReadyForInput
        }
        crate::services::tmux_turn_liveness::IndependentTmuxReadiness::LiveOrAmbiguous => {
            IndependentLiveness::LiveOrAmbiguous
        }
    }
}

async fn reconcile_candidate_pg(pool: &PgPool, session_key: &str) -> Result<usize> {
    sqlx::query(
        "UPDATE sessions
            SET session_info = 'reconciled stale ' || status ||
                               ' (no dispatch, stale heartbeat, terminal tmux)',
                status = 'idle'
          WHERE session_key = $2
            AND status IN ('turn_active', 'working')
            AND COALESCE(BTRIM(active_dispatch_id), '') = ''
            AND last_heartbeat < NOW() - ($1::BIGINT * INTERVAL '1 second')",
    )
    .bind(STALE_TURN_GRACE.as_secs() as i64)
    .bind(session_key)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("reconcile stale busy session {session_key}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

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
        sqlx::query(
            "INSERT INTO sessions (
                session_key, provider, status, active_dispatch_id, last_heartbeat, session_info
             ) VALUES (
                $1, 'claude', $2, $3,
                NOW() - ($4::BIGINT * INTERVAL '1 second'), 'original'
             )",
        )
        .bind(session_key)
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

    #[test]
    fn tmux_identity_rejects_provider_mismatch_and_spinner_is_busy() {
        let identity =
            SessionIdentity::parse("claude/hash/mac-mini:AgentDesk-codex-channel").unwrap();
        let db_provider = ProviderKind::Claude;
        let (tmux_provider, _) = identity.provider_and_channel().unwrap();
        assert_ne!(tmux_provider, db_provider);

        let spinner = "─────────────────────────────────────────\n❯ \n✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)";
        assert!(crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(spinner));
        assert_eq!(
            crate::services::provider::fallback_capture_ready_for_input(
                spinner,
                &ProviderKind::Claude,
                Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper),
            )
            .map(crate::services::pane_readiness::FallbackPaneReadiness::is_ready),
            Some(false)
        );
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
            reconcile_stale_turn_by_key_pg(&pool, "remote-host:live-turn")
                .await
                .unwrap(),
            SessionReconcileOutcome::Unchanged
        );
        assert_eq!(
            load_state(&pool, "remote-host:live-turn").await,
            ("turn_active".to_string(), Some("original".to_string()))
        );
        assert_eq!(
            reconcile_stale_turn_by_key_pg(&pool, "missing")
                .await
                .unwrap(),
            SessionReconcileOutcome::NotFound
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
