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
        channel_inflight_observation,
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
        channel_inflight_observation,
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
    L: Fn(&str, &str) -> IndependentLiveness + Clone + Send + 'static,
    I: Fn(&str, &str) -> InflightObservation + Clone + Send + 'static,
{
    match reconcile_stale_turns_matching_with_warrant_pg(
        pool,
        Some(session_key),
        liveness_probe.clone(),
        inflight_probe.clone(),
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

/// One observation of the persistent inflight-turn record for a session's
/// channel. `Unknown` is NOT "absent": an unparseable session key, an
/// unreadable runtime root, or two rows claiming one tmux session all mean "we
/// could not establish this", never "nothing is running". A `Present`
/// observation carries the episode's identity so that `Present(A) ->
/// Present(B)` cannot collapse into "nothing changed".
///
/// How `Unknown` behaves depends on which gate reads it, and the two are
/// deliberately different. The `IdleWithoutInflight` qualification demands
/// `Absent`, so `Unknown` fails closed there. The apply-time check below is a
/// transition test, so `Unknown -> Unknown` proceeds: a session this host can
/// never probe (a remote session key, for instance) must not become
/// permanently unreconcilable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InflightObservation {
    Present(crate::services::discord::zombie_foreground_release::InflightEpisodeIdentity),
    Absent,
    Unknown,
}

impl InflightObservation {
    /// The 3-valued presence, without identity, for logs. The reconciler never
    /// records an episode identity in a log line.
    fn presence_label(&self) -> &'static str {
        match self {
            Self::Present(_) => "present",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }

    /// Did the external inflight authority move between the candidate decision
    /// and this observation?
    ///
    /// This is a transition test, NOT an absolute predicate. The stale-heartbeat
    /// qualification never required inflight absence, so demanding `Absent` at
    /// apply time would let one leftover orphan record block that row's recovery
    /// forever. Requiring only "no transition since the candidate decision"
    /// keeps a pre-existing `Present` reconcilable while still refusing to idle
    /// a row whose authority changed under us.
    fn changed_since(&self, baseline: &Self) -> bool {
        match (baseline, self) {
            (Self::Present(before), Self::Present(after)) => {
                // Required axes: no production writer reassigns either one
                // inside an episode.
                before.started_at != after.started_at
                    || before.user_msg_id != after.user_msg_id
                    // Auxiliary axes: a production writer can fill each in, or
                    // restamp it, on a row that is already on disk, so each
                    // fires only when BOTH observations carry a value. See
                    // `InflightEpisodeIdentity` for the per-field measurement.
                    || differs_when_both_present(&before.turn_nonce, &after.turn_nonce)
                    || differs_when_both_present(
                        &before.turn_start_offset,
                        &after.turn_start_offset,
                    )
            }
            // Every other pairing is a presence change and holds the tick:
            // `Present->Absent`, `Present->Unknown`, `Absent->Present`,
            // `Absent->Unknown`, `Unknown->Present`, `Unknown->Absent`.
            (Self::Absent, Self::Absent) | (Self::Unknown, Self::Unknown) => false,
            _ => true,
        }
    }
}

/// An auxiliary identity axis fires only when both observations carry a value.
/// A field going from absent to present is the writer filling it in, not the
/// authority changing hands.
fn differs_when_both_present<T: PartialEq>(before: &Option<T>, after: &Option<T>) -> bool {
    matches!((before, after), (Some(before), Some(after)) if before != after)
}

fn channel_inflight_observation(session_key: &str, provider: &str) -> InflightObservation {
    let Some(identity) = SessionIdentity::parse(session_key) else {
        return InflightObservation::Unknown;
    };
    let Some(db_provider) = ProviderKind::from_str(provider) else {
        return InflightObservation::Unknown;
    };
    if identity.host != crate::services::platform::hostname_short() {
        return InflightObservation::Unknown;
    }
    let Some((tmux_provider, _)) = identity.provider_and_channel() else {
        return InflightObservation::Unknown;
    };
    if tmux_provider != db_provider {
        return InflightObservation::Unknown;
    }
    use crate::services::discord::zombie_foreground_release::InflightEpisodeLookup;
    match crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name(
        &db_provider,
        &identity.tmux_name,
    ) {
        InflightEpisodeLookup::Claimed(episode) => InflightObservation::Present(episode),
        InflightEpisodeLookup::Unclaimed => InflightObservation::Absent,
        // A store that could not be read, and a tmux session two rows both
        // claim, are both "we do not know". Reporting either as `Absent` would
        // let a read failure authorize destruction, and would let whichever row
        // a directory scan happened to yield first decide the verdict.
        InflightEpisodeLookup::Unprobeable | InflightEpisodeLookup::Ambiguous => {
            InflightObservation::Unknown
        }
    }
}

async fn probe_inflight<I>(candidate: &StaleTurnCandidate, probe: &I) -> InflightObservation
where
    I: Fn(&str, &str) -> InflightObservation + Clone + Send + 'static,
{
    let key = candidate.session_key.clone();
    let provider = candidate.provider.clone();
    let probe = probe.clone();
    tokio::task::spawn_blocking(move || probe(&key, &provider))
        .await
        .unwrap_or(InflightObservation::Unknown)
}

async fn probe_tmux<L>(candidate: &StaleTurnCandidate, probe: &L) -> IndependentLiveness
where
    L: Fn(&str, &str) -> IndependentLiveness + Clone + Send + 'static,
{
    let key = candidate.session_key.clone();
    let provider = candidate.provider.clone();
    let probe = probe.clone();
    tokio::task::spawn_blocking(move || probe(&key, &provider))
        .await
        .unwrap_or(IndependentLiveness::LiveOrAmbiguous)
}

fn tmux_is_terminal(liveness: IndependentLiveness) -> bool {
    matches!(
        liveness,
        IndependentLiveness::NoPane | IndependentLiveness::ReadyForInput
    )
}

/// The apply-time re-read of both external authorities.
///
/// The two axes are checked differently, exactly as the design specifies. The
/// inflight axis is a compare-and-swap against what the candidate decision saw,
/// because a pre-existing record is not disqualifying. The tmux axis is an
/// absolute predicate — apply-time evidence must still be terminal — so
/// `NoPane -> ReadyForInput` moves but still passes. Neither is stronger than
/// the other; a live pane and a changed episode each hold the tick.
///
/// A held tick is written to nothing — no ledger, no circuit, no column, no
/// file — so the next tick re-captures a fresh baseline and decides again. That
/// is why a transition can never become a permanent denial: whatever the new
/// authority is, the next tick treats it as the baseline and reconciles as soon
/// as it stops moving.
async fn external_authority_permits_apply<L, I>(
    candidate: &StaleTurnCandidate,
    liveness_probe: &L,
    inflight_probe: &I,
    baseline_inflight: &InflightObservation,
) -> bool
where
    L: Fn(&str, &str) -> IndependentLiveness + Clone + Send + 'static,
    I: Fn(&str, &str) -> InflightObservation + Clone + Send + 'static,
{
    let apply_tmux = probe_tmux(candidate, liveness_probe).await;
    let apply_inflight = probe_inflight(candidate, inflight_probe).await;
    if tmux_is_terminal(apply_tmux) && !apply_inflight.changed_since(baseline_inflight) {
        return true;
    }
    tracing::info!(
        target: "reconcile",
        session_key = %candidate.session_key,
        ?apply_tmux,
        baseline_inflight = baseline_inflight.presence_label(),
        apply_inflight = apply_inflight.presence_label(),
        "held this tick because external authority moved after the candidate decision; the next tick re-decides from a fresh baseline"
    );
    false
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
    L: Fn(&str, &str) -> IndependentLiveness + Clone + Send + 'static,
    I: Fn(&str, &str) -> InflightObservation + Clone + Send + 'static,
{
    let Some(candidate) = load_busy_session_pg(pool, session_key).await? else {
        return Ok(CandidateApplyOutcome::Unchanged);
    };

    let baseline_inflight = probe_inflight(&candidate, &inflight_probe).await;
    if baseline_inflight != InflightObservation::Absent {
        tracing::info!(
            target: "reconcile",
            session_key = %candidate.session_key,
            inflight = baseline_inflight.presence_label(),
            "preserved busy session because an inflight turn record was present or unprobeable"
        );
        return Ok(CandidateApplyOutcome::Unchanged);
    }

    let baseline_tmux = probe_tmux(&candidate, &liveness_probe).await;
    if !tmux_is_terminal(baseline_tmux) {
        tracing::info!(
            target: "reconcile",
            session_key = %candidate.session_key,
            liveness = ?baseline_tmux,
            "preserved busy session without inflight because independent liveness was not terminal"
        );
        return Ok(CandidateApplyOutcome::Unchanged);
    }

    #[cfg(test)]
    await_stale_sweep_apply_barrier().await;

    // Both external authorities are re-read here, immediately before the
    // destructive UPDATE. This path's candidate qualification already required
    // `Absent`, so the transition test reduces to "still `Absent`" — one rule
    // shared with the stale-heartbeat path above.
    if !external_authority_permits_apply(
        &candidate,
        &liveness_probe,
        &inflight_probe,
        &baseline_inflight,
    )
    .await
    {
        // Same reporting as a failed DB CAS, for the same reason: retryable,
        // not "this session does not qualify".
        return Ok(CandidateApplyOutcome::PreconditionChanged);
    }

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

/// The outer wrapper hands the production inflight probe to the shared path, so
/// the existing suites exercise the real `channel_inflight_observation` on both
/// sides of the barrier rather than an injected stand-in.
#[cfg(test)]
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
        channel_inflight_observation,
        None,
    )
    .await?
    .reconciled)
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

async fn reconcile_stale_turns_matching_with_warrant_pg<L, I>(
    pool: &PgPool,
    session_key: Option<&str>,
    liveness_probe: L,
    inflight_probe: I,
    automatic_warrant: Option<(Option<&HealthRegistry>, AxisBSite)>,
) -> Result<ApplySummary>
where
    L: Fn(&str, &str) -> IndependentLiveness + Clone + Send + 'static,
    I: Fn(&str, &str) -> InflightObservation + Clone + Send + 'static,
{
    let candidates = load_stale_turn_candidates_pg(pool, session_key).await?;
    let mut summary = ApplySummary::default();

    for candidate in candidates {
        // Read the inflight authority FIRST, before the tmux gate, so the
        // protected span starts as early as this loop can make it. Anything
        // later — after the tmux probe, or after the warrant await — would let
        // a turn that starts during the skipped step be captured as the
        // baseline and then compare equal to itself at apply time, which is the
        // exact shape this slice exists to refuse. The cost of reading it for
        // candidates that go on to fail the tmux gate is accepted: a wider
        // protected span is worth more than a saved probe.
        //
        // It is a baseline, never a gate. The stale-heartbeat qualification
        // does not require inflight absence, so gating on it here would let a
        // pre-existing orphan record deny this row forever.
        let baseline_inflight = probe_inflight(&candidate, &inflight_probe).await;

        let baseline_tmux = probe_tmux(&candidate, &liveness_probe).await;
        if !tmux_is_terminal(baseline_tmux) {
            tracing::info!(
                target: "reconcile",
                session_key = %candidate.session_key,
                liveness = ?baseline_tmux,
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

        // The correction this slice lands: this shared path used to read no
        // inflight record at all, on any of the three entry points that reach
        // it (both automatic sweeps and the operator's first branch).
        if !external_authority_permits_apply(
            &candidate,
            &liveness_probe,
            &inflight_probe,
            &baseline_inflight,
        )
        .await
        {
            // Surface this the same way a failed DB CAS is surfaced. Both mean
            // "a precondition this decision was built on is no longer true, so
            // retry against a fresh snapshot", and the operator has no way to
            // act on the difference. Reporting it as a plain no-op instead
            // would tell a caller its session simply did not qualify, which is
            // a different and wrong instruction.
            summary.precondition_changed = true;
            continue;
        }

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

    /// Upper bound for any single wait in this module. Generous enough that a
    /// loaded PostgreSQL fixture never trips it, short enough that a wedged
    /// witness releases the shared build token in minutes rather than an hour.
    const WITNESS_DEADLINE: Duration = Duration::from_secs(90);

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
            crate::services::discord::health::StaleSweepWarrantFixture::transport_unknown(
                5_464_101,
            )
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
        let automatic_warrant = Some((Some(fixture.registry()), AxisBSite::BootReconcileSweep));
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
        let _root_guard =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let root = tmp.path().join("runtime").join("discord_inflight");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(ProviderKind::Claude.as_str()), b"not a directory").unwrap();

        let tmux_name = ProviderKind::Claude.build_tmux_session_name("5464999");
        let session_key = format!(
            "{}:{tmux_name}",
            crate::services::platform::hostname_short()
        );
        assert_eq!(
            channel_inflight_observation(&session_key, "claude"),
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
                channel_inflight_observation,
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
                |_, _| InflightObservation::Absent,
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
                |_, _| episode("episode-live", 7, Some("nonce-live")),
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
                |_, _| InflightObservation::Absent,
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
                |_, _| InflightObservation::Unknown,
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
                |_, _| InflightObservation::Absent,
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

    /// One `Present` observation with an explicit episode identity.
    fn episode(
        started_at: &str,
        user_msg_id: u64,
        turn_nonce: Option<&str>,
    ) -> InflightObservation {
        episode_at_offset(started_at, user_msg_id, turn_nonce, None)
    }

    fn episode_at_offset(
        started_at: &str,
        user_msg_id: u64,
        turn_nonce: Option<&str>,
        turn_start_offset: Option<u64>,
    ) -> InflightObservation {
        InflightObservation::Present(
            crate::services::discord::zombie_foreground_release::InflightEpisodeIdentity {
                started_at: started_at.to_string(),
                user_msg_id,
                turn_nonce: turn_nonce.map(str::to_string),
                turn_start_offset,
            },
        )
    }

    /// A probe whose reading changes after the candidate decision: call `0` is
    /// the candidate baseline, every later call is the apply-time re-read.
    fn transitioning<T: Clone + Send + 'static>(
        baseline: T,
        after: T,
    ) -> impl Fn(&str, &str) -> T + Clone + Send + 'static {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        move |_, _| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                baseline.clone()
            } else {
                after.clone()
            }
        }
    }

    /// Every witness in this module runs under a hard deadline. A probe, a pool
    /// wait, or a join that never returns must FAIL the test rather than park
    /// the lane; expiry that resolved to green would disarm the witness.
    async fn within_deadline<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(WITNESS_DEADLINE, future)
            .await
            .expect("stale-turn witness exceeded its hard deadline")
    }

    /// Rendezvous at the apply barrier under the same deadline. A barrier that
    /// is never reached means the production path bailed out before the seam —
    /// a mis-built fixture, not a passing contract — and that must surface as a
    /// failure within `WITNESS_DEADLINE` instead of blocking forever.
    async fn rendezvous(barrier: &std::sync::Arc<StaleSweepApplyBarrierPoint>, phase: &str) {
        tokio::time::timeout(WITNESS_DEADLINE, barrier.wait())
            .await
            .unwrap_or_else(|_| panic!("stale-sweep apply barrier was never reached at {phase}"));
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
                |_, _| InflightObservation::Unknown,
            )
            .await
            .unwrap()
        });
        rendezvous(&reached, "candidate load").await;
        sqlx::query("UPDATE sessions SET last_heartbeat = NOW() - INTERVAL '10 minutes' WHERE session_key = $1")
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        rendezvous(&resume, "apply resume").await;
        assert!(matches!(
            within_deadline(task).await.unwrap(),
            SessionReconcileOutcome::PreconditionChanged(_)
        ));
        assert_eq!(load_state(&pool, key).await.0, "turn_active");
        drop(guard);

        assert_eq!(
            reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                key,
                |_, _| IndependentLiveness::NoPane,
                |_, _| InflightObservation::Unknown,
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
                |_, _| InflightObservation::Absent,
            )
            .await
            .unwrap()
        });
        rendezvous(&reached, "candidate load").await;
        sqlx::query("UPDATE sessions SET last_heartbeat = NOW() WHERE session_key = $1")
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        rendezvous(&resume, "apply resume").await;

        assert!(matches!(
            within_deadline(task).await.unwrap(),
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
                |_, _| InflightObservation::Unknown,
                None,
            )
            .await
            .unwrap()
        });
        rendezvous(&reached, "candidate load").await;
        sqlx::query("UPDATE sessions SET provider = 'codex' WHERE session_key = $1")
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        rendezvous(&resume, "apply resume").await;
        assert!(within_deadline(task).await.unwrap().precondition_changed);
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
                |_, _| InflightObservation::Unknown,
            )
            .await
            .unwrap()
        });
        rendezvous(&reached, "candidate load").await;
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
        rendezvous(&resume, "apply resume").await;

        assert!(matches!(
            within_deadline(task).await.unwrap(),
            SessionReconcileOutcome::PreconditionChanged(_)
        ));
        assert_eq!(load_state(&pool, key).await.0, "turn_active");
        pool.close().await;
        pg_db.drop().await;
    }

    /// I-5a — the destructive UPDATE is preceded by a tmux re-probe on the
    /// shared stale-heartbeat path, so a pane that comes back to life after the
    /// candidate decision holds the tick. The counter pins that the probe is
    /// actually called twice; a single reading cannot satisfy both assertions.
    #[tokio::test]
    async fn apply_time_tmux_reprobe_holds_a_pane_that_came_back_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let held = "remote-host:tmux-reprobe-held";
        let idle_path = "remote-host:tmux-reprobe-idle-path";
        seed_session(
            &pool,
            held,
            "turn_active",
            None,
            STALE_TURN_GRACE.as_secs() as i64 + 60,
        )
        .await;
        // The operator's IdleWithoutInflight branch takes a fresh heartbeat.
        seed_session(&pool, idle_path, "turn_active", None, 30).await;

        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(held),
                transitioning(
                    IndependentLiveness::NoPane,
                    IndependentLiveness::LiveOrAmbiguous,
                ),
                |_, _| InflightObservation::Unknown,
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            0,
            "a pane that revived between the candidate decision and the update must hold the tick"
        );
        assert_eq!(load_state(&pool, held).await.0, "turn_active");

        // Same rule on the operator's second branch, reported as retryable.
        assert!(matches!(
            within_deadline(reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                idle_path,
                transitioning(
                    IndependentLiveness::ReadyForInput,
                    IndependentLiveness::LiveOrAmbiguous,
                ),
                |_, _| InflightObservation::Absent,
            ))
            .await
            .unwrap(),
            SessionReconcileOutcome::PreconditionChanged(_)
        ));
        assert_eq!(load_state(&pool, idle_path).await.0, "turn_active");

        pool.close().await;
        pg_db.drop().await;
    }

    /// I-5b — the shared stale-heartbeat path re-reads the inflight record too.
    /// It never did before, and that gap is exactly what let a turn that started
    /// after the candidate decision be idled.
    ///
    /// Both directions are asserted together on purpose. A transition
    /// (`Absent -> Present`) holds the tick, but a presence that was already
    /// there at the candidate decision and never moved still reconciles: the
    /// stale-heartbeat qualification never required inflight absence, so an
    /// absolute `Absent` requirement at apply time would turn one leftover
    /// orphan record into a permanent denial for that row.
    #[tokio::test]
    async fn apply_time_inflight_reprobe_holds_a_new_turn_without_denying_a_stable_orphan_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let arrived = "remote-host:inflight-arrived";
        let orphan = "remote-host:inflight-stable-orphan";
        let idle_path = "remote-host:inflight-idle-path";
        for key in [arrived, orphan] {
            seed_session(
                &pool,
                key,
                "turn_active",
                None,
                STALE_TURN_GRACE.as_secs() as i64 + 60,
            )
            .await;
        }
        seed_session(&pool, idle_path, "turn_active", None, 30).await;

        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(arrived),
                |_, _| IndependentLiveness::NoPane,
                transitioning(
                    InflightObservation::Absent,
                    episode("episode-arrived", 21, Some("nonce-arrived")),
                ),
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            0,
            "a turn that started after the candidate decision must hold the tick"
        );
        assert_eq!(load_state(&pool, arrived).await.0, "turn_active");

        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(orphan),
                |_, _| IndependentLiveness::NoPane,
                |_, _| episode("episode-orphan", 22, Some("nonce-orphan")),
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            1,
            "an inflight record that never moved is not a new permanent denial"
        );
        assert_eq!(load_state(&pool, orphan).await.0, "idle");

        // The operator's second branch shares the rule; there the candidate
        // qualification is already `Absent`, so it reduces to "still absent".
        assert!(matches!(
            within_deadline(reconcile_stale_turn_by_key_with_probes_pg(
                &pool,
                idle_path,
                |_, _| IndependentLiveness::ReadyForInput,
                transitioning(
                    InflightObservation::Absent,
                    episode("episode-idle-path", 23, None),
                ),
            ))
            .await
            .unwrap(),
            // An external hold is reported exactly like a failed DB CAS: the
            // operator is told to retry against a fresh snapshot, not that the
            // session failed to qualify.
            SessionReconcileOutcome::PreconditionChanged(_)
        ));
        assert_eq!(load_state(&pool, idle_path).await.0, "turn_active");

        pool.close().await;
        pg_db.drop().await;
    }

    /// I-10 — the inflight baseline is captured before the tmux gate, so the
    /// span this slice protects starts as early as the candidate loop can make
    /// it. A turn that starts while the tmux pane is being probed must still
    /// land on the `Absent -> Present` side of the comparison rather than being
    /// absorbed as the baseline.
    #[tokio::test]
    async fn inflight_baseline_is_captured_before_the_tmux_gate_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let key = "remote-host:probe-order";
        seed_session(
            &pool,
            key,
            "turn_active",
            None,
            STALE_TURN_GRACE.as_secs() as i64 + 60,
        )
        .await;

        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let tmux_order = order.clone();
        let inflight_order = order.clone();
        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(key),
                move |_, _| {
                    tmux_order.lock().unwrap().push("tmux");
                    IndependentLiveness::NoPane
                },
                move |_, _| {
                    inflight_order.lock().unwrap().push("inflight");
                    InflightObservation::Unknown
                },
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            1
        );
        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["inflight", "tmux", "tmux", "inflight"],
            "the inflight baseline must precede the tmux gate, or a turn that starts \
             during the tmux probe is captured as the baseline and compares equal to itself"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// I-9 (ERRATUM E1) — presence alone is not the re-probe operand.
    /// `Present(A) -> Present(B)` is a transition even though both readings are
    /// "present", because the authority behind them is not the same episode.
    ///
    /// The hold is not a denial: the same row reconciles on the next tick, whose
    /// candidate decision re-captures `Present(B)` as its own baseline. And a
    /// re-write of the SAME episode is not a transition, which is why identity
    /// is compared on stable fields rather than on mtime or size.
    #[tokio::test]
    async fn inflight_identity_swap_holds_one_tick_then_self_resolves_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let swapped = "remote-host:identity-swap";
        let rewritten = "remote-host:identity-rewritten";
        for key in [swapped, rewritten] {
            seed_session(
                &pool,
                key,
                "turn_active",
                None,
                STALE_TURN_GRACE.as_secs() as i64 + 60,
            )
            .await;
        }

        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(swapped),
                |_, _| IndependentLiveness::NoPane,
                transitioning(
                    episode("episode-a", 31, Some("nonce-a")),
                    episode("episode-b", 32, Some("nonce-b")),
                ),
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            0,
            "a live turn that replaced an orphan record must not collapse into no transition"
        );
        assert_eq!(load_state(&pool, swapped).await.0, "turn_active");

        // Self-resolution: nothing recorded the hold, so the next tick takes
        // `Present(B)` as its baseline and applies once B stops moving.
        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(swapped),
                |_, _| IndependentLiveness::NoPane,
                |_, _| episode("episode-b", 32, Some("nonce-b")),
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            1,
            "the held tick must not latch: the next tick re-decides from a fresh baseline"
        );
        assert_eq!(load_state(&pool, swapped).await.0, "idle");

        // The counter-direction that rules out an mtime/size identity: the same
        // episode persisted again, with only unobserved progress advancing.
        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(rewritten),
                |_, _| IndependentLiveness::NoPane,
                |_, _| episode("episode-same", 33, Some("nonce-same")),
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            1,
            "re-persisting the same episode is not a transition"
        );
        assert_eq!(load_state(&pool, rewritten).await.0, "idle");

        pool.close().await;
        pg_db.drop().await;
    }

    /// I-11 — `turn_start_offset` is the axis that separates two turns the
    /// required axes cannot.
    ///
    /// `started_at` has one-second resolution and TUI-direct turns carry
    /// `user_msg_id == 0`, so two consecutive turns collide on both required
    /// axes; a legacy row with no `turn_nonce` leaves that auxiliary axis
    /// abstaining too. The canonical `inflight::InflightTurnIdentity` already
    /// carries the offset for exactly this collision.
    #[tokio::test]
    async fn same_second_zero_id_turns_are_separated_by_the_start_offset_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let collided = "remote-host:offset-collision";
        let abstains = "remote-host:offset-abstains";
        for key in [collided, abstains] {
            seed_session(
                &pool,
                key,
                "turn_active",
                None,
                STALE_TURN_GRACE.as_secs() as i64 + 60,
            )
            .await;
        }
        let same_second = "2026-08-24 12:00:00";

        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(collided),
                |_, _| IndependentLiveness::NoPane,
                transitioning(
                    // The orphan: legacy row, no nonce to break the tie.
                    episode_at_offset(same_second, 0, None, Some(4096)),
                    // The live turn that replaced it: same second, same zero id.
                    episode_at_offset(same_second, 0, Some("nonce-new"), Some(8192)),
                ),
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            0,
            "two turns that collide on started_at and user_msg_id must still be told apart"
        );
        assert_eq!(load_state(&pool, collided).await.0, "turn_active");

        // The counter-direction that keeps the axis auxiliary rather than
        // required: production restamps the offset on a row already on disk, so
        // an observation that carries no offset must not fabricate a change.
        assert_eq!(
            within_deadline(reconcile_stale_turns_matching_with_warrant_pg(
                &pool,
                Some(abstains),
                |_, _| IndependentLiveness::NoPane,
                transitioning(
                    episode_at_offset(same_second, 41, Some("nonce-same"), None),
                    episode_at_offset(same_second, 41, Some("nonce-same"), Some(512)),
                ),
                None,
            ))
            .await
            .unwrap()
            .reconciled,
            1,
            "an axis only one observation carries must abstain, not invent a transition"
        );
        assert_eq!(load_state(&pool, abstains).await.0, "idle");

        pool.close().await;
        pg_db.drop().await;
    }
}
