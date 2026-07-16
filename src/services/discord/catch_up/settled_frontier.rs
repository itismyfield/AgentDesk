use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::services::discord::runtime_store::{self, AtomicWriteContext};

/// Durable settled evidence is adjacent to the mutable checkpoint but uses a
/// non-`.txt` extension so checkpoint discovery and mtime pruning ignore it.
fn frontier_path(checkpoint_path: &Path) -> PathBuf {
    checkpoint_path.with_extension("txt.settled")
}

fn parse_frontier(path: &Path, contents: &str) -> Result<u64, String> {
    contents
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid settled frontier at {}: {error}", path.display()))
}

fn read_optional_frontier(path: &Path) -> Result<Option<u64>, String> {
    match fs::read_to_string(path) {
        Ok(contents) => parse_frontier(path, &contents).map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "failed to read settled frontier at {}: {error}",
            path.display()
        )),
    }
}

pub(super) fn load(dir: &Path, channel_id: u64) -> Option<u64> {
    let path = dir.join(format!("{channel_id}.txt.settled"));
    match read_optional_frontier(&path) {
        Ok(frontier) => frontier,
        Err(error) => {
            tracing::warn!(
                channel_id,
                path = %path.display(),
                error = %error,
                "catch-up settled frontier could not be loaded"
            );
            None
        }
    }
}

pub(super) fn contains(frontier: Option<u64>, message_id: u64) -> bool {
    frontier.is_some_and(|frontier| message_id <= frontier)
}

pub(super) fn prune_stale_checkpoint(
    checkpoint_path: &Path,
    max_checkpoint_age: Duration,
) -> Result<bool, String> {
    let _lock = runtime_store::lock_last_message_id_path(checkpoint_path)?;
    let metadata = fs::metadata(checkpoint_path).map_err(|error| {
        format!(
            "failed to inspect checkpoint at {}: {error}",
            checkpoint_path.display()
        )
    })?;
    if !metadata.is_file() {
        return Ok(false);
    }
    let modified = metadata.modified().map_err(|error| {
        format!(
            "failed to read checkpoint mtime at {}: {error}",
            checkpoint_path.display()
        )
    })?;
    if modified.elapsed().unwrap_or_default() <= max_checkpoint_age {
        return Ok(false);
    }

    let checkpoint_contents = fs::read_to_string(checkpoint_path).map_err(|error| {
        format!(
            "failed to read stale checkpoint at {}: {error}",
            checkpoint_path.display()
        )
    })?;
    let checkpoint = parse_frontier(checkpoint_path, &checkpoint_contents)?;
    let settled_path = frontier_path(checkpoint_path);
    let settled = read_optional_frontier(&settled_path)?
        .map_or(checkpoint, |frontier| frontier.max(checkpoint));
    let channel_id = checkpoint_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse::<u64>().ok());
    let provider = checkpoint_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let mut context = AtomicWriteContext::new("catch_up_settled_frontier");
    if let Some(provider) = provider {
        context = context.provider(provider);
    }
    if let Some(channel_id) = channel_id {
        context = context.channel_id(channel_id);
    }
    runtime_store::critical_atomic_write(&settled_path, &settled.to_string(), context)?;
    fs::remove_file(checkpoint_path).map_err(|error| {
        format!(
            "failed to prune promoted checkpoint at {}: {error}",
            checkpoint_path.display()
        )
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{frontier_path, load, prune_stale_checkpoint};
    use std::fs;
    use std::time::{Duration, SystemTime};

    #[test]
    fn settled_frontier_uses_checkpoint_sidecar_path_and_decimal_format() {
        let temp = tempfile::tempdir().expect("create settled-frontier test dir");
        let checkpoint_path = temp.path().join("123.txt");
        let settled_path = frontier_path(&checkpoint_path);
        assert_eq!(settled_path, temp.path().join("123.txt.settled"));

        fs::write(&checkpoint_path, "456").expect("write checkpoint");
        filetime::set_file_mtime(
            &checkpoint_path,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(700)),
        )
        .expect("age checkpoint");

        assert!(
            prune_stale_checkpoint(&checkpoint_path, Duration::from_secs(600))
                .expect("promote stale checkpoint")
        );
        assert_eq!(
            fs::read_to_string(&settled_path).expect("read settled sidecar"),
            "456"
        );
        assert_eq!(load(temp.path(), 123), Some(456));
    }

    #[test]
    fn promotion_never_moves_an_existing_frontier_backward() {
        let temp = tempfile::tempdir().expect("create settled-frontier test dir");
        let checkpoint_path = temp.path().join("123.txt");
        let settled_path = frontier_path(&checkpoint_path);
        fs::write(&checkpoint_path, "456").expect("write checkpoint");
        fs::write(&settled_path, "789").expect("write settled sidecar");
        filetime::set_file_mtime(
            &checkpoint_path,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(700)),
        )
        .expect("age checkpoint");

        assert!(
            prune_stale_checkpoint(&checkpoint_path, Duration::from_secs(600))
                .expect("promote stale checkpoint")
        );
        assert_eq!(load(temp.path(), 123), Some(789));
    }
}
