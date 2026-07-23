use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{
    RelayQueueFileLock, file_is_older_than, lock_relay_queue_file, lock_relay_queue_file_with_mode,
};

pub(super) struct IdleQueueRetentionGuard {
    queue_dir: PathBuf,
    lock_files_existed: bool,
    _worker_lock: RelayQueueFileLock,
}

impl IdleQueueRetentionGuard {
    pub(super) fn acquire(queue_dir: &Path) -> Option<Self> {
        let worker_lock_path = queue_dir.join("worker.lock");
        let producer_lock_path = queue_dir.join("producer.lock");
        let lock_files_existed = worker_lock_path.exists() && producer_lock_path.exists();
        let worker_lock = lock_relay_queue_file(&worker_lock_path, true).ok()??;
        Some(Self {
            queue_dir: queue_dir.to_path_buf(),
            lock_files_existed,
            _worker_lock: worker_lock,
        })
    }

    pub(super) fn remove_if_stale_and_idle(&self, retention: Duration) {
        if !self.lock_files_existed {
            return;
        }
        let Some(last_activity) = latest_queue_activity(&self.queue_dir).ok().flatten() else {
            return;
        };
        if !last_activity.elapsed().is_ok_and(|age| age >= retention) {
            return;
        }
        let producer_lock_path = self.queue_dir.join("producer.lock");
        let Ok(Some(_producer_lock)) =
            lock_relay_queue_file_with_mode(&producer_lock_path, true, true)
        else {
            return;
        };
        // Producers may publish while artifact pruning runs. The exclusive producer lock is
        // acquired only for this final revalidation and removal, so a stale pre-lock scan can
        // never authorize deletion and hook-boundary blocking excludes unbounded pruning work.
        if queue_contains_only_idle_state(&self.queue_dir) {
            remove_idle_queue_dir(&self.queue_dir);
        }
    }
}

pub(super) fn prune_artifact_dir(dir: &Path, cap: usize, retention: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let excess = paths.len().saturating_sub(cap);
    for (index, path) in paths.into_iter().enumerate() {
        if index < excess || file_is_older_than(&path, retention) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn latest_queue_activity(queue_dir: &Path) -> Result<Option<SystemTime>, String> {
    let mut latest = std::fs::symlink_metadata(queue_dir)
        .and_then(|metadata| metadata.modified())
        .ok();
    let entries = std::fs::read_dir(queue_dir).map_err(|error| {
        format!(
            "read hook relay queue activity {}: {error}",
            queue_dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "read hook relay queue activity {}: {error}",
                queue_dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            format!("read hook relay queue metadata {}: {error}", path.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Ok(None);
        }
        latest = latest.max(metadata.modified().ok());
        if metadata.is_dir() {
            let children = std::fs::read_dir(&path).map_err(|error| {
                format!("read hook relay queue activity {}: {error}", path.display())
            })?;
            for child in children {
                let child = child.map_err(|error| {
                    format!("read hook relay queue activity {}: {error}", path.display())
                })?;
                let child_path = child.path();
                let child_metadata = std::fs::symlink_metadata(&child_path).map_err(|error| {
                    format!(
                        "read hook relay queue metadata {}: {error}",
                        child_path.display()
                    )
                })?;
                if child_metadata.file_type().is_symlink() || child_metadata.is_dir() {
                    return Ok(None);
                }
                latest = latest.max(child_metadata.modified().ok());
            }
        }
    }
    Ok(latest)
}

fn queue_contains_only_idle_state(queue_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(queue_dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
        if metadata.is_dir() {
            if !matches!(name, "ingress" | "responses" | "quarantine")
                || std::fs::read_dir(&path)
                    .map(|mut entries| entries.next().is_some())
                    .unwrap_or(true)
            {
                return false;
            }
        } else if !matches!(
            name,
            "worker.lock" | "producer.lock" | "next-sequence" | "completed-high-water"
        ) {
            return false;
        }
    }
    true
}

fn remove_idle_queue_dir(queue_dir: &Path) {
    for path in [
        queue_dir.join("next-sequence"),
        queue_dir.join("completed-high-water"),
        queue_dir.join("producer.lock"),
    ] {
        let _ = std::fs::remove_file(path);
    }
    for path in [
        queue_dir.join("ingress"),
        queue_dir.join("responses"),
        queue_dir.join("quarantine"),
    ] {
        let _ = std::fs::remove_dir(path);
    }
    let _ = std::fs::remove_file(queue_dir.join("worker.lock"));
    let _ = std::fs::remove_dir(queue_dir);
}
