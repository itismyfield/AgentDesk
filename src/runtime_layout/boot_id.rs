//! The dcserver boot id, read from the runtime root's pid file.

use std::fs;
use std::time::UNIX_EPOCH;

fn boot_id_from_pid_file(pid_file: &std::path::Path) -> Result<String, String> {
    let pid = fs::read_to_string(pid_file)
        .map_err(|error| format!("read dcserver pid file {}: {error}", pid_file.display()))?
        .trim()
        .to_string();
    if pid.is_empty() {
        return Err(format!("dcserver pid file {} is empty", pid_file.display()));
    }
    let mtime = fs::metadata(pid_file)
        .and_then(|meta| meta.modified())
        .map_err(|error| {
            format!(
                "read dcserver pid file metadata {}: {error}",
                pid_file.display()
            )
        })?
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("dcserver pid file mtime before epoch: {error}"))?
        .as_secs();
    Ok(format!("{pid}-{mtime}"))
}

pub(crate) fn current_boot_id_at(root: &std::path::Path) -> Result<String, String> {
    boot_id_from_pid_file(&root.join("runtime").join("dcserver.pid"))
}
