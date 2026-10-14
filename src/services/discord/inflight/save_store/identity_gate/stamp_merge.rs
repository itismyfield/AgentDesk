use super::*;

/// Stamp call target that keeps the long-standing outcome-only API usable by
/// read-only callers while allowing bridge callers to adopt the exact row
/// stamped under the inflight lock. A `(baseline, local)` target additionally
/// exposes the pre-mutation runtime projection for three-way field groups.
pub(in crate::services::discord) trait GuardedStampTarget {
    fn local_state(&self) -> &InflightTurnState;

    fn baseline_state(&self) -> Option<&InflightTurnState> {
        None
    }

    fn adopt_persisted(self, _persisted: InflightTurnState)
    where
        Self: Sized,
    {
    }
}

impl GuardedStampTarget for &InflightTurnState {
    fn local_state(&self) -> &InflightTurnState {
        *self
    }
}

impl GuardedStampTarget for &mut InflightTurnState {
    fn local_state(&self) -> &InflightTurnState {
        &**self
    }

    fn adopt_persisted(self, persisted: InflightTurnState) {
        self.clone_from(&persisted);
    }
}

impl<'a> GuardedStampTarget for (&'a InflightTurnState, &'a mut InflightTurnState) {
    fn local_state(&self) -> &InflightTurnState {
        &*self.1
    }

    fn baseline_state(&self) -> Option<&InflightTurnState> {
        Some(self.0)
    }

    fn adopt_persisted(self, persisted: InflightTurnState) {
        self.1.clone_from(&persisted);
    }
}

pub(super) fn merge_forward_response_progress(
    durable: (&str, usize),
    local: (&str, usize),
) -> Option<(String, usize)> {
    let (durable_body, durable_offset) = durable;
    let (local_body, local_offset) = local;
    let merged_body = if local_body.starts_with(durable_body) {
        local_body
    } else if durable_body.starts_with(local_body) {
        durable_body
    } else {
        return None;
    };
    Some((
        merged_body.to_string(),
        normalize_response_sent_offset(merged_body, durable_offset.max(local_offset)),
    ))
}

/// Preserve same-turn response/tool progress while a narrow runtime stamp is
/// applied. When the caller's save generation is current, its tool projection
/// is safe to carry forward. After a concurrent durable write, durable tool
/// values win conflicts while monotonic/previously-empty evidence still merges.
pub(super) fn merge_runtime_stamp_progress(
    durable: &mut InflightTurnState,
    local: &InflightTurnState,
) -> bool {
    // A stamp planned against another coordinate source must not move this source's runtime,
    // cursor or body; it is refused like a diverged response.
    if !durable.same_coordinate_source(local) || !durable.merge_replay_projection(local) {
        return false;
    }
    let Some((full_response, response_sent_offset)) = merge_forward_response_progress(
        (&durable.full_response, durable.response_sent_offset),
        (&local.full_response, local.response_sent_offset),
    ) else {
        return false;
    };
    durable.full_response = full_response;
    durable.response_sent_offset = response_sent_offset;

    let same_generation = durable.save_generation == local.save_generation;
    macro_rules! preserve_tool_field {
        ($field:ident) => {
            if same_generation || durable.$field.is_none() {
                durable.$field.clone_from(&local.$field);
            }
        };
    }
    preserve_tool_field!(current_tool_line);
    preserve_tool_field!(prev_tool_status);
    preserve_tool_field!(last_tool_name);
    preserve_tool_field!(last_tool_summary);
    preserve_tool_field!(task_notification_kind);
    durable.any_tool_used |= local.any_tool_used;
    durable.has_post_tool_text |= local.has_post_tool_text;
    durable.last_offset = durable.last_offset.max(local.last_offset);
    for frozen_id in &local.streaming_rollover_frozen_msg_ids {
        if !durable
            .streaming_rollover_frozen_msg_ids
            .contains(frozen_id)
        {
            durable.streaming_rollover_frozen_msg_ids.push(*frozen_id);
        }
    }
    true
}

