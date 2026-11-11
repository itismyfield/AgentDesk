#[cfg(unix)]
use super::{ChannelId, ProviderKind, SnapshotCatalog};
#[cfg(unix)]
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct RecoveryPass(String);

impl Default for RecoveryPass {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryGate {
    Complete,
    Held,
}

impl RecoveryGate {
    pub(crate) fn combine(self, other: Self) -> Self {
        if self == Self::Complete && other == Self::Complete {
            Self::Complete
        } else {
            Self::Held
        }
    }

    pub(crate) fn is_complete(self) -> bool {
        self == Self::Complete
    }
}

impl RecoveryPass {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn id(&self) -> &str {
        &self.0
    }

    #[cfg(unix)]
    pub(crate) fn gate(&self, discovery: &OwnedRecoveryDiscovery) -> RecoveryGate {
        discovery.observe_errors();
        if discovery.failed.is_empty() && discovery.namespace_errors.is_empty() {
            RecoveryGate::Complete
        } else {
            RecoveryGate::Held
        }
    }

    #[cfg(unix)]
    pub(crate) fn notice_key(&self, failure: &OwnedRecoveryFailure) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        let path: PathBuf = failure.expected_path.components().collect();
        let identity = (
            &failure.provider,
            &failure.namespace,
            failure.channel.get(),
            path.as_os_str().as_encoded_bytes(),
            &self.0,
            failure.stage,
        );
        let bytes = serde_json::to_vec(&identity).map_err(|error| error.to_string())?;
        Ok(format!(
            "boot-queue-io:v1:{}",
            hex::encode(Sha256::digest(bytes))
        ))
    }
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub(crate) struct OwnedRecoveryFailure {
    pub(crate) provider: String,
    pub(crate) channel: ChannelId,
    pub(crate) expected_path: PathBuf,
    pub(crate) namespace: String,
    pub(crate) stage: &'static str,
    pub(crate) error: String,
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub(crate) enum OwnedRecoveryTarget {
    Ready(ChannelId),
    Failed(OwnedRecoveryFailure),
}

#[cfg(unix)]
impl OwnedRecoveryFailure {
    pub(super) fn new(
        provider: &ProviderKind,
        token_hash: &str,
        channel: ChannelId,
        expected_path: PathBuf,
        error: String,
    ) -> Self {
        Self {
            provider: provider.as_str().into(),
            channel,
            expected_path,
            namespace: format!("{}/{token_hash}", provider.as_str()),
            stage: "catalog_unreadable",
            error,
        }
    }
}

#[cfg(unix)]
pub(crate) struct OwnedRecoveryDiscovery {
    pub(crate) ready: Vec<ChannelId>,
    pub(crate) failed: Vec<OwnedRecoveryFailure>,
    pub(crate) namespace_errors: Vec<String>,
}

#[cfg(unix)]
impl OwnedRecoveryDiscovery {
    pub(super) fn from_catalog(catalog: SnapshotCatalog) -> Self {
        let ready = catalog
            .snapshots
            .into_iter()
            .filter(|(_, snapshot)| snapshot.v2_holds_input())
            .map(|(channel, _)| channel)
            .collect();
        Self {
            ready,
            failed: catalog.failed,
            namespace_errors: catalog.scan_errors,
        }
    }

    fn observe_errors(&self) {
        for failure in &self.failed {
            tracing::debug!(
                provider = %failure.provider, channel_id = failure.channel.get(),
                namespace = %failure.namespace, path = %failure.expected_path.display(),
                stage = failure.stage, error = %failure.error,
                "pending queue recovery quarantined channel"
            );
        }
        for error in &self.namespace_errors {
            tracing::debug!(%error, "pending queue recovery scan error");
        }
    }
}

#[cfg(all(test, unix))]
#[path = "owned_recovery_gate_tests.rs"]
mod gate_tests;
