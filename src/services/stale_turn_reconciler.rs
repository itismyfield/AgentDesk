use anyhow::{Result, anyhow};
use sqlx::PgPool;
use std::time::Duration;

use crate::services::discord::health::HealthRegistry;
use crate::services::discord::relay_recovery::AxisBSite;
use crate::services::discord::session_identity::SessionIdentity;
use crate::services::provider::ProviderKind;

mod test_barriers;
#[cfg(test)]
use test_barriers::await_stale_sweep_apply_barrier;
#[cfg(test)]
pub(crate) use test_barriers::{
    StaleSweepApplyBarrier, StaleSweepApplyBarrierPoint, install_stale_sweep_apply_barrier,
};

/// A live turn refreshes its heartbeat roughly once per minute. Five minutes
/// leaves enough margin for transient database or scheduler delays while still
/// bounding how long a stale busy state can block mailbox injection.
pub(crate) const STALE_TURN_GRACE: Duration = Duration::from_secs(5 * 60);

/// Which guard qualified a session for reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StaleTurnQualification {
    /// The historical guard: no active dispatch AND an expired heartbeat.
    StaleHeartbeat,
    /// #5176: no active dispatch AND no persistent inflight-turn record for the
    /// session's channel, with the heartbeat still fresh. A session whose turn
    /// record is gone has no turn — the heartbeat is being kept alive by
    /// something that is not the turn, which is exactly why the heartbeat-only
    /// guard reported the incident channel as "live" and left it locked.
    IdleWithoutInflight,
}

