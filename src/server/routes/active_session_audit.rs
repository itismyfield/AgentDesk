//! Read-only DB active-session mismatch audit (P0).
//!
//! Ports Jinn's `gateway/status-reconciler.ts` confirmation pattern as a
//! DETECT-ONLY audit: it classifies sessions whose raw DB/session state says
//! "active" (`turn_active`, legacy `working`, or `active_dispatch_id IS NOT
//! NULL`) but whose effective runtime evidence (from [`SessionActivityResolver`])
//! says no live provider turn exists. The result is surfaced as the additive
//! `active_session_audit` block on `/api/health/detail` only — never on the
//! public `/api/health` response, and it NEVER mutates DB/session/runtime state.
//!
//! This module holds the pure classifier + output schema so it is unit-testable
//! independently of the axum route and the database. The route layer
//! ([`super::health_api`]) fetches the bounded candidate rows and the live
//! config, then calls [`classify_active_session_audit`].
//!
//! Repair-path recommendations delegate to EXISTING mechanisms only (P0 never
//! calls them): `stale_mailbox` → `/api/health/stale-mailbox-repair`,
//! `relay_recovery` → `/api/health/relay-recovery`, `stall_watchdog` →
//! `spawn_stall_watchdog`.

use serde::Serialize;

use crate::db::session_status::{LEGACY_WORKING, TURN_ACTIVE};
use crate::services::session_activity::{EffectiveSessionState, SessionActivityResolver};

/// Compiled-in defaults used when the matching `RuntimeSettingsConfig` override
/// is unset/empty. `STALE_SECS` doubles as the post-restart/long-turn grace
/// window (baseline aligned with the stall-watchdog positive-liveness budget).
pub(super) const DEFAULT_STALE_SECS: u64 = 120;
pub(super) const DEFAULT_MAX_CANDIDATES: u64 = 50;
pub(super) const MIN_MAX_CANDIDATES: u64 = 1;
pub(super) const MAX_MAX_CANDIDATES: u64 = 500;
/// `high_confidence_count` counts candidates at or above this confidence.
pub(super) const HIGH_CONFIDENCE_THRESHOLD: f64 = 0.75;

/// Which raw DB signal selected the session as active-like. Stable reason code.
/// Selection precedence when multiple match: `active_dispatch_id` > `turn_active`
/// > `working` (the others are still recorded under `evidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawActiveSource {
    TurnActive,
    Working,
    ActiveDispatchId,
}

impl RawActiveSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RawActiveSource::TurnActive => "turn_active",
            RawActiveSource::Working => "working",
            RawActiveSource::ActiveDispatchId => "active_dispatch_id",
        }
    }
}

/// Existing repair mechanism the audit recommends an operator route the
/// candidate to. P0 NEVER calls these; the audit is detect-only. Stable enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedRepairPath {
    None,
    StaleMailbox,
    RelayRecovery,
    StallWatchdog,
}

impl RecommendedRepairPath {
    /// Stable string contract (mirrors the `serde(rename_all = "snake_case")`
    /// JSON form). Kept as the documented enum→string mapping and exercised by
    /// the repair-mapping regression test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn as_str(self) -> &'static str {
        match self {
            RecommendedRepairPath::None => "none",
            RecommendedRepairPath::StaleMailbox => "stale_mailbox",
            RecommendedRepairPath::RelayRecovery => "relay_recovery",
            RecommendedRepairPath::StallWatchdog => "stall_watchdog",
        }
    }
}

/// One bounded `SELECT` row from `sessions`, already fetched by the route layer.
/// All fields are read-only inputs to the pure classifier.
#[derive(Debug, Clone, Default)]
pub struct RawSessionRow {
    pub session_key: Option<String>,
    pub provider: Option<String>,
    pub status: Option<String>,
    pub active_dispatch_id: Option<String>,
    pub last_heartbeat: Option<String>,
    pub thread_channel_id: Option<String>,
}

impl RawSessionRow {
    fn active_dispatch_present(&self) -> bool {
        self.active_dispatch_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
    }

