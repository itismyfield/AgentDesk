//! A consent preview binds the complete file CAS token to one current peer and checker image.

use std::path::Path;
use std::sync::Arc;

use super::super::{PeerSource, apply, checker, peer::PeerSnapshot};
use super::{ConsentKey, Preview};

#[derive(Debug, Clone)]
pub(crate) struct Prepared {
    pub token: Preview,
    pub(super) plan: apply::ApplyPlan,
    pub(super) peer: PeerSnapshot,
}

impl Prepared {
    pub(crate) fn logical(&self) -> &Path {
        self.plan.logical()
    }
    pub(crate) fn target(&self) -> &Path {
        self.plan.target()
    }
    pub(crate) fn before_hash(&self) -> Option<[u8; 32]> {
        self.plan.before_hash()
    }
    pub(crate) fn matches_key(&self, key: &ConsentKey) -> bool {
        key.config == self.logical()
            && key.target == self.target()
            && key.socket == self.peer.witness.socket
    }
    pub(super) fn peer_unchanged(&self, source: &dyn PeerSource) -> Result<(), String> {
        if source.snapshot().map_err(|e| format!("peer: {e:?}"))? != self.peer {
            return Err("consented peer changed".into());
        }
        self.peer
            .image
            .unchanged()
            .map_err(|e| format!("checker: {e:?}"))?;
        if source.snapshot().map_err(|e| format!("peer: {e:?}"))? != self.peer {
            return Err("consented peer changed".into());
        }
        Ok(())
    }
}

pub(crate) async fn prepare(
    source: Arc<dyn PeerSource>,
    revision: u64,
) -> Result<Prepared, String> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let peer = source.snapshot().map_err(|e| format!("peer: {e:?}"))?;
        let plan = apply::preview(&peer.config).map_err(|e| e.detail)?;
        let token = Preview {
            revision,
            witness: peer.witness.clone(),
            target_hash: plan.before_hash().unwrap_or([0; 32]),
            chain_hash: plan.fingerprint()?,
            checker_hash: peer.image.digest(),
        };
        let prepared = Prepared { token, plan, peer };
        prepared.peer_unchanged(source.as_ref())?;
        prepared.plan.check_candidate(&|path: &Path| {
            prepared.peer_unchanged(source.as_ref())?;
            runtime
                .block_on(checker::candidate(&prepared.peer, path))
                .map_err(|e| format!("preview full config checker: {e:?}"))?;
            prepared.peer_unchanged(source.as_ref())
        })?;
        prepared.peer_unchanged(source.as_ref())?;
        prepared.plan.unchanged()?;
        Ok(prepared)
    })
    .await
    .map_err(|e| format!("preview owner: {e}"))?
}

#[cfg(test)]
#[path = "preview_tests.rs"]
pub(crate) mod tests;
