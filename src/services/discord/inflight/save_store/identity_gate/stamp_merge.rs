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