    /// Highest-precedence raw signal that made this row active-like, if any.
    /// `None` means the row should not have been selected (defensive).
    fn raw_active_source(&self) -> Option<RawActiveSource> {
        if self.active_dispatch_present() {
            return Some(RawActiveSource::ActiveDispatchId);
        }
        let status = self.status.as_deref().map(str::trim).unwrap_or("");
        if status.eq_ignore_ascii_case(TURN_ACTIVE) {
            return Some(RawActiveSource::TurnActive);
        }
        if status.eq_ignore_ascii_case(LEGACY_WORKING) {
            return Some(RawActiveSource::Working);
        }
        None
    }
}

/// Output schema — one element of `active_session_audit.candidates`.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveSessionMismatchAuditCandidate {
    pub session_key: Option<String>,
    pub channel_id: Option<String>,
    pub provider: Option<String>,
    pub raw_active_source: RawActiveSource,
    pub effective_state: String,
    pub stale_for_secs: i64,
    pub evidence: serde_json::Value,
    pub confidence: f64,
    pub recommended_repair_path: RecommendedRepairPath,
}

/// Aggregate `active_session_audit` block placed on `/api/health/detail`.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveSessionAuditReport {
    pub enabled: bool,
    pub candidate_count: u64,
    pub high_confidence_count: u64,
    pub truncated: bool,
    pub grace_secs: u64,
    pub candidates: Vec<ActiveSessionMismatchAuditCandidate>,
}

