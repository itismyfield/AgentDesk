use super::*;

pub(super) fn normalize_existing(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    existing: Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>> {
    let Some(mut binding) = existing else {
        return Some(None);
    };
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui
        || !Path::new(&binding.output_path).exists()
    {
        return Some(Some(binding));
    }
    let path = Path::new(&binding.output_path);
    let boundary =
        crate::services::codex_tui::session::checkpoint::at(path, binding.last_offset).ok()?;
    if boundary != binding.last_offset {
        crate::services::tui_prompt_dedupe::migrate_codex_raw_marker(authority, &binding).ok()?;
        binding.last_offset = boundary;
        if binding.relay_output_path.is_none() {
            binding.relay_last_offset = Some(boundary);
        }
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_under_source_authority(
            authority,
            path,
            binding.session_id.as_deref(),
            Some(boundary),
        )
        .ok()?;
        if !crate::services::tui_prompt_dedupe::register_tmux_runtime_binding_under_source_authority(
            authority,
            binding.clone(),
        ) {
            return None;
        }
    }
    Some(Some(binding))
}

/// Eligibility never freezes a new history boundary for an already published source.
pub(super) fn existing_source_is_live(
    tmux: &str,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    path: &Path,
    session: Option<&str>,
) -> Option<bool> {
    use crate::services::codex_tui::session::{checkpoint::Source, codex_tui_rollout_paths_same};
    let observed = codex_rollout_session_id(path);
    let session = session.map(str::to_owned).or_else(|| observed.clone());
    if crate::services::codex_tui::rollout_index::rollout_is_subagent(path)
        || session
            .as_ref()
            .zip(observed.as_ref())
            .is_some_and(|(a, b)| a != b)
    {
        return None;
    }
    if !codex_tui_rollout_paths_same(path, Path::new(&binding.output_path))
        || binding
            .session_id
            .as_ref()
            .zip(session.as_ref())
            .is_some_and(|(a, b)| a != b)
        || binding
            .relay_output_path
            .as_ref()
            .is_some_and(|current| Some(current) != relay_checkpoint(tmux, path).0.as_ref())
    {
        return Some(false);
    }
    Source::open(tmux, path, session).ok()?.0.check(tmux).ok()?;
    Some(crate::services::tui_prompt_dedupe::codex_restore_source_current(tmux, path))
}

pub(super) fn bootstrap(tmux: &str, path: &Path, session: Option<String>) -> Option<u64> {
    use crate::services::codex_tui::session::checkpoint::{ScanStats, Source};
    use crate::services::tui_prompt_dedupe as dedupe;
    let start = (|| {
        let (source, mut file) = Source::open(tmux, path, session).ok()?;
        let observed_session = codex_rollout_session_id(path);
        if source
            .session_id
            .as_ref()
            .zip(observed_session.as_ref())
            .is_some_and(|(a, b)| a != b)
        {
            return None;
        }
        let custody = dedupe::codex_restore_has_custody(tmux).ok()?;
        if custody && !custody_source_matches(tmux, path, source.session_id.as_deref())? {
            return None;
        }
        if let Some((start, restored)) = dedupe::codex_restore_scan_floor(tmux, path) {
            return (!custody || restored).then_some(start);
        }
        #[cfg(test)]
        super::idempotency_tests::bootstrap_pause(tmux);
        finish_confirmed_parked_owners(tmux, None);
        let plan = dedupe::prepare_codex_restore(tmux, source.clone(), custody).ok()?;
        let mut offset = 0;
        let mut total = ScanStats::default();
        while offset < source.complete_end {
            source.check(tmux).ok()?;
            let mut end = crate::services::codex_tui::session::checkpoint::boundary(
                &mut file,
                (offset + 64 * 1024).min(source.complete_end),
            )
            .ok()?;
            if end == offset {
                end = source.complete_end;
            }
            let (scan, stats) = super::super::idle_transcript_scan::scan_codex_records(
                file.try_clone().ok()?,
                offset,
                Some(end),
                false,
                offset,
            )
            .ok()?;
            if !matches!(scan, CodexIdleRolloutScan::NoPrompt { offset } if offset == end) {
                return None;
            }
            total.add(stats);
            offset = end;
        }
        dedupe::finish_codex_restore(tmux, plan, total).ok()
    })();
    if start.is_none() {
        tracing::warn!(
            status = "unknown_pending",
            "Codex bootstrap observation remains pending"
        );
    }
    start
}

pub(super) fn relay_checkpoint(tmux: &str, native: &Path) -> (Option<String>, Option<u64>) {
    let path = crate::services::tmux_common::session_temp_path(tmux, "jsonl");
    if crate::services::codex_tui::session::codex_tui_rollout_paths_same(Path::new(&path), native) {
        return (None, None);
    }
    let end = std::fs::metadata(&path)
        .ok()
        .and_then(|metadata| {
            crate::services::codex_tui::session::checkpoint::at(Path::new(&path), metadata.len())
                .ok()
        })
        .unwrap_or(0);
    (Some(path), Some(end))
}

fn custody_source_matches(tmux: &str, path: &Path, session: Option<&str>) -> Option<bool> {
    use crate::services::discord::inflight;
    let mut linked = crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux)
        .is_some_and(|marker| {
            crate::services::codex_tui::session::codex_tui_rollout_paths_same(
                &marker.rollout_path,
                path,
            ) && marker
                .session_id
                .as_deref()
                .zip(session)
                .is_some_and(|(a, b)| a == b)
        });
    let root = inflight::inflight_runtime_root()?;
    let entries = match std::fs::read_dir(root.join("codex")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(linked),
        Err(_) => return None,
    };
    for entry in entries {
        let path_entry = entry.ok()?.path();
        let Some(channel) = path_entry
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse().ok())
        else {
            continue;
        };
        let Some(row) =
            inflight::load_inflight_state_read_only_result(&ProviderKind::Codex, channel).ok()?
        else {
            continue;
        };
        if row.tmux_session_name.as_deref() == Some(tmux)
            && (row.output_path.as_deref().is_some_and(|saved| {
                !crate::services::codex_tui::session::codex_tui_rollout_paths_same(
                    Path::new(saved),
                    path,
                )
            }) || row
                .session_id
                .as_deref()
                .zip(session)
                .is_some_and(|(a, b)| a != b))
        {
            return Some(false);
        }
        linked |= row.tmux_session_name.as_deref() == Some(tmux)
            && row.output_path.as_deref().is_some_and(|saved| {
                crate::services::codex_tui::session::codex_tui_rollout_paths_same(
                    Path::new(saved),
                    path,
                )
            })
            && row
                .session_id
                .as_deref()
                .zip(session)
                .is_some_and(|(a, b)| a == b);
    }
    Some(linked)
}

