use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::stream_relay::{SourceFileIdentity, SourceWitness};
use crate::services::discord::delivery_lease_cell::source_epoch_observer::read_source_epoch_witness;
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};
use dedupe::binding_context::{self, BindingContext, SpawnNonceMarker};
use dedupe::binding_events::{self, BindingTarget, SourceId};
use std::path::Path;

pub struct OOnlyInstallRequest {
    pub(in crate::services::discord) runtime: Arc<SharedData>,
    pub(in crate::services::discord) http: Arc<serenity::Http>,
    pub(in crate::services::discord) provider: ProviderKind,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) session_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OOnlyInstallReason {
    NotRetired,
    OwnershipUnavailable,
    InvalidTarget,
    BindingBusy,
    SourceUnavailable,
    SourceMismatch,
    RequiresDirectResumeCommit,
    WatchWithheld,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OOnlyInstallOutcome {
    Spawned,
    AlreadyLive,
    Deferred(OOnlyInstallReason),
    Failed(OOnlyInstallReason),
}

impl OOnlyInstallReason {
    fn outcome(self) -> OOnlyInstallOutcome {
        match self {
            Self::InvalidTarget
            | Self::SourceMismatch
            | Self::RequiresDirectResumeCommit
            | Self::WatchWithheld => OOnlyInstallOutcome::Failed(self),
            _ => OOnlyInstallOutcome::Deferred(self),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SourceSnapshot {
    path: String,
    binding: Option<TuiRuntimeBinding>,
    context: Option<BindingContext>,
    proof: Option<(u64, SourceId)>,
    marker: Option<(std::path::PathBuf, Option<String>)>,
    file: SourceFileIdentity,
    witness: SourceWitness,
}

// Nonblocking open reaches the descriptor type check even when a FIFO replaced the source.
fn open_regular_source(
    path: &Path,
) -> Result<(std::fs::File, std::fs::Metadata), OOnlyInstallReason> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| OOnlyInstallReason::SourceUnavailable)?;
    let metadata = file
        .metadata()
        .map_err(|_| OOnlyInstallReason::SourceUnavailable)?;
    if !metadata.is_file() {
        return Err(OOnlyInstallReason::SourceMismatch);
    }
    Ok((file, metadata))
}

fn routed(
    request: &OOnlyInstallRequest,
    settings: &super::super::super::DiscordBotSettings,
) -> bool {
    let Some((provider, channel)) =
        parse_provider_and_channel_from_tmux_name(&request.session_name)
    else {
        return false;
    };
    settings.provider == request.provider
        && provider == request.provider
        && validate_bot_channel_routing_with_provider_channel(
            settings,
            &provider,
            request.channel_id,
            Some(&channel),
            Some(&channel),
            channel.starts_with("dm-"),
        )
        .is_ok()
}

fn source(
    request: &OOnlyInstallRequest,
    authority: &TmuxSourceAuthority<'_>,
) -> Result<(SourceSnapshot, u64), OOnlyInstallReason> {
    use OOnlyInstallReason::*;
    let name = authority.session();
    let kind = match request.provider {
        ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
        ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
        _ => return Err(InvalidTarget),
    };
    if !session_belongs_to_current_runtime(name, &tc::current_tmux_owner_marker())
        || tc::read_tmux_channel_binding(name) != Some(request.channel_id.get())
        || tc::resolve_tmux_runtime_kind_marker(name) != Some(kind)
    {
        return Err(InvalidTarget);
    }
    let mut binding = dedupe::try_peek_tmux_runtime_binding(name)
        .map_err(|dedupe::BindingPeekBusy| BindingBusy)?;
    if binding.as_ref().is_some_and(|b| b.runtime_kind != kind) {
        return Err(SourceMismatch);
    }
    let context = match binding_context::observe_spawn_nonce_marker(name) {
        SpawnNonceMarker::Known(nonce) => Some(
            binding_context::execution_context(request.provider.as_str(), &nonce)
                .map_err(|_| SourceUnavailable)?,
        ),
        SpawnNonceMarker::Absent => None,
        SpawnNonceMarker::Unreadable => return Err(SourceUnavailable),
    };
    if context.as_ref().is_some_and(|c| {
        c.provider != request.provider.as_str()
            || c.tmux_session != name
            || c.channel_id != Some(request.channel_id.get())
            || c.owner_runtime_root != tc::current_tmux_owner_marker()
    }) {
        return Err(SourceMismatch);
    }
    let mut proof = None;
    if let Some(context) = context.as_ref() {
        if kind == RuntimeHandoffKind::CodexTui
            && context.source_policy.as_deref() == Some("verified")
        {
            let fold =
                binding_events::codex::read_ownership(context).map_err(|_| SourceUnavailable)?;
            let current = fold
                .verified
                .filter(|_| !fold.conflicted && fold.pending.is_empty())
                .ok_or(SourceUnavailable)?;
            proof = Some((current.seq, current.source));
        } else if kind == RuntimeHandoffKind::ClaudeTui {
            let records = binding_events::records_strict(request.channel_id.get())
                .map_err(|_| SourceUnavailable)?
                .map_err(|_| SourceUnavailable)?;
            if let Some(event) = records.into_iter().rev().find(|event| {
                event.provider == "claude"
                    && event.tmux_session == name
                    && event.execution_nonce.as_deref() == Some(&context.execution_nonce)
                    && !matches!(event.new, BindingTarget::Rejected { .. })
            }) {
                match event.new {
                    BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                        proof = Some((event.seq, source))
                    }
                    _ => return Err(SourceUnavailable),
                }
            }
        }
    }
    let marker = crate::services::codex_tui::session::read_codex_tui_rollout_marker(name);
    let candidate = binding
        .as_ref()
        .map(|b| (b.output_path.clone(), b.session_id.clone()))
        .or_else(|| {
            proof
                .as_ref()
                .map(|(_, s)| (s.path.display().to_string(), Some(s.session_id.clone())))
        })
        .or_else(|| {
            marker
                .as_ref()
                .filter(|_| kind == RuntimeHandoffKind::CodexTui)
                .map(|m| (m.rollout_path.display().to_string(), m.session_id.clone()))
        });
    if let Some(context) = context
        .as_ref()
        .filter(|c| c.source_policy.as_deref() == Some("verified"))
    {
        if context.provider != "codex"
            || !candidate.as_ref().is_some_and(|(path, session)| {
                let candidate = TuiRuntimeBinding {
                    runtime_kind: kind,
                    output_path: path.clone(),
                    session_id: session.clone(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    last_offset: 0,
                    relay_last_offset: None,
                };
                dedupe::codex_verified_publication_allowed(authority, &candidate)
            })
        {
            return Err(SourceMismatch);
        }
    } else if kind == RuntimeHandoffKind::CodexTui {
        let marker_path = tc::session_temp_path(name, tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT);
        if std::fs::read(marker_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|marker| marker.get("codex_ownership").is_some())
        {
            return Err(SourceUnavailable);
        }
    }
    let wrapper = tc::session_temp_path(name, "jsonl");
    let mut native_file = None;
    let path = if Path::new(&wrapper).exists() {
        if binding
            .as_ref()
            .and_then(|b| b.relay_output_path.as_deref())
            .is_some_and(|p| p != wrapper)
        {
            return Err(SourceMismatch);
        }
        wrapper
    } else {
        let Some(candidate) = candidate else {
            return Err(
                if codex_restore::rollout_fallback_for_live_direct_resume(
                    &request.provider,
                    name,
                    request.channel_id,
                )
                .is_some()
                {
                    RequiresDirectResumeCommit
                } else {
                    SourceUnavailable
                },
            );
        };
        let path = Path::new(&candidate.0);
        let id = candidate.1.as_deref().ok_or(SourceUnavailable)?;
        if kind == RuntimeHandoffKind::CodexTui {
            use crate::services::codex_tui::session::source_observation::*;
            let root = context
                .as_ref()
                .and_then(|c| c.provider_root.clone())
                .or_else(crate::services::codex_tui::rollout_tail::default_codex_sessions_dir)
                .ok_or(SourceUnavailable)?
                .canonicalize()
                .map_err(|_| SourceUnavailable)?;
            native_file = Some(
                verify_codex_hook_source(
                    &root,
                    &CodexHookSourceClaim {
                        session_id: id,
                        transcript_path: Some(path),
                        expected_source: CodexRolloutSource::Cli,
                    },
                )
                .map_err(|_| SourceMismatch)?
                .identity,
            );
            if marker.as_ref().is_some_and(|m| {
                m.session_id.as_deref() != Some(id)
                    || !crate::services::codex_tui::session::codex_tui_rollout_paths_same(
                        path,
                        &m.rollout_path,
                    )
            }) {
                return Err(SourceMismatch);
            }
        } else {
            use crate::services::claude_tui::source_verify::*;
            use std::os::fd::AsRawFd;
            let root = context
                .as_ref()
                .and_then(|c| c.provider_root.clone())
                .or_else(
                    crate::services::claude_tui::hook_output_guard::configured_claude_projects_root,
                )
                .ok_or(SourceUnavailable)?
                .canonicalize()
                .map_err(|_| SourceUnavailable)?;
            let native = path.canonicalize().map_err(|_| SourceUnavailable)?;
            if !is_top_level_transcript(&root, &native, id) {
                return Err(SourceMismatch);
            }
            let (native_fd, _) = open_regular_source(&native)?;
            // Verify the checked regular descriptor without reopening a replaceable native name.
            let descriptor = format!("/dev/fd/{}", native_fd.as_raw_fd());
            native_file = Some(match observe_transcript(Path::new(&descriptor)) {
                Ok(OpenedTranscript::Opened {
                    file,
                    first: FirstRecord::Session(session),
                }) if session == id => file,
                _ => return Err(SourceMismatch),
            });
        }
        if proof.as_ref().is_some_and(|(_, source)| {
            source.session_id != id
                || source.path != path
                || !binding_events::codex::source_file_matches(source)
        }) {
            return Err(SourceMismatch);
        }
        candidate.0
    };
    #[cfg(test)]
    tests::before_reopen();
    let (opened, metadata) = open_regular_source(Path::new(&path))?;
    let file = SourceFileIdentity::from_open_file(&opened);
    if file == SourceFileIdentity::Unavailable {
        return Err(SourceUnavailable);
    }
    if native_file.is_some_and(|verified| verified != file) {
        return Err(SourceMismatch);
    }
    // Offsets are progress, while source identity and incarnation must survive the host await.
    if let Some(binding) = binding.as_mut() {
        binding.last_offset = 0;
        binding.relay_last_offset = None;
    }
    Ok((
        SourceSnapshot {
            path,
            binding,
            context,
            proof,
            marker: marker.map(|m| (m.rollout_path, m.session_id)),
            file,
            witness: read_source_epoch_witness(name),
        },
        metadata.len(),
    ))
}

/// Installs one retired channel using current ownership and source evidence, without Legacy restore.
pub async fn install_retired_o_watcher(request: OOnlyInstallRequest) -> OOnlyInstallOutcome {
    use OOnlyInstallOutcome::*;
    use OOnlyInstallReason::*;
    if !crate::services::discord::health::legacy_supervision::is_retired(
        request.provider.as_str(),
        request.channel_id.get(),
    ) {
        return Deferred(NotRetired);
    }
    let Some(registry) = request.runtime.health_registry.upgrade() else {
        return Deferred(OwnershipUnavailable);
    };
    {
        let entries = registry.provider_entries_guard().await;
        let settings = entries
            .iter()
            .map(|entry| entry.shared.settings.try_read())
            .collect::<Result<Vec<_>, _>>();
        let Ok(settings) = settings else {
            return Deferred(OwnershipUnavailable);
        };
        let owners: Vec<_> = entries
            .iter()
            .zip(settings.iter())
            .filter(|(_, settings)| routed(&request, settings))
            .collect();
        if owners.len() != 1 || !Arc::ptr_eq(&owners[0].0.shared, &request.runtime) {
            return Deferred(OwnershipUnavailable);
        }
    }
    let initial = match tc::with_tmux_source_authority(&request.session_name, |authority| {
        source(&request, authority)
    }) {
        Ok((source, _)) => source,
        Err(reason) => return reason.outcome(),
    };
    let host = watch_host_of(
        &request.runtime,
        &request.provider,
        request.channel_id.get(),
        &request.session_name,
    )
    .await;
    #[cfg(test)]
    tests::after_host(request.channel_id).await;
    // Pin roster and routing through the synchronous source check, claim and spawn submission.
    let entries = registry.provider_entries_guard().await;
    let settings = entries
        .iter()
        .map(|entry| entry.shared.settings.try_read())
        .collect::<Result<Vec<_>, _>>();
    let Ok(settings) = settings else {
        return Deferred(OwnershipUnavailable);
    };
    let owners: Vec<_> = entries
        .iter()
        .zip(settings.iter())
        .filter(|(_, settings)| routed(&request, settings))
        .collect();
    if owners.len() != 1 || !Arc::ptr_eq(&owners[0].0.shared, &request.runtime) {
        return Deferred(OwnershipUnavailable);
    }
    let session = request.session_name.clone();
    tc::with_tmux_source_authority(&session, |authority| {
        let (current, initial_offset) = match source(&request, authority) {
            Ok(source) => source,
            Err(reason) => return reason.outcome(),
        };
        if current != initial {
            return Failed(SourceMismatch);
        }
        let handle = TmuxWatcherHandle {
            tmux_session_name: request.session_name.clone(),
            output_path: current.path.clone(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                super::super::super::tmux_watcher_now_ms(),
            )),
        };
        let cancel = handle.cancel.clone();
        let paused = handle.paused.clone();
        let resume_offset = handle.resume_offset.clone();
        let pause_epoch = handle.pause_epoch.clone();
        let turn_delivered = handle.turn_delivered.clone();
        let last_heartbeat_ts_ms = handle.last_heartbeat_ts_ms.clone();
        let claim = match claim_or_reuse_watcher_for_host(
            &request.runtime.tmux_watchers,
            request.channel_id,
            handle,
            &request.provider,
            "n4d_o_only_respawn",
            None,
            host,
        ) {
            Ok(claim) => claim,
            Err(_) => return Failed(WatchWithheld),
        };
        if !claim.should_spawn() {
            let incumbent = request.runtime.tmux_watchers.get(&request.channel_id);
            return if claim.owner_channel_id() == request.channel_id
                && incumbent.is_some_and(|h| {
                    h.tmux_session_name == request.session_name
                        && h.output_path == current.path
                        && Arc::ptr_eq(&h.cancel, &claim.incarnation().cancel)
                        && !h.cancel.load(Ordering::Relaxed)
                }) {
                AlreadyLive
            } else {
                Failed(SourceMismatch)
            };
        }
        request
            .runtime
            .record_tmux_watcher_reconnect(request.channel_id);
        super::super::super::task_supervisor::spawn_observed_tmux_watcher(
            "retired_o_watcher",
            request.runtime.clone(),
            request.session_name.clone(),
            cancel.clone(),
            tmux_output_watcher_with_restore(
                request.channel_id,
                request.http,
                request.runtime,
                current.path,
                request.session_name,
                initial_offset,
                cancel,
                paused,
                resume_offset,
                pause_epoch,
                turn_delivered,
                last_heartbeat_ts_ms,
                None,
            ),
        );
        Spawned
    })
}

#[cfg(test)]
#[path = "o_only_install_tests.rs"]
pub(in crate::services::discord) mod tests;
