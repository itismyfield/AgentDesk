use std::fs;
use std::path::{Path, PathBuf};

use libsql_rusqlite::{
    Connection, OptionalExtension, params, params_from_iter, types::Value as SqlValue,
};

pub(crate) const AUTO_REMEMBER_MAX_RETRIES: u32 = 3;
pub(crate) const AUTO_REMEMBER_RETRY_BACKOFF_MS: [i64; 2] = [30_000, 300_000];
pub(crate) const AUTO_REMEMBER_RETRY_DRAIN_LIMIT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutoRememberMemoryStatus {
    Remembered,
    VerifiedPromoted,
    OperatorVerified,
    OperatorRejected,
    DuplicateSkip,
    ValidationSkipped,
    RememberFailed,
    AbandonedAfterRetries,
}

impl AutoRememberMemoryStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Remembered => "remembered",
            Self::VerifiedPromoted => "verified_promoted",
            Self::OperatorVerified => "operator_verified",
            Self::OperatorRejected => "operator_rejected",
            Self::DuplicateSkip => "duplicate_skip",
            Self::ValidationSkipped => "validation_skipped",
            Self::RememberFailed => "remember_failed",
            Self::AbandonedAfterRetries => "abandoned_after_retries",
        }
    }

    pub(crate) fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "remembered" => Some(Self::Remembered),
            "verified_promoted" => Some(Self::VerifiedPromoted),
            "operator_verified" => Some(Self::OperatorVerified),
            "operator_rejected" => Some(Self::OperatorRejected),
            "duplicate_skip" => Some(Self::DuplicateSkip),
            "validation_skipped" => Some(Self::ValidationSkipped),
            "remember_failed" => Some(Self::RememberFailed),
            "abandoned_after_retries" => Some(Self::AbandonedAfterRetries),
            _ => None,
        }
    }

    pub(crate) fn suppresses_repeat(self) -> bool {
        matches!(
            self,
            Self::Remembered
                | Self::VerifiedPromoted
                | Self::OperatorVerified
                | Self::OperatorRejected
                | Self::DuplicateSkip
                | Self::ValidationSkipped
                | Self::AbandonedAfterRetries
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutoRememberStage {
    Validate,
    Remember,
    Verify,
    Dedupe,
}

impl AutoRememberStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Remember => "remember",
            Self::Verify => "verify",
            Self::Dedupe => "dedupe",
        }
    }

    pub(crate) fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "validate" => Some(Self::Validate),
            "remember" => Some(Self::Remember),
            "verify" => Some(Self::Verify),
            "dedupe" => Some(Self::Dedupe),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberAuditEntry<'a> {
    pub(crate) turn_id: &'a str,
    pub(crate) candidate_hash: &'a str,
    pub(crate) signal_kind: &'a str,
    pub(crate) workspace: &'a str,
    pub(crate) stage: AutoRememberStage,
    pub(crate) status: AutoRememberMemoryStatus,
    pub(crate) retry_count: u32,
    pub(crate) error: Option<&'a str>,
    pub(crate) raw_content: Option<&'a str>,
    pub(crate) entity_key: Option<&'a str>,
    pub(crate) supporting_evidence: Option<&'a [String]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberAuditRecord {
    pub(crate) status: AutoRememberMemoryStatus,
    pub(crate) retry_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberAuditDetail {
    pub(crate) turn_id: String,
    pub(crate) workspace: String,
    pub(crate) candidate_hash: String,
    pub(crate) signal_kind: String,
    pub(crate) stage: AutoRememberStage,
    pub(crate) status: AutoRememberMemoryStatus,
    pub(crate) retry_count: u32,
    pub(crate) error: Option<String>,
    pub(crate) raw_content: Option<String>,
    pub(crate) entity_key: Option<String>,
    pub(crate) supporting_evidence: Vec<String>,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberRetryRecord {
    pub(crate) turn_id: String,
    pub(crate) workspace: String,
    pub(crate) candidate_hash: String,
    pub(crate) signal_kind: String,
    pub(crate) raw_content: String,
    pub(crate) entity_key: Option<String>,
    pub(crate) supporting_evidence: Vec<String>,
    pub(crate) retry_count: u32,
    pub(crate) error: Option<String>,
    pub(crate) available_at_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AutoRememberAuditFilter<'a> {
    pub(crate) workspace: Option<&'a str>,
    pub(crate) status: Option<AutoRememberMemoryStatus>,
    pub(crate) stage: Option<AutoRememberStage>,
    pub(crate) signal_kind: Option<&'a str>,
    pub(crate) candidate_hash: Option<&'a str>,
    pub(crate) resubmittable_only: bool,
    pub(crate) limit: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct AutoRememberStore {
    path: PathBuf,
}

impl AutoRememberStore {
    pub(crate) fn open() -> Result<Self, String> {
        let path = sidecar_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create auto-remember sidecar dir: {err}"))?;
        }

        let store = Self { path };
        store.ensure_schema()?;
        Ok(store)
    }

    pub(crate) fn open_existing() -> Result<Option<Self>, String> {
        let path = sidecar_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let store = Self { path };
        store.ensure_schema()?;
        Ok(Some(store))
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &PathBuf {
        &self.path
    }

    pub(crate) fn lookup_record(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<Option<AutoRememberAuditRecord>, String> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT memory_status, retry_count
                 FROM auto_remember_audit
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![workspace, candidate_hash],
                |row| {
                    let status = row.get::<_, String>(0)?;
                    let retry_count = row.get::<_, u32>(1)?;
                    Ok((status, retry_count))
                },
            )
            .optional()
            .map(|record| {
                record.and_then(|(status, retry_count)| {
                    AutoRememberMemoryStatus::from_str(&status).map(|status| {
                        AutoRememberAuditRecord {
                            status,
                            retry_count,
                        }
                    })
                })
            })
        })
    }

    pub(crate) fn upsert_audit(&self, entry: AutoRememberAuditEntry<'_>) -> Result<(), String> {
        let error = entry
            .error
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let raw_content = entry
            .raw_content
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let entity_key = entry
            .entity_key
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let supporting_evidence_json = serde_json::to_string(
            &entry
                .supporting_evidence
                .unwrap_or(&[])
                .iter()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>(),
        )
        .map_err(|err| format!("failed to encode auto-remember audit evidence: {err}"))?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO auto_remember_audit (
                    workspace,
                    candidate_hash,
                    turn_id,
                    signal_kind,
                    stage,
                    memory_status,
                    retry_count,
                    error,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(workspace, candidate_hash) DO UPDATE SET
                    turn_id = excluded.turn_id,
                    signal_kind = excluded.signal_kind,
                    stage = excluded.stage,
                    memory_status = excluded.memory_status,
                    retry_count = excluded.retry_count,
                    error = excluded.error,
                    raw_content = COALESCE(excluded.raw_content, auto_remember_audit.raw_content),
                    entity_key = COALESCE(excluded.entity_key, auto_remember_audit.entity_key),
                    supporting_evidence_json = CASE
                        WHEN excluded.supporting_evidence_json = '[]'
                            THEN auto_remember_audit.supporting_evidence_json
                        ELSE excluded.supporting_evidence_json
                    END,
                    created_at = excluded.created_at",
                params![
                    entry.workspace,
                    entry.candidate_hash,
                    entry.turn_id,
                    entry.signal_kind,
                    entry.stage.as_str(),
                    entry.status.as_str(),
                    entry.retry_count,
                    error,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn next_retry_count(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<u32, String> {
        Ok(self
            .lookup_record(workspace, candidate_hash)?
            .map(|record| record.retry_count.saturating_add(1))
            .unwrap_or(1))
    }

    pub(crate) fn next_failure_status(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<(AutoRememberMemoryStatus, u32), String> {
        let retry_count = self.next_retry_count(workspace, candidate_hash)?;
        let status = if retry_count >= AUTO_REMEMBER_MAX_RETRIES {
            AutoRememberMemoryStatus::AbandonedAfterRetries
        } else {
            AutoRememberMemoryStatus::RememberFailed
        };
        Ok((status, retry_count))
    }

    pub(crate) fn next_retry_available_at_ms(&self, retry_count: u32) -> i64 {
        let delay = retry_backoff_ms(retry_count);
        chrono::Utc::now().timestamp_millis().saturating_add(delay)
    }

    pub(crate) fn upsert_retry(&self, entry: &AutoRememberRetryRecord) -> Result<(), String> {
        let evidence_json = serde_json::to_string(&entry.supporting_evidence)
            .map_err(|err| format!("failed to encode auto-remember retry evidence: {err}"))?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO auto_remember_retry_queue (
                    workspace,
                    candidate_hash,
                    turn_id,
                    signal_kind,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    retry_count,
                    error,
                    available_at_ms,
                    updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(workspace, candidate_hash) DO UPDATE SET
                    turn_id = excluded.turn_id,
                    signal_kind = excluded.signal_kind,
                    raw_content = excluded.raw_content,
                    entity_key = excluded.entity_key,
                    supporting_evidence_json = excluded.supporting_evidence_json,
                    retry_count = excluded.retry_count,
                    error = excluded.error,
                    available_at_ms = excluded.available_at_ms,
                    updated_at_ms = excluded.updated_at_ms",
                params![
                    entry.workspace,
                    entry.candidate_hash,
                    entry.turn_id,
                    entry.signal_kind,
                    entry.raw_content,
                    entry.entity_key,
                    evidence_json,
                    entry.retry_count,
                    entry.error,
                    entry.available_at_ms,
                    chrono::Utc::now().timestamp_millis(),
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn load_retry_batch(&self) -> Result<Vec<AutoRememberRetryRecord>, String> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT
                    turn_id,
                    workspace,
                    candidate_hash,
                    signal_kind,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    retry_count,
                    error,
                    available_at_ms
                 FROM auto_remember_retry_queue
                 WHERE available_at_ms <= ?1
                 ORDER BY available_at_ms ASC, updated_at_ms ASC
                 LIMIT ?2",
            )?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let rows = stmt.query_map(
                params![now_ms, AUTO_REMEMBER_RETRY_DRAIN_LIMIT as i64],
                |row| {
                    let evidence_json = row.get::<_, String>(6)?;
                    let supporting_evidence =
                        serde_json::from_str::<Vec<String>>(&evidence_json).unwrap_or_default();
                    Ok(AutoRememberRetryRecord {
                        turn_id: row.get(0)?,
                        workspace: row.get(1)?,
                        candidate_hash: row.get(2)?,
                        signal_kind: row.get(3)?,
                        raw_content: row.get(4)?,
                        entity_key: row.get(5)?,
                        supporting_evidence,
                        retry_count: row.get(7)?,
                        error: row.get(8)?,
                        available_at_ms: row.get(9)?,
                    })
                },
            )?;

            let mut records = Vec::new();
            for row in rows {
                records.push(row?);
            }
            Ok(records)
        })
    }

    pub(crate) fn delete_retry(&self, workspace: &str, candidate_hash: &str) -> Result<(), String> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM auto_remember_retry_queue
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![workspace, candidate_hash],
            )?;
            Ok(())
        })
    }

    pub(crate) fn list_audit(
        &self,
        workspace: Option<&str>,
        status: Option<AutoRememberMemoryStatus>,
        limit: usize,
    ) -> Result<Vec<AutoRememberAuditDetail>, String> {
        self.list_audit_filtered(&AutoRememberAuditFilter {
            workspace,
            status,
            limit,
            ..AutoRememberAuditFilter::default()
        })
    }

    pub(crate) fn list_audit_filtered(
        &self,
        filter: &AutoRememberAuditFilter<'_>,
    ) -> Result<Vec<AutoRememberAuditDetail>, String> {
        let limit = filter.limit.max(1) as i64;
        self.with_conn(|conn| {
            let mut query = String::from(
                "SELECT
                    turn_id,
                    workspace,
                    candidate_hash,
                    signal_kind,
                    stage,
                    memory_status,
                    retry_count,
                    error,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    created_at
                 FROM auto_remember_audit",
            );
            let mut conditions = Vec::new();
            let mut params = Vec::<SqlValue>::new();

            if let Some(workspace) = filter
                .workspace
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                conditions.push("workspace = ?");
                params.push(SqlValue::Text(workspace.to_string()));
            }
            if let Some(status) = filter.status {
                conditions.push("memory_status = ?");
                params.push(SqlValue::Text(status.as_str().to_string()));
            }
            if let Some(stage) = filter.stage {
                conditions.push("stage = ?");
                params.push(SqlValue::Text(stage.as_str().to_string()));
            }
            if let Some(signal_kind) = filter
                .signal_kind
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                conditions.push("signal_kind = ?");
                params.push(SqlValue::Text(signal_kind.to_string()));
            }
            if let Some(candidate_hash) = filter
                .candidate_hash
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                conditions.push("candidate_hash = ?");
                params.push(SqlValue::Text(candidate_hash.to_string()));
            }
            if filter.resubmittable_only {
                conditions.push("(memory_status = ? OR memory_status = ?)");
                params.push(SqlValue::Text(
                    AutoRememberMemoryStatus::RememberFailed
                        .as_str()
                        .to_string(),
                ));
                params.push(SqlValue::Text(
                    AutoRememberMemoryStatus::AbandonedAfterRetries
                        .as_str()
                        .to_string(),
                ));
            }

            if !conditions.is_empty() {
                query.push_str(" WHERE ");
                query.push_str(&conditions.join(" AND "));
            }
            query.push_str(" ORDER BY created_at DESC LIMIT ?");
            params.push(SqlValue::Integer(limit));

            let mut stmt = conn.prepare(&query)?;
            let rows = stmt.query_map(params_from_iter(params.iter()), decode_audit_detail_row)?;
            let mut records = Vec::new();
            for row in rows {
                records.push(row?);
            }
            Ok(records)
        })
    }

    pub(crate) fn load_audit_detail(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<Option<AutoRememberAuditDetail>, String> {
        Ok(self
            .list_audit_filtered(&AutoRememberAuditFilter {
                workspace: Some(workspace),
                candidate_hash: Some(candidate_hash),
                limit: 1,
                ..AutoRememberAuditFilter::default()
            })?
            .into_iter()
            .next())
    }

    pub(crate) fn count_by_status(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<(String, u64)>, String> {
        grouped_count_query(self, workspace, "memory_status", None)
    }

    pub(crate) fn count_validation_skip_reasons(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<(String, u64)>, String> {
        grouped_count_query(
            self,
            workspace,
            "error",
            Some(AutoRememberMemoryStatus::ValidationSkipped),
        )
    }

    pub(crate) fn load_resubmittable_candidate(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<Option<AutoRememberRetryRecord>, String> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT
                    signal_kind,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    retry_count,
                    error,
                    memory_status,
                    turn_id
                 FROM auto_remember_audit
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![workspace, candidate_hash],
                |row| {
                    let evidence_json = row
                        .get::<_, Option<String>>(3)?
                        .unwrap_or_else(|| "[]".to_string());
                    let supporting_evidence =
                        serde_json::from_str::<Vec<String>>(&evidence_json).unwrap_or_default();
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        supporting_evidence,
                        row.get::<_, u32>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()
            .map(|record| {
                record.and_then(
                    |(
                        signal_kind,
                        raw_content,
                        entity_key,
                        supporting_evidence,
                        retry_count,
                        error,
                        status_raw,
                        turn_id,
                    )| {
                        let status = AutoRememberMemoryStatus::from_str(&status_raw)?;
                        if !matches!(
                            status,
                            AutoRememberMemoryStatus::RememberFailed
                                | AutoRememberMemoryStatus::AbandonedAfterRetries
                        ) {
                            return None;
                        }
                        raw_content.map(|raw_content| AutoRememberRetryRecord {
                            turn_id,
                            workspace: workspace.to_string(),
                            candidate_hash: candidate_hash.to_string(),
                            signal_kind,
                            raw_content,
                            entity_key,
                            supporting_evidence,
                            retry_count,
                            error,
                            available_at_ms: chrono::Utc::now().timestamp_millis(),
                        })
                    },
                )
            })
        })
    }

    pub(crate) fn reset_retry_state(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<(), String> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE auto_remember_audit
                 SET stage = ?3,
                     memory_status = ?4,
                     retry_count = 0,
                     error = ?5,
                     created_at = ?6
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![
                    workspace,
                    candidate_hash,
                    AutoRememberStage::Remember.as_str(),
                    AutoRememberMemoryStatus::RememberFailed.as_str(),
                    "manual resubmit requested",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                ],
            )?;
            conn.execute(
                "DELETE FROM auto_remember_retry_queue
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![workspace, candidate_hash],
            )?;
            Ok(())
        })
    }

    pub(crate) fn set_operator_status(
        &self,
        workspace: &str,
        candidate_hash: &str,
        status: AutoRememberMemoryStatus,
        note: Option<&str>,
    ) -> Result<(), String> {
        if !matches!(
            status,
            AutoRememberMemoryStatus::OperatorVerified | AutoRememberMemoryStatus::OperatorRejected
        ) {
            return Err(format!("unsupported operator status '{}'", status.as_str()));
        }

        let note = note
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| match status {
                AutoRememberMemoryStatus::OperatorVerified => "operator verified candidate",
                AutoRememberMemoryStatus::OperatorRejected => "operator rejected candidate",
                _ => "operator updated candidate",
            });

        self.with_conn(|conn| {
            let updated = conn.execute(
                "UPDATE auto_remember_audit
                 SET stage = ?3,
                     memory_status = ?4,
                     error = ?5,
                     created_at = ?6
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![
                    workspace,
                    candidate_hash,
                    AutoRememberStage::Verify.as_str(),
                    status.as_str(),
                    note,
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                ],
            )?;
            if updated == 0 {
                return Err(libsql_rusqlite::Error::QueryReturnedNoRows);
            }
            conn.execute(
                "DELETE FROM auto_remember_retry_queue
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![workspace, candidate_hash],
            )?;
            Ok(())
        })
        .map_err(|error| match error.as_str() {
            "auto-remember sidecar query failed: Query returned no rows" => format!(
                "no auto-remember candidate found for workspace='{workspace}' hash='{candidate_hash}'"
            ),
            _ => error,
        })
    }

    pub(crate) fn requeue_candidate(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<(), String> {
        let record = self
            .load_resubmittable_candidate(workspace, candidate_hash)?
            .or_else(|| {
                self.load_audit_detail(workspace, candidate_hash)
                    .ok()
                    .flatten()
                    .and_then(|detail| {
                        let raw_content = detail.raw_content?;
                        Some(AutoRememberRetryRecord {
                            turn_id: detail.turn_id,
                            workspace: detail.workspace,
                            candidate_hash: detail.candidate_hash,
                            signal_kind: detail.signal_kind,
                            raw_content,
                            entity_key: detail.entity_key,
                            supporting_evidence: detail.supporting_evidence,
                            retry_count: 0,
                            error: detail.error,
                            available_at_ms: chrono::Utc::now().timestamp_millis(),
                        })
                    })
            })
            .ok_or_else(|| {
                format!(
                    "no requeueable auto-remember candidate found for workspace='{workspace}' hash='{candidate_hash}'"
                )
            })?;

        self.with_conn(|conn| {
            conn.execute(
                "UPDATE auto_remember_audit
                 SET stage = ?3,
                     memory_status = ?4,
                     retry_count = 0,
                     error = ?5,
                     created_at = ?6
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![
                    workspace,
                    candidate_hash,
                    AutoRememberStage::Remember.as_str(),
                    AutoRememberMemoryStatus::RememberFailed.as_str(),
                    "manual requeue requested",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                ],
            )?;
            Ok(())
        })?;

        self.upsert_retry(&AutoRememberRetryRecord {
            retry_count: 0,
            available_at_ms: chrono::Utc::now().timestamp_millis(),
            error: Some("manual requeue requested".to_string()),
            ..record
        })
    }

    fn ensure_schema(&self) -> Result<(), String> {
        self.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS auto_remember_audit (
                    workspace TEXT NOT NULL,
                    candidate_hash TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    signal_kind TEXT NOT NULL,
                    stage TEXT NOT NULL,
                    memory_status TEXT NOT NULL,
                    retry_count INTEGER NOT NULL DEFAULT 0,
                    error TEXT,
                    raw_content TEXT,
                    entity_key TEXT,
                    supporting_evidence_json TEXT NOT NULL DEFAULT '[]',
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (workspace, candidate_hash)
                );
                CREATE TABLE IF NOT EXISTS auto_remember_retry_queue (
                    workspace TEXT NOT NULL,
                    candidate_hash TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    signal_kind TEXT NOT NULL,
                    raw_content TEXT NOT NULL,
                    entity_key TEXT,
                    supporting_evidence_json TEXT NOT NULL,
                    retry_count INTEGER NOT NULL DEFAULT 0,
                    error TEXT,
                    available_at_ms INTEGER NOT NULL DEFAULT 0,
                    updated_at_ms INTEGER NOT NULL,
                    PRIMARY KEY (workspace, candidate_hash)
                );",
            )?;
            ensure_column(
                conn,
                "auto_remember_audit",
                "retry_count",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(conn, "auto_remember_audit", "raw_content", "TEXT")?;
            ensure_column(conn, "auto_remember_audit", "entity_key", "TEXT")?;
            ensure_column(
                conn,
                "auto_remember_audit",
                "supporting_evidence_json",
                "TEXT NOT NULL DEFAULT '[]'",
            )?;
            ensure_column(
                conn,
                "auto_remember_retry_queue",
                "supporting_evidence_json",
                "TEXT NOT NULL DEFAULT '[]'",
            )?;
            ensure_column(
                conn,
                "auto_remember_retry_queue",
                "retry_count",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                conn,
                "auto_remember_retry_queue",
                "available_at_ms",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                conn,
                "auto_remember_retry_queue",
                "updated_at_ms",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            conn.execute(
                "UPDATE auto_remember_retry_queue
                 SET available_at_ms = CASE
                     WHEN COALESCE(available_at_ms, 0) <= 0
                         THEN COALESCE(updated_at_ms, 0)
                     ELSE available_at_ms
                 END",
                [],
            )?;
            Ok(())
        })
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> libsql_rusqlite::Result<T>,
    ) -> Result<T, String> {
        let conn = Connection::open(&self.path)
            .map_err(|err| format!("failed to open auto-remember sidecar: {err}"))?;
        f(&conn).map_err(|err| format!("auto-remember sidecar query failed: {err}"))
    }
}