impl StaleTurnQualification {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::StaleHeartbeat => "stale_heartbeat",
            Self::IdleWithoutInflight => "idle_without_inflight",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionReconcileOutcome {
    Reconciled(StaleTurnQualification),
    Unchanged,
    NotFound,
    PreconditionChanged(PreconditionDiagnostic),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub(crate) struct PreconditionDiagnostic {
    pub(crate) row_exists: bool,
    pub(crate) status: Option<String>,
    pub(crate) active_dispatch_id: Option<String>,
    pub(crate) last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct StaleTurnCandidate {
    session_key: String,
    provider: String,
    status: String,
    active_dispatch_id: Option<String>,
    last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
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
pub(crate) async fn reconcile_stale_turns_pg(
    pool: &PgPool,
    registry: Option<&HealthRegistry>,
    site: AxisBSite,
) -> Result<usize> {
    Ok(reconcile_stale_turns_matching_with_warrant_pg(
        pool,
        None,
        independent_tmux_liveness,
        Some((registry, site)),
    )
    .await?
    .reconciled)
}

/// Reconcile one session for the operator API without weakening the liveness
/// gates used by startup and periodic sweeps.
pub(crate) async fn reconcile_stale_turn_by_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<SessionReconcileOutcome> {
    reconcile_stale_turn_by_key_with_probes_pg(
        pool,
        session_key,
        independent_tmux_liveness,
        channel_inflight_state_present,
    )
    .await
}

/// #5176 — the operator endpoint, with both liveness probes injectable so the
/// widened guard can be pinned in BOTH directions without a tmux pane.
async fn reconcile_stale_turn_by_key_with_probes_pg<L, I>(
    pool: &PgPool,
    session_key: &str,
    liveness_probe: L,
    inflight_probe: I,
) -> Result<SessionReconcileOutcome>
where
    L: Fn(&str, &str) -> IndependentLiveness + Copy + Send + 'static,
    I: Fn(&str, &str) -> InflightPresence + Copy + Send + 'static,
{
    match reconcile_stale_turns_matching_with_warrant_pg(
        pool,
        Some(session_key),
        liveness_probe,
        None,
    )
    .await?
    {
        ApplySummary {
            reconciled: 1..,
            precondition_changed: false,
        } => {
            return Ok(SessionReconcileOutcome::Reconciled(
                StaleTurnQualification::StaleHeartbeat,
            ));
        }
        ApplySummary {
            precondition_changed: true,
            ..
        } => {
            // Do not fall through to the operator-only IdleWithoutInflight
            // qualification after a stale-candidate CAS miss. Besides avoiding a
            // second destructive attempt from a later snapshot in the same
            // request, this keeps the single candidate/apply test seam from being
            // reached twice. The retryable 409 makes the caller obtain a fresh
            // snapshot instead.
            return precondition_changed_outcome_pg(pool, session_key).await;
        }
        _ => {}
    }

    // #5176 — the widened qualification is scoped to this keyed operator call:
    // no active dispatch, no inflight episode, and terminal tmux evidence.
    match reconcile_idle_without_inflight_pg(pool, session_key, liveness_probe, inflight_probe)
        .await?
    {
        CandidateApplyOutcome::Reconciled => {
            return Ok(SessionReconcileOutcome::Reconciled(
                StaleTurnQualification::IdleWithoutInflight,
            ));
        }
        CandidateApplyOutcome::PreconditionChanged => {
            return precondition_changed_outcome_pg(pool, session_key).await;
        }
        CandidateApplyOutcome::Unchanged => {}
    }

    let diagnostic = load_precondition_diagnostic_pg(pool, session_key).await?;
    Ok(if diagnostic.row_exists {
        SessionReconcileOutcome::Unchanged
    } else {
        SessionReconcileOutcome::NotFound
    })
}

async fn precondition_changed_outcome_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<SessionReconcileOutcome> {
    // This is diagnostic only and is intentionally not fed back into the apply
    // decision. It observes a later instant than the failed UPDATE.
    let diagnostic = load_precondition_diagnostic_pg(pool, session_key).await?;
    tracing::info!(
        target: "reconcile",
        session_key,
        reason = "precondition_changed",
        diagnostic_at = "after_failed_update",
        row_exists = diagnostic.row_exists,
        status = ?diagnostic.status,
        active_dispatch_id = ?diagnostic.active_dispatch_id,
        last_heartbeat = ?diagnostic.last_heartbeat,
        "stale-turn operator apply precondition changed; retry with a fresh snapshot"
    );
    Ok(SessionReconcileOutcome::PreconditionChanged(diagnostic))
}

async fn load_precondition_diagnostic_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<PreconditionDiagnostic> {
    let row = sqlx::query_as::<_, PreconditionDiagnostic>(
        "SELECT TRUE AS row_exists,
                status,
                active_dispatch_id,
                last_heartbeat
           FROM sessions
          WHERE session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load stale-turn post-update diagnostic: {error}"))?;
    Ok(row.unwrap_or(PreconditionDiagnostic {
        row_exists: false,
        status: None,
        active_dispatch_id: None,
        last_heartbeat: None,
    }))
}

/// Whether a persistent inflight-turn record exists for a session's channel.
/// `Unknown` is NOT "absent": an unparseable session key or an unreadable
/// runtime root must fail closed, exactly like an unprobeable tmux pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InflightPresence {
    Present,
    Absent,
    Unknown,
}

fn channel_inflight_state_present(session_key: &str, provider: &str) -> InflightPresence {
    let Some(identity) = SessionIdentity::parse(session_key) else {
        return InflightPresence::Unknown;
    };
    let Some(db_provider) = ProviderKind::from_str(provider) else {
        return InflightPresence::Unknown;
    };
    if identity.host != crate::services::platform::hostname_short() {
        return InflightPresence::Unknown;
    }
    let Some((tmux_provider, _)) = identity.provider_and_channel() else {
        return InflightPresence::Unknown;
    };
    if tmux_provider != db_provider {
        return InflightPresence::Unknown;
    }
    if crate::services::discord::zombie_foreground_release::inflight_state_present_for_tmux_name(
        &db_provider,
        &identity.tmux_name,
    ) {
        InflightPresence::Present
    } else {
        InflightPresence::Absent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateApplyOutcome {
    Reconciled,
    Unchanged,
    PreconditionChanged,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ApplySummary {
    reconciled: usize,
    precondition_changed: bool,
}

async fn reconcile_idle_without_inflight_pg<L, I>(
    pool: &PgPool,
    session_key: &str,
    liveness_probe: L,
    inflight_probe: I,
) -> Result<CandidateApplyOutcome>
where
    L: Fn(&str, &str) -> IndependentLiveness + Copy + Send + 'static,
    I: Fn(&str, &str) -> InflightPresence + Copy + Send + 'static,
{
    let Some(candidate) = load_busy_session_pg(pool, session_key).await? else {
        return Ok(CandidateApplyOutcome::Unchanged);
    };

    let key = candidate.session_key.clone();
    let provider = candidate.provider.clone();
    let inflight = tokio::task::spawn_blocking(move || inflight_probe(&key, &provider))
        .await
        .unwrap_or(InflightPresence::Unknown);
    if inflight != InflightPresence::Absent {
        tracing::info!(
            target: "reconcile",
            session_key = %candidate.session_key,
            ?inflight,
            "preserved busy session because an inflight turn record was present or unprobeable"
        );
        return Ok(CandidateApplyOutcome::Unchanged);
    }

    let key = candidate.session_key.clone();
    let provider = candidate.provider.clone();
    let liveness = tokio::task::spawn_blocking(move || liveness_probe(&key, &provider))
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
            "preserved busy session without inflight because independent liveness was not terminal"
        );
        return Ok(CandidateApplyOutcome::Unchanged);
    }

    #[cfg(test)]
    await_stale_sweep_apply_barrier().await;

    let reconciled = reconcile_idle_without_inflight_candidate_pg(pool, &candidate).await?;
    if reconciled > 0 {
        tracing::warn!(
            target: "reconcile",
            session_key = %candidate.session_key,
            qualification = StaleTurnQualification::IdleWithoutInflight.as_str(),
            "reconciled a busy session with no inflight turn record and terminal tmux evidence"
        );
        Ok(CandidateApplyOutcome::Reconciled)
    } else {
        log_precondition_changed(&candidate);
        Ok(CandidateApplyOutcome::PreconditionChanged)
    }
}

/// The same busy-session shape as `load_stale_turn_candidates_pg`, minus the
/// heartbeat predicate. The heartbeat is the ONLY relaxed gate; the no-active-
/// dispatch requirement is carried unchanged.
async fn load_busy_session_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<StaleTurnCandidate>> {
    sqlx::query_as::<_, StaleTurnCandidate>(
        "SELECT session_key,
                COALESCE(provider, 'claude') AS provider,
                status,
                active_dispatch_id,
                last_heartbeat
           FROM sessions
          WHERE status IN ('turn_active', 'working')
            AND COALESCE(BTRIM(active_dispatch_id), '') = ''
            AND session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load busy session for idle-without-inflight reconcile: {error}"))
}

async fn reconcile_idle_without_inflight_candidate_pg(
    pool: &PgPool,
    candidate: &StaleTurnCandidate,
) -> Result<usize> {
    sqlx::query(
        "UPDATE sessions
            SET session_info = 'reconciled busy ' || status ||
                               ' (no dispatch, no inflight turn, terminal tmux)',
                status = 'idle'
          WHERE session_key = $1
            AND COALESCE(provider, 'claude') = $2
            AND status = $3
            AND COALESCE(BTRIM(active_dispatch_id), '') =
                COALESCE(BTRIM($4::TEXT), '')
            AND last_heartbeat IS NOT DISTINCT FROM $5",
    )
    .bind(&candidate.session_key)
    .bind(&candidate.provider)
    .bind(&candidate.status)
    .bind(&candidate.active_dispatch_id)
    .bind(candidate.last_heartbeat)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| {
        anyhow!(
            "reconcile busy session without inflight {}: {error}",
            candidate.session_key
        )
    })
}

#[cfg(test)]
async fn reconcile_stale_turns_matching_pg<F>(
    pool: &PgPool,
    session_key: Option<&str>,
    probe: F,
) -> Result<usize>
where
    F: Fn(&str, &str) -> IndependentLiveness + Copy + Send + 'static,
{
    Ok(
        reconcile_stale_turns_matching_with_warrant_pg(pool, session_key, probe, None)
            .await?
            .reconciled,
    )
}

fn structural_candidate_apply(eligible: bool) -> bool {
    eligible
}

async fn destructive_warrant_bind(
    registry: Option<&HealthRegistry>,
    session_key: &str,
    provider: &str,
    site: AxisBSite,
) -> bool {
    crate::services::discord::relay_recovery::automatic_stale_sweep_warrants(
        registry,
        session_key,
        provider,
        site,
    )
    .await
}

async fn reconcile_stale_turns_matching_with_warrant_pg<F>(
    pool: &PgPool,
    session_key: Option<&str>,
    probe: F,
    automatic_warrant: Option<(Option<&HealthRegistry>, AxisBSite)>,
) -> Result<ApplySummary>
where
    F: Fn(&str, &str) -> IndependentLiveness + Copy + Send + 'static,
{
    let candidates = load_stale_turn_candidates_pg(pool, session_key).await?;
    let mut summary = ApplySummary::default();

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

        let structural_candidate_apply = structural_candidate_apply(true);
        let destructive_warrant_bind = match automatic_warrant {
            Some((registry, site)) => {
                destructive_warrant_bind(
                    registry,
                    &candidate.session_key,
                    &candidate.provider,
                    site,
                )
                .await
            }
            None => structural_candidate_apply,
        };
        if !destructive_warrant_bind {
            continue;
        }

        #[cfg(test)]
        await_stale_sweep_apply_barrier().await;

        let updated = reconcile_candidate_pg(pool, &candidate).await?;
        if updated == 0 {
            summary.precondition_changed = true;
            log_precondition_changed(&candidate);
            continue;
        }
        summary.reconciled += updated;
    }

    if summary.reconciled > 0 {
        tracing::warn!(
            target: "reconcile",
            reconciled = summary.reconciled,
            session_key = session_key.unwrap_or("*"),
            grace_seconds = STALE_TURN_GRACE.as_secs(),
            "reconciled stale busy sessions with terminal tmux evidence"
        );
    }
    Ok(summary)
}

fn log_precondition_changed(candidate: &StaleTurnCandidate) {
    tracing::info!(
        target: "reconcile",
        session_key = %candidate.session_key,
        reason = "precondition_changed",
        "stale-turn apply skipped because its captured precondition changed"
    );
}

async fn load_stale_turn_candidates_pg(
    pool: &PgPool,
    session_key: Option<&str>,
) -> Result<Vec<StaleTurnCandidate>> {
    sqlx::query_as::<_, StaleTurnCandidate>(
        "SELECT session_key,
                COALESCE(provider, 'claude') AS provider,
                status,
                active_dispatch_id,
                last_heartbeat
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

async fn reconcile_candidate_pg(pool: &PgPool, candidate: &StaleTurnCandidate) -> Result<usize> {
    sqlx::query(
        "UPDATE sessions
            SET session_info = 'reconciled stale ' || status ||
                               ' (no dispatch, stale heartbeat, terminal tmux)',
                status = 'idle'
          WHERE session_key = $2
            AND COALESCE(provider, 'claude') = $3
            AND status = $4
            AND COALESCE(BTRIM(active_dispatch_id), '') =
                COALESCE(BTRIM($5::TEXT), '')
            AND last_heartbeat IS NOT DISTINCT FROM $6
            AND last_heartbeat < NOW() - ($1::BIGINT * INTERVAL '1 second')",
    )
    .bind(STALE_TURN_GRACE.as_secs() as i64)
    .bind(&candidate.session_key)
    .bind(&candidate.provider)
    .bind(&candidate.status)
    .bind(&candidate.active_dispatch_id)
    .bind(candidate.last_heartbeat)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| {
        anyhow!(
            "reconcile stale busy session {}: {error}",
            candidate.session_key
        )
    })
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

    /// #5176 — the reproduction. A busy session with a FRESH heartbeat (so the
    /// historical guard reports "session is live"), no active dispatch, no
    /// inflight turn record, and a pane parked at the prompt is a zombie, and
    /// the operator endpoint must be able to reconcile it.
    #[tokio::test]
    async fn keyed_reconcile_pg_releases_busy_session_without_inflight() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        // 30s: far inside STALE_TURN_GRACE, i.e. the heartbeat guard says LIVE.
        seed_session(
            &pool,
            "host:zombie-fresh-heartbeat",
            "turn_active",
            None,
            30,
        )
        .await;

        assert_eq!(
            reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                "host:zombie-fresh-heartbeat",
                |_, _| IndependentLiveness::ReadyForInput,
                |_, _| InflightPresence::Absent,
            )
            .await
            .unwrap(),
            SessionReconcileOutcome::Reconciled(StaleTurnQualification::IdleWithoutInflight)
        );
        assert_eq!(
            load_state(&pool, "host:zombie-fresh-heartbeat").await,
            (
                "idle".to_string(),
                Some(
                    "reconciled busy turn_active (no dispatch, no inflight turn, terminal tmux)"
                        .to_string()
                )
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// The counter-direction for the widened guard: every gate that can prove a
    /// turn is alive must independently keep the row `turn_active`. Over-release
    /// here is worse than the zombie — it abandons work a user is waiting on.
    #[tokio::test]
    async fn keyed_reconcile_pg_never_idles_a_live_busy_session() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_session(&pool, "host:live-inflight", "turn_active", None, 30).await;
        seed_session(&pool, "host:live-pane", "turn_active", None, 30).await;
        seed_session(&pool, "host:unknown-inflight", "turn_active", None, 30).await;
        seed_session(
            &pool,
            "host:live-dispatch-fresh",
            "turn_active",
            Some("dispatch-live"),
            30,
        )
        .await;

        // A turn-bridge loop still owns this turn.
        assert_eq!(
            reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                "host:live-inflight",
                |_, _| IndependentLiveness::ReadyForInput,
                |_, _| InflightPresence::Present,
            )
            .await
            .unwrap(),
            SessionReconcileOutcome::Unchanged
        );

        // No inflight record, but the pane is streaming or unprobeable.
        assert_eq!(
            reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                "host:live-pane",
                |_, _| IndependentLiveness::LiveOrAmbiguous,
                |_, _| InflightPresence::Absent,
            )
            .await
            .unwrap(),
            SessionReconcileOutcome::Unchanged
        );

        // Inflight presence could not be established — absence of proof is not
        // proof of absence, so it must fail closed like the tmux probe does.
        assert_eq!(
            reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                "host:unknown-inflight",
                |_, _| IndependentLiveness::ReadyForInput,
                |_, _| InflightPresence::Unknown,
            )
            .await
            .unwrap(),
            SessionReconcileOutcome::Unchanged
        );

        // The no-active-dispatch gate is carried over unchanged from the
        // historical guard: a dispatched turn is never reconciled, no matter
        // how terminal the local evidence looks.
        assert_eq!(
            reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                "host:live-dispatch-fresh",
                |_, _| IndependentLiveness::NoPane,
                |_, _| InflightPresence::Absent,
            )
            .await
            .unwrap(),
            SessionReconcileOutcome::Unchanged
        );

        for key in [
            "host:live-inflight",
            "host:live-pane",
            "host:unknown-inflight",
            "host:live-dispatch-fresh",
        ] {
            assert_eq!(
                load_state(&pool, key).await,
                ("turn_active".to_string(), Some("original".to_string())),
                "{key} must survive the widened stale-turn guard"
            );
        }

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

    fn apply_barrier() -> (
        StaleSweepApplyBarrier,
        std::sync::Arc<StaleSweepApplyBarrierPoint>,
        std::sync::Arc<StaleSweepApplyBarrierPoint>,
    ) {
        let reached = std::sync::Arc::new(StaleSweepApplyBarrierPoint::new(2));
        let resume = std::sync::Arc::new(StaleSweepApplyBarrierPoint::new(2));
        (
            StaleSweepApplyBarrier {
                reached: reached.clone(),
                resume: resume.clone(),
            },
            reached,
            resume,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_candidate_cas_rejects_heartbeat_move_and_is_non_latching_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let key = "remote-host:cas-heartbeat";
        seed_session(
            &pool,
            key,
            "turn_active",
            None,
            STALE_TURN_GRACE.as_secs() as i64 + 60,
        )
        .await;

        let (barrier, reached, resume) = apply_barrier();
        let guard = install_stale_sweep_apply_barrier(barrier);
        let task_pool = pool.clone();
        let task = tokio::spawn(async move {
            reconcile_stale_turn_by_key_with_probes_pg(
                &task_pool,
                key,
                |_, _| IndependentLiveness::NoPane,
                |_, _| InflightPresence::Unknown,
            )
            .await
            .unwrap()
        });
        reached.wait().await;
        sqlx::query("UPDATE sessions SET last_heartbeat = NOW() - INTERVAL '10 minutes' WHERE session_key = $1")
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        resume.wait().await;
        assert!(matches!(
            task.await.unwrap(),
            SessionReconcileOutcome::PreconditionChanged(_)
        ));
        assert_eq!(load_state(&pool, key).await.0, "turn_active");
        drop(guard);

        assert_eq!(
            reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                key,
                |_, _| IndependentLiveness::NoPane,
                |_, _| InflightPresence::Unknown,
            )
            .await
            .unwrap(),
            SessionReconcileOutcome::Reconciled(StaleTurnQualification::StaleHeartbeat)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_without_inflight_cas_rejects_heartbeat_move_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let key = "remote-host:idle-cas-heartbeat";
        seed_session(&pool, key, "turn_active", None, 30).await;

        let (barrier, reached, resume) = apply_barrier();
        let _guard = install_stale_sweep_apply_barrier(barrier);
        let task_pool = pool.clone();
        let task = tokio::spawn(async move {
            reconcile_stale_turn_by_key_with_probes_pg(
                &task_pool,
                key,
                |_, _| IndependentLiveness::ReadyForInput,
                |_, _| InflightPresence::Absent,
            )
            .await
            .unwrap()
        });
        reached.wait().await;
        sqlx::query("UPDATE sessions SET last_heartbeat = NOW() WHERE session_key = $1")
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        resume.wait().await;

        assert!(matches!(
            task.await.unwrap(),
            SessionReconcileOutcome::PreconditionChanged(_)
        ));
        assert_eq!(load_state(&pool, key).await.0, "turn_active");
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn automatic_cas_log_names_only_precondition_changed_pg() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
            type Writer = Writer;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let key = "remote-host:automatic-log-cas";
        seed_session(
            &pool,
            key,
            "turn_active",
            None,
            STALE_TURN_GRACE.as_secs() as i64 + 60,
        )
        .await;
        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(Writer(logs.clone()))
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let (barrier, reached, resume) = apply_barrier();
        let _guard = install_stale_sweep_apply_barrier(barrier);
        let task_pool = pool.clone();
        let task = tokio::spawn(async move {
            reconcile_stale_turns_matching_with_warrant_pg(
                &task_pool,
                Some(key),
                |_, _| IndependentLiveness::NoPane,
                None,
            )
            .await
            .unwrap()
        });
        reached.wait().await;
        sqlx::query("UPDATE sessions SET provider = 'codex' WHERE session_key = $1")
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        resume.wait().await;
        assert!(task.await.unwrap().precondition_changed);
        let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("reason=\"precondition_changed\""),
            "{output}"
        );
        for unsupported in [
            "provider_changed",
            "heartbeat_changed",
            "status_changed",
            "dispatch_changed",
        ] {
            assert!(
                !output.contains(unsupported),
                "unsupported cause label in {output}"
            );
        }
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provider_only_interleaving_fails_the_candidate_cas_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let key = "remote-host:provider-cas";
        seed_session(
            &pool,
            key,
            "turn_active",
            None,
            STALE_TURN_GRACE.as_secs() as i64 + 60,
        )
        .await;
        let heartbeat_before: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT last_heartbeat FROM sessions WHERE session_key = $1")
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();

        let (barrier, reached, resume) = apply_barrier();
        let _guard = install_stale_sweep_apply_barrier(barrier);
        let task_pool = pool.clone();
        let task = tokio::spawn(async move {
            reconcile_stale_turn_by_key_with_probes_pg(
                &task_pool,
                key,
                |_, _| IndependentLiveness::NoPane,
                |_, _| InflightPresence::Unknown,
            )
            .await
            .unwrap()
        });
        reached.wait().await;
        sqlx::query("UPDATE sessions SET provider = 'codex' WHERE session_key = $1")
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        let heartbeat_after: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT last_heartbeat FROM sessions WHERE session_key = $1")
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            heartbeat_before, heartbeat_after,
            "interleaving must change only provider"
        );
        resume.wait().await;

        assert!(matches!(
            task.await.unwrap(),
            SessionReconcileOutcome::PreconditionChanged(_)
        ));
        assert_eq!(load_state(&pool, key).await.0, "turn_active");
        pool.close().await;
        pg_db.drop().await;
    }
}
