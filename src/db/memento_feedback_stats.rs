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

pub fn upsert_turn_stat(_db: &Db, _stat: &MementoFeedbackTurnStat) -> Result<()> {
    Err(anyhow!(
        "sqlite memento feedback stats backend is unavailable in production"
    ))
}
