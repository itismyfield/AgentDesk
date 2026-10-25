#![warn(dead_code, unused_imports)]

//! Evidence for user-owned servers, kept separate from the dedicated-server policy.

#[cfg(unix)]
pub(crate) mod apply;
pub(crate) mod boot_grants;
mod cache;
pub(crate) mod checker;
#[cfg(unix)]
pub(crate) mod consent;
pub(crate) mod diagnostics;
pub(crate) mod durable;
#[cfg(unix)]
pub(crate) mod editor;
pub(crate) mod file;
pub(crate) mod grant_store;
pub(crate) mod peer;
pub(crate) mod permit;
mod types;
mod witness_codec;
#[cfg(unix)]
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
    /// The mapped executable's inode changed after this server process started; only the
    /// macOS adapter judges change time and layout, so these causes exist only there.
    #[cfg(target_os = "macos")]
    ImageChangedSinceStart,
    /// The executable changed within the start microsecond, so its order is unknown.
    #[cfg(target_os = "macos")]
    ImageTimeAmbiguous,
    /// The mapped executable lives on a filesystem whose timestamps are not trusted here.
    #[cfg(target_os = "macos")]
    ImageFilesystemUnsupported,
    /// The process image is not a native arm64 single-architecture mapping.
    #[cfg(target_os = "macos")]
    ImageLayoutUnsupported,
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
