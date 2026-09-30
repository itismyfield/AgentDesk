//! Host-aware facade for the timeouts policy's repairs and `session.sendCommand/kill`. A key
//! reaches tmux only when it finds a sessions row with no hosted record; anything else defers.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Acquire, PgPool, Row};

use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, load_hosted_execution_pg,
};
use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::session_host::{
    AutomaticEffect, GuardVerdict, HostKind, HostLiveness, HostSessionRef, HostWitness,
    ResolvedSessionTarget, SessionTargetEvidence, SessionTargetEvidenceSource, SessionTargetInput,
    StateChange, TargetHost, TmuxHost, UnknownHost, guard_first_state_change, probe_for_policy,
    resolve_session_target, session_record_witness,
};

/// Why a row is left alone: `herdr`, `session_missing`, `lookup_failed`, `row_conflict`,
/// `host_unknown`, `host_conflict`, `probe_failed` or `row_changed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::engine::ops) struct Deferral {
    pub reason: &'static str,
    pub detail: String,
}

fn defer(reason: &'static str, detail: impl Into<String>) -> Deferral {
    Deferral {
        reason,
        detail: detail.into(),
    }
}

/// A key resolved to a found legacy sessions row and its tmux target.
#[derive(Debug)]
pub(in crate::engine::ops) struct LegacyTarget {
    pub session_id: i64,
    pub target: ResolvedSessionTarget,
}

impl LegacyTarget {
    fn tmux_name(&self) -> Option<&str> {
        self.target.legacy_ref().and_then(|r| r.legacy_name().ok())
    }
}

struct RowEvidence(SessionTargetEvidence);

impl SessionTargetEvidenceSource for RowEvidence {
    fn read_evidence(&self, _input: &SessionTargetInput) -> SessionTargetEvidence {
        self.0.clone()
    }
}

/// Full key → one sessions row → host from the row's record and the `.host_kind` marker.
pub(in crate::engine::ops) async fn resolve_policy_target_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<LegacyTarget, Deferral> {
    let lookup = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(session_key)).await;
    policy_target(session_key, lookup)
}

fn policy_target(session_key: &str, lookup: HostedLookup) -> Result<LegacyTarget, Deferral> {
    let session_id = match &lookup {
        HostedLookup::Found(found) => found.session_id(),
        HostedLookup::Missing => return Err(defer("session_missing", session_key)),
        HostedLookup::Unknown(detail) => return Err(defer("lookup_failed", detail.clone())),
        HostedLookup::Conflict(kind) => return Err(defer("row_conflict", format!("{kind:?}"))),
    };
    // The policy's tmux name for the key; a malformed key keeps its last `:` segment.
    let session_name = tmux_name_from_session_key(session_key).or_else(|| {
        let last = session_key.rsplit(':').next()?;
        (!last.trim().is_empty()).then(|| last.to_string())
    });
    let evidence = SessionTargetEvidence {
        session_key: Some(session_key.to_string()),
        session_name,
        session_record: session_record_witness(&lookup),
        // A Herdr launch writes the sessions record before any inflight row names its pane.
        inflight_locator: HostWitness::Absent,
        ..SessionTargetEvidence::unread()
    }
    .with_host_marker();
    let input = SessionTargetInput::SessionKey(session_key.to_string());
    let target = resolve_session_target(input, &RowEvidence(evidence));
    match &target.host {
        TargetHost::Known {
            kind: HostKind::Tmux,
            ..
        } => Ok(LegacyTarget { session_id, target }),
        TargetHost::Known {
            kind: HostKind::Herdr,
            ..
        }
        | TargetHost::Unknown(UnknownHost::MissingTarget(HostKind::Herdr)) => {
            Err(defer("herdr", session_key))
        }
        TargetHost::Conflict { first, second } => {
            Err(defer("host_conflict", format!("{first:?} vs {second:?}")))
        }
        other => Err(defer("host_unknown", format!("{other:?}"))),
    }
}

fn resolve_blocking(pool: &PgPool, session_key: &str) -> Result<LegacyTarget, Deferral> {
    let key = session_key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move { Ok(resolve_policy_target_pg(&bridge_pool, &key).await) },
        |error| error,
    )
    .unwrap_or_else(|error| Err(defer("lookup_failed", error)))
}

