//! Agent quality rule engine and Discord alerting (#1104 / 911-4).
//!
//! This module owns the regression-alert pipeline that watches the
//! `agent_quality_daily` rollup (#1101 / 909-4) for week-over-month drops
//! and raises Discord notifications when a sustained regression is observed.
//!
//! [`regression_alerts`] is the sole regression-alert authority. The legacy
//! producer formerly coupled to `services::observability` rollup was removed
//! in #4448; the rollup now only materializes `agent_quality_daily`. #1104
//! promotes the rule engine to its own crate-internal module with:
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
//!
//! The coexistence note in immutable migration
//! `0020_quality_regression_cooldowns.sql` records the historical rollout
//! state. It is superseded by this authority contract; numbered migrations
//! remain byte-immutable after deployment.

pub mod regression_alerts;
