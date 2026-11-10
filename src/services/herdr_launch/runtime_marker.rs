//! A Herdr incarnation may discard its inherited TUI runtime hint, never its execution evidence.
use std::io;
use std::path::{Path, PathBuf};

use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, HostedOwner, HostedState};
use crate::services::platform::tmux::{self, SessionPresence};
use crate::services::session_host::HostKind;
use crate::services::tmux_common::{self as tc, host_marker};

pub(super) async fn clear_fresh(owner: &HostedOwner, nonce: &str) -> Result<(), String> {
    clear(owner, nonce).await
}

pub(crate) async fn clear_retired(record: &HostedExecution) -> Result<(), String> {
    if record.state != HostedState::Retired {
        return Err("runtime marker cleanup requires a retired execution".into());
    }
    clear(&record.owner, &record.execution_nonce).await
}

async fn clear(owner: &HostedOwner, nonce: &str) -> Result<(), String> {
    if owner.provider != "codex" {
        return Ok(());
    }
    let (logical, nonce) = (owner.logical_key.clone(), nonce.to_owned());
    tokio::task::spawn_blocking(move || {
        tc::with_tmux_source_authority(&logical, |_| clear_owned(&logical, &nonce))
    })
    .await
    .map_err(|error| format!("runtime marker cleanup task: {error}"))?
}

fn runtime_paths(logical: &str) -> Vec<PathBuf> {
    let extension = tc::TMUX_RUNTIME_KIND_TEMP_EXT;
    let canonical = PathBuf::from(tc::session_temp_path(logical, extension));
    let legacy = PathBuf::from(tc::legacy_tmp_session_path(logical, extension));
    if canonical == legacy {
        vec![canonical]
    } else {
        vec![canonical, legacy]
    }
}

fn read_runtime(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Ok(entry) if entry.is_file() => std::fs::read(path)
            .map(Some)
            .map_err(|error| format!("read runtime marker: {error}")),
        Ok(_) => Err("runtime marker is not a regular file".into()),
        Err(error) => Err(format!("inspect runtime marker: {error}")),
    }
}

fn clear_owned(logical: &str, nonce: &str) -> Result<(), String> {
    let mut markers = Vec::new();
    for path in runtime_paths(logical) {
        if let Some(bytes) = read_runtime(&path)? {
            if std::str::from_utf8(&bytes).map(str::trim) != Ok("codex_tui") {
                return Err("runtime marker belongs to another runtime".into());
            }
            markers.push((path, bytes));
        }
    }
    if markers.is_empty() {
        return Ok(());
    }
    let owned = || {
        !nonce.is_empty()
            && host_marker::read_host_kind_marker(logical)
                == host_marker::HostKindMarker::Known(HostKind::Herdr)
            && std::fs::read_to_string(tc::session_temp_path(logical, "spawn_nonce"))
                .is_ok_and(|stored| stored.trim() == nonce)
    };
    if !owned() {
        return Err("runtime marker execution or host differs".into());
    }
    if crate::services::session_backend::process_session_pid(logical).is_some()
        || tmux::session_presence(logical) != SessionPresence::Missing
    {
        return Err("runtime marker session vacancy is not proven".into());
    }
    // The source lock also fences successor nonce publication; keep every unjudged file intact.
    if !owned()
        || markers.iter().any(|(path, bytes)| {
            !read_runtime(path).is_ok_and(|current| current.as_deref() == Some(bytes.as_slice()))
        })
    {
        return Err("runtime marker changed during cleanup".into());
    }
    for (path, _) in markers {
        std::fs::remove_file(path).map_err(|error| format!("remove runtime marker: {error}"))?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
