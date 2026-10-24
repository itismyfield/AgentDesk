//! Canonical observation identity, deliberately separate from an Apply approval.
use crate::services::tui_o::writer::codex_snapshot::Observation;
use sha2::{Digest, Sha256};

pub(crate) fn identity(observation: &Observation) -> Result<String, String> {
    let Observation::Complete(snapshot) = observation else {
        return Err("Codex observation is unknown".into());
    };
    let mut hash = Sha256::new();
    hash.update(b"agentdesk/hrot/codex-observation\0v1\0");
    hash.update(serde_json::to_vec(snapshot).map_err(|e| e.to_string())?);
    Ok(format!(
        "hrot-codex-observation-v1:{}",
        hex::encode(hash.finalize())
    ))
}
