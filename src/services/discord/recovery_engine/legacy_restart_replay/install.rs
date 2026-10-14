use std::path::{Path, PathBuf};

use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::discord::{inflight, input_runtime::fence, live_bridge};
use crate::services::provider::ProviderKind;

use super::super::{
    manual_rebind::watcher_claim::PreparedClaim,
    rebind_runtime::{
        codex_relay_generation::publish_reconstruction,
        codex_spawn::{PreparedWriter, prepare_verified_writer},
    },
};
use super::{
    CoordinateHold, CoordinateRecoveryDisposition, SourcePolicy, classify, probe_normalized_output,
    proof, reader::VerifiedReader,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::recovery_engine) enum InstallCheckpoint {
    PendingSaved,
    TemporaryPrepared,
    GenerationPublished,
    FileInstalled,
    CoordinateInstalled,
    WriterClaimPrepared,
    HoldReleased,
}

pub(super) struct InstallRequest<'a> {
    pub(super) shared: &'a std::sync::Arc<crate::services::discord::SharedData>,
    pub(super) snapshot: &'a inflight::InflightTurnState,
    pub(super) raw_path: &'a Path,
    pub(super) source_policy: SourcePolicy,
}

pub(super) struct InstalledReplay<'a> {
    pub(super) state: inflight::InflightTurnState,
    pub(super) writer: PreparedWriter,
    pub(super) claim: PreparedClaim<'a>,
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct RawSnapshot {
    file: std::fs::File,
    len: u64,
    digest: [u8; 32],
    modified: std::time::SystemTime,
}

fn fingerprint(file: &mut std::fs::File, len: u64) -> Result<[u8; 32], CoordinateHold> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek};
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|_| CoordinateHold::RetryInstall("raw_seek"))?;
    let mut hash = Sha256::new();
    let mut remaining = len;
    let mut buffer = [0_u8; 8192];
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..count])
            .map_err(|_| CoordinateHold::RetryInstall("raw_snapshot_changed"))?;
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    Ok(hash.finalize().into())
}

fn saved(outcome: inflight::GuardedSaveOutcome) -> Result<(), CoordinateHold> {
    if outcome == inflight::GuardedSaveOutcome::Saved {
        Ok(())
    } else {
        Err(CoordinateHold::RetryInstall("durable_write_rejected"))
    }
}

fn lock_exact(
    snapshot: &inflight::InflightTurnState,
) -> Result<inflight::LockedInflightEpisode, CoordinateHold> {
    let locked = inflight::lock_inflight_episode(
        &ProviderKind::Codex,
        snapshot.channel_id,
        &inflight::InflightEpisodePin::from_state(snapshot),
    )
    .map_err(|_| CoordinateHold::RetryInstall("episode_changed"))?;
    let durable = serde_json::to_value(locked.state())
        .map_err(|_| CoordinateHold::NeedsEvidence("snapshot_unserializable"))?;
    let expected = serde_json::to_value(snapshot)
        .map_err(|_| CoordinateHold::NeedsEvidence("snapshot_unserializable"))?;
    if durable != expected {
        return Err(CoordinateHold::RetryInstall("snapshot_changed"));
    }
    Ok(locked)
}

fn source_is_installed(state: &inflight::InflightTurnState) -> bool {
    let Some((_, installed)) = state.coordinate_source() else {
        return false;
    };
    let Some(session) = state.tmux_session_name.as_deref() else {
        return false;
    };
    if installed.generation_mtime_ns == 0
        || crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(session)
            != installed.generation_mtime_ns
    {
        return false;
    }
    let Ok(file) = std::fs::File::open(&installed.output_path) else {
        return false;
    };
    match SourceFileIdentity::from_open_file(&file) {
        #[cfg(unix)]
        SourceFileIdentity::Unix { dev, ino } => (dev, ino) == installed.file_identity,
        SourceFileIdentity::Unavailable => false,
    }
}

fn raw_is_current(path: &Path, snapshot: &RawSnapshot) -> bool {
    let Ok(current) = std::fs::File::open(path) else {
        return false;
    };
    SourceFileIdentity::from_open_file(&current)
        == SourceFileIdentity::from_open_file(&snapshot.file)
        && current.metadata().is_ok_and(|m| {
            m.is_file()
                && m.len() >= snapshot.len
                && (m.len() > snapshot.len || m.modified().ok() == Some(snapshot.modified))
        })
}

