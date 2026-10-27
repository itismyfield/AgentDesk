use super::*;

pub(super) async fn admit_uncaptured_terminal(
    local: &mut InflightTurnState,
    baseline: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    can_deliver_directly: bool,
    message: StreamMessage,
) -> Result<(StreamMessage, Option<TuiTerminalRange>, bool), GuardedSaveOutcome> {
    if !matches!(
        &message,
        StreamMessage::CodexTuiTerminalDone {
            captured_source: None,
            ..
        }
    ) {
        return Ok((message, None, false));
    }
    let requires_admission = matches!(
        &message,
        StreamMessage::CodexTuiTerminalDone {
            kind: crate::services::agent_protocol::NativeTerminalKind::Aborted,
            ..
        }
    ) || ((can_deliver_directly || mutant("headless_admission"))
        && matches!(&message, StreamMessage::CodexTuiTerminalDone { result, .. } if result.is_empty()));
    let mut owned_local = local.clone();
    let mut owned_baseline = baseline.clone();
    let expected = expected.clone();
    // Legacy Codex admission must also acquire, commit and release off the scheduler.
    let (admitted, owned_local, owned_baseline) = tokio::task::spawn_blocking(move || {
        let admitted = owned_local.admit_codex_tui_terminal_frame(
            &mut owned_baseline,
            &expected,
            can_deliver_directly,
            message,
        );
        (admitted, owned_local, owned_baseline)
    })
    .await
    .map_err(|_| GuardedSaveOutcome::IoError)?;
    *local = owned_local;
    *baseline = owned_baseline;
    // Direct empty completions and aborts keep their obligation when source proof is rejected.
    match admitted {
        (_, None, _) if requires_admission => Err(GuardedSaveOutcome::AuthorityPinned),
        admitted => Ok(admitted),
    }
}

fn mutant(name: &str) -> bool {
    #[cfg(test)]
    return std::env::var("ADK_EMPTY_TERMINAL_MUTANT").ok().as_deref() == Some(name);
    #[cfg(not(test))]
    {
        let _ = name;
        false
    }
}

pub(super) fn nonce_matches(row: &InflightTurnState, nonce: &str) -> bool {
    nonempty(row.turn_nonce.as_deref()) == Some(nonce) || mutant("nonce")
}

pub(super) fn body_admissible(result: &str) -> bool {
    result.is_empty() || !result.trim().is_empty()
}

pub(super) fn range_matches(row: &InflightTurnState, start: u64) -> bool {
    (row.turn_start_offset == Some(start) && row.last_offset == start) || mutant("range")
}

pub(super) fn legacy_source(
    row: &InflightTurnState,
    rollout: &str,
    tmux: &str,
    session: &str,
    (start, end): (u64, u64),
) -> Result<(PathBuf, i64), GuardedSaveOutcome> {
    let (canonical, file_len) =
        canonical_regular_file(rollout).ok_or(GuardedSaveOutcome::AuthorityPinned)?;
    if file_len < end || (!marker_matches(tmux, &canonical, session, start) && !mutant("source")) {
        return Err(GuardedSaveOutcome::AuthorityPinned);
    }
    // A dead prelaunch source may have no raw path; live sources retain their exact binding.
    let cold = start == 0
        && row
            .output_path
            .as_deref()
            .is_none_or(|path| path == tmux_runtime_paths(tmux).0);
    let warm = row
        .output_path
        .as_deref()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .as_deref()
        == Some(canonical.as_path())
        && binding_matches(tmux, &canonical, session, [start, end]);
    let generation = tmux_generation_file_mtime_ns(tmux);
    if ((!cold && !warm) && !mutant("source")) || generation == 0 {
        return Err(GuardedSaveOutcome::AuthorityPinned);
    }
    Ok((canonical, generation))
}

pub(super) struct CapturedTerminalAdmission {
    pub(super) provider: ProviderKind,
    pub(super) runtime: RuntimeHandoffKind,
    pub(super) result: String,
    pub(super) session_id: Option<String>,
    pub(super) transcript_path: String,
    pub(super) tmux_session_name: String,
    pub(super) turn_nonce: String,
    pub(super) source_start: u64,
    pub(super) complete_record_end: u64,
    pub(super) generation_mtime_ns: i64,
    pub(super) source_file_dev: u64,
    pub(super) source_file_ino: u64,
    pub(super) captured: std::sync::Arc<crate::services::provider::CancelToken>,
    pub(super) can_deliver_directly: bool,
    pub(super) kind: crate::services::agent_protocol::NativeTerminalKind,
}

