//! db_table_metadata — per-table source-of-truth markers (#1097 / 910-3).
//!
//! Some DB tables in AgentDesk are canonical in code/files (e.g. the
//! pipeline definition in `policies/default-pipeline.yaml`) and the DB
//! merely holds a materialized mirror.  This module exposes a small
//! helper layer so:
//!
//!   * API routes can ask `source_of_truth("pipeline_stages")` and
//!     reject mutations with HTTP 405 when the table is `file-canonical`.
//!   * Startup code can sync materialized tables from their source file
//!     (`sync_pipeline_stages_from_yaml`) and stamp `last_synced_at`.
//!
//! The table itself is created in the Postgres migration
//! `0019_db_table_metadata.sql` and in `src/db/schema.rs` for SQLite.

use anyhow::Result;
use libsql_rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Source-of-truth designation for a DB table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// The DB is canonical.  Free to mutate via API.
    Db,
    /// A file on disk is canonical.  API must refuse mutations.
    File,
    /// The DB row is a materialized mirror of a file.  API must refuse
    /// mutations; startup code rebuilds from the file.
    FileCanonical,
}

impl Source {
    /// True if the source-of-truth lives outside the DB (i.e. API writes
    /// must be rejected).
    pub fn is_readonly(self) -> bool {
        matches!(self, Source::File | Source::FileCanonical)
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "db" => Some(Source::Db),
            "file" => Some(Source::File),
            "file-canonical" => Some(Source::FileCanonical),
            _ => None,
        }
    }
}

/// `fn db_source_of_truth(table_name) -> Option<Source>` — Postgres path.
/// Returns `None` if the table has no metadata row (i.e. defaults to `Db`).
pub async fn source_of_truth_pg(pool: &PgPool, table_name: &str) -> Option<Source> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT source_of_truth FROM db_table_metadata WHERE table_name = $1")
            .bind(table_name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    row.and_then(|(s,)| Source::from_str(&s))
}

/// `fn db_source_of_truth(table_name) -> Option<Source>` — SQLite path.
pub fn source_of_truth_sqlite(conn: &Connection, table_name: &str) -> Option<Source> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT source_of_truth FROM db_table_metadata WHERE table_name = ?1",
            [table_name],
            |row| row.get::<_, String>(0),
        )
        .ok();
    raw.as_deref().and_then(Source::from_str)
}

/// Upsert a metadata row.  Used by startup sync and tests.
pub fn upsert_sqlite(
    conn: &Connection,
    table_name: &str,
    source: Source,
    file_path: Option<&str>,
) -> Result<()> {
    let source_str = match source {
        Source::Db => "db",
        Source::File => "file",
        Source::FileCanonical => "file-canonical",
    };
    conn.execute(
        "INSERT INTO db_table_metadata (table_name, source_of_truth, file_path, last_synced_at)
         VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
         ON CONFLICT(table_name) DO UPDATE SET
             source_of_truth = excluded.source_of_truth,
             file_path = excluded.file_path,
             last_synced_at = CURRENT_TIMESTAMP",
        libsql_rusqlite::params![table_name, source_str, file_path],
    )?;
    Ok(())
}