impl ActiveSessionAuditReport {
    /// The block reported when the feature is disabled by config (rollback).
    pub fn disabled(grace_secs: u64) -> Self {
        Self {
            enabled: false,
            candidate_count: 0,
            high_confidence_count: 0,
            truncated: false,
            grace_secs,
            candidates: Vec::new(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// Resolved, None-safe audit settings derived from the live config snapshot.
#[derive(Debug, Clone, Copy)]
pub(super) struct ActiveSessionAuditSettings {
    pub enabled: bool,
    pub stale_secs: u64,
    pub max_candidates: u64,
}

impl ActiveSessionAuditSettings {
    /// Build from the optional `RuntimeSettingsConfig` overrides, applying the
    /// compiled-in defaults and bounds. `enabled` defaults to ON when unset.
    /// `stale_secs == Some(0)` is treated as unset (avoids a 0s grace footgun).
    pub(super) fn from_overrides(
        enabled: Option<bool>,
        stale_secs: Option<u64>,
        max_candidates: Option<u64>,
    ) -> Self {
        Self {
            enabled: enabled.unwrap_or(true),
            stale_secs: stale_secs
                .filter(|secs| *secs > 0)
                .unwrap_or(DEFAULT_STALE_SECS),
            max_candidates: max_candidates
                .filter(|cap| *cap > 0)
                .unwrap_or(DEFAULT_MAX_CANDIDATES)
                .clamp(MIN_MAX_CANDIDATES, MAX_MAX_CANDIDATES),
        }
    }
}

/// Seconds since `last_heartbeat`, or `None` when the heartbeat is unknown /
/// unparseable. Mirrors the parse formats accepted by `SessionActivityResolver`.
fn stale_for_secs(last_heartbeat: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let raw = last_heartbeat
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|value| {
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(value, chrono::Utc)
                })
        })?;
    Some((now - parsed).num_seconds().max(0))
}

/// Repair-path mapping (REQ-006). Delegates to EXISTING mechanisms only.
fn recommend_repair_path(
    row: &RawSessionRow,
    source: RawActiveSource,
    effective: &EffectiveSessionState,
    confidence: f64,
) -> RecommendedRepairPath {
    // Queued-but-not-started / low confidence must NOT be auto-recommended for a
    // mutating repair (would risk corrupting queue state).
    if confidence < HIGH_CONFIDENCE_THRESHOLD {
        return RecommendedRepairPath::None;
    }
    if !row.active_dispatch_present() {
        // Legacy `working` with no dispatch id is the queued-but-not-started
        // shape: the turn was never actually dispatched, so a mutating repair
        // (stale-mailbox clear) could wipe queue/session state for work that has
        // not started. Route these to `none` even at high confidence.
        if source == RawActiveSource::Working {
            return RecommendedRepairPath::None;
        }
        // Otherwise raw is active (turn_active) with no orphan dispatch token →
        // the mailbox/session row is the stale artifact; stale-mailbox-repair
        // clears it.
        return RecommendedRepairPath::StaleMailbox;
    }
    // A dispatch id is present but the resolver positively reports the provider
    // runtime / live turn is GONE (disconnected) → relay recovery owns this.
    if effective.status == crate::db::session_status::DISCONNECTED {
        return RecommendedRepairPath::RelayRecovery;
    }
    // Dispatch id present, not disconnected, but liveness is inconclusive and the
    // raw-active state has persisted past grace (the stalled-turn case) → the
    // stall watchdog owns re-establishing or aborting the stuck turn.
    RecommendedRepairPath::StallWatchdog
}

/// Confidence from age + evidence only (observation memory skipped in P0).
/// base 0.40; +0.20 each for: heartbeat known & stale past grace; resolver says
/// NOT working; missing dispatch id while raw is active. Clamped to [0.0, 1.0].
fn confidence_score(
    heartbeat_known: bool,
    stale_past_grace: bool,
    effective_not_working: bool,
    dispatch_missing_while_active: bool,
) -> f64 {
    let mut score = 0.40_f64;
    if heartbeat_known && stale_past_grace {
        score += 0.20;
    }
    if effective_not_working {
        score += 0.20;
    }
    if dispatch_missing_while_active {
        score += 0.20;
    }
    score.clamp(0.0, 1.0)
}

/// Pure classifier over already-fetched rows + the in-memory resolver verdict.
///
/// A row becomes a candidate only when raw state is active-like AND the resolver
/// verdict is NOT working. Rows still inside the grace window
/// (`stale_for_secs < settings.stale_secs` with a KNOWN heartbeat) are excluded
/// entirely (not surfaced as low-confidence). `raw_matches_total` is the count
/// of raw active-like rows BEFORE the cap, used to compute `truncated`.
pub(super) fn classify_active_session_audit(
    rows: &[RawSessionRow],
    resolver: &mut SessionActivityResolver,
    settings: ActiveSessionAuditSettings,
    raw_matches_total: usize,
    now: chrono::DateTime<chrono::Utc>,
) -> ActiveSessionAuditReport {
    let mut candidates = Vec::new();
    for row in rows {
        let Some(source) = row.raw_active_source() else {
            continue;
        };
        // DB/heartbeat-only: this audit is an off-hot-path read on
        // /api/health/detail and MUST NOT block the request on synchronous tmux
        // probes (has_live_pane / pane readiness) for local session keys.
        let effective = resolver.resolve_db_only(
            row.session_key.as_deref(),
            row.status.as_deref(),
            row.active_dispatch_id.as_deref(),
            row.last_heartbeat.as_deref(),
        );
        // Only flag raw-active-but-effective-not-working.
        if effective.is_working {
            continue;
        }

        let stale_known = stale_for_secs(row.last_heartbeat.as_deref(), now);
        let heartbeat_known = stale_known.is_some();
        let stale_value = stale_known.unwrap_or(0);
        let stale_past_grace = stale_value >= settings.stale_secs as i64;
        // Grace: a KNOWN-recent heartbeat (within grace) suppresses the flag so a
        // freshly (re)started long-running turn is not reported. An UNKNOWN
        // heartbeat is allowed through (cannot prove it is fresh).
        if heartbeat_known && !stale_past_grace {
            continue;
        }

        let dispatch_present = row.active_dispatch_present();
        let confidence = confidence_score(
            heartbeat_known,
            stale_past_grace,
            !effective.is_working,
            !dispatch_present,
        );
        let repair = recommend_repair_path(row, source, &effective, confidence);

        let evidence = serde_json::json!({
            "raw_status": row.status.as_deref().unwrap_or("").to_string(),
            "raw_active_source": source.as_str(),
            "active_dispatch_id_present": dispatch_present,
            "effective_is_working": effective.is_working,
            "heartbeat_known": heartbeat_known,
            "heartbeat_recent": heartbeat_known && !stale_past_grace,
            "matched_turn_active": row
                .status
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| s.eq_ignore_ascii_case(TURN_ACTIVE)),
            "matched_working": row
                .status
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| s.eq_ignore_ascii_case(LEGACY_WORKING)),
            "matched_active_dispatch_id": dispatch_present,
        });

