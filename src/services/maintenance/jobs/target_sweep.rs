//! `storage.target_sweep` — monthly `cargo sweep --time 30` over the main
//! workspace `target/` directory, with a 50 GB disk-usage escape hatch.
//!
//! Resilience rules:
//!   * If `cargo-sweep` binary is missing (not installed on the host), log a
//!     single warning and return `Ok(())` — this is a best-effort cleanup, not
//!     a critical path.
//!   * If the target directory doesn't exist (e.g. a fresh checkout), also
//!     return `Ok(())` silently.
//!   * Disk-usage measurement is walk-based (via `walkdir`-style `std::fs`
//!     recursion). For a `target/` dir this is O(N files) but N is bounded and
//!     we only run monthly, so the cost is negligible.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// 50 GB threshold (bytes).
pub const DISK_THRESHOLD_BYTES: u64 = 50 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// Workspace root (`target/` is resolved relative to this).
    pub workspace_root: PathBuf,
    /// `cargo sweep --time N` value.
    pub sweep_time_days: u32,
    /// Disk-usage threshold (bytes) that forces a sweep regardless of cadence.
    pub disk_threshold_bytes: u64,
    /// If true, skip actually invoking `cargo sweep` (used by tests). The
    /// function still measures disk usage and logs what it *would* do.
    pub dry_run: bool,
}

impl Config {
    /// Default config for production: the managed workspace repo
    /// (`~/.adk/release/workspaces/agentdesk`), whose `target/` is the
    /// build-cache that accumulates and triggered the #3231 disk-full incident.
    ///
    /// In the release runtime the process cwd is `AGENTDESK_ROOT_DIR`
    /// (`~/.adk/release`) and `CARGO_MANIFEST_DIR` is unset, so the old
    /// `CARGO_MANIFEST_DIR`→`"."` fallback resolved `~/.adk/release/target`
    /// (wrong/absent) and the sweep silently no-op'd. Resolve via the runtime
    /// root when available; fall back to `CARGO_MANIFEST_DIR` (dev checkouts)
    /// then `"."`.
    pub fn default_runtime() -> Self {
        // Order matters: `CARGO_MANIFEST_DIR` is set by cargo during dev/test runs
        // (resolving the actual checkout) but is UNSET in the deployed release
        // binary, so it cleanly distinguishes dev from prod. In prod we fall
        // through to the runtime root (`agentdesk_runtime_root()` →
        // `config::runtime_root()`, which yields `$AGENTDESK_ROOT_DIR` or
        // `$HOME/.adk/release`) and target the managed repo
        // `~/.adk/release/workspaces/agentdesk`. Final `"."` guards the rare case
        // where neither resolves. (Putting runtime_root first would shadow the
        // dev checkout because runtime_root always returns Some.)
        let workspace_root = std::env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                crate::cli::agentdesk_runtime_root().map(|root| root.join("workspaces/agentdesk"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            workspace_root,
            sweep_time_days: 30,
            disk_threshold_bytes: DISK_THRESHOLD_BYTES,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub target_exists: bool,
    pub disk_usage_bytes: u64,
    pub threshold_triggered: bool,
    pub cargo_sweep_available: bool,
    pub invoked_sweep: bool,
    pub removed_files: u64,
    pub removed_bytes: u64,
}

pub async fn run(config: Config) -> Result<()> {
    let report = collect_and_sweep(config).await?;
    tracing::info!(
        target: "maintenance",
        job = "storage.target_sweep",
        target_exists = report.target_exists,
        disk_usage_bytes = report.disk_usage_bytes,
        threshold_triggered = report.threshold_triggered,
        cargo_sweep_available = report.cargo_sweep_available,
        invoked_sweep = report.invoked_sweep,
        removed_files = report.removed_files,
        removed_bytes = report.removed_bytes,
        "target_sweep completed"
    );
    Ok(())
}

/// Split out for unit-testability — returns the structured report.
pub async fn collect_and_sweep(config: Config) -> Result<SweepReport> {
    let target_dir = config.workspace_root.join("target");
    let target_exists = target_dir.exists();

    if !target_exists {
        return Ok(SweepReport {
            target_exists: false,
            ..SweepReport::default()
        });
    }

    let disk_usage_bytes = measure_dir_size(&target_dir).unwrap_or(0);
    let threshold_triggered = disk_usage_bytes >= config.disk_threshold_bytes;

    let cargo_sweep_available = which::which("cargo-sweep").is_ok();

    if config.dry_run || !cargo_sweep_available {
        return Ok(SweepReport {
            target_exists: true,
            disk_usage_bytes,
            threshold_triggered,
            cargo_sweep_available,
            invoked_sweep: false,
            removed_files: 0,
            removed_bytes: 0,
        });
    }

    // Run: `cargo sweep --time <N>` in the workspace root. Output is parsed
    // loosely for a "bytes freed" / "files removed" line; on parse miss we
    // still report invocation success.
    let output = tokio::process::Command::new("cargo")
        .arg("sweep")
        .arg("--time")
        .arg(config.sweep_time_days.to_string())
        .current_dir(&config.workspace_root)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (removed_files, removed_bytes) = parse_cargo_sweep_output(&stdout);

    Ok(SweepReport {
        target_exists: true,
        disk_usage_bytes,
        threshold_triggered,
        cargo_sweep_available: true,
        invoked_sweep: true,
        removed_files,
        removed_bytes,
    })
}

/// Recursively sum file sizes under `dir`. Best-effort: fs errors are swallowed
/// per-entry (we don't want a single permission-denied to abort the whole
/// maintenance tick).
fn measure_dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

/// Parse `cargo sweep` stdout for summary lines. `cargo-sweep` prints a line
/// like `Total bytes cleaned: 1234567890` and per-file remove lines; we match
/// the former and count the latter.
fn parse_cargo_sweep_output(stdout: &str) -> (u64, u64) {
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("removing") {
            removed_files = removed_files.saturating_add(1);
        }
        if let Some(rest) = line.split("cleaned:").nth(1) {
            // Accept either bare integer or human-readable variants; keep it simple.
            if let Some(num) = rest
                .trim()
                .split_whitespace()
                .next()
                .and_then(|s| s.replace(['_', ','], "").parse::<u64>().ok())
            {
                removed_bytes = num;
            }
        }
    }
    (removed_files, removed_bytes)
}