impl CapturedTerminalAdmission {
    pub(super) fn admit(
        self,
        local: &mut InflightTurnState,
        baseline: &mut InflightTurnState,
        expected: &InflightTurnIdentity,
    ) -> Result<(StreamMessage, Option<TuiTerminalRange>, bool), GuardedSaveOutcome> {
        let Self {
            provider,
            runtime,
            result,
            session_id,
            transcript_path,
            tmux_session_name,
            turn_nonce,
            source_start,
            complete_record_end,
            generation_mtime_ns,
            source_file_dev,
            source_file_ino,
            captured,
            can_deliver_directly,
            kind,
        } = self;
        let mismatch = GuardedSaveOutcome::SuccessorOwned;
        let root = inflight_runtime_root().ok_or(mismatch)?;
        let path = inflight_state_path(&root, &provider, local.channel_id);
        let _lock = lock_inflight_state_path(&path).map_err(|_| GuardedSaveOutcome::IoError)?;
        let mut fresh = read_inflight_state_for_guarded_write(
            &path,
            &provider,
            local.channel_id,
            expected,
            "turn_bridge::captured_tui_terminal_range",
        )?;
        crate::services::tmux_common::with_tmux_source_authority(&tmux_session_name, |authority| {
            let (canonical, file_len) = canonical_regular_file(&transcript_path).ok_or(mismatch)?;
            let mut binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
            .ok_or(mismatch)?;
            let session = nonempty(session_id.as_deref())
                .or(nonempty(local.session_id.as_deref()))
                .unwrap_or_default()
                .to_owned();
            // Only the captured FD/generation/original-actor witness fills a missing session ID;
            // conflicting known sessions remain a hard mismatch.
            if [
                local.session_id.as_deref(),
                fresh.session_id.as_deref(),
                binding.session_id.as_deref(),
            ]
            .into_iter()
            .filter_map(nonempty)
            .any(|known| known != session)
            {
                return Err(mismatch);
            }
            binding.session_id = nonempty(Some(&session)).map(str::to_owned);
            #[cfg(unix)]
            let parked = ParkedProof::new(
                &fresh,
                &captured,
                &canonical,
                &session,
                (source_start, complete_record_end),
            );
            #[cfg(not(unix))]
            let parked: Option<ParkedProof> = None;
            if !can_deliver_directly
                || local.provider_kind() != Some(provider.clone())
                || local.runtime_kind != Some(runtime)
                || local.turn_start_offset != Some(source_start)
                || source_start >= complete_record_end
                || file_len < complete_record_end
                || generation_mtime_ns <= 0
                || tmux_generation_file_mtime_ns(&tmux_session_name) != generation_mtime_ns
                || file_identity(&canonical) != Some((source_file_dev, source_file_ino))
                || !captured_binding_matches(
                    &binding,
                    &canonical,
                    &session,
                    (source_start, complete_record_end),
                    false,
                    runtime,
                    parked.as_ref(),
                )
            {
                return Err(mismatch);
            }
            if fresh.turn_nonce.as_deref() != Some(turn_nonce.as_str())
                || fresh.tmux_session_name.as_deref() != Some(tmux_session_name.as_str())
                || fresh.runtime_kind != Some(runtime)
                || fresh.turn_start_offset != Some(source_start)
                || fresh.last_offset > complete_record_end
                || fresh.restart_mode.is_some()
                || fresh.rebind_origin
                || fresh.terminal_delivery_committed
                || !StreamRelayAuthority::from_state(&fresh).bridge_owns_relay()
                || fresh
                    .output_path
                    .as_deref()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    .as_deref()
                    != Some(canonical.as_path())
            {
                return Err(mismatch);
            }
            let before_admission = InflightEpisodePin::from_state(&fresh);
            fresh.session_id.clone_from(&binding.session_id);
            let mut range = persist_terminal_range(
                &root,
                &path,
                (local, &mut *baseline),
                fresh,
                can_deliver_directly,
                (&result, canonical, &session),
                (
                    (source_start, complete_record_end),
                    generation_mtime_ns,
                    Some((source_file_dev, source_file_ino)),
                ),
            )?;
            range.parked = parked.map(|mut proof| {
                proof.identity = range.identity.clone();
                Box::new(proof)
            });
            crate::services::discord::tui_prompt_relay::preserve_admitted_source(
                &before_admission,
                baseline,
                &captured,
            );
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding_under_source_authority(authority, binding);
            Ok((
                StreamMessage::Done { result, session_id },
                Some(range.of_kind(kind)),
                true,
            ))
        })
    }
}

