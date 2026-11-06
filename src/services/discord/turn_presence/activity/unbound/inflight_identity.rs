//! Presence excludes relay progress, including the episode pin's current_msg_id rollover axis.
//! The exhaustive field partition lives under the row model to preserve its private intake ID.
use super::inflight_diagnostics::Rejection;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::turn_presence::activity) enum Kind {
    Projection,
    LegacyStrict,
}

pub(super) fn compare(
    before: &Files,
    after: &Files,
    phase: &'static str,
) -> Result<Kind, Rejection> {
    fn row<'a>(files: &'a Files, phase: &'static str) -> Result<&'a InflightTurnState, Rejection> {
        match &files.row {
            Ok(Some(row)) => Ok(row),
            Ok(None) => Err(Rejection::GuardRejected {
                guard: "row_missing",
            }),
            Err(_) => Err(Rejection::ReadFailed {
                phase,
                field: "row",
            }),
        }
    }
    let (a, b) = (row(before, phase)?, row(after, phase)?);
    let legacy = |row: &InflightTurnState| {
        row.turn_nonce
            .as_deref()
            .is_none_or(|nonce| nonce.trim().is_empty())
            && inflight::InflightTurnIdentity::from_state(row).is_unnameable()
    };
    if legacy(a) || legacy(b) {
        return if before.row_identity == after.row_identity {
            Ok(Kind::LegacyStrict)
        } else {
            Err(Rejection::InputsChanged {
                phase,
                field: "row.legacy_strict",
            })
        };
    }
    let fields = |row: &InflightTurnState| {
        row.presence_identity_fields()
            .map_err(|_| Rejection::ReadFailed {
                phase,
                field: "row.projection",
            })
    };
    for ((field, a), (_, b)) in fields(a)?.into_iter().zip(fields(b)?) {
        if a != b {
            return Err(Rejection::InputsChanged { phase, field });
        }
    }
    Ok(Kind::Projection)
}

pub(super) fn ignored(before: &Files, after: &Files) -> Vec<&'static str> {
    let (Ok(Some(a)), Ok(Some(b))) = (&before.row, &after.row) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    macro_rules! progress {
        ($($field:ident),+ $(,)?) => {$(
            if a.$field != b.$field { fields.push(concat!("row.", stringify!($field))); }
        )+};
    }
    progress!(
        updated_at,
        save_generation,
        last_offset,
        last_watcher_relayed_offset,
        full_response,
        response_sent_offset,
        current_msg_id,
        current_msg_len,
        streaming_rollover_frozen_msg_ids,
        status_message_id,
        status_panel_generation,
        current_tool_line,
        prev_tool_status,
        last_tool_name,
        last_tool_summary,
        any_tool_used,
        has_post_tool_text,
        long_running_placeholder_active
    );
    if fields.is_empty() && before.row_identity != after.row_identity {
        fields.push("row.payload");
    }
    fields
}

#[derive(Clone, Copy)]
pub(super) enum Comparison {
    Strict,
    Inflight,
}
impl Comparison {
    pub(super) fn snapshots(
        self,
        a: &Snapshot,
        b: &Snapshot,
        phase: &'static str,
    ) -> Result<(), Rejection> {
        match self {
            Self::Strict if a.same_inputs(b) => Ok(()),
            Self::Strict => Err(Rejection::InputsChanged {
                phase,
                field: "strict_inputs",
            }),
            Self::Inflight => a.same_inflight_inputs(b, phase).map(|_| ()),
        }
    }
    pub(super) fn files(self, a: &Files, b: &Files, phase: &'static str) -> Result<(), Rejection> {
        match self {
            Self::Strict if a.same(b) => Ok(()),
            Self::Strict => Err(Rejection::InputsChanged {
                phase,
                field: "strict_files",
            }),
            Self::Inflight => {
                if let Some(field) = a.context_changed(b) {
                    return Err(Rejection::InputsChanged { phase, field });
                }
                compare(a, b, phase).map(|_| ())
            }
        }
    }
    pub(super) fn memory(self, snapshot: &Snapshot) -> Result<(), Rejection> {
        match self {
            Self::Strict if snapshot.memory_current() => Ok(()),
            Self::Strict => Err(Rejection::Superseded {
                phase: "publication_fence",
                field: "memory",
            }),
            Self::Inflight => snapshot.inflight_memory_current(),
        }
    }
}

#[cfg(test)]
#[path = "inflight_identity_tests.rs"]
mod tests;
