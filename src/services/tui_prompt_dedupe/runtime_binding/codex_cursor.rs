//! Proof-linked cursors are derivative checkpoints, never source authority.

use super::*;
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use serde_json::{Value, json};
use std::{fs, io, io::Write, path::Path};

fn marker_path(authority: &TmuxSourceAuthority<'_>) -> String {
    tc::session_temp_path(authority.session(), tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT)
}

fn linked_marker(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> io::Result<Option<Value>> {
    let Some(context) = codex_verified::current_context(authority)? else {
        return Ok(None);
    };
    let proof = codex_verified::proof_for_binding(authority, &context, binding)?;
    Ok(fs::read(marker_path(authority))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|marker| {
            marker["codex_ownership"] == codex_verified::marker_proof(&context, &proof)
                && marker["rollout_path"].as_str() == proof.source.path.to_str()
                && marker["session_id"].as_str() == Some(&proof.source.session_id)
        }))
}

pub(crate) fn preservable_marker(
    authority: &TmuxSourceAuthority<'_>,
    path: &Path,
    session_id: Option<&str>,
) -> io::Result<Option<Value>> {
    let Some(context) = codex_verified::current_context(authority)? else {
        return Ok(None);
    };
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: session_id.map(str::to_owned),
        last_offset: 0,
        relay_last_offset: Some(0),
    };
    let current = codex_verified::proof_for_binding(authority, &context, &binding)?;
    let marker = fs::read(marker_path(authority))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(marker) = marker else {
        return Ok(None);
    };
    let Some(seq) = marker["codex_ownership"]["proof_seq"].as_u64() else {
        return Ok(None);
    };
    let Some(previous) = binding_events::codex::proof_at_seq(&context, seq)? else {
        return Ok(None);
    };
    Ok((previous.source == current.source
        && marker["codex_ownership"] == codex_verified::marker_proof(&context, &previous)
        && marker["rollout_path"].as_str() == current.source.path.to_str()
        && marker["session_id"].as_str() == Some(&current.source.session_id))
    .then_some(marker))
}

