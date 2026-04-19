use std::fs;
use std::path::PathBuf;

use libsql_rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutoRememberMemoryStatus {
    Remembered,
    DuplicateSkip,
    RememberFailed,
}

impl AutoRememberMemoryStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Remembered => "remembered",
            Self::DuplicateSkip => "duplicate_skip",
            Self::RememberFailed => "remember_failed",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "remembered" => Some(Self::Remembered),
            "duplicate_skip" => Some(Self::DuplicateSkip),
            "remember_failed" => Some(Self::RememberFailed),
            _ => None,
        }
    }

    pub(crate) fn suppresses_repeat(self) -> bool {
        matches!(self, Self::Remembered | Self::DuplicateSkip)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberAuditEntry<'a> {
    pub(crate) turn_id: &'a str,
    pub(crate) candidate_hash: &'a str,
    pub(crate) signal_kind: &'a str,
    pub(crate) workspace: &'a str,
    pub(crate) status: AutoRememberMemoryStatus,
    pub(crate) error: Option<&'a str>,
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
        store.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS auto_remember_audit (
                    workspace TEXT NOT NULL,
                    candidate_hash TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    signal_kind TEXT NOT NULL,
                    memory_status TEXT NOT NULL,
                    error TEXT,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (workspace, candidate_hash)
                );",
            )
        })?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &PathBuf {
        &self.path
    }

    pub(crate) fn lookup_status(
        &self,
        workspace: &str,
        candidate_hash: &str,
    ) -> Result<Option<AutoRememberMemoryStatus>, String> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT memory_status
                 FROM auto_remember_audit
                 WHERE workspace = ?1 AND candidate_hash = ?2",
                params![workspace, candidate_hash],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|status| {
                status
                    .as_deref()
                    .and_then(AutoRememberMemoryStatus::from_str)
            })
        })
    }

    pub(crate) fn upsert_audit(&self, entry: AutoRememberAuditEntry<'_>) -> Result<(), String> {
        let error = entry
            .error
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO auto_remember_audit (
                    workspace,
                    candidate_hash,
                    turn_id,
                    signal_kind,
                    memory_status,
                    error,
                    created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(workspace, candidate_hash) DO UPDATE SET
                    turn_id = excluded.turn_id,
                    signal_kind = excluded.signal_kind,
                    memory_status = excluded.memory_status,
                    error = excluded.error,
                    created_at = excluded.created_at",
                params![
                    entry.workspace,
                    entry.candidate_hash,
                    entry.turn_id,
                    entry.signal_kind,
                    entry.status.as_str(),
                    error,
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                ],
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
    Ok(root.join("data").join("memory-auto-remember.sqlite"))
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
    fn sidecar_store_uses_workspace_hash_dedup_key() {
        with_temp_root(|temp| {
            let store = AutoRememberStore::open().unwrap();
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
                    status: AutoRememberMemoryStatus::Remembered,
                    error: None,
                })
                .unwrap();

            assert_eq!(
                store.lookup_status("agentdesk-default", "hash-1").unwrap(),
                Some(AutoRememberMemoryStatus::Remembered)
            );

            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-2",
                    candidate_hash: "hash-1",
                    signal_kind: "technical_decision",
                    workspace: "agentdesk-default",
                    status: AutoRememberMemoryStatus::DuplicateSkip,
                    error: None,
                })
                .unwrap();

            assert_eq!(
                store.lookup_status("agentdesk-default", "hash-1").unwrap(),
                Some(AutoRememberMemoryStatus::DuplicateSkip)
            );
        });
    }

    #[test]
    fn remember_failed_status_does_not_suppress_retries() {
        with_temp_root(|_| {
            let store = AutoRememberStore::open().unwrap();
            store
                .upsert_audit(AutoRememberAuditEntry {
                    turn_id: "turn-1",
                    candidate_hash: "hash-2",
                    signal_kind: "config_change",
                    workspace: "agentdesk-default",
                    status: AutoRememberMemoryStatus::RememberFailed,
                    error: Some("network error"),
                })
                .unwrap();

            let status = store.lookup_status("agentdesk-default", "hash-2").unwrap();
            assert_eq!(status, Some(AutoRememberMemoryStatus::RememberFailed));
            assert!(!status.unwrap().suppresses_repeat());
        });
    }
}
