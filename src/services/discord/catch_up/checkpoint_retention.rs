use crate::services::discord::runtime_store::checkpoint_lock::CheckpointLock;
use crate::services::maintenance::runtime_retention;
use std::{
    collections::HashSet,
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

pub(super) fn prune_stale_checkpoint_files(dir: &Path, max_checkpoint_age: Duration) -> usize {
    let now = SystemTime::now();
    let Some(cutoff) = runtime_retention::cutoff(now) else {
        return 0;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let channels: HashSet<_> = entries
        .flatten()
        .filter_map(|entry| channel_stem(entry.file_name().to_str()?))
        .collect();
    channels
        .into_iter()
        .map(|channel| prune_channel(dir, channel, now, max_checkpoint_age, cutoff))
        .sum()
}

fn channel_stem(name: &str) -> Option<u64> {
    let (stem, suffix) = name.split_once('.')?;
    if !matches!(
        suffix,
        "txt" | "sweep.json" | "txt.lock" | "sweep.json.lock"
    ) {
        return None;
    }
    let channel = stem.parse::<u64>().ok()?;
    (channel != 0 && stem == channel.to_string()).then_some(channel)
}

fn prune_channel(
    dir: &Path,
    channel: u64,
    now: SystemTime,
    max_checkpoint_age: Duration,
    cutoff: SystemTime,
) -> usize {
    let checkpoint = dir.join(format!("{channel}.txt"));
    let scan = checkpoint.with_extension("sweep.json");
    let checkpoint_lock = checkpoint.with_extension("txt.lock");
    let scan_lock = scan.with_extension("json.lock");
    let checkpoint_expired = || {
        runtime_retention::regular_file_modified(&checkpoint)
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age > max_checkpoint_age)
    };
    let old = |path: &Path| runtime_retention::regular_file_expired(path, cutoff);
    if !checkpoint_expired() && !old(&scan) && !old(&checkpoint_lock) && !old(&scan_lock) {
        return 0;
    }

    let Ok(checkpoint_guard) = CheckpointLock::acquire(&checkpoint_lock, false) else {
        return 0;
    };
    let Ok(scan_guard) = CheckpointLock::acquire(&scan_lock, false) else {
        if checkpoint_guard.created {
            unlink(&checkpoint_lock);
        }
        return 0;
    };

    let pruned = checkpoint_expired() && unlink(&checkpoint);
    if old(&scan) {
        unlink(&scan);
    }

    for (lock_path, guard) in [
        (&checkpoint_lock, &checkpoint_guard),
        (&scan_lock, &scan_guard),
    ] {
        if guard.created || old(lock_path) {
            unlink(lock_path);
        }
    }
    usize::from(pruned)
}

fn unlink(path: &Path) -> bool {
    match crate::runtime_layout::remove_file_if_present(path) {
        Ok(removed) => removed,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "checkpoint retention remove failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_hygiene_checkpoint_prunes_old_unlocked_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let old = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60),
        );
        let suffixes = ["txt", "txt.lock", "sweep.json", "sweep.json.lock"];
        for channel in [21, 22, 23, 24] {
            for suffix in suffixes {
                let path = temp.path().join(format!("{channel}.{suffix}"));
                std::fs::write(&path, "").unwrap();
                if channel != 24 {
                    filetime::set_file_mtime(&path, old).unwrap();
                }
            }
        }
        let orphan = temp.path().join("25.txt.lock");
        std::fs::write(&orphan, "").unwrap();
        filetime::set_file_mtime(&orphan, old).unwrap();
        let locks = ["22.txt.lock", "23.sweep.json.lock"].map(|name| {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp.path().join(name))
                .unwrap();
            file.lock().unwrap();
            file
        });

        prune_stale_checkpoint_files(temp.path(), Duration::from_secs(600));

        for channel in [22, 23, 24] {
            for suffix in suffixes {
                assert!(temp.path().join(format!("{channel}.{suffix}")).exists());
            }
        }
        for suffix in suffixes {
            assert!(!temp.path().join(format!("21.{suffix}")).exists());
        }
        assert!(!orphan.exists());
        drop(locks);
    }
}
