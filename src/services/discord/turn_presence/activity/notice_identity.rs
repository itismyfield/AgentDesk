//! Notice metadata follows the same publication stamp as its activity judgment.
use super::{Answer, Observed};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) enum ExecutionIdentity {
    Nonce(String),
    LegacySession(String),
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct NoticeObservation {
    pub identity: ExecutionIdentity,
    pub target_label: Option<String>,
    pub reason: &'static str,
    pub hold_leaf: Option<&'static str>,
    pub progress: bool,
}

impl NoticeObservation {
    pub(super) fn legacy(reason: &'static str, label: &str) -> Self {
        let target_label = (!label.is_empty()).then(|| label.to_string());
        let identity = target_label.clone().map_or(
            ExecutionIdentity::Unresolved,
            ExecutionIdentity::LegacySession,
        );
        Self {
            identity,
            target_label,
            reason,
            hold_leaf: None,
            progress: false,
        }
    }

    pub(super) fn bound(reason: &'static str, label: Option<&str>) -> Self {
        let mut notice = Self::legacy(reason, label.unwrap_or_default());
        if let Some(label) = &notice.target_label {
            notice.identity = marker_identity(label);
        }
        notice
    }

    pub(super) fn current(&self) -> bool {
        self.target_label
            .as_ref()
            .is_none_or(|label| marker_identity(label) == self.identity)
    }

    pub(super) fn observed(mut self, observed: Observed) -> Self {
        self.reason = observed.reason;
        self
    }
}

// Construct canonical paths without the session path helper's lazy directory creation.
fn marker_identity(label: &str) -> ExecutionIdentity {
    let tc = crate::services::tmux_common::legacy_tmp_session_path(label, "spawn_nonce");
    let legacy = PathBuf::from(tc);
    let canonical = crate::services::tmux_common::persistent_sessions_dir()
        .and_then(|root| legacy.file_name().map(|file| root.join(file)));
    for path in canonical.into_iter().chain(std::iter::once(legacy)) {
        match std::fs::read_to_string(path) {
            Ok(raw) if !raw.trim().is_empty() => {
                return ExecutionIdentity::Nonce(raw.trim().into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return ExecutionIdentity::Unresolved,
        }
    }
    ExecutionIdentity::LegacySession(label.into())
}

pub(super) fn progress(mut answer: Answer) -> Answer {
    if let Some(stamp) = &mut answer.1 {
        stamp.notice.progress = true;
    }
    answer
}
