//! `storage.hang_dump_cleanup` — weekly deletion of `adk-hang-*.txt` files
//! older than 14 days.
//!
//! Rust equivalent of `find logs/ -name 'adk-hang-*.txt' -mtime +14 -delete`.
//!
//! Safety notes:
//!   * Only matches the `adk-hang-` prefix + `.txt` suffix — never touches
//!     unrelated log files.
//!   * Uses file-modified-time, not created-time, because macOS reports
//!     `mtime` consistently across filesystems while `ctime` has different
//!     semantics on APFS/HFS+.
//!   * Directory-scan failures are swallowed per-entry; the job never aborts
//!     halfway through.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

/// Default retention: 14 days.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct Config {
    pub logs_dir: PathBuf,
    pub max_age: Duration,
}

impl Config {
    pub fn default_runtime() -> Self {
        // Prefer `~/.adk/release/logs/` when the runtime root is resolvable;
        // otherwise fall back to `./logs/` (works in dev checkouts).
        let logs_dir = crate::cli::agentdesk_runtime_root()
            .map(|root| root.join("logs"))
            .unwrap_or_else(|| PathBuf::from("logs"));
        Self {
            logs_dir,
            max_age: DEFAULT_MAX_AGE,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub scanned_files: u64,
    pub deleted_files: u64,
    pub deleted_bytes: u64,
    pub errors: u64,
}

pub async fn run(config: Config) -> Result<()> {
    let report = run_inner(&config)?;
    tracing::info!(
        target: "maintenance",
        job = "storage.hang_dump_cleanup",
        logs_dir = %config.logs_dir.display(),
        scanned = report.scanned_files,
        deleted = report.deleted_files,
        deleted_bytes = report.deleted_bytes,
        errors = report.errors,
        "hang_dump_cleanup completed"
    );
    Ok(())
}

pub fn run_inner(config: &Config) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    if !config.logs_dir.exists() {
        return Ok(report);
    }

    let cutoff = SystemTime::now()
        .checked_sub(config.max_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let Ok(entries) = std::fs::read_dir(&config.logs_dir) else {
        return Ok(report);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_hang_dump(&path) {
            continue;
        }
        report.scanned_files = report.scanned_files.saturating_add(1);

        let Ok(metadata) = entry.metadata() else {
            report.errors = report.errors.saturating_add(1);
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            report.errors = report.errors.saturating_add(1);
            continue;
        };
        if modified >= cutoff {
            continue; // not old enough
        }

        let size = metadata.len();
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.deleted_files = report.deleted_files.saturating_add(1);
                report.deleted_bytes = report.deleted_bytes.saturating_add(size);
            }
            Err(error) => {
                tracing::warn!(
                    target: "maintenance",
                    path = %path.display(),
                    error = %error,
                    "hang_dump_cleanup: failed to delete file"
                );
                report.errors = report.errors.saturating_add(1);
            }
        }
    }

    Ok(report)
}

fn is_hang_dump(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    name.starts_with("adk-hang-") && name.ends_with(".txt")
}