/// The tmux name `session.<op>` may act on, or its refusal JSON: explicit Unsupported for
/// Herdr, refused for a missing row, unresolved host or conflict.
pub(in crate::engine::ops) fn session_command_target(
    pg_pool: Option<&PgPool>,
    session_key: &str,
    op: &str,
) -> Result<String, String> {
    let resolved = match pg_pool {
        None => Err(defer("lookup_failed", "postgres backend is required")),
        Some(pool) => resolve_blocking(pool, session_key.trim()),
    };
    let deferral = match resolved {
        Ok(legacy) => match legacy.tmux_name() {
            Some(name) => return Ok(name.to_string()),
            None => defer("host_unknown", "no tmux name"),
        },
        Err(deferral) => deferral,
    };
    let (reason, error) = match deferral.reason {
        "herdr" => (
            "herdr_unsupported",
            format!("session.{op} is not supported for herdr sessions"),
        ),
        reason => (reason, format!("session.{op} refused: {}", deferral.detail)),
    };
    Err(json!({ "ok": false, "refused": true, "reason": reason, "error": error }).to_string())
}

fn deferred(deferral: Deferral) -> Value {
    json!({ "state": "unknown", "reason": deferral.reason, "detail": deferral.detail })
}

pub(super) fn observe_session_host_raw(pg_pool: Option<&PgPool>, session_key: &str) -> String {
    observe_session_host_with(pg_pool, session_key, |session, budget| {
        TmuxHost.liveness_within(session, budget)
    })
}

