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
        normalize_native(&mut previous, authority)?;
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
        .unwrap_or(0);
    normalize_native(&mut binding, authority)?;
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

fn normalize_native(
    binding: &mut TuiRuntimeBinding,
    authority: &TmuxSourceAuthority<'_>,
) -> io::Result<()> {
    let (source, mut file) = crate::services::codex_tui::session::checkpoint::Source::open(
        authority.session(),
        Path::new(&binding.output_path),
        binding.session_id.clone(),
    )?;
    binding.last_offset =
        crate::services::codex_tui::session::checkpoint::boundary(&mut file, binding.last_offset)?;
    source.check(authority.session())
}

pub(crate) fn migrate_raw_native_marker(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> io::Result<()> {
    use crate::services::codex_tui::session::{checkpoint::Source, codex_tui_rollout_paths_same};
    if !codex_verified::publication_allowed(authority, binding) {
        return Err(io::Error::other("Codex migration source rejected"));
    }
    let path = marker_path(authority);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut marker: Value = serde_json::from_slice(&bytes)?;
    let Some(saved) = marker["rollout_path"].as_str() else {
        return Err(io::Error::other("Codex marker has no source"));
    };
    if !codex_tui_rollout_paths_same(Path::new(saved), Path::new(&binding.output_path)) {
        return Ok(());
    }
    if marker["session_id"]
        .as_str()
        .zip(binding.session_id.as_deref())
        .is_some_and(|(a, b)| a != b)
    {
        return Err(io::Error::other("Codex migration session changed"));
    }
    if codex_verified::current_context(authority)?.is_some()
        && linked_marker(authority, binding)?.is_none()
    {
        return Err(io::Error::other("Codex migration marker proof changed"));
    }
    let Some(old) = marker["rollout_start_offset"].as_u64() else {
        return Ok(());
    };
    let (source, mut file) = Source::open(
        authority.session(),
        Path::new(&binding.output_path),
        binding.session_id.clone(),
    )?;
    let boundary = crate::services::codex_tui::session::checkpoint::boundary(&mut file, old)?;
    if old == boundary {
        return Ok(());
    }
    let previous = with_runtime_binding_state_under_source_authority(authority, |state| {
        state
            .runtime_by_tmux
            .get(authority.session())
            .map(|entry| entry.value.clone())
    });
    if previous
        .as_ref()
        .is_some_and(|binding| binding.last_offset != old && binding.last_offset > boundary)
    {
        return Err(io::Error::other("Codex migration runtime cursor changed"));
    }
    source.check(authority.session())?;
    if fs::read(&path)? != bytes {
        return Err(io::Error::other("Codex migration marker changed"));
    }
    marker["rollout_start_offset"] = json!(boundary);
    crate::services::discord::runtime_store::atomic_write(Path::new(&path), &format!("{marker}\n"))
        .map_err(io::Error::other)?;
    with_runtime_binding_state_under_source_authority(authority, |state| {
        if let Some(entry) = state.runtime_by_tmux.get_mut(authority.session())
            && Some(&entry.value) == previous.as_ref()
            && entry.value.last_offset == old
        {
            entry.value.last_offset = boundary;
            if entry.value.relay_output_path.is_none() {
                entry.value.relay_last_offset = Some(boundary);
            }
        }
    });
    source.check(authority.session())
}

pub(super) fn persist(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> io::Result<()> {
    persist_checkpoint(authority, binding, false)
}

pub(super) fn persist_after_restore(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> io::Result<()> {
    persist_checkpoint(authority, binding, true)
}

fn persist_checkpoint(
    authority: &TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
    restored: bool,
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
        let same_relay = marker.get("codex_relay").is_some_and(|relay| {
            relay["output_path"].as_str() == Some(path)
                && relay["dev"].as_u64() == Some(dev)
                && relay["ino"].as_u64() == Some(ino)
        });
        // Only an explicit restore can connect a new spool at its beginning.
        if !same_relay && !(restored && offset == 0) {
            return Err(io::Error::other("Codex relay checkpoint identity changed"));
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
    matches!(
        advance_cursor(authority, output_path, None, last_offset, true),
        crate::services::codex_tui::session::checkpoint::Advance::Advanced { .. }
            | crate::services::codex_tui::session::checkpoint::Advance::AlreadyAdvanced
    )
}

pub(crate) fn advance_checked(
    authority: &TmuxSourceAuthority<'_>,
    output_path: &str,
    expected: Option<u64>,
    last_offset: u64,
) -> crate::services::codex_tui::session::checkpoint::Advance {
    advance_cursor(authority, output_path, expected, last_offset, false)
}

fn advance_cursor(
    authority: &TmuxSourceAuthority<'_>,
    output_path: &str,
    expected: Option<u64>,
    last_offset: u64,
    legacy: bool,
) -> crate::services::codex_tui::session::checkpoint::Advance {
    use crate::services::codex_tui::session::checkpoint::{Advance, Source, at};
    let tmux_session_name = authority.session();
    let previous = with_runtime_binding_state_under_source_authority(authority, |state| {
        state
            .runtime_by_tmux
            .get(tmux_session_name)
            .map(|entry| entry.value.clone())
    });
    let Some(mut binding) = previous.clone() else {
        return Advance::SourceChanged;
    };
    let native = binding.output_path == output_path;
    let start = if native {
        binding.last_offset
    } else {
        binding.relay_last_offset()
    };
    if last_offset == start {
        return Advance::AlreadyAdvanced;
    }
    #[cfg(unix)]
    let legacy = legacy && native && codex_restore_observed_end(tmux_session_name).is_none();
    #[cfg(not(unix))]
    let legacy = {
        let _ = legacy;
        false
    };
    if expected.is_some_and(|expected| start != expected)
        || (binding.runtime_kind == RuntimeHandoffKind::CodexTui && last_offset < start && !legacy)
    {
        return Advance::StartMismatch;
    }
    let source = if binding.runtime_kind == RuntimeHandoffKind::CodexTui && cfg!(unix) {
        match Source::open(
            tmux_session_name,
            Path::new(output_path),
            binding.session_id.clone(),
        ) {
            Ok((source, file))
                if at(Path::new(output_path), last_offset).ok() == Some(last_offset) =>
            {
                (
                    Some(source),
                    crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
                        &file,
                    ),
                )
            }
            _ => return Advance::SourceChanged,
        }
    } else {
        (
            None,
            crate::services::cluster::stream_relay::SourceFileIdentity::Unavailable,
        )
    };
    if binding.output_path == output_path {
        binding.last_offset = last_offset;
        if binding.relay_output_path.is_none() {
            binding.relay_last_offset = Some(last_offset);
        }
    } else if binding.relay_output_path.as_deref() == Some(output_path) {
        binding.relay_last_offset = Some(last_offset);
    } else {
        return Advance::SourceChanged;
    }
    if persist(authority, &binding).is_err() {
        return Advance::PersistFailed;
    }
    if source
        .0
        .as_ref()
        .is_some_and(|source| source.check(tmux_session_name).is_err())
    {
        return Advance::SourceChanged;
    }
    with_runtime_binding_state_under_source_authority(authority, |state| {
        let Some(entry) = state.runtime_by_tmux.get_mut(tmux_session_name) else {
            return Advance::SourceChanged;
        };
        if Some(&entry.value) != previous.as_ref() {
            return Advance::StartMismatch;
        }
        entry.value = binding;
        entry.recorded_at = Instant::now();
        Advance::Advanced {
            source: source.1,
            start,
            end: last_offset,
        }
    })
}

#[cfg(unix)]
pub(crate) fn codex_restore_scan_floor(tmux: &str, path: &Path) -> Option<u64> {
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let scope = state.codex_restore_by_tmux.get(tmux)?;
    (scope.source.matches_path(path) && scope.source.check(tmux).is_ok())
        .then_some(scope.input_start)
}

#[cfg(unix)]
pub(crate) fn codex_restore_checkpoint(tmux: &str, path: &Path) -> Option<u64> {
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let scope = state.codex_restore_by_tmux.get(tmux)?;
    (scope.source.matches_path(path) && scope.source.check(tmux).is_ok())
        .then_some(scope.source.complete_end)
}

#[cfg(unix)]
pub(crate) fn codex_restore_observed_end(tmux: &str) -> Option<u64> {
    STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .codex_restore_by_tmux
        .get(tmux)
        .map(|scope| scope.observed_end)
}

#[cfg(unix)]
pub(crate) fn codex_restore_source_current(tmux: &str, path: &Path) -> bool {
    STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .codex_restore_by_tmux
        .get(tmux)
        .is_none_or(|scope| scope.source.matches_path(path) && scope.source.check(tmux).is_ok())
}

#[cfg(unix)]
pub(crate) fn record_codex_unknown_span(
    tmux: &str,
    start: u64,
    end: u64,
    stats: crate::services::codex_tui::session::checkpoint::ScanStats,
    kind: crate::services::codex_tui::session::checkpoint::ObservationKind,
) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(scope) = state.codex_restore_by_tmux.get_mut(tmux) {
        scope.record_unknown(tmux, start, end, stats, kind);
    }
}

#[cfg(all(test, unix))]
pub(crate) fn codex_restore_observation_for_tests(
    tmux: &str,
) -> Option<(
    u64,
    u64,
    Option<u64>,
    crate::services::codex_tui::session::checkpoint::ScanStats,
)> {
    STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .codex_restore_by_tmux
        .get(tmux)
        .map(|scope| {
            (
                scope.source.complete_end,
                scope.input_start,
                scope.replay_from,
                scope.unknown,
            )
        })
}

/// Links one restored parked record to the existing captured bridge episode.
#[cfg(unix)]
pub(crate) fn link_codex_parked_owner(
    tmux: &str,
    turn: &str,
    prompt: &str,
    owner: crate::services::codex_tui::session::checkpoint::ParkedOwnerKey,
) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let generation = state
        .handoff_by_tmux
        .get(&PromptKey::new("codex", tmux))
        .map(|entry| entry.value.generation);
    let Some(scope) = state.codex_restore_by_tmux.get_mut(tmux) else {
        return;
    };
    let Some(held) = &scope.held else {
        return;
    };
    if generation.is_some_and(|generation| generation != held.generation)
        || owner.5 > scope.source.complete_end
    {
        return;
    }
    let Ok(json) = scope.source.record(tmux, owner.5) else {
        return;
    };
    let Some((text, entry, evidence)) = extract_codex_rollout_user_prompt_with_evidence(&json)
    else {
        return;
    };
    if evidence
        .turn_id
        .as_deref()
        .is_some_and(|native| native != turn)
        || !prompts_match(&text, prompt)
        || !evidence.created_since(held.reserved_wall)
        || !held.parked.iter().any(|input| {
            prompts_match(&input.prompt, prompt)
                && input
                    .native_turn
                    .as_deref()
                    .is_none_or(|native| native == turn)
                && (input.entry_ids.is_empty()
                    || entry
                        .as_deref()
                        .is_none_or(|id| input.entry_ids.iter().any(|own| own == id)))
        })
    {
        return;
    }
    scope.owners.entry(turn.to_owned()).or_insert(owner);
}

#[cfg(unix)]
pub(crate) fn codex_parked_owner_source(
    tmux: &str,
    turn: &str,
    owner: &crate::services::codex_tui::session::checkpoint::ParkedOwnerKey,
) -> Option<crate::services::codex_tui::session::checkpoint::Source> {
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let scope = state.codex_restore_by_tmux.get(tmux)?;
    (scope.owners.get(turn) == Some(owner) && scope.source.check(tmux).is_ok())
        .then(|| scope.source.clone())
}

#[cfg(unix)]
pub(in crate::services::tui_prompt_dedupe) fn codex_restore_replacement_allowed(
    state: &TuiPromptDedupeState,
    tmux: &str,
    current: &super::super::handoff::CodexRestore,
    source: &crate::services::codex_tui::session::checkpoint::Source,
) -> bool {
    current.held.is_none()
        && !current.source.matches_path(&source.path)
        && source.session_id.is_some()
        && state
            .handoff_by_tmux
            .get(&PromptKey::new("codex", tmux))
            .is_none_or(|entry| entry.value.parked.is_empty())
        && crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux).is_some_and(
            |marker| {
                source.matches_path(&marker.rollout_path) && marker.session_id == source.session_id
            },
        )
}