/// Materialized view sync.  Parses `policies/default-pipeline.yaml`
/// (just the top-level `states:` list for now) and upserts one
/// `pipeline_stages` row per state for the sentinel repo
/// `__default__`, then stamps `last_synced_at`.
///
/// Startup calls this *after* `pipeline::load` so we know the file is
/// already parseable.  If the yaml has entries the DB does not, they
/// are inserted; if the DB has extra rows, we log a warning rather
/// than deleting (per DoD: "don't destroy").
pub fn sync_pipeline_stages_from_yaml_sqlite(
    conn: &Connection,
    yaml_path: &std::path::Path,
) -> Result<usize> {
    use std::fs;

    if !yaml_path.exists() {
        tracing::warn!(
            "[db_table_metadata] pipeline yaml not found at {}; skipping sync",
            yaml_path.display()
        );
        return Ok(0);
    }

    let raw = fs::read_to_string(yaml_path)?;
    let parsed: serde_yaml::Value = serde_yaml::from_str(&raw)?;

    let states = parsed
        .get("states")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    let sentinel_repo = "__default__";
    let mut yaml_names: Vec<String> = Vec::new();

    for (idx, state) in states.iter().enumerate() {
        let id = match state.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        yaml_names.push(id.clone());

        conn.execute(
            "INSERT INTO pipeline_stages
                (repo_id, stage_name, stage_order, entry_skill, timeout_minutes, on_failure)
             SELECT ?1, ?2, ?3, NULL, 60, 'fail'
             WHERE NOT EXISTS (
                 SELECT 1 FROM pipeline_stages
                 WHERE repo_id = ?1 AND stage_name = ?2
             )",
            libsql_rusqlite::params![sentinel_repo, id, idx as i64 + 1],
        )?;
    }

    // Warn on DB-only entries (yaml has entries not in db → sync;
    // db has entries not in yaml → warn, don't destroy).
    let mut stmt = conn.prepare(
        "SELECT stage_name FROM pipeline_stages WHERE repo_id = ?1 AND stage_name IS NOT NULL",
    )?;
    let rows = stmt.query_map([sentinel_repo], |row| row.get::<_, String>(0))?;
    for row in rows.flatten() {
        if !yaml_names.contains(&row) {
            tracing::warn!(
                "[db_table_metadata] pipeline_stages has DB-only entry '{}' not present in {}; leaving untouched",
                row,
                yaml_path.display()
            );
        }
    }

    upsert_sqlite(
        conn,
        "pipeline_stages",
        Source::FileCanonical,
        yaml_path.to_str(),
    )?;

    Ok(yaml_names.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn db_source_of_truth_returns_seeded_value() {
        let conn = test_conn();
        // pipeline_stages is seeded as file-canonical by the schema migrator.
        let got = source_of_truth_sqlite(&conn, "pipeline_stages");
        assert_eq!(got, Some(Source::FileCanonical));
        assert!(got.unwrap().is_readonly());
    }

    #[test]
    fn db_source_of_truth_returns_none_for_unknown_table() {
        let conn = test_conn();
        let got = source_of_truth_sqlite(&conn, "no_such_table_xyz");
        assert_eq!(got, None);
    }

    #[test]
    fn db_source_of_truth_upsert_roundtrip() {
        let conn = test_conn();
        upsert_sqlite(
            &conn,
            "role_bindings",
            Source::File,
            Some("config/agentdesk.yaml"),
        )
        .unwrap();
        assert_eq!(
            source_of_truth_sqlite(&conn, "role_bindings"),
            Some(Source::File)
        );

        // Flip to db-canonical.
        upsert_sqlite(&conn, "role_bindings", Source::Db, None).unwrap();
        assert_eq!(
            source_of_truth_sqlite(&conn, "role_bindings"),
            Some(Source::Db)
        );
        assert!(!Source::Db.is_readonly());
    }

    #[test]
    fn materialized_view_sync_populates_stages_from_yaml() {
        use std::io::Write;
        let conn = test_conn();

        // Make sure the metadata table starts empty for this table.
        conn.execute(
            "DELETE FROM db_table_metadata WHERE table_name = 'pipeline_stages'",
            [],
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "agentdesk_1097_yaml_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let yaml_path = dir.join("fake-pipeline.yaml");
        let mut f = std::fs::File::create(&yaml_path).unwrap();
        writeln!(
            f,
            "name: fake\nversion: 1\nstates:\n  - id: alpha\n  - id: beta\n  - id: gamma\n"
        )
        .unwrap();
        drop(f);

        let inserted = sync_pipeline_stages_from_yaml_sqlite(&conn, &yaml_path).unwrap();
        assert_eq!(inserted, 3);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pipeline_stages WHERE repo_id = '__default__'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);

        // The metadata row is now stamped and marks the table readonly.
        assert_eq!(
            source_of_truth_sqlite(&conn, "pipeline_stages"),
            Some(Source::FileCanonical)
        );

        // Re-running should be idempotent.
        let inserted2 = sync_pipeline_stages_from_yaml_sqlite(&conn, &yaml_path).unwrap();
        assert_eq!(inserted2, 3);
        let count2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pipeline_stages WHERE repo_id = '__default__'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count2, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
