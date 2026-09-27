//! Shadow counters, folded from records so the live log line and the report agree.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{DeriveOutput, DiffRecord, ShadowRecord};

/// SchemaBlocked reason prefix the derive side uses for a split piece over the Discord limit.
pub const SPLIT_OVER_LIMIT_PREFIX: &str = "split piece ";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MetricsSnapshot {
    /// Keyed `class/cause`, e.g. `legacy_missing/Unknown`.
    pub diff_total: BTreeMap<String, u64>,
    pub schema_blocked_total: u64,
    pub split_over_limit_total: u64,
    pub tap_dropped_total: u64,
    pub binding_changes_total: u64,
    /// Live only: largest seen gap between a transcript write and its capture.
    pub capture_lag_ms_max: u64,
}

impl MetricsSnapshot {
    pub fn record(&mut self, record: &ShadowRecord) {
        match record {
            ShadowRecord::Diff { diff } => {
                *self.diff_total.entry(diff_label(diff)).or_default() += 1
            }
            ShadowRecord::Derived {
                output: DeriveOutput::SchemaBlocked { reason, .. },
            } => {
                self.schema_blocked_total += 1;
                if reason.starts_with(SPLIT_OVER_LIMIT_PREFIX) {
                    self.split_over_limit_total += 1;
                }
            }
            ShadowRecord::TapGap { dropped } => self.tap_dropped_total += dropped,
            ShadowRecord::Binding { .. } => self.binding_changes_total += 1,
            _ => {}
        }
    }

    pub fn record_capture_lag(&mut self, lag_ms: u64) {
        self.capture_lag_ms_max = self.capture_lag_ms_max.max(lag_ms);
    }
}

fn diff_label(diff: &DiffRecord) -> String {
    let class = serde_json::to_value(diff.class).ok();
    let cause = serde_json::to_value(diff.cause).ok();
    let text = |value: Option<serde_json::Value>| {
        value
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    };
    format!("{}/{}", text(class), text(cause))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use crate::services::tui_o::shadow::unit_plan::digest_pieces;
    use crate::services::tui_o::shadow::{SourceId, SourceRange};

    #[test]
    fn the_derive_reason_for_an_oversized_split_piece_counts_as_split_over_limit() {
        let oversized = vec![("x".to_string(), DISCORD_MSG_LIMIT + 1)];
        let reason = digest_pieces(oversized).unwrap_err();
        let source = SourceId {
            session_id: "s".into(),
            path: "t.jsonl".into(),
            dev: 1,
            ino: 1,
        };
        let (start, end) = (0, 1);
        let output = DeriveOutput::SchemaBlocked {
            channel_id: 7,
            source_range: SourceRange { source, start, end },
            reason,
        };
        let mut metrics = MetricsSnapshot::default();
        metrics.record(&ShadowRecord::Derived { output });
        assert_eq!(
            (metrics.schema_blocked_total, metrics.split_over_limit_total),
            (1, 1)
        );
    }
}
