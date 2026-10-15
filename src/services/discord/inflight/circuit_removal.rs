use super::*;

pub(super) trait CircuitDiscardGuard {
    fn state_path(&self) -> &Path;
}

impl CircuitDiscardGuard for super::store::InflightStateFileLock {
    fn state_path(&self) -> &Path {
        self.state_path()
    }
}

impl CircuitDiscardGuard for super::store::BorrowedInflightRow<'_> {
    fn state_path(&self) -> &Path {
        self.state_path()
    }
}

pub(super) fn handoff_before_discard_locked(
    guard: &impl CircuitDiscardGuard,
) -> Result<(), String> {
    let path = guard.state_path();
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&data).map_err(|error| error.to_string())?;
    let Some(pending) = value.get("pending_circuit_evidence") else {
        return Ok(());
    };
    let pending: Vec<super::super::relay_recovery::circuit_evidence::CircuitDeliveryEvidence> =
        serde_json::from_value(pending.clone()).map_err(|error| error.to_string())?;
    let runtime = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("inflight path lacks runtime root")?;
    for evidence in &pending {
        super::super::relay_recovery::circuit_evidence::handoff(runtime, evidence)?;
    }
    Ok(())
}

// Preserve the bool API while returning its exact lock-held decision to cleanup callers.
pub(in crate::services::discord) struct InflightDeleteRequest<'a> {
    pub(super) provider: &'a ProviderKind,
    pub(super) result: Option<&'a std::cell::Cell<InflightDeleteOutcome>>,
}
impl<'a> From<&'a ProviderKind> for InflightDeleteRequest<'a> {
    fn from(provider: &'a ProviderKind) -> Self {
        Self {
            provider,
            result: None,
        }
    }
}

impl InflightDeleteRequest<'_> {
    pub(super) fn delete(self, channel_id: u64) -> bool {
        let request = self;
        let provider = request.provider;
        let channel = channel_id;
        let outcome = 'decision: {
            let Some(root) = inflight_runtime_root() else {
                break 'decision InflightDeleteOutcome::Failed;
            };
            let path = inflight_state_path(&root, provider, channel);
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                break 'decision InflightDeleteOutcome::Failed;
            };
            if receipt_protected_path(&path) && !receipt_mutant("delete") {
                break 'decision InflightDeleteOutcome::ProtectedReceipt;
            }
            if handoff_before_discard_locked(&_lock).is_err() {
                break 'decision InflightDeleteOutcome::Failed;
            }
            log_inflight_remove_for_path(provider, channel, "delete_inflight_state_file", &path);
            match fs::remove_file(path) {
                Ok(()) => InflightDeleteOutcome::Deleted,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    InflightDeleteOutcome::AlreadyAbsent
                }
                _ => InflightDeleteOutcome::Failed,
            }
        };
        if let Some(result) = request.result {
            result.set(outcome);
        }
        outcome == InflightDeleteOutcome::Deleted
    }
}