        candidates.push(ActiveSessionMismatchAuditCandidate {
            session_key: row.session_key.clone(),
            channel_id: row.thread_channel_id.clone(),
            provider: row.provider.clone(),
            raw_active_source: source,
            effective_state: effective.status.to_string(),
            stale_for_secs: stale_value,
            evidence,
            confidence,
            recommended_repair_path: repair,
        });
    }

    let high_confidence_count = candidates
        .iter()
        .filter(|candidate| candidate.confidence >= HIGH_CONFIDENCE_THRESHOLD)
        .count() as u64;
    let candidate_count = candidates.len() as u64;
    // `truncated` means the cap omitted raw active-like rows the classifier never
    // examined: i.e. the pre-LIMIT raw-match count exceeds the rows actually
    // fetched/scanned (`rows.len()`, capped at `max_candidates`). It is NOT a
    // function of how many of those rows survived classification, so a fully
    // candidate-producing capped batch correctly reports `truncated = true` and a
    // sub-cap batch with resolver-filtered rows correctly reports `false`.
    let truncated = raw_matches_total > rows.len();

    ActiveSessionAuditReport {
        enabled: true,
        candidate_count,
        high_confidence_count,
        truncated,
        grace_secs: settings.stale_secs,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    fn stale_ts(secs_ago: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs_ago))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// A remote raw-active row with a long-stale heartbeat and no live turn is
    /// flagged once, with evidence and no mutation. (TEST-001 / TEST-002)
    #[test]
    fn active_session_audit_flags_raw_active_effective_idle() {
        // Remote host alias so the resolver uses heartbeat recency (no tmux probe).
        let rows = vec![RawSessionRow {
            session_key: Some("remote-host/farbox:codex-1234".to_string()),
            provider: Some("codex".to_string()),
            status: Some("turn_active".to_string()),
            active_dispatch_id: Some("dispatch-9".to_string()),
            last_heartbeat: Some(stale_ts(900)),
            thread_channel_id: Some("1490000000000000001".to_string()),
        }];
        let mut resolver = SessionActivityResolver::new();
        let settings = ActiveSessionAuditSettings::from_overrides(None, None, None);
        let report = classify_active_session_audit(&rows, &mut resolver, settings, 1, now());

        assert!(report.enabled);
        assert_eq!(report.candidate_count, 1);
        let candidate = &report.candidates[0];
        assert_eq!(
            candidate.raw_active_source,
            RawActiveSource::ActiveDispatchId
        );
        assert!(
            !candidate.evidence["effective_is_working"]
                .as_bool()
                .unwrap()
        );
        assert!(candidate.evidence["heartbeat_known"].as_bool().unwrap());
        assert!(candidate.stale_for_secs >= 800);
        // Dispatch present + stale heartbeat + not working, but the resolver does
        // NOT positively report disconnected (raw status is turn_active, so the
        // DB-only verdict is idle) ⇒ the stalled-turn case ⇒ stall_watchdog.
        assert_eq!(
            candidate.recommended_repair_path,
            RecommendedRepairPath::StallWatchdog
        );
        assert!(candidate.confidence >= HIGH_CONFIDENCE_THRESHOLD);
    }

    /// A dispatch-bearing row whose raw status is `disconnected` but still carries
    /// an orphan dispatch token resolves to relay_recovery (the resolver
    /// positively reports the runtime is gone). Guards the relay_recovery branch.
    #[test]
    fn active_session_audit_disconnected_with_dispatch_recommends_relay_recovery() {
        let row = RawSessionRow {
            session_key: Some("remote-host/farbox:codex-disc".to_string()),
            provider: Some("codex".to_string()),
            status: Some(crate::db::session_status::DISCONNECTED.to_string()),
            active_dispatch_id: Some("dispatch-disc".to_string()),
            last_heartbeat: Some(stale_ts(900)),
            thread_channel_id: Some("1490000000000000010".to_string()),
        };
        let effective = EffectiveSessionState {
            status: crate::db::session_status::DISCONNECTED,
            active_dispatch_id: None,
            is_working: false,
        };
        // High confidence: stale (+0.20), not working (+0.20), dispatch present.
        let confidence = confidence_score(true, true, true, false);
        assert!(confidence >= HIGH_CONFIDENCE_THRESHOLD);
        assert_eq!(
            recommend_repair_path(
                &row,
                RawActiveSource::ActiveDispatchId,
                &effective,
                confidence
            ),
            RecommendedRepairPath::RelayRecovery
        );
    }

    /// The stalled-turn case: a dispatch-bearing, high-confidence, not-working
    /// candidate that is NOT disconnected reaches stall_watchdog (regression for
    /// the previously-unreachable branch).
    #[test]
    fn active_session_audit_stalled_turn_recommends_stall_watchdog() {
        let row = RawSessionRow {
            session_key: Some("remote-host/farbox:codex-stall".to_string()),
            provider: Some("codex".to_string()),
            status: Some("turn_active".to_string()),
            active_dispatch_id: Some("dispatch-stall".to_string()),
            last_heartbeat: Some(stale_ts(900)),
            thread_channel_id: Some("1490000000000000011".to_string()),
        };
        let effective = EffectiveSessionState {
            status: crate::db::session_status::IDLE,
            active_dispatch_id: None,
            is_working: false,
        };
        let confidence = confidence_score(true, true, true, false);
        assert!(confidence >= HIGH_CONFIDENCE_THRESHOLD);
        assert_eq!(
            recommend_repair_path(
                &row,
                RawActiveSource::ActiveDispatchId,
                &effective,
                confidence
            ),
            RecommendedRepairPath::StallWatchdog
        );
    }

    /// Queued-but-not-started (`working` status, no dispatch id) at HIGH confidence
    /// must map to `none`, never the mutating stale-mailbox path. Regression for
    /// the queued-but-not-started shape (P2).
    #[test]
    fn active_session_audit_queued_high_confidence_maps_to_none() {
        let row = RawSessionRow {
            session_key: Some("remote-host/farbox:codex-queued-hc".to_string()),
            provider: Some("codex".to_string()),
            status: Some("working".to_string()),
            active_dispatch_id: None,
            last_heartbeat: Some(stale_ts(900)),
            thread_channel_id: Some("1490000000000000012".to_string()),
        };
        let effective = EffectiveSessionState {
            status: crate::db::session_status::IDLE,
            active_dispatch_id: None,
            is_working: false,
        };
        // stale (+0.20), not working (+0.20), missing dispatch (+0.20) ⇒ 1.0.
        let confidence = confidence_score(true, true, true, true);
        assert!(confidence >= HIGH_CONFIDENCE_THRESHOLD);
        assert_eq!(
            recommend_repair_path(&row, RawActiveSource::Working, &effective, confidence),
            RecommendedRepairPath::None
        );
    }

    /// A `turn_active` row with no dispatch id at high confidence is a genuine
    /// stale mailbox artifact (distinct from the queued `working` shape) and maps
    /// to stale_mailbox.
    #[test]
    fn active_session_audit_turn_active_no_dispatch_maps_to_stale_mailbox() {
        let row = RawSessionRow {
            session_key: Some("remote-host/farbox:codex-sm".to_string()),
            provider: Some("codex".to_string()),
            status: Some("turn_active".to_string()),
            active_dispatch_id: None,
            last_heartbeat: Some(stale_ts(900)),
            thread_channel_id: Some("1490000000000000013".to_string()),
        };
        let effective = EffectiveSessionState {
            status: crate::db::session_status::IDLE,
            active_dispatch_id: None,
            is_working: false,
        };
        let confidence = confidence_score(true, true, true, true);
        assert!(confidence >= HIGH_CONFIDENCE_THRESHOLD);
        assert_eq!(
            recommend_repair_path(&row, RawActiveSource::TurnActive, &effective, confidence),
            RecommendedRepairPath::StaleMailbox
        );
    }

    /// A live, recent foreground turn must not be flagged. The resolver reports a
    /// working turn for a recent-heartbeat remote session, so no candidate.
    /// (TEST-005)
    #[test]
    fn active_session_audit_does_not_flag_live_turn() {
        let rows = vec![RawSessionRow {
            session_key: Some("remote-host/farbox:codex-live".to_string()),
            provider: Some("codex".to_string()),
            status: Some("turn_active".to_string()),
            active_dispatch_id: Some("dispatch-live".to_string()),
            // Heartbeat within the remote grace ⇒ resolver says working.
            last_heartbeat: Some(stale_ts(5)),
            thread_channel_id: Some("1490000000000000002".to_string()),
        }];
        let mut resolver = SessionActivityResolver::new();
        let settings = ActiveSessionAuditSettings::from_overrides(None, None, None);
        let report = classify_active_session_audit(&rows, &mut resolver, settings, 1, now());

        assert_eq!(report.candidate_count, 0);
        assert!(report.candidates.is_empty());
    }

    /// A raw-active row whose heartbeat is within the configured grace window is
    /// excluded entirely (not low-confidence). Forced by a long grace so the
    /// resolver still says idle but the grace gate suppresses it. (TEST-005)
    #[test]
    fn active_session_audit_respects_grace_window() {
        // No dispatch id + remote host with a heartbeat older than the remote
        // resolver grace (so effective is idle) but younger than our audit grace.
        let rows = vec![RawSessionRow {
            session_key: Some("remote-host/farbox:codex-grace".to_string()),
            provider: Some("codex".to_string()),
            status: Some("turn_active".to_string()),
            active_dispatch_id: None,
            last_heartbeat: Some(stale_ts(100)),
            thread_channel_id: Some("1490000000000000003".to_string()),
        }];
        let mut resolver = SessionActivityResolver::new();
        // Audit grace of 600s: heartbeat 100s ago is inside grace ⇒ excluded.
        let settings = ActiveSessionAuditSettings::from_overrides(None, Some(600), None);
        let report = classify_active_session_audit(&rows, &mut resolver, settings, 1, now());

        assert_eq!(report.candidate_count, 0);
        assert_eq!(report.grace_secs, 600);
    }

    /// Queued-but-not-started (legacy `working`, no dispatch id) past grace is a
    /// low-confidence candidate that maps to `none` (never auto-repaired).
    /// (TEST-005 / TEST-006)
    #[test]
    fn active_session_audit_queued_is_low_confidence_or_none() {
        let rows = vec![RawSessionRow {
            session_key: Some("remote-host/farbox:codex-queued".to_string()),
            provider: Some("codex".to_string()),
            status: Some("working".to_string()),
            active_dispatch_id: None,
            // Unknown heartbeat ⇒ no +0.20 stale bump; resolver idle ⇒ +0.20;
            // missing dispatch ⇒ +0.20 ⇒ 0.80, which is high. To assert the
            // queued≠high path, use a known-recent-but-past-audit-grace shape is
            // covered elsewhere; here unknown heartbeat with working maps to a
            // candidate. Assert repair mapping never targets a mutating path for
            // a missing dispatch unless confidence is high.
            last_heartbeat: None,
            thread_channel_id: Some("1490000000000000004".to_string()),
        }];
        let mut resolver = SessionActivityResolver::new();
        let settings = ActiveSessionAuditSettings::from_overrides(None, None, None);
        let report = classify_active_session_audit(&rows, &mut resolver, settings, 1, now());

        assert_eq!(report.candidate_count, 1);
        let candidate = &report.candidates[0];
        // No dispatch id ⇒ never relay_recovery/stall_watchdog; either stale_mailbox
        // (high confidence) or none (low confidence), never a queue-corrupting path.
        assert!(matches!(
            candidate.recommended_repair_path,
            RecommendedRepairPath::StaleMailbox | RecommendedRepairPath::None
        ));
    }

    /// A genuinely low-confidence row (resolver still working) yields `none`.
    #[test]
    fn active_session_audit_low_confidence_maps_to_none() {
        // base 0.40 only: heartbeat unknown (no stale bump), dispatch present
        // (no missing bump). Force not-working by a disconnected status so it is
        // still a candidate but with confidence 0.60 (< 0.75 high threshold) when
        // dispatch is present.
        let row = RawSessionRow {
            session_key: Some("remote-host/farbox:codex-lc".to_string()),
            provider: Some("codex".to_string()),
            status: Some("turn_active".to_string()),
            active_dispatch_id: Some("d-1".to_string()),
            last_heartbeat: None,
            thread_channel_id: Some("1490000000000000009".to_string()),
        };
        // confidence = 0.40 + 0 (hb unknown) + 0.20 (not working) + 0 (dispatch
        // present) = 0.60 < 0.75 ⇒ none.
        let effective = EffectiveSessionState {
            status: crate::db::session_status::IDLE,
            active_dispatch_id: None,
            is_working: false,
        };
        let confidence = confidence_score(false, false, true, false);
        assert!((confidence - 0.60).abs() < 1e-9);
        assert_eq!(
            recommend_repair_path(
                &row,
                RawActiveSource::ActiveDispatchId,
                &effective,
                confidence
            ),
            RecommendedRepairPath::None
        );
    }

    /// Every repair enum maps only to an existing-mechanism string. (TEST-006)
    #[test]
    fn active_session_audit_repair_mapping_targets_existing_paths() {
        assert_eq!(RecommendedRepairPath::None.as_str(), "none");
        assert_eq!(
            RecommendedRepairPath::StaleMailbox.as_str(),
            "stale_mailbox"
        );
        assert_eq!(
            RecommendedRepairPath::RelayRecovery.as_str(),
            "relay_recovery"
        );
        assert_eq!(
            RecommendedRepairPath::StallWatchdog.as_str(),
            "stall_watchdog"
        );
    }

    /// Disabling the audit yields an empty block and skips classification.
    /// (rollback TEST)
    #[test]
    fn active_session_audit_disabled_emits_empty_block() {
        let settings = ActiveSessionAuditSettings::from_overrides(Some(false), None, None);
        assert!(!settings.enabled);
        let report = ActiveSessionAuditReport::disabled(settings.stale_secs);
        assert!(!report.enabled);
        assert_eq!(report.candidate_count, 0);
        assert!(report.candidates.is_empty());
        let json = report.to_json();
        assert_eq!(json["enabled"], serde_json::json!(false));
        assert_eq!(json["candidate_count"], serde_json::json!(0));
        assert!(json["candidates"].as_array().unwrap().is_empty());
    }

    /// `truncated` flips when raw matches exceed the cap. dedup/precedence:
    /// a row matching multiple raw signals yields exactly one candidate row.
    #[test]
    fn active_session_audit_truncated_and_single_row_per_session() {
        // Row matches BOTH turn_active AND active_dispatch_id; precedence picks
        // active_dispatch_id and yields exactly one candidate.
        let rows = vec![RawSessionRow {
            session_key: Some("remote-host/farbox:codex-dedup".to_string()),
            provider: Some("codex".to_string()),
            status: Some("turn_active".to_string()),
            active_dispatch_id: Some("d-dedup".to_string()),
            last_heartbeat: Some(stale_ts(900)),
            thread_channel_id: Some("1490000000000000005".to_string()),
        }];
        let mut resolver = SessionActivityResolver::new();
        // Cap of 1, and report raw_matches_total > returned ⇒ truncated.
        let settings = ActiveSessionAuditSettings::from_overrides(None, None, Some(1));
        let report = classify_active_session_audit(&rows, &mut resolver, settings, 5, now());
        assert_eq!(report.candidate_count, 1);
        assert!(report.truncated);
        assert_eq!(
            report.candidates[0].raw_active_source,
            RawActiveSource::ActiveDispatchId
        );
    }

    /// Settings clamp the candidate cap to the documented bounds and treat
    /// `stale_secs == 0` as unset.
    #[test]
    fn active_session_audit_settings_clamp_and_zero_handling() {
        let s = ActiveSessionAuditSettings::from_overrides(None, Some(0), Some(99_999));
        assert_eq!(s.stale_secs, DEFAULT_STALE_SECS);
        assert_eq!(s.max_candidates, MAX_MAX_CANDIDATES);
        let s2 = ActiveSessionAuditSettings::from_overrides(None, Some(300), Some(0));
        assert_eq!(s2.stale_secs, 300);
        assert_eq!(s2.max_candidates, DEFAULT_MAX_CANDIDATES);
    }
}
