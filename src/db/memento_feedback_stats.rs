use anyhow::{Result, anyhow};
use libsql_rusqlite::{Connection, params};

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

pub fn upsert_turn_stat(db: &Db, stat: &MementoFeedbackTurnStat) -> Result<()> {
    let mut conn = db.lock().map_err(|error| {
        anyhow!("db lock failed while recording memento feedback stats: {error}")
    })?;
    upsert_turn_stat_on_conn(&mut conn, stat)
}

pub fn upsert_turn_stat_on_conn(
    conn: &mut Connection,
    stat: &MementoFeedbackTurnStat,
) -> Result<()> {
    validate_turn_stat(stat)?;
    conn.execute(
        "INSERT INTO memento_feedback_turn_stats (
            turn_id,
            stat_date,
            agent_id,
            provider,
            recall_count,
            manual_tool_feedback_count,
            manual_covered_recall_count,
            auto_tool_feedback_count,
            covered_recall_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(turn_id) DO UPDATE SET
            stat_date = excluded.stat_date,
            agent_id = excluded.agent_id,
            provider = excluded.provider,
            recall_count = excluded.recall_count,
            manual_tool_feedback_count = excluded.manual_tool_feedback_count,
            manual_covered_recall_count = excluded.manual_covered_recall_count,
            auto_tool_feedback_count = excluded.auto_tool_feedback_count,
            covered_recall_count = excluded.covered_recall_count",
        params![
            &stat.turn_id,
            &stat.stat_date,
            &stat.agent_id,
            &stat.provider,
            stat.recall_count,
            stat.manual_tool_feedback_count,
            stat.manual_covered_recall_count,
            stat.auto_tool_feedback_count,
            stat.covered_recall_count,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
pub fn list_daily_stats(conn: &Connection) -> Result<Vec<MementoFeedbackDailyStat>> {
    let mut stmt = conn.prepare(
        "SELECT
            stat_date,
            agent_id,
            provider,
            recall_count,
            tool_feedback_count,
            manual_tool_feedback_count,
            manual_covered_recall_count,
            auto_tool_feedback_count,
            covered_recall_count,
            compliance_rate,
            coverage_rate
         FROM memento_feedback_daily_stats
         ORDER BY stat_date DESC, agent_id ASC, provider ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(MementoFeedbackDailyStat {
            stat_date: row.get(0)?,
            agent_id: row.get(1)?,
            provider: row.get(2)?,
            recall_count: row.get(3)?,
            tool_feedback_count: row.get(4)?,
            manual_tool_feedback_count: row.get(5)?,
            manual_covered_recall_count: row.get(6)?,
            auto_tool_feedback_count: row.get(7)?,
            covered_recall_count: row.get(8)?,
            compliance_rate: row.get(9)?,
            coverage_rate: row.get(10)?,
        })
    })?;
    Ok(rows.collect::<libsql_rusqlite::Result<Vec<_>>>()?)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_stats_view_aggregates_manual_compliance_and_coverage() {
        let db = crate::db::test_db();
        let mut conn = db.lock().unwrap();

        upsert_turn_stat_on_conn(
            &mut conn,
            &MementoFeedbackTurnStat {
                turn_id: "turn-1".to_string(),
                stat_date: "2026-04-12".to_string(),
                agent_id: "project-agentdesk".to_string(),
                provider: "codex".to_string(),
                recall_count: 2,
                manual_tool_feedback_count: 1,
                manual_covered_recall_count: 1,
                auto_tool_feedback_count: 1,
                covered_recall_count: 2,
            },
        )
        .unwrap();
        upsert_turn_stat_on_conn(
            &mut conn,
            &MementoFeedbackTurnStat {
                turn_id: "turn-2".to_string(),
                stat_date: "2026-04-12".to_string(),
                agent_id: "project-agentdesk".to_string(),
                provider: "codex".to_string(),
                recall_count: 1,
                manual_tool_feedback_count: 1,
                manual_covered_recall_count: 1,
                auto_tool_feedback_count: 0,
                covered_recall_count: 1,
            },
        )
        .unwrap();

        let stats = list_daily_stats(&conn).unwrap();
        assert_eq!(
            stats,
            vec![MementoFeedbackDailyStat {
                stat_date: "2026-04-12".to_string(),
                agent_id: "project-agentdesk".to_string(),
                provider: "codex".to_string(),
                recall_count: 3,
                tool_feedback_count: 3,
                manual_tool_feedback_count: 2,
                manual_covered_recall_count: 2,
                auto_tool_feedback_count: 1,
                covered_recall_count: 3,
                compliance_rate: 2.0 / 3.0,
                coverage_rate: 1.0,
            }]
        );
    }

    #[test]
    fn turn_stats_upsert_replaces_existing_row() {
        let db = crate::db::test_db();
        let mut conn = db.lock().unwrap();

        let mut stat = MementoFeedbackTurnStat {
            turn_id: "turn-1".to_string(),
            stat_date: "2026-04-12".to_string(),
            agent_id: "project-agentdesk".to_string(),
            provider: "codex".to_string(),
            recall_count: 1,
            manual_tool_feedback_count: 0,
            manual_covered_recall_count: 0,
            auto_tool_feedback_count: 1,
            covered_recall_count: 1,
        };
        upsert_turn_stat_on_conn(&mut conn, &stat).unwrap();

        stat.manual_tool_feedback_count = 1;
        stat.manual_covered_recall_count = 1;
        stat.auto_tool_feedback_count = 0;
        upsert_turn_stat_on_conn(&mut conn, &stat).unwrap();

        let stats = list_daily_stats(&conn).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].manual_tool_feedback_count, 1);
        assert_eq!(stats[0].auto_tool_feedback_count, 0);
        assert_eq!(stats[0].compliance_rate, 1.0);
    }
}