fn sidecar_path() -> Result<PathBuf, String> {
    let root = crate::config::runtime_root()
        .ok_or_else(|| "AgentDesk runtime root is unavailable".to_string())?;
    let legacy_path = runtime_local_sidecar_path(&root);
    let target_path = configured_sidecar_path(&root);

    if target_path != legacy_path && legacy_path.exists() && !target_path.exists() {
        migrate_legacy_sidecar(&legacy_path, &target_path)?;
    }

    Ok(target_path)
}

fn configured_sidecar_path(root: &Path) -> PathBuf {
    let configured = crate::runtime_layout::load_memory_backend(root)
        .auto_remember
        .sidecar_path;
    match configured
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        }
        None => runtime_local_sidecar_path(root),
    }
}

fn runtime_local_sidecar_path(root: &Path) -> PathBuf {
    root.join("data").join("memory-auto-remember.sqlite")
}

fn migrate_legacy_sidecar(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create auto-remember sidecar dir '{}': {err}",
                parent.display()
            )
        })?;
    }

    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            fs::copy(from, to).map_err(|copy_error| {
                format!(
                    "failed to migrate auto-remember sidecar from '{}' to '{}': rename={rename_error}; copy={copy_error}",
                    from.display(),
                    to.display()
                )
            })?;
            fs::remove_file(from).map_err(|remove_error| {
                format!(
                    "copied auto-remember sidecar to '{}' but failed to remove legacy file '{}': {remove_error}",
                    to.display(),
                    from.display()
                )
            })?;
            Ok(())
        }
    }
}

