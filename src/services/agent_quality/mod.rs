//! Agent quality rule engine and Discord alerting (#1104 / 911-4).
//!
//! This module owns the regression-alert pipeline that watches the
//! `agent_quality_daily` rollup (#1101 / 909-4) for week-over-month drops
//! and raises Discord notifications when a sustained regression is observed.
//!
//! The pipeline is intentionally isolated from the legacy regression path
//! that lives inside `services::observability` (#1101): that path uses
//! `kv_meta`-based dedupe with a 24h horizon and a 15%/20%p split between
//! turn / review thresholds. #1104 promotes the rule engine to its own
//! crate-internal module with:
//!
//!   * a uniform 20%p delta threshold for both metrics
//!   * an explicit `sample_size >= 5` guard
//!   * a dedicated `quality_regression_cooldowns` table for the 24h window
//!   * an `ADK_QUALITY_ALERT_CHANNEL` env override (with adk-cc fallback)
//!   * drill-down link in the rendered Discord payload.
//!
//! Wired into the hourly maintenance scheduler in
//! `src/server/maintenance.rs` as job `quality_regression_alerter`,
//! sequenced after `agent_quality_rollup` (#1101) so the rule engine runs
//! against fresh aggregates.

pub mod regression_alerts;