#[derive(Clone, Debug)]
pub(super) struct ParkedProof {
    source: crate::services::codex_tui::session::checkpoint::Source,
    identity: InflightTurnIdentity,
    exact: ExactJsonlSourceIdentity,
    actor: std::sync::Weak<crate::services::provider::CancelToken>,
    /// Owner and opening record read while the source was live, to settle after its receipt.
    turn: String,
    owner: crate::services::codex_tui::session::checkpoint::ParkedOwnerKey,
    opening: Option<serde_json::Value>,
}

impl ParkedProof {
    #[cfg(unix)]
    fn new(
        row: &InflightTurnState,
        actor: &std::sync::Arc<crate::services::provider::CancelToken>,
        path: &Path,
        session: &str,
        range: (u64, u64),
    ) -> Option<Self> {
        let tmux = row.tmux_session_name.as_deref()?;
        let owner = (
            row.channel_id,
            row.turn_nonce.clone()?,
            row.external_turn_id.clone()?,
            row.session_key.clone()?,
            row.user_msg_id,
            range.0,
        );
        let turn = row.native_turn_id.as_deref()?;
        let source =
            crate::services::tui_prompt_dedupe::codex_parked_owner_source(tmux, turn, &owner)?;
        if row.provider_kind() != Some(ProviderKind::Codex)
            || source.session_id.as_deref() != Some(session)
            || std::fs::canonicalize(&source.path).ok()?.as_path() != path
            || range.0 >= range.1
            || range.1 > source.complete_end
        {
            return None;
        }
        let terminal = source.record(tmux, range.1).ok()?;
        if terminal["type"] != "event_msg"
            || terminal["payload"]["type"] != "task_complete"
            || terminal["payload"]["turn_id"].as_str() != Some(turn)
        {
            return None;
        }
        let opening = source.record(tmux, range.0).ok();
        Some(Self {
            source,
            identity: InflightTurnIdentity::from_state(row),
            actor: std::sync::Arc::downgrade(actor),
            turn: turn.to_owned(),
            opening,
            owner: owner.clone(),
            exact: ExactJsonlSourceIdentity {
                provider: row.provider.clone(),
                tmux_session_name: tmux.to_owned(),
                turn_nonce: owner.1,
                range,
                generation_mtime_ns: tmux_generation_file_mtime_ns(tmux),
                offset_authority_channel_id: row.delivery_record_owner_channel_id(),
                delivery_channel_id: row.channel_id,
            },
        })
    }

    fn permits(&self, path: &Path, session: &str, range: (u64, u64)) -> bool {
        self.exact.provider == ProviderKind::Codex.as_str()
            && self.exact.range == range
            && self.source.session_id.as_deref() == Some(session)
            && std::fs::canonicalize(&self.source.path).ok().as_deref() == Some(path)
            && self.source.check(&self.exact.tmux_session_name).is_ok()
            && self.actor.upgrade().is_some_and(|actor| {
                !actor.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    && actor.turn_nonce() == Some(self.exact.turn_nonce.as_str())
            })
    }