/// `live`/`dead` only for a legacy row whose probe answered, with the same budget
/// `session.hasLivePane` uses; anything else is `unknown` with the deferral reason.
pub(super) fn observe_session_host_with(
    pg_pool: Option<&PgPool>,
    session_key: &str,
    probe: impl FnOnce(HostSessionRef<'_>, Duration) -> HostLiveness,
) -> String {
    let session_key = match super::valid_session_key(session_key) {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let Some(pool) = pg_pool else {
        return super::unavailable();
    };
    let legacy = match resolve_blocking(pool, &session_key) {
        Ok(legacy) => legacy,
        Err(deferral) => return deferred(deferral).to_string(),
    };
    let budget = crate::engine::loader::bridge_op_deadline_remaining()
        .unwrap_or(Duration::from_secs(2))
        .min(Duration::from_secs(2));
    let answer = probe_for_policy(&legacy.target, |session| match budget.is_zero() {
        true => HostLiveness::ProbeError,
        false => probe(session, budget),
    });
    match answer.js_state() {
        "unknown" => deferred(defer("probe_failed", session_key)).to_string(),
        state => json!({
            "state": state,
            "session_id": legacy.session_id,
            "tmux_name": legacy.tmux_name(),
        })
        .to_string(),
    }
}

/// One repair the policy decided from an `observeSessionHost` answer.
#[derive(Debug, Deserialize)]
pub(super) struct RepairRequest {
    session_id: i64,
    #[serde(default)]
    active_dispatch_id: Option<String>,
    /// The listed row's `active_turn_nonce` (`null` for none); required so no caller skips it.
    #[serde(deserialize_with = "Option::deserialize")]
    active_turn_nonce: Option<String>,
    /// The liveness the decision was made on: `live` or `dead`.
    observed: String,
    #[serde(default)]
    fail_dispatch: bool,
    #[serde(default)]
    fail_reason: String,
    #[serde(default)]
    clear_active_dispatch_id: bool,
}

pub(super) fn repair_stale_session_raw(
    pg_pool: Option<&PgPool>,
    session_key: &str,
    request_json: &str,
) -> String {
    let session_key = match super::valid_session_key(session_key) {
        Ok(value) => value,
        Err(error) => return json!({ "error": error }).to_string(),
    };
    let request: RepairRequest = match serde_json::from_str(request_json) {
        Ok(request) => request,
        Err(error) => {
            return json!({ "error": format!("invalid repair request: {error}") }).to_string();
        }
    };
    let Some(pool) = pg_pool else {
        return super::unavailable();
    };
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let result = repair_stale_session_pg(&bridge_pool, &session_key, request).await;
            result.map(|value| value.to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn not_repaired(deferral: Deferral) -> Value {
    json!({ "ok": true, "repaired": false, "deferred": deferral.reason, "detail": deferral.detail })
}

/// Re-resolves row and host, then in one transaction fails the observed dispatch, re-checks
/// the row, turn nonce and dispatch are unchanged and marks it idle; any change rolls it back.
pub(super) async fn repair_stale_session_pg(
    pool: &PgPool,
    session_key: &str,
    request: RepairRequest,
) -> Result<Value, String> {
    let legacy = match resolve_policy_target_pg(pool, session_key).await {
        Ok(legacy) if legacy.session_id == request.session_id => legacy,
        Ok(_) => return Ok(not_repaired(defer("row_changed", "session row id changed"))),
        Err(deferral) => return Ok(not_repaired(deferral)),
    };
    let observed = match request.observed.as_str() {
        "live" => HostLiveness::Live,
        "dead" => HostLiveness::DeadOrAbsent,
        _ => HostLiveness::ProbeError,
    };
    let effect = match request.fail_dispatch {
        true => AutomaticEffect::FailDispatch,
        false => AutomaticEffect::Clear,
    };
    let change = StateChange::Automatic {
        effect,
        observed: Some(observed),
    };
    match guard_first_state_change(&legacy.target, change) {
        GuardVerdict::Proceed => {}
        verdict => return Ok(not_repaired(defer("probe_failed", format!("{verdict:?}")))),
    }

    let db =
        |what: &'static str| move |error: sqlx::Error| format!("{what} {session_key}: {error}");
    let mut tx = pool.begin().await.map_err(db("begin repair"))?;
    let mut observability = Vec::new();
    let (mut dispatch_changed, mut dispatch_error) = (0, None);
    let fail_id = (request.active_dispatch_id.as_deref()).filter(|_| request.fail_dispatch);
    if let Some(dispatch_id) = fail_id {
        // A savepoint keeps a failed dispatch transition from aborting the idle repair.
        let mut savepoint = (&mut tx).begin().await.map_err(db("dispatch savepoint"))?;
        let reason = json!({ "reason": request.fail_reason });
        let failed = crate::dispatch::set_dispatch_status_on_pg_tx_async(
            &mut savepoint,
            dispatch_id,
            "failed",
            Some(&reason),
            "js_dispatch_mark_failed_raw",
            Some(&["pending", "dispatched"]),
            false,
            false,
            Some(&mut observability),
        )
        .await;
        match failed {
            Ok(changed) => {
                savepoint.commit().await.map_err(db("dispatch savepoint"))?;
                dispatch_changed = changed;
            }
            Err(error) => {
                savepoint
                    .rollback()
                    .await
                    .map_err(db("dispatch savepoint"))?;
                observability.clear();
                dispatch_error = Some(error.to_string());
            }
        }
    }

    // Locked after the dispatch transition, the order every dispatch writer takes. The
    // transition never writes the turn nonce, so a different one here is a new turn.
    let row = sqlx::query(
        "SELECT s.hosted_execution IS NULL AS legacy, s.active_dispatch_id,
                s.active_turn_nonce IS NOT DISTINCT FROM $3 AS same_turn
         FROM sessions s
         WHERE s.id = $1
           AND (s.session_key = $2 OR EXISTS (SELECT 1 FROM session_key_aliases a
                                               WHERE a.session_id = s.id AND a.session_key = $2))
         FOR UPDATE OF s",
    )
    .bind(legacy.session_id)
    .bind(session_key)
    .bind(request.active_turn_nonce.as_deref())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db("recheck session"))?;
    let changed = match row {
        None => Some("row or record changed"),
        Some(row) if !row.try_get::<bool, _>("legacy").unwrap_or(false) => {
            Some("row or record changed")
        }
        Some(row) if !row.try_get::<bool, _>("same_turn").unwrap_or(false) => {
            Some("turn nonce changed")
        }
        Some(row) => {
            let active: Option<String> = row.try_get("active_dispatch_id").ok().flatten();
            let cleared_by_fail = dispatch_changed > 0 && active.is_none();
            (active != request.active_dispatch_id && !cleared_by_fail).then_some("dispatch changed")
        }
    };
    if let Some(detail) = changed {
        tx.rollback().await.map_err(db("rollback repair"))?;
        return Ok(not_repaired(defer("row_changed", detail)));
    }
    let rows_affected = sqlx::query(
        "UPDATE sessions
         SET status = 'idle',
             active_dispatch_id = CASE WHEN $2 THEN NULL ELSE active_dispatch_id END,
             last_heartbeat = NOW()
         WHERE id = $1 AND status IN ('turn_active', 'working')",
    )
    .bind(legacy.session_id)
    .bind(request.clear_active_dispatch_id)
    .execute(&mut *tx)
    .await
    .map_err(db("mark session idle"))?
    .rows_affected();
    tx.commit().await.map_err(db("commit repair"))?;

    for event in observability {
        event.emit();
    }
    if let Some(dispatch_id) = fail_id.filter(|_| dispatch_changed > 0) {
        crate::services::dispatches::wait_queue::spawn_cached_constraint_release_wake(
            pool.clone(),
            "constraint_release",
            dispatch_id.to_string(),
            "dispatch_terminal_status",
        );
    }
    Ok(json!({
        "ok": true,
        "repaired": true,
        "rows_affected": rows_affected,
        "dispatch_rows_affected": dispatch_changed,
        "dispatch_error": dispatch_error,
    }))
}

#[cfg(test)]
#[path = "host_repair_tests.rs"]
mod tests;