#[cfg(all(unix, test))]
pub(crate) fn run_codex_rehydrate_pass_for_tests(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<u64> {
    struct RestoreView(Option<Vec<String>>);
    impl Drop for RestoreView {
        fn drop(&mut self) {
            CODEX_PASS_TMUX_VIEW.set(self.0.take());
        }
    }
    let _view = RestoreView(CODEX_PASS_TMUX_VIEW.replace(Some(vec![tmux_session_name.to_owned()])));
    rehydrate_existing_codex_tui_bindings(shared);
    shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)
        .map(|channel| channel.get())
}

pub(super) fn restore_codex_owner_channel(
    shared: &Arc<SharedData>,
    tmux: &str,
    channel: u64,
) -> Option<bool> {
    crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
        crate::services::tui_prompt_dedupe::codex_verified_channel_allowed_under_source_authority(
            authority, channel,
        )
        .then(|| {
            shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(tmux, ChannelId::new(channel))
        })
    })
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CodexTuiMarkerRehydrateDecision {
    Use {
        rollout_path: PathBuf,
        session_id: Option<String>,
    },
    TryFallback,
}

#[cfg(unix)]
pub(super) fn codex_tui_marker_rehydrate_decision(
    marker: &crate::services::codex_tui::session::CodexTuiRolloutMarker,
    claimed_rollout_paths: &HashSet<PathBuf>,
    duplicate_marker_paths: &HashSet<PathBuf>,
) -> CodexTuiMarkerRehydrateDecision {
    let path = &marker.rollout_path;
    let claim_path = canonical_rollout_claim_path(path);
    if path.exists()
        && !crate::services::codex_tui::rollout_index::rollout_is_subagent(path)
        && !duplicate_marker_paths.contains(&claim_path)
        && !rollout_path_is_claimed_for_other_session(path, claimed_rollout_paths)
    {
        return CodexTuiMarkerRehydrateDecision::Use {
            rollout_path: path.clone(),
            session_id: marker.session_id.clone(),
        };
    }
    CodexTuiMarkerRehydrateDecision::TryFallback
}

