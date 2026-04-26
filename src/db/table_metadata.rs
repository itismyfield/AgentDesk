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
//! `0019_db_table_metadata.sql`.

use anyhow::{Result, anyhow};
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

    fn as_str(self) -> &'static str {
        match self {
            Source::Db => "db",
            Source::File => "file",
            Source::FileCanonical => "file-canonical",
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

#[allow(dead_code)]
pub async fn upsert_pg(
    pool: &PgPool,
    table_name: &str,
    source: Source,
    file_path: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO db_table_metadata (table_name, source_of_truth, file_path, last_synced_at)
         VALUES ($1, $2, $3, NOW())
         ON CONFLICT (table_name) DO UPDATE SET
             source_of_truth = EXCLUDED.source_of_truth,
             file_path = EXCLUDED.file_path,
             last_synced_at = NOW()",
    )
    .bind(table_name)
    .bind(source.as_str())
    .bind(file_path)
    .execute(pool)
    .await
    .map_err(|error| anyhow!("upsert postgres db_table_metadata {table_name}: {error}"))?;
    Ok(())
}

fn load_pipeline_state_ids(yaml_path: &std::path::Path) -> Result<Option<Vec<String>>> {
    use std::fs;

    if !yaml_path.exists() {
        tracing::warn!(
            "[db_table_metadata] pipeline yaml not found at {}; skipping sync",
            yaml_path.display()
        );
        return Ok(None);
    }

    let raw = fs::read_to_string(yaml_path)?;
    let parsed: serde_yaml::Value = serde_yaml::from_str(&raw)?;

    let states = parsed
        .get("states")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    Ok(Some(
        states
            .iter()
            .filter_map(|state| state.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect(),
    ))
}

#[allow(dead_code)]
pub async fn sync_pipeline_stages_from_yaml_pg(
    pool: &PgPool,
    yaml_path: &std::path::Path,
) -> Result<usize> {
    let Some(yaml_names) = load_pipeline_state_ids(yaml_path)? else {
        return Ok(0);
    };

    let sentinel_repo = "__default__";
    for (idx, id) in yaml_names.iter().enumerate() {
        sqlx::query(
            "INSERT INTO pipeline_stages
                (repo_id, stage_name, stage_order, entry_skill, timeout_minutes, on_failure)
             VALUES ($1, $2, $3, NULL, 60, 'fail')
             ON CONFLICT (repo_id, stage_name) DO NOTHING",
        )
        .bind(sentinel_repo)
        .bind(id)
        .bind(idx as i64 + 1)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow!(
                "sync postgres pipeline_stages from {}: {error}",
                yaml_path.display()
            )
        })?;
    }

    let db_names = sqlx::query_scalar::<_, String>(
        "SELECT stage_name
         FROM pipeline_stages
         WHERE repo_id = $1 AND stage_name IS NOT NULL",
    )
    .bind(sentinel_repo)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("scan postgres pipeline_stages default rows: {error}"))?;
    for row in db_names {
        if !yaml_names.contains(&row) {
            tracing::warn!(
                "[db_table_metadata] pipeline_stages has DB-only entry '{}' not present in {}; leaving untouched",
                row,
                yaml_path.display()
            );
        }
    }

    upsert_pg(
        pool,
        "pipeline_stages",
        Source::FileCanonical,
        yaml_path.to_str(),
    )
    .await?;

    Ok(yaml_names.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_roundtrips_storage_values() {
        assert_eq!(Source::from_str(Source::Db.as_str()), Some(Source::Db));
        assert_eq!(Source::from_str(Source::File.as_str()), Some(Source::File));
        assert_eq!(
            Source::from_str(Source::FileCanonical.as_str()),
            Some(Source::FileCanonical)
        );
        assert_eq!(Source::from_str("unknown"), None);
    }

    #[test]
    fn readonly_marker_matches_file_sources() {
        assert!(Source::File.is_readonly());
        assert!(Source::FileCanonical.is_readonly());
        assert!(!Source::Db.is_readonly());
    }

    #[test]
    fn load_pipeline_state_ids_reads_top_level_states() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!(
            "agentdesk_table_metadata_yaml_{}",
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

        let ids = load_pipeline_state_ids(&yaml_path).unwrap().unwrap();
        assert_eq!(ids, vec!["alpha", "beta", "gamma"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
