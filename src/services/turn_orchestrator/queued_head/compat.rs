//! V2 is inspectable, but is never a writable legacy queue.
use super::v2_types::SnapshotV2;
use super::*;

pub(crate) enum SnapshotRead {
    Legacy(Vec<PendingQueueItem>, Option<Box<SnapshotMeta>>),
    ReadOnlyV2(Box<SnapshotV2>),
}

impl SnapshotRead {
    pub(crate) fn unfinished_claude_handoff(&self) -> bool {
        matches!(self, Self::ReadOnlyV2(v2) if v2.unfinished_handoff.is_some())
    }
    /// Whether a V2 file holds input by [`SnapshotV2::holds_input`]; a legacy file never does here.
    pub(crate) fn v2_holds_input(&self) -> bool {
        matches!(self, Self::ReadOnlyV2(v2) if v2.holds_input())
    }
}

impl SnapshotV2 {
    /// Whether the queue holds input, including a continuation awaiting ready/admission.
    /// Settled binds and terminal identities are delivered history.
    pub(crate) fn holds_input(&self) -> bool {
        use super::v2_types::BindAt;
        let closed = |identity: &QueueIdentity, attempt: &super::v2_types::AttemptIdentity| {
            (self.retained_inputs.iter()).any(|entry| entry.closes_bind(identity, attempt))
        };
        let bound = |bind: &super::v2_types::AdmissionBind| match &bind.at {
            BindAt::Queued { .. } | BindAt::Settled { .. } => false,
            BindAt::Attempt { identity, attempt } => !closed(identity, attempt),
            BindAt::Opaque => true,
        };
        !self.items.is_empty()
            || self
                .effort_restart
                .as_ref()
                .is_some_and(|intent| intent.pending().is_some())
            || !self.owed_drop_receipts.is_empty()
            || self.head_injection.is_some()
            || (self.retained_inputs.iter()).any(|entry| entry.owed_to_owner())
            || self.admission_binds.iter().any(bound)
    }
}

pub(crate) fn inspect(content: &str) -> Result<SnapshotRead, String> {
    let value: serde_json::Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
    if value.get("version").and_then(serde_json::Value::as_u64) == Some(2) {
        let snapshot = serde_json::from_value(value).map_err(|e| e.to_string())?;
        return Ok(SnapshotRead::ReadOnlyV2(Box::new(snapshot)));
    }
    let (items, meta) = parse_snapshot(content)?;
    Ok(SnapshotRead::Legacy(items, meta.map(Box::new)))
}

/// /clear's guard: only a parsed non-legacy object refuses; a failed read or parse lets the clear
/// replace the file as main does.
pub(crate) fn require_clearable(path: &Path) -> Result<(), String> {
    match std::fs::read(path) {
        Ok(content) => refuse_non_legacy(path, &String::from_utf8_lossy(&content)),
        Err(_) => Ok(()),
    }
}

/// The legacy save's guard: a missing file or a legacy one saves, but a file it cannot read is
/// kept, so the save never replaces a snapshot nobody read.
pub(crate) fn require_legacy_write(path: &Path) -> Result<(), String> {
    match std::fs::read(path) {
        Ok(content) => refuse_non_legacy(path, &String::from_utf8_lossy(&content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "read {}: {error}; legacy save refused and the original file kept",
            path.display()
        )),
    }
}

fn refuse_non_legacy(path: &Path, content: &str) -> Result<(), String> {
    if content.trim_start().starts_with('{')
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
        && value.get("version").and_then(serde_json::Value::as_u64) != Some(1)
    {
        return Err(format!(
            "read-only queue snapshot {}: legacy mutation refused; preserve the original file and use a V2-capable recovery binary for manual inspection",
            path.display()
        ));
    }
    Ok(())
}