#[cfg(unix)]
pub(in crate::services::discord::tui_prompt_relay) fn codex_rollout_session_id(
    rollout_path: &Path,
) -> Option<String> {
    let file = std::fs::File::open(rollout_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    if !first_line.ends_with('\n') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(first_line.trim()).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return None;
    }
    value
        .get("payload")
        .and_then(|payload| payload.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(unix)]
pub(super) fn codex_tui_launch_modified_since(tmux_session_name: &str) -> std::time::SystemTime {
    crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "sh")
        .and_then(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        })
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// Only an exact native receipt can classify an already delivered span.
pub(in crate::services::discord::tui_prompt_relay) fn span_delivery_kind(
    channel: u64,
    path: &Path,
    span: (u64, u64),
) -> crate::services::codex_tui::session::checkpoint::ObservationKind {
    use crate::services::codex_tui::session::checkpoint::ObservationKind::{Delegated, Unknown};
    use crate::services::discord::{inflight, outbound::delivery_record as dr};
    let Some(row) = inflight::load_inflight_state_read_only(&ProviderKind::Codex, channel) else {
        return Unknown;
    };
    let Some(captured) = inflight::CodexRange::from_retained_tui_terminal(&row) else {
        return Unknown;
    };
    if captured.source.range != span
        || captured.receipt_source_path() != std::fs::canonicalize(path).ok()
    {
        return Unknown;
    }
    if dr::read_record(
        &ProviderKind::Codex,
        captured.source.offset_authority_channel_id,
    )
    .is_some_and(|record| {
        record.confirmed_deliveries.iter().any(|receipt| {
            receipt.source == captured.source
                && captured.confirmed_receipt_for_captured_row(&row, receipt.message_id)
        })
    }) {
        Delegated
    } else {
        Unknown
    }
}

pub(in crate::services::discord) fn finish_confirmed_parked_owners(
    tmux: &str,
    expected: Option<&crate::services::discord::inflight::CodexRange>,
) {
    use crate::services::{discord::outbound::delivery_record as dr, tui_prompt_dedupe as dedupe};
    for (turn, owner, source) in dedupe::codex_restore_parked_owners(tmux) {
        if expected.is_some_and(|range| {
            !source.matches_path(Path::new(&range.rollout_path))
                || source.session_id.as_deref() != Some(range.session_id.as_str())
                || owner.0 != range.source.delivery_channel_id
                || owner.1 != range.source.turn_nonce
                || owner.4 != range.identity.user_msg_id
                || owner.5 != range.source.range.0
        }) {
            continue;
        }
        let authority = expected.map_or_else(
            || {
                dr::watcher_owner_channel_for_delivery_channel(
                    &ProviderKind::Codex,
                    ChannelId::new(owner.0),
                    tmux,
                )
                .map_or(owner.0, |channel| channel.get())
            },
            |range| range.source.offset_authority_channel_id,
        );
        let Some(record) = dr::read_record(&ProviderKind::Codex, authority) else {
            continue;
        };
        let delivered = record.confirmed_deliveries.iter().find(|receipt| {
            let exact = &receipt.source;
            expected.is_none_or(|range| exact == &range.source)
                && exact.is_authoritative()
                && receipt.message_id != 0
                && receipt.delivery_channel_id == owner.0
                && exact.provider == ProviderKind::Codex.as_str()
                && exact.tmux_session_name == tmux
                && exact.turn_nonce == owner.1
                && exact.offset_authority_channel_id == authority
                && exact.delivery_channel_id == owner.0
                && exact.generation_mtime_ns == dr::current_generation_mtime_ns(tmux)
                && exact.range.0 == owner.5
                && source.record(tmux, exact.range.1).is_ok_and(|terminal| {
                    terminal["type"] == "event_msg"
                        && terminal["payload"]["type"] == "task_complete"
                        && terminal["payload"]["turn_id"].as_str() == Some(turn.as_str())
                })
        });
        if let Some(receipt) = delivered {
            dedupe::complete_codex_parked_owner(
                tmux,
                &turn,
                &owner,
                &source,
                receipt.source.range.1,
            );
        }
    }
}
