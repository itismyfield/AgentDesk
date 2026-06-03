use anyhow::{Result, anyhow};

use crate::db::Db;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MementoFeedbackTurnStat {
    pub turn_id: String,
    pub stat_date: String,
    pub agent_id: String,
    pub provider: String,
    pub recall_count: i64,
    pub manual_tool_feedback_count: i64,
    pub manual_covered_recall_count: i64,
    pub auto_tool_feedback_count: i64,
    pub covered_recall_count: i64,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub struct MementoFeedbackDailyStat {
    pub stat_date: String,
    pub agent_id: String,
    pub provider: String,
    pub recall_count: i64,
    pub tool_feedback_count: i64,
    pub manual_tool_feedback_count: i64,
    pub manual_covered_recall_count: i64,
    pub auto_tool_feedback_count: i64,
    pub covered_recall_count: i64,
    pub compliance_rate: f64,
    pub coverage_rate: f64,
}

pub fn upsert_turn_stat(_db: &Db, _stat: &MementoFeedbackTurnStat) -> Result<()> {
    Err(anyhow!(
        "sqlite memento feedback stats backend is unavailable in production"
    ))
}

// reason: only caller is the legacy-sqlite-tests-gated upsert_turn_stat_on_conn;
// dead in the production build. See #3034 / #3035.
#[allow(dead_code)]
fn validate_turn_stat(stat: &MementoFeedbackTurnStat) -> Result<()> {
    if stat.turn_id.trim().is_empty() {
        return Err(anyhow!("memento feedback stats require non-empty turn_id"));
    }
    if stat.stat_date.trim().is_empty() {
        return Err(anyhow!(
            "memento feedback stats require non-empty stat_date"
        ));
    }
    if stat.agent_id.trim().is_empty() {
        return Err(anyhow!("memento feedback stats require non-empty agent_id"));
    }
    if stat.provider.trim().is_empty() {
        return Err(anyhow!("memento feedback stats require non-empty provider"));
    }

    for (label, value) in [
        ("recall_count", stat.recall_count),
        (
            "manual_tool_feedback_count",
            stat.manual_tool_feedback_count,
        ),
        (
            "manual_covered_recall_count",
            stat.manual_covered_recall_count,
        ),
        ("auto_tool_feedback_count", stat.auto_tool_feedback_count),
        ("covered_recall_count", stat.covered_recall_count),
    ] {
        if value < 0 {
            return Err(anyhow!(
                "memento feedback stats {label} must be non-negative"
            ));
        }
    }

    if stat.manual_covered_recall_count > stat.recall_count {
        return Err(anyhow!(
            "manual_covered_recall_count cannot exceed recall_count"
        ));
    }
    if stat.covered_recall_count > stat.recall_count {
        return Err(anyhow!("covered_recall_count cannot exceed recall_count"));
    }

    Ok(())
}