/// The caller supplies a prepared, gated claim; no startup or idle caller selects this mode.
pub(super) async fn install<'a>(
    request: InstallRequest<'_>,
    checkpoint: &mut impl FnMut(InstallCheckpoint) -> Result<(), String>,
    claim: impl FnOnce(
        &inflight::LockedInflightEpisode,
        &PreparedWriter,
        bool,
    ) -> Result<PreparedClaim<'a>, String>,
) -> Result<InstalledReplay<'a>, CoordinateHold> {
    let snapshot = request.snapshot;
    let session = snapshot
        .tmux_session_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or(CoordinateHold::NeedsEvidence("tmux_unknown"))?;
    let prior_generation =
        crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(session);
    let channel = serenity::model::id::ChannelId::new(snapshot.channel_id);
    let writer_snapshot = request.shared.tmux_watchers.get(&channel).map(|w| {
        (
            w.tmux_session_name.clone(),
            w.output_path.clone(),
            w.cancel.clone(),
        )
    });
    if !matches!(
        classify(
            snapshot,
            probe_normalized_output(snapshot.output_path.as_deref()),
            request.source_policy
        ),
        CoordinateRecoveryDisposition::NeedsProof
            | CoordinateRecoveryDisposition::RetryInstall
            | CoordinateRecoveryDisposition::ResumeManagedSource
    ) {
        return Err(CoordinateHold::NeedsEvidence("authority_refused"));
    }
    let permit = fence::effect::admit(&ProviderKind::Codex, snapshot.channel_id)
        .map_err(|_| CoordinateHold::RetryInstall("input_authority"))?;
    let recovery = live_bridge::try_recovery(&ProviderKind::Codex, snapshot.channel_id)
        .map_err(|_| CoordinateHold::RetryInstall("recovery_authority"))?;
    recovery
        .run(fence::effect::scope(permit.clone(), async {
            let preparing = fence::blocking(|| {
                let mut locked = lock_exact(snapshot)?;
                let install_id = uuid::Uuid::new_v4().to_string();
                saved(locked.begin_coordinate_preparation(snapshot, install_id.clone()))?;
                let preparing = locked.state().clone();
                Ok::<_, CoordinateHold>((preparing, install_id))
            })?;
            let (preparing, install_id) = preparing;
            checkpoint(InstallCheckpoint::PendingSaved)
                .map_err(|_| CoordinateHold::RetryInstall("pending_checkpoint"))?;

            // Proof owns the opened raw FD; neither scanning nor temporary preparation holds flock.
            let raw_path = request.raw_path.to_path_buf();
            let proof_state = preparing.clone();
            let held_recovery = live_bridge::held_recovery();
            let (plan, mut raw_snapshot) = tokio::task::spawn_blocking(move || {
                let _recovery = held_recovery;
                let mut file = std::fs::File::open(&raw_path)
                    .map_err(|_| CoordinateHold::AwaitEvidence("source_unreadable"))?;
                let metadata = file
                    .metadata()
                    .map_err(|_| CoordinateHold::AwaitEvidence("source_unreadable"))?;
                if !metadata.is_file() {
                    return Err(CoordinateHold::NeedsEvidence("source_not_regular"));
                }
                let len = metadata.len();
                let modified = metadata
                    .modified()
                    .map_err(|_| CoordinateHold::RetryInstall("raw_stat"))?;
                let digest = fingerprint(&mut file, len)?;
                let plan = proof::prove(&proof::ProofRequest {
                    raw_path: &raw_path,
                    session_id: proof_state.session_id.as_deref(),
                    native_turn_id: proof_state.native_turn_id.as_deref(),
                    composed: &proof_state.full_response,
                })?;
                if SourceFileIdentity::from_open_file(&file)
                    != SourceFileIdentity::from_open_file(plan.reader.get_ref())
                    || plan.witness.prefix_record_end > len
                {
                    return Err(CoordinateHold::RetryInstall("raw_changed_during_proof"));
                }
                Ok((
                    plan,
                    RawSnapshot {
                        file,
                        len,
                        digest,
                        modified,
                    },
                ))
            })
            .await
            .map_err(|_| CoordinateHold::RetryInstall("proof_worker"))??;
            let output = crate::services::tmux_common::session_temp_path(session, "jsonl");
            let temporary = Temporary(PathBuf::from(format!(
                "{output}.replay.{}.{install_id}",
                std::process::id()
            )));
            let temporary_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary.0)
                .map_err(|_| CoordinateHold::RetryInstall("temporary_create"))?;
            temporary_file.sync_all()
                .map_err(|_| CoordinateHold::RetryInstall("temporary_sync"))?;
            drop(temporary_file);
            checkpoint(InstallCheckpoint::TemporaryPrepared)
                .map_err(|_| CoordinateHold::RetryInstall("temporary_checkpoint"))?;

            // Check the pre-proof bytes without flock. Appends beyond that range remain readable.
            if fingerprint(&mut raw_snapshot.file, raw_snapshot.len)? != raw_snapshot.digest {
                return Err(CoordinateHold::RetryInstall("raw_rewritten_after_proof"));
            }

            let (locked, generation_gate, generation) = fence::blocking(|| {
                let mut locked = lock_exact(&preparing)?;
                if let Some(permit) = permit.as_ref() {
                    permit
                        .validate(&ProviderKind::Codex, preparing.channel_id)
                        .map_err(|_| CoordinateHold::RetryInstall("input_authority_changed"))?;
                }
                let current = std::fs::File::open(request.raw_path)
                    .map_err(|_| CoordinateHold::RetryInstall("raw_disappeared"))?;
                let opened_identity = SourceFileIdentity::from_open_file(plan.reader.get_ref());
                if SourceFileIdentity::from_open_file(&current) != opened_identity
                    || current
                        .metadata()
                        .map_err(|_| CoordinateHold::RetryInstall("raw_stat"))?
                        .len()
                        < plan.witness.prefix_record_end
                {
                    return Err(CoordinateHold::RetryInstall("raw_changed"));
                }
                saved(locked.attach_coordinate_proof(
                    &preparing,
                    install_id.clone(),
                    plan.witness.clone(),
                ))?;
                let proven = locked.state().clone();
                let (generation_gate, generation, file_identity, mtime) = publish_reconstruction(
                    session,
                    &temporary.0,
                    &output,
                    prior_generation,
                    checkpoint,
                    || {
                        let guard = crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
                        let registry = &request.shared.tmux_watchers;
                        if registry.owner_channel_for_tmux_session(session).is_some_and(|owner| owner != channel) {
                            return Err("source belongs to another watcher".into());
                        }
                        let current = registry.get(&channel);
                        let unchanged = match (&writer_snapshot, current.as_ref()) {
                            (None, None) => true,
                            (Some((name, path, cancel)), Some(current)) => {
                                name == session && name == &current.tmux_session_name && path == &current.output_path
                                    && std::sync::Arc::ptr_eq(cancel, &current.cancel)
                            }
                            _ => false,
                        };
                        if !unchanged { return Err("writer changed during proof".into()); }
                        drop(current);
                        Ok(guard)
                    },
                )
                .map_err(|_| CoordinateHold::RetryInstall("source_publication"))?;
                saved(locked.install_coordinate_source(
                    &proven,
                    install_id,
                    output.clone(),
                    file_identity,
                    mtime,
                ))?;
                checkpoint(InstallCheckpoint::CoordinateInstalled)
                    .map_err(|_| CoordinateHold::RetryInstall("installed_checkpoint"))?;
                Ok::<_, CoordinateHold>((locked, generation_gate, generation))
            })?;
            let (mut locked, finish_mailbox) =
                super::super::manual_rebind::episode_handoff::readopt_prepared(
                    request.shared,
                    locked,
                )
                .await
                .map_err(|_| CoordinateHold::RetryInstall("mailbox_readoption"))?;
            fence::blocking(|| {
                if !source_is_installed(locked.state())
                    || !raw_is_current(request.raw_path, &raw_snapshot)
                {
                    return Err(CoordinateHold::RetryInstall(
                        "source_changed_after_readoption",
                    ));
                }
                let writer = prepare_verified_writer(
                    PathBuf::from(output),
                    VerifiedReader::from_plan(plan),
                    locked.state().full_response.clone(),
                    (generation_gate, generation),
                )
                .map_err(|_| CoordinateHold::RetryInstall("writer_preparation"))?;
                let mut prepared_claim = claim(&locked, &writer, finish_mailbox)
                    .map_err(|_| CoordinateHold::RetryInstall("claim_preparation"))?;
                checkpoint(InstallCheckpoint::WriterClaimPrepared)
                    .map_err(|_| CoordinateHold::RetryInstall("claim_checkpoint"))?;
                let installed = locked.state().clone();
                if !source_is_installed(&installed) || !prepared_claim.is_current() {
                    return Err(CoordinateHold::RetryInstall("prepared_actor_changed"));
                }
                saved(locked.release_coordinate_hold(&installed))?;
                let state = locked.state().clone();
                if checkpoint(InstallCheckpoint::HoldReleased).is_err()
                    || !crate::services::tmux_common::with_tmux_source_authority(session, |_| {
                        source_is_installed(&state)
                            && raw_is_current(request.raw_path, &raw_snapshot)
                            && prepared_claim.open().is_ok()
                    })
                {
                    let id = state
                        .legacy_replay_coordinate
                        .as_ref()
                        .ok_or(CoordinateHold::RetryInstall("marker_missing"))?
                        .install_id
                        .clone();
                    saved(locked.begin_coordinate_preparation(&state, id))?;
                    return Err(CoordinateHold::RetryInstall("gate_recheck"));
                }
                drop(locked);
                Ok(InstalledReplay {
                    state,
                    writer,
                    claim: prepared_claim,
                })
            })
        }))
        .await
}

#[path = "install_tests.rs"]
mod tests;