fn retry_backoff_ms(retry_count: u32) -> i64 {
    let index = retry_count
        .saturating_sub(1)
        .min((AUTO_REMEMBER_RETRY_BACKOFF_MS.len() as u32).saturating_sub(1))
        as usize;
    AUTO_REMEMBER_RETRY_BACKOFF_MS[index]
}

fn decode_audit_detail_row(
    row: &libsql_rusqlite::Row<'_>,
) -> libsql_rusqlite::Result<AutoRememberAuditDetail> {
    let stage_raw = row.get::<_, String>(4)?;
    let status_raw = row.get::<_, String>(5)?;
    let supporting_evidence_json = row
        .get::<_, Option<String>>(10)?
        .unwrap_or_else(|| "[]".to_string());
    let supporting_evidence =
        serde_json::from_str::<Vec<String>>(&supporting_evidence_json).unwrap_or_default();
    Ok(AutoRememberAuditDetail {
        turn_id: row.get(0)?,
        workspace: row.get(1)?,
        candidate_hash: row.get(2)?,
        signal_kind: row.get(3)?,
        stage: AutoRememberStage::from_str(&stage_raw).unwrap_or(AutoRememberStage::Remember),
        status: AutoRememberMemoryStatus::from_str(&status_raw)
            .unwrap_or(AutoRememberMemoryStatus::RememberFailed),
        retry_count: row.get(6)?,
        error: row.get(7)?,
        raw_content: row.get(8)?,
        entity_key: row.get(9)?,
        supporting_evidence,
        created_at: row.get(11)?,
    })
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> libsql_rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == column);
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn grouped_count_query(
    store: &AutoRememberStore,
    workspace: Option<&str>,
    group_column: &str,
    status_filter: Option<AutoRememberMemoryStatus>,
) -> Result<Vec<(String, u64)>, String> {
    store.with_conn(|conn| {
        let mut rows_out = Vec::new();
        match (
            workspace.map(str::trim).filter(|value| !value.is_empty()),
            status_filter,
        ) {
            (Some(workspace), Some(status)) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT COALESCE({group_column}, '') AS key, COUNT(*)
                     FROM auto_remember_audit
                     WHERE workspace = ?1 AND memory_status = ?2
                     GROUP BY key
                     ORDER BY COUNT(*) DESC, key ASC"
                ))?;
                let rows = stmt.query_map(params![workspace, status.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?;
                for row in rows {
                    rows_out.push(row?);
                }
            }
            (Some(workspace), None) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT COALESCE({group_column}, '') AS key, COUNT(*)
                     FROM auto_remember_audit
                     WHERE workspace = ?1
                     GROUP BY key
                     ORDER BY COUNT(*) DESC, key ASC"
                ))?;
                let rows = stmt.query_map(params![workspace], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?;
                for row in rows {
                    rows_out.push(row?);
                }
            }
            (None, Some(status)) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT COALESCE({group_column}, '') AS key, COUNT(*)
                     FROM auto_remember_audit
                     WHERE memory_status = ?1
                     GROUP BY key
                     ORDER BY COUNT(*) DESC, key ASC"
                ))?;
                let rows = stmt.query_map(params![status.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?;
                for row in rows {
                    rows_out.push(row?);
                }
            }
            (None, None) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT COALESCE({group_column}, '') AS key, COUNT(*)
                     FROM auto_remember_audit
                     GROUP BY key
                     ORDER BY COUNT(*) DESC, key ASC"
                ))?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?;
                for row in rows {
                    rows_out.push(row?);
                }
            }
        }
        Ok(rows_out)
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::services::discord::runtime_store::lock_test_env;

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(&TempDir),
    {
        let _guard = lock_test_env();
        let temp = TempDir::new().unwrap();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        f(&temp);
        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn sidecar_store_tracks_retry_count_and_status() {
        with_temp_root(|temp| {
            let store = AutoRememberStore::open().unwrap();
            let evidence = vec!["SQLite sidecar is the audit store.".to_string()];
            assert_eq!(
                store.path(),
                &temp.path().join("data").join("memory-auto-remember.sqlite")
            );

            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-1",
                    candidate_hash: "hash-1",
                    signal_kind: "technical_decision",
                    workspace: "agentdesk-default",
                    stage: AutoRememberStage::Remember,
                    status: AutoRememberMemoryStatus::RememberFailed,
                    retry_count: 1,
                    error: Some("network error"),
                    raw_content: Some("SQLite sidecar is the audit store."),
                    entity_key: None,
                    supporting_evidence: Some(evidence.as_slice()),
                })
                .unwrap();

            let record = store
                .lookup_record("agentdesk-default", "hash-1")
                .unwrap()
                .unwrap();
            assert_eq!(record.status, AutoRememberMemoryStatus::RememberFailed);
            assert_eq!(record.retry_count, 1);
            assert_eq!(
                store
                    .next_failure_status("agentdesk-default", "hash-1")
                    .unwrap(),
                (AutoRememberMemoryStatus::RememberFailed, 2)
            );
        });
    }

    #[test]
    fn remember_failed_status_abandons_after_max_retries() {
        with_temp_root(|_| {
            let store = AutoRememberStore::open().unwrap();
            let evidence = vec!["memory.backend changed from file to memento.".to_string()];
            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-1",
                    candidate_hash: "hash-2",
                    signal_kind: "config_change",
                    workspace: "agentdesk-default",
                    stage: AutoRememberStage::Remember,
                    status: AutoRememberMemoryStatus::RememberFailed,
                    retry_count: AUTO_REMEMBER_MAX_RETRIES - 1,
                    error: Some("network error"),
                    raw_content: Some("memory.backend changed from file to memento."),
                    entity_key: Some("memory.backend"),
                    supporting_evidence: Some(evidence.as_slice()),
                })
                .unwrap();

            let (status, retry_count) = store
                .next_failure_status("agentdesk-default", "hash-2")
                .unwrap();
            assert_eq!(status, AutoRememberMemoryStatus::AbandonedAfterRetries);
            assert_eq!(retry_count, AUTO_REMEMBER_MAX_RETRIES);
            assert!(status.suppresses_repeat());
        });
    }

    #[test]
    fn audit_filters_and_summary_counts_match_observable_surface() {
        with_temp_root(|_| {
            let store = AutoRememberStore::open().unwrap();

            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-1",
                    candidate_hash: "hash-validate",
                    signal_kind: "technical_decision",
                    workspace: "agentdesk-default",
                    stage: AutoRememberStage::Validate,
                    status: AutoRememberMemoryStatus::ValidationSkipped,
                    retry_count: 0,
                    error: Some("uncertain"),
                    raw_content: Some("AgentDesk might switch stores later."),
                    entity_key: None,
                    supporting_evidence: None,
                })
                .unwrap();
            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-2",
                    candidate_hash: "hash-failed",
                    signal_kind: "technical_decision",
                    workspace: "agentdesk-default",
                    stage: AutoRememberStage::Remember,
                    status: AutoRememberMemoryStatus::RememberFailed,
                    retry_count: 2,
                    error: Some("temporary failure"),
                    raw_content: Some("SQLite sidecar is the audit store."),
                    entity_key: None,
                    supporting_evidence: None,
                })
                .unwrap();
            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-3",
                    candidate_hash: "hash-verified",
                    signal_kind: "config_change",
                    workspace: "other-workspace",
                    stage: AutoRememberStage::Verify,
                    status: AutoRememberMemoryStatus::OperatorVerified,
                    retry_count: 0,
                    error: Some("operator verified candidate"),
                    raw_content: Some("memory.backend changed from file to memento."),
                    entity_key: Some("memory.backend"),
                    supporting_evidence: None,
                })
                .unwrap();

            let filtered = store
                .list_audit_filtered(&AutoRememberAuditFilter {
                    workspace: Some("agentdesk-default"),
                    status: Some(AutoRememberMemoryStatus::ValidationSkipped),
                    stage: Some(AutoRememberStage::Validate),
                    signal_kind: Some("technical_decision"),
                    limit: 10,
                    ..AutoRememberAuditFilter::default()
                })
                .unwrap();
            assert_eq!(filtered.len(), 1);
            assert_eq!(filtered[0].candidate_hash, "hash-validate");
            assert_eq!(
                filtered[0].status,
                AutoRememberMemoryStatus::ValidationSkipped
            );
            assert_eq!(filtered[0].stage, AutoRememberStage::Validate);

            let status_counts = store.count_by_status(Some("agentdesk-default")).unwrap();
            assert!(
                status_counts
                    .iter()
                    .any(|(status, count)| status == "remember_failed" && *count == 1)
            );
            assert!(
                status_counts
                    .iter()
                    .any(|(status, count)| status == "validation_skipped" && *count == 1)
            );

            let skip_reason_counts = store
                .count_validation_skip_reasons(Some("agentdesk-default"))
                .unwrap();
            assert_eq!(skip_reason_counts, vec![("uncertain".to_string(), 1)]);
        });
    }

    #[test]
    fn manual_requeue_and_operator_verify_update_audit_and_retry_views() {
        with_temp_root(|_| {
            let store = AutoRememberStore::open().unwrap();
            let evidence = vec!["SQLite sidecar is the audit store.".to_string()];
            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-4",
                    candidate_hash: "hash-manual",
                    signal_kind: "technical_decision",
                    workspace: "agentdesk-default",
                    stage: AutoRememberStage::Remember,
                    status: AutoRememberMemoryStatus::AbandonedAfterRetries,
                    retry_count: 3,
                    error: Some("temporary failure"),
                    raw_content: Some("SQLite sidecar is the audit store."),
                    entity_key: None,
                    supporting_evidence: Some(evidence.as_slice()),
                })
                .unwrap();

            store
                .requeue_candidate("agentdesk-default", "hash-manual")
                .unwrap();

            let detail = store
                .load_audit_detail("agentdesk-default", "hash-manual")
                .unwrap()
                .unwrap();
            assert_eq!(detail.stage, AutoRememberStage::Remember);
            assert_eq!(detail.status, AutoRememberMemoryStatus::RememberFailed);
            assert_eq!(detail.retry_count, 0);
            assert_eq!(detail.error.as_deref(), Some("manual requeue requested"));

            let retry_batch = store.load_retry_batch().unwrap();
            assert_eq!(retry_batch.len(), 1);
            assert_eq!(retry_batch[0].candidate_hash, "hash-manual");
            assert_eq!(retry_batch[0].retry_count, 0);

            store
                .set_operator_status(
                    "agentdesk-default",
                    "hash-manual",
                    AutoRememberMemoryStatus::OperatorVerified,
                    Some("operator confirmed replay result"),
                )
                .unwrap();

            let verified_detail = store
                .load_audit_detail("agentdesk-default", "hash-manual")
                .unwrap()
                .unwrap();
            assert_eq!(verified_detail.stage, AutoRememberStage::Verify);
            assert_eq!(
                verified_detail.status,
                AutoRememberMemoryStatus::OperatorVerified
            );
            assert_eq!(
                verified_detail.error.as_deref(),
                Some("operator confirmed replay result")
            );
            assert!(store.load_retry_batch().unwrap().is_empty());
        });
    }

    #[test]
    fn open_existing_migrates_legacy_retry_count_columns() {
        with_temp_root(|temp| {
            let sidecar_path = temp.path().join("data").join("memory-auto-remember.sqlite");
            std::fs::create_dir_all(sidecar_path.parent().unwrap()).unwrap();

            let conn = Connection::open(&sidecar_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE auto_remember_audit (
                    workspace TEXT NOT NULL,
                    candidate_hash TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    signal_kind TEXT NOT NULL,
                    stage TEXT NOT NULL,
                    memory_status TEXT NOT NULL,
                    error TEXT,
                    raw_content TEXT,
                    entity_key TEXT,
                    supporting_evidence_json TEXT NOT NULL DEFAULT '[]',
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (workspace, candidate_hash)
                );
                CREATE TABLE auto_remember_retry_queue (
                    workspace TEXT NOT NULL,
                    candidate_hash TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    signal_kind TEXT NOT NULL,
                    raw_content TEXT NOT NULL,
                    entity_key TEXT,
                    supporting_evidence_json TEXT NOT NULL DEFAULT '[]',
                    error TEXT,
                    available_at_ms INTEGER NOT NULL DEFAULT 0,
                    updated_at_ms INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (workspace, candidate_hash)
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_remember_audit (
                    workspace,
                    candidate_hash,
                    turn_id,
                    signal_kind,
                    stage,
                    memory_status,
                    error,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    "agentdesk-default",
                    "legacy-hash",
                    "turn-legacy",
                    "technical_decision",
                    "remember",
                    "remember_failed",
                    "temporary failure",
                    "SQLite sidecar is the audit store.",
                    Option::<String>::None,
                    "[]",
                    "2026-04-19T00:00:00Z",
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_remember_retry_queue (
                    workspace,
                    candidate_hash,
                    turn_id,
                    signal_kind,
                    raw_content,
                    entity_key,
                    supporting_evidence_json,
                    error,
                    available_at_ms,
                    updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    "agentdesk-default",
                    "legacy-hash",
                    "turn-legacy",
                    "technical_decision",
                    "SQLite sidecar is the audit store.",
                    Option::<String>::None,
                    "[]",
                    "temporary failure",
                    0_i64,
                    0_i64,
                ],
            )
            .unwrap();
            drop(conn);

            let store = AutoRememberStore::open_existing()
                .unwrap()
                .expect("legacy sidecar should be opened");
            let record = store
                .lookup_record("agentdesk-default", "legacy-hash")
                .unwrap()
                .expect("legacy audit row should survive migration");
            assert_eq!(record.retry_count, 0);

            let retry_batch = store.load_retry_batch().unwrap();
            assert_eq!(retry_batch.len(), 1);
            assert_eq!(retry_batch[0].candidate_hash, "legacy-hash");
            assert_eq!(retry_batch[0].retry_count, 0);
        });
    }
}