pub(super) fn coordinate_projection_matches(
    durable: &InflightTurnState,
    local: &InflightTurnState,
    expected: &InflightTurnIdentity,
) -> bool {
    let Some(marker) = durable.legacy_replay_coordinate.as_ref() else {
        return false;
    };
    let (Some(raw), Some(installed)) = (&marker.raw, &marker.installed) else {
        return false;
    };
    marker.version == 1
        && marker.phase
            == crate::services::discord::inflight::model::CoordinateInstallPhase::Installed
        && durable.output_path.as_deref() == Some(installed.output_path.as_str())
        && durable.turn_start_offset == Some(0)
        && local.coordinate_source().is_none()
        && local.turn_start_offset == marker.prior.turn_start_offset
        && expected.matches_state(local)
        && durable.user_msg_id == local.user_msg_id
        && durable.started_at == local.started_at
        && durable.provider == local.provider
        && durable.channel_id == local.channel_id
        && durable.request_owner_user_id == local.request_owner_user_id
        && durable.born_generation == local.born_generation
        && durable.tmux_session_name == local.tmux_session_name
        && durable
            .turn_nonce
            .as_deref()
            .is_some_and(|nonce| !nonce.trim().is_empty())
        && durable.turn_nonce == local.turn_nonce
        && durable.session_id == local.session_id
        && durable.native_turn_id == local.native_turn_id
        && durable.external_turn_id == local.external_turn_id
        && durable.finalizer_turn_id == local.finalizer_turn_id
        && durable.injected_prompt_message_id == local.injected_prompt_message_id
        && durable.session_id.as_deref() == Some(raw.session_id.as_str())
        && durable.native_turn_id.as_deref() == Some(raw.native_turn_id.as_str())
        && raw.prefix_record_end > raw.turn_start
        && raw.prefix_len <= durable.full_response.len()
        && durable
            .full_response
            .get(..raw.prefix_len)
            .is_some_and(|prefix| local.full_response.get(..raw.prefix_len) == Some(prefix))
}

fn merge_coordinate_projection(
    durable: &mut InflightTurnState,
    local: &InflightTurnState,
) -> Option<bool> {
    let before = (
        durable.full_response.len(),
        durable.response_sent_offset,
        durable.replay_hold_reasons.len(),
        durable.streaming_rollover_frozen_msg_ids.len(),
        durable.replay_receipt_id,
    );
    let (body, offset) = merge_forward_response_progress(
        (&durable.full_response, durable.response_sent_offset),
        (&local.full_response, local.response_sent_offset),
    )?;
    if !durable.merge_replay_projection(local) {
        return None;
    }
    durable.full_response = body;
    durable.response_sent_offset = offset;
    for id in &local.streaming_rollover_frozen_msg_ids {
        if !durable.streaming_rollover_frozen_msg_ids.contains(id) {
            durable.streaming_rollover_frozen_msg_ids.push(*id);
        }
    }
    let after = (
        durable.full_response.len(),
        durable.response_sent_offset,
        durable.replay_hold_reasons.len(),
        durable.streaming_rollover_frozen_msg_ids.len(),
        durable.replay_receipt_id,
    );
    Some(before != after)
}

/// Source-refused stamps preserve only proven same-turn projection. The outcome denies
/// runtime adoption; a mutable target still receives the actual durable projection.
pub(super) fn guard_coordinate_stamp<T: GuardedStampTarget>(
    root: &Path,
    path: &Path,
    durable: &mut InflightTurnState,
    local: &InflightTurnState,
    expected: &InflightTurnIdentity,
    target: T,
    caller: &'static str,
) -> Result<T, GuardedSaveOutcome> {
    if durable.restart_mode.is_some() || durable.rebind_origin {
        return Err(GuardedSaveOutcome::AuthorityPinned);
    }
    if durable.same_coordinate_source(local) {
        return if expected.matches_state(durable) {
            Ok(target)
        } else {
            Err(GuardedSaveOutcome::SuccessorOwned)
        };
    }
    if durable.terminal_delivery_committed
        || !coordinate_projection_matches(durable, local, expected)
    {
        return Err(GuardedSaveOutcome::AuthorityPinned);
    }
    let mut projection = durable.clone();
    match merge_coordinate_projection(&mut projection, local) {
        Some(false) => target.adopt_persisted(durable.clone()),
        Some(true) => {
            let saved = crate::services::discord::inflight::store::persist_under_lock_with_snapshot(
                root,
                path,
                &projection,
                caller,
            );
            match saved {
                Ok(Some(saved)) => match load_inflight_state_unlocked(path) {
                    Some(reread) if reread.save_generation == saved.save_generation => {
                        target.adopt_persisted(reread)
                    }
                    _ => return Err(GuardedSaveOutcome::IoError),
                },
                Ok(None) => return Err(GuardedSaveOutcome::AuthorityPinned),
                Err(_) => return Err(GuardedSaveOutcome::IoError),
            }
        }
        None => return Err(GuardedSaveOutcome::AuthorityPinned),
    }
    Err(GuardedSaveOutcome::AuthorityPinned)
}
