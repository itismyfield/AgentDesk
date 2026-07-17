use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::services::discord::runtime_store;

/// Delete a checkpoint file whose mtime is older than `max_checkpoint_age`.
///
/// The raw intake checkpoint is advanced before a turn is dispatched, so it can
/// name a received-but-undispatched message. Pruning must never reinterpret that
/// cursor as settlement evidence; deleting it lets the message re-enter the
/// Recent/TooOld path (#4564 P1).
///
/// This path intentionally does not parse checkpoint contents. An unparseable
/// stale cursor should be removed rather than survive and warn on every sweep.
pub(super) fn prune_stale_checkpoint(
    checkpoint_path: &Path,
    max_checkpoint_age: Duration,
) -> Result<bool, String> {
    let _lock = runtime_store::lock_last_message_id_path(checkpoint_path)?;
    let Ok(metadata) = fs::metadata(checkpoint_path) else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    let Ok(modified) = metadata.modified() else {
        return Ok(false);
    };
    if modified.elapsed().unwrap_or_default() <= max_checkpoint_age {
        return Ok(false);
    }
    fs::remove_file(checkpoint_path).map_err(|error| {
        format!(
            "failed to prune stale checkpoint at {}: {error}",
            checkpoint_path.display()
        )
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::prune_stale_checkpoint;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn age(path: &std::path::Path, secs: u64) {
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(secs)),
        )
        .expect("age file mtime");
    }

    #[test]
    fn prune_deletes_stale_checkpoint_without_minting_a_settled_sidecar() {
        let temp = tempfile::tempdir().expect("create checkpoint prune test dir");
        let checkpoint_path = temp.path().join("123.txt");
        let settled_path = temp.path().join("123.txt.settled");
        fs::write(&checkpoint_path, "456").expect("write checkpoint");
        age(&checkpoint_path, 700);

        assert!(
            prune_stale_checkpoint(&checkpoint_path, Duration::from_secs(600))
                .expect("prune stale checkpoint"),
            "a stale checkpoint must be pruned"
        );
        assert!(
            !checkpoint_path.exists(),
            "the stale checkpoint file must be removed"
        );
        assert!(
            !settled_path.exists(),
            "prune must not promote the raw checkpoint into settled evidence"
        );
    }

    #[test]
    fn prune_leaves_an_existing_settled_sidecar_untouched() {
        let temp = tempfile::tempdir().expect("create checkpoint prune test dir");
        let checkpoint_path = temp.path().join("123.txt");
        let settled_path = temp.path().join("123.txt.settled");
        fs::write(&checkpoint_path, "456").expect("write checkpoint");
        fs::write(&settled_path, "789").expect("write settled sidecar");
        age(&checkpoint_path, 700);

        assert!(
            prune_stale_checkpoint(&checkpoint_path, Duration::from_secs(600))
                .expect("prune stale checkpoint")
        );
        assert!(!checkpoint_path.exists(), "stale checkpoint removed");
        assert_eq!(
            fs::read_to_string(&settled_path).expect("read settled sidecar"),
            "789",
            "prune must not mutate independent sidecars"
        );
    }

    #[test]
    fn prune_removes_an_unparseable_stale_checkpoint() {
        let temp = tempfile::tempdir().expect("create checkpoint prune test dir");
        let checkpoint_path = temp.path().join("123.txt");
        fs::write(&checkpoint_path, "not-a-decimal-id").expect("write garbage checkpoint");
        age(&checkpoint_path, 700);

        assert!(
            prune_stale_checkpoint(&checkpoint_path, Duration::from_secs(600))
                .expect("prune must not parse stale contents"),
            "an unparseable stale checkpoint must be pruned"
        );
        assert!(
            !checkpoint_path.exists(),
            "the unparseable stale checkpoint file must be removed"
        );
    }

    #[test]
    fn fresh_checkpoint_is_not_pruned() {
        let temp = tempfile::tempdir().expect("create checkpoint prune test dir");
        let checkpoint_path = temp.path().join("123.txt");
        fs::write(&checkpoint_path, "456").expect("write checkpoint");

        assert!(
            !prune_stale_checkpoint(&checkpoint_path, Duration::from_secs(600))
                .expect("prune fresh checkpoint"),
            "a checkpoint newer than max_checkpoint_age must survive"
        );
        assert!(
            checkpoint_path.exists(),
            "fresh checkpoint file must survive"
        );
    }
}
