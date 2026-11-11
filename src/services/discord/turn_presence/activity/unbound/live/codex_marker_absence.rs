use super::*;
use crate::services::codex::legacy_hint::evidence::AbsenceStamp;

#[derive(Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct LegacyMarkerOnly;

impl super::super::super::Reading {
    // Classifies a stored snapshot; publication and the physical fence remain caller obligations.
    pub(in crate::services::discord) fn marker_only_legacy(
        &self,
        expected: &AbsenceStamp,
        current: &AbsenceStamp,
    ) -> Option<LegacyMarkerOnly> {
        let stamp = self.unbound.as_ref()?;
        let snapshot = stamp.snapshot.as_ref()?;
        let shared = snapshot.shared.upgrade()?;
        let inventory = &stamp.namespace.as_ref()?.inventory;
        if self.observed != observed(Activity::Unknown, "session_identity_unresolved")
            || !expected.matches(current)
            || snapshot.provider != ProviderKind::Codex
            || snapshot.exact != snapshot.canonical
            || snapshot.verdict(&shared, inventory) != Verdict::Hold("hosted_active_or_unknown")
        {
            return None;
        }
        let HostedLookup::Found(row) = &snapshot.exact else {
            return None;
        };
        let candidates = snapshot.files.candidates.as_ref().ok()?;
        let name = candidates.current.as_deref()?;
        let trace = snapshot.files.traces.get(name)?;
        let absent = match inventory {
            Inventory::Sessions(names) => !names.iter().any(|n| n == name),
            Inventory::Empty(_) => true,
            Inventory::ProbeFailed(_) => false,
        };
        let owner = HostedOwner {
            provider: snapshot.provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: snapshot.channel.to_string(),
            logical_key: name.into(),
            owner_node: crate::config::session_hosts::local_node().unwrap_or_default(),
            runtime_root: snapshot.runtime_root.as_ref()?.display().to_string(),
        };
        let key =
            adk_session::build_namespaced_session_key(&shared.token_hash, &snapshot.provider, name);
        (row.record == HostedRecord::Legacy
            && matches!(&expected.expected,
                crate::services::turn_orchestrator::MarkerRowExpectation::Legacy {
                    row_id, exact_session_key, ..
                } if row.session_id() == *row_id && key == *exact_session_key)
            && owner == expected.owner
            && matches!(snapshot.files.row, Ok(None))
            && trace.process.is_none()
            && trace.runtime == Ok(None)
            && trace.marker == HostKindMarker::Absent
            && expected.paths[0].path == marker_path(name, tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT)
            && expected.paths[1].path.as_path()
                == std::path::Path::new(&tmux_common::legacy_tmp_session_path(
                    name,
                    tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
                ))
            && absent)
            .then_some(LegacyMarkerOnly)
    }
}

#[cfg(test)]
#[path = "codex_marker_absence_tests.rs"]
mod tests;
