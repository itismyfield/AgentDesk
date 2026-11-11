use super::{ChannelId, ProviderKind, SnapshotCatalog};
use std::path::PathBuf;

pub(crate) struct OwnedRecoveryFailure {
    pub(crate) provider: String,
    pub(crate) channel: ChannelId,
    pub(crate) expected_path: PathBuf,
    pub(crate) namespace: String,
    pub(crate) stage: &'static str,
    pub(crate) error: String,
}

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

pub(crate) struct OwnedRecoveryDiscovery {
    pub(crate) ready: Vec<ChannelId>,
    pub(crate) failed: Vec<OwnedRecoveryFailure>,
    pub(crate) namespace_errors: Vec<String>,
}

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

    /// Errors remain observations until an admission-capable controller owns their recovery.
    pub(crate) fn observed_ready(self) -> Vec<ChannelId> {
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
        self.ready
    }
}

#[cfg(test)]
#[path = "owned_recovery_gate_tests.rs"]
mod gate_tests;
