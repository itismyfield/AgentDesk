//! Evidence for user-owned servers, kept separate from the dedicated-server policy.

#[cfg(all(test, unix))]
pub(crate) mod apply;
mod cache;
pub(crate) mod checker;
#[cfg(all(test, unix))]
pub(crate) mod editor;
pub(crate) mod file;
pub(crate) mod peer;
pub(crate) mod permit;
mod types;
#[cfg(all(test, unix))]
pub(crate) mod writer_compat;

pub(crate) use cache::ProofCache;
pub(crate) use types::{NextRestartProof, StartupProof};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unverified {
    Peer,
    Unsupported,
    Ambiguous,
    File,
    Changed,
    ResumeEnabled,
    ConfigInvalid,
    Checker,
    Timeout,
    Diagnostics,
    Identity,
    Cache,
}

/// Supplies a fresh, validated peer observation for the consented endpoint.
pub(crate) trait PeerSource: Send + Sync {
    fn snapshot(&self) -> Result<peer::PeerSnapshot, Unverified>;
}

#[async_trait::async_trait]
pub(crate) trait ConfigChecker: Send + Sync {
    async fn check(&self, peer: &peer::PeerSnapshot) -> Result<(), Unverified>;
}

pub(crate) struct LocalChecker;

#[async_trait::async_trait]
impl ConfigChecker for LocalChecker {
    async fn check(&self, peer: &peer::PeerSnapshot) -> Result<(), Unverified> {
        checker::run(peer).await
    }
}

#[cfg(all(test, unix))]
#[path = "proof_tests.rs"]
mod tests;
