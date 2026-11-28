use super::{Check, CheckGroup};
use serde_json::json;
use std::{fs, io, path::Path};

const DISK_WARN_BYTES: u64 = 30 * 1024 * 1024 * 1024;
const DISK_FAIL_BYTES: u64 = 80 * 1024 * 1024 * 1024;
const DISK_USAGE_EXCLUDED_ROOT_CHILDREN: &[&str] = &["workspaces", "worktrees"];

#[cfg(test)]
thread_local! {
    static DISK_DIRECTORY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn disk_read_dir(path: &Path) -> std::io::Result<fs::ReadDir> {
    #[cfg(test)]
    DISK_DIRECTORY_VISITS.with(|visits| visits.set(visits.get() + 1));
    fs::read_dir(path)
}

#[cfg(test)]
#[test]
fn runtime_hygiene_doctor_visits_each_directory_once() {
    let temp = tempfile::tempdir().unwrap();
    let guard = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    for (dir, size) in [("runtime/nested", 3), ("workspaces", 7)] {
        let path = temp.path().join(dir);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("data"), vec![0; size]).unwrap();
    }
    DISK_DIRECTORY_VISITS.with(|visits| visits.set(0));

    let evidence = check_disk_usage().evidence.unwrap();

    assert_eq!(evidence["total_bytes"], 3);
    assert_eq!(evidence["raw_total_bytes"], 10);
    assert_eq!(evidence["top_children"][0]["name"], "workspaces");
    assert_eq!(evidence["top_children"][0]["excluded_from_threshold"], true);
    assert_eq!(DISK_DIRECTORY_VISITS.with(|visits| visits.get()), 4);
    drop(guard);
}

fn recursive_dir_size(path: &Path) -> std::io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in disk_read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(recursive_dir_size(&entry.path()).unwrap_or(0));
    }
    Ok(total)
}

fn scan_root(path: &Path) -> io::Result<(u64, Vec<(String, u64)>)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok((
            if metadata.is_file() {
                metadata.len()
            } else {
                0
            },
            Vec::new(),
        ));
    }

    let mut total = 0u64;
    let mut children = Vec::new();
    for entry in disk_read_dir(path)? {
        let entry = entry?;
        if let Ok(size) = recursive_dir_size(&entry.path()) {
            let name = entry
                .file_name()
                .into_string()
                .unwrap_or_else(|_| "unknown".into());
            total = total.saturating_add(size);
            children.push((name, size));
        }
    }
    Ok((total, children))
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
        Some(path) => match scan_root(&path) {
            Ok((raw_total, mut children)) => {
                let mut excluded_total = 0u64;
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
