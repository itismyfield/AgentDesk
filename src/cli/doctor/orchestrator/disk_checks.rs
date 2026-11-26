use super::{Check, CheckGroup, FixSafety, Path, SecurityExposure, config, fs, json};

pub(super) fn check_github_repo_registry(cfg: &config::Config) -> Check {
    let db_path = cfg.data.dir.join(&cfg.data.db_name);
    if crate::db::postgres::database_enabled(cfg) {
        return Check::ok(
            "github_repo_registry",
            CheckGroup::Core,
            "Legacy SQLite GitHub Repo Registry",
            "demoted while Postgres is enabled",
        )
        .with_subsystem("sqlite_cache")
        .with_fix_safety(FixSafety::ReadOnly)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_path(db_path.display().to_string())
        .with_expected_actual(
            "Postgres source-of-truth active; SQLite github_repos is non-authoritative",
            "legacy SQLite registry check demoted",
        );
    }
    return Check::warn(
        "github_repo_registry",
        CheckGroup::Core,
        "Legacy SQLite GitHub Repo Registry",
        "retired from normal builds",
        "Postgres is the AgentDesk source-of-truth; legacy SQLite github_repos comparison is no longer compiled.",
    )
    .with_subsystem("sqlite_cache")
    .with_fix_safety(FixSafety::ReadOnly)
    .with_security_exposure(SecurityExposure::OperationalMetadata)
    .with_path(db_path.display().to_string())
    .with_expected_actual("Postgres source-of-truth active", "SQLite registry check retired");
}

const DISK_WARN_BYTES: u64 = 30 * 1024 * 1024 * 1024;
const DISK_FAIL_BYTES: u64 = 80 * 1024 * 1024 * 1024;
const DISK_USAGE_EXCLUDED_ROOT_CHILDREN: &[&str] = &["workspaces", "worktrees"];

fn recursive_dir_size(path: &Path) -> std::io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(recursive_dir_size(&entry.path()).unwrap_or(0));
    }
    Ok(total)
}

pub(super) fn check_disk_usage() -> Check {
    match crate::config::runtime_root() {
        Some(path) if !path.exists() => Check::warn(
            "disk_usage",
            CheckGroup::Core,
            "Disk Usage",
            format!("{} — runtime root missing", path.display()),
            "you can create the default runtime directory with agentdesk doctor --fix.",
        )
        .with_path(path.display().to_string())
        .with_expected_actual(
            "runtime root exists for disk usage scan",
            "runtime root missing",
        )
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
        Some(path) => match recursive_dir_size(&path) {
            Ok(raw_total) => {
                let mut excluded_total = 0u64;
                let mut children = fs::read_dir(&path)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        let entry = entry.ok()?;
                        let child_path = entry.path();
                        let name = child_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let size = recursive_dir_size(&child_path).ok()?;
                        Some((name, size))
                    })
                    .collect::<Vec<_>>();
                for (name, size) in &children {
                    if DISK_USAGE_EXCLUDED_ROOT_CHILDREN.contains(&name.as_str()) {
                        excluded_total = excluded_total.saturating_add(*size);
                    }
                }
                let total = raw_total.saturating_sub(excluded_total);
                let mb = total as f64 / 1_048_576.0;
                children.sort_by(|left, right| right.1.cmp(&left.1));
                let top_children = children
                    .into_iter()
                    .take(5)
                    .map(|(name, size)| {
                        json!({
                            "name": name,
                            "mb": (size as f64 / 1_048_576.0),
                            "excluded_from_threshold": DISK_USAGE_EXCLUDED_ROOT_CHILDREN.contains(&name.as_str())
                        })
                    })
                    .collect::<Vec<_>>();
                let evidence = json!({
                    "runtime_root": path.display().to_string(),
                    "total_bytes": total,
                    "raw_total_bytes": raw_total,
                    "excluded_root_children": DISK_USAGE_EXCLUDED_ROOT_CHILDREN,
                    "excluded_bytes": excluded_total,
                    "warn_threshold_bytes": DISK_WARN_BYTES,
                    "fail_threshold_bytes": DISK_FAIL_BYTES,
                    "top_children": top_children
                });
                let detail = if excluded_total > 0 {
                    format!(
                        "{:.1} MB operational usage in {} ({:.1} MB excluded: workspaces/worktrees)",
                        mb,
                        path.display(),
                        excluded_total as f64 / 1_048_576.0
                    )
                } else {
                    format!("{:.1} MB recursively in {}", mb, path.display())
                };
                if total >= DISK_FAIL_BYTES {
                    Check::fail(
                        "disk_usage",
                        CheckGroup::Core,
                        "Disk Usage",
                        detail.clone(),
                        "runtime root disk usage exceeded failure threshold; inspect large child directories before deleting anything.",
                    )
                    .with_path(path.display().to_string())
                    .with_expected_actual(
                        format!("< {:.1} MB", DISK_FAIL_BYTES as f64 / 1_048_576.0),
                        format!("{:.1} MB", mb),
                    )
                    .with_evidence(evidence)
                    .with_next_steps(vec![format!("du -sh {}/*", path.display())])
                } else if total >= DISK_WARN_BYTES {
                    Check::warn(
                        "disk_usage",
                        CheckGroup::Core,
                        "Disk Usage",
                        detail.clone(),
                        "runtime root disk usage is above warning threshold; review generated logs/artifacts.",
                    )
                    .with_path(path.display().to_string())
                    .with_expected_actual(
                        format!("< {:.1} MB", DISK_WARN_BYTES as f64 / 1_048_576.0),
                        format!("{:.1} MB", mb),
                    )
                    .with_evidence(evidence)
                    .with_next_steps(vec![format!("du -sh {}/*", path.display())])
                } else {
                    Check::ok("disk_usage", CheckGroup::Core, "Disk Usage", detail)
                        .with_path(path.display().to_string())
                        .with_expected_actual("disk usage below threshold", format!("{:.1} MB", mb))
                        .with_evidence(evidence)
                }
            }
            Err(e) => Check::warn(
                "disk_usage",
                CheckGroup::Core,
                "Disk Usage",
                format!("{} — unreadable ({e})", path.display()),
                "runtime root 권한을 확인하세요.",
            )
            .with_path(path.display().to_string())
            .with_expected_actual(
                "runtime root readable",
                format!("runtime root unreadable: {e}"),
            )
            .with_next_steps(vec![format!("ls -la {}", path.display())]),
        },
        None => Check::fail(
            "disk_usage",
            CheckGroup::Core,
            "Disk Usage",
            "cannot determine runtime root",
            "check AGENTDESK_ROOT_DIR or the default ~/.adk/release path.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        ),
    }
}
