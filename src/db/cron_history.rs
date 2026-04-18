use libsql_rusqlite::{Connection, Row, params};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CronJobRunRecord {
    pub job_id: String,
    pub job_name: String,
    pub status: String,
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
    pub duration_ms: i64,
}

fn map_run_row(row: &Row<'_>) -> libsql_rusqlite::Result<CronJobRunRecord> {
    Ok(CronJobRunRecord {
        job_id: row.get(0)?,
        job_name: row.get(1)?,
        status: row.get(2)?,
        started_at_ms: row.get(3)?,
        completed_at_ms: row.get(4)?,
        duration_ms: row.get(5)?,
    })
}

pub fn record_run(conn: &Connection, run: &CronJobRunRecord) -> libsql_rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO cron_job_runs (
            job_id,
            job_name,
            status,
            started_at_ms,
            completed_at_ms,
            duration_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            run.job_id,
            run.job_name,
            run.status,
            run.started_at_ms,
            run.completed_at_ms,
            run.duration_ms
        ],
    )?;
    Ok(())
}

pub fn latest_runs(conn: &Connection) -> libsql_rusqlite::Result<HashMap<String, CronJobRunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT job_id, job_name, status, started_at_ms, completed_at_ms, duration_ms
         FROM cron_job_runs
         ORDER BY started_at_ms DESC, id DESC",
    )?;
    let rows = stmt.query_map([], map_run_row)?;
    let mut latest = HashMap::new();
    for row in rows {
        let run = row?;
        latest.entry(run.job_id.clone()).or_insert(run);
    }
    Ok(latest)
}

pub fn list_runs_since(
    conn: &Connection,
    since_ms: i64,
) -> libsql_rusqlite::Result<Vec<CronJobRunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT job_id, job_name, status, started_at_ms, completed_at_ms, duration_ms
         FROM cron_job_runs
         WHERE started_at_ms >= ?1
         ORDER BY started_at_ms DESC, id DESC",
    )?;
    let rows = stmt.query_map([since_ms], map_run_row)?;
    rows.collect()
}