    pub(super) async fn current_actor_matches(&self, shared: &SharedData) -> bool {
        let Some(actor) = self.actor.upgrade() else {
            return false;
        };
        let Some(mailbox) = shared.mailbox_peek(ChannelId::new(self.exact.delivery_channel_id))
        else {
            return false;
        };
        let current = mailbox.snapshot().await;
        current.active_user_message_id == Some(MessageId::new(self.identity.user_msg_id))
            && current.active_turn_nonce.as_deref() == Some(self.exact.turn_nonce.as_str())
            && current
                .cancel_token
                .as_ref()
                .is_some_and(|now| std::sync::Arc::ptr_eq(now, &actor))
            && !actor.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn matches(&self, range: &TuiTerminalRange) -> bool {
        self.identity == range.identity
            && self.exact == range.source
            && self.permits(
                Path::new(&range.rollout_path),
                &range.session_id,
                range.source.range,
            )
    }
}

pub(super) fn captured_binding_matches(
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    path: &Path,
    session: &str,
    (start, end): (u64, u64),
    receipt: bool,
    runtime: RuntimeHandoffKind,
    parked: Option<&ParkedProof>,
) -> bool {
    let (source_path, cursor) = if runtime == RuntimeHandoffKind::CodexTui {
        (binding.output_path.as_str(), binding.last_offset)
    } else {
        (binding.relay_output_path(), binding.relay_last_offset())
    };
    binding.runtime_kind == runtime
        && nonempty(binding.session_id.as_deref()) == nonempty(Some(session))
        && canonical_regular_file(source_path).is_some_and(|(bound, _)| bound == path)
        && (cursor >= start)
        && (cursor <= end
            || receipt
            || (runtime == RuntimeHandoffKind::CodexTui
                && parked.is_some_and(|proof| proof.permits(path, session, (start, end)))))
}

impl TuiTerminalRange {
    pub(in crate::services::discord) fn complete_parked_delivery(&self) {
        #[cfg(unix)]
        if self.source.provider == ProviderKind::Codex.as_str() {
            // The admission proof settles without current liveness; a range admitted without one
            // still looks up its receipt on the live source.
            let settled = self.parked.as_deref().is_some_and(|proof| {
                proof.exact == self.source
                    && proof.opening.as_ref().is_some_and(|opening| {
                        crate::services::tui_prompt_dedupe::settle_codex_parked_delivery(
                            &self.source.tmux_session_name,
                            &proof.turn,
                            &proof.owner,
                            &proof.source,
                            opening,
                            self.source.range.1,
                        )
                    })
            });
            if !settled {
                crate::services::discord::tui_prompt_relay::rehydration::codex_marker::finish_confirmed_parked_owners(&self.source.tmux_session_name, Some(self));
            }
        }
    }
    /// Consume an existing ACK after process-local bindings disappear. This
    /// never returns publication authority or installs a replacement binding.
    pub(in crate::services::discord) fn confirmed_receipt_for_captured_row(
        &self,
        local: &InflightTurnState,
        message_id: u64,
    ) -> bool {
        use crate::services::{discord::outbound::delivery_record, tui_prompt_dedupe as dedupe};
        let confirmed = self
            .with_revalidated_row(local, |fresh| {
                let source = &self.source;
                if !source.is_authoritative()
                    || nonempty(fresh.session_id.as_deref()) != nonempty(Some(&self.session_id))
                    || (self.source_file_identity.is_some()
                        && fresh.tui_terminal_source_file_identity != self.source_file_identity)
                {
                    return false;
                }
                crate::services::tmux_common::with_tmux_source_authority(
                    &source.tmux_session_name,
                    |authority| {
                        let Some(path) = self.receipt_source_path() else {
                            return false;
                        };
                        if !self.matches_durable_terminal(fresh, &path)
                            || tmux_generation_file_mtime_ns(&source.tmux_session_name)
                                != source.generation_mtime_ns
                        {
                            return false;
                        }
                        if dedupe::runtime_binding_for_tmux_session_under_source_authority(
                            authority,
                        )
                        .is_some()
                            && !self.source_receipt_is_live(authority)
                        {
                            return false;
                        }
                        delivery_record::confirmed_delivery_receipt_exists(
                            &ProviderKind::from_str_or_unsupported(&source.provider),
                            poise::serenity_prelude::ChannelId::new(fresh.channel_id),
                            message_id,
                            source,
                        )
                    },
                )
            })
            .unwrap_or(false);
        if confirmed {
            self.complete_parked_delivery();
        }
        confirmed
    }
}