fn relay_identity(authority: &TmuxSourceAuthority<'_>, path: &str) -> Option<(u64, u64, u64)> {
    let expected = tc::session_temp_path(authority.session(), "jsonl");
    if Path::new(path).canonicalize().ok()? != Path::new(&expected).canonicalize().ok()? {
        return None;
    }
    let metadata = fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Some((metadata.dev(), metadata.ino(), metadata.len()));
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn saved_relay(
    authority: &TmuxSourceAuthority<'_>,
    marker: Option<&Value>,
) -> Option<(String, u64)> {
    let relay = marker?.get("codex_relay")?;
    let path = relay["output_path"].as_str()?;
    let (dev, ino, len) = relay_identity(authority, path)?;
    if relay["dev"].as_u64() != Some(dev) || relay["ino"].as_u64() != Some(ino) {
        return None;
    }
    Some((
        path.to_owned(),
        relay["last_offset"]
            .as_u64()
            .filter(|offset| *offset <= len)?,
    ))
}

pub(super) fn restore(
    authority: &TmuxSourceAuthority<'_>,
    binding: TuiRuntimeBinding,
) -> io::Result<TuiRuntimeBinding> {
    restore_checkpoint(authority, binding, false)
}

pub(super) fn restore_after_publication(
    authority: &TmuxSourceAuthority<'_>,
    binding: TuiRuntimeBinding,
) -> io::Result<TuiRuntimeBinding> {
    restore_checkpoint(authority, binding, true)
}

fn restore_checkpoint(
    authority: &TmuxSourceAuthority<'_>,
    mut binding: TuiRuntimeBinding,
    from_marker: bool,
) -> io::Result<TuiRuntimeBinding> {
    let marker = linked_marker(authority, &binding)?;
    let checkpoint = saved_relay(authority, marker.as_ref());
    let previous = with_runtime_binding_state_under_source_authority(authority, |state| {
        state
            .runtime_by_tmux
            .get(authority.session())
            .map(|entry| entry.value.clone())
    })
    .filter(|old| {
        !from_marker
            && old.output_path == binding.output_path
            && old.session_id == binding.session_id
            && codex_verified::consumer_allowed(authority, old)
    });
    if let Some(mut previous) = previous {
        if let Some((path, offset)) = checkpoint {
            previous.relay_output_path = Some(path);
            previous.relay_last_offset = Some(offset);
        } else {
            let relay = previous
                .relay_output_path
                .clone()
                .or(binding.relay_output_path)
                .filter(|path| relay_identity(authority, path).is_some());
            if previous.relay_output_path.is_some() || relay.is_some() {
                previous.relay_output_path = relay;
                previous.relay_last_offset = Some(0);
            }
        }
        return Ok(previous);
    }
    binding.last_offset = marker
        .as_ref()
        .and_then(|value| value["rollout_start_offset"].as_u64())
        .filter(|offset| {
            fs::metadata(&binding.output_path).is_ok_and(|metadata| *offset <= metadata.len())
        })
        .unwrap_or(0);
    binding.relay_last_offset = Some(0);
    // Discovery's spool EOF is not evidence that its bytes were delivered.
    binding.relay_output_path = binding
        .relay_output_path
        .filter(|path| relay_identity(authority, path).is_some());
    if let Some((path, offset)) = checkpoint {
        binding.relay_output_path = Some(path);
        binding.relay_last_offset = Some(offset);
    } else if binding.relay_output_path.is_none()
        && marker
            .as_ref()
            .is_some_and(|marker| marker.get("codex_relay").is_none())
    {
        // With no normalized spool, relay reads the native cursor's own namespace.
        binding.relay_last_offset = Some(binding.last_offset);
    }
    Ok(binding)
}

pub(super) fn persist(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> io::Result<()> {
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui
        || codex_verified::current_context(authority)?.is_none()
    {
        return Ok(());
    }
    if !codex_verified::consumer_allowed(authority, binding) {
        return Err(io::Error::other("Codex cursor has no deliverable proof"));
    }
    let mut marker = linked_marker(authority, binding)?
        .ok_or_else(|| io::Error::other("Codex cursor marker unavailable"))?;
    marker["rollout_start_offset"] = json!(binding.last_offset);
    if let Some(path) = binding.relay_output_path.as_deref() {
        let (dev, ino, len) = relay_identity(authority, path)
            .ok_or_else(|| io::Error::other("Codex relay namespace changed"))?;
        let offset = binding.relay_last_offset.unwrap_or(0);
        if offset > len {
            return Err(io::Error::other("Codex relay cursor beyond file"));
        }
        marker["codex_relay"] =
            json!({"output_path":path,"last_offset":offset,"dev":dev,"ino":ino});
    } else {
        marker
            .as_object_mut()
            .ok_or_else(|| io::Error::other("Codex cursor marker is not an object"))?
            .remove("codex_relay");
    }
    let path = marker_path(authority);
    let parent = Path::new(&path)
        .parent()
        .ok_or_else(|| io::Error::other("Codex cursor has no parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(format!("{marker}\n").as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(&path).map_err(|error| error.error)?;
    crate::services::discord::runtime_store::fsync_parent_dir(Path::new(&path))
}

pub(crate) fn advance_tmux_runtime_binding_offset_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    output_path: &str,
    last_offset: u64,
) -> bool {
    let tmux_session_name = authority.session();
    let previous = with_runtime_binding_state_under_source_authority(authority, |state| {
        state
            .runtime_by_tmux
            .get(tmux_session_name)
            .map(|entry| entry.value.clone())
    });
    let Some(mut binding) = previous.clone() else {
        return false;
    };
    if binding.output_path == output_path {
        binding.last_offset = last_offset;
        if binding.relay_output_path.is_none() {
            binding.relay_last_offset = Some(last_offset);
        }
    } else if binding.relay_output_path.as_deref() == Some(output_path) {
        binding.relay_last_offset = Some(last_offset);
    } else {
        return false;
    }
    if persist(authority, &binding).is_err() {
        return false;
    }
    with_runtime_binding_state_under_source_authority(authority, |state| {
        let Some(entry) = state.runtime_by_tmux.get_mut(tmux_session_name) else {
            return false;
        };
        if Some(&entry.value) != previous.as_ref() {
            return false;
        }
        entry.value = binding;
        entry.recorded_at = Instant::now();
        true
    })
}
