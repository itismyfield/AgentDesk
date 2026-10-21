use super::{PeerSource, Unverified, file::FileToken, peer::PeerSnapshot};
use crate::services::session_host::herdr::contract::ServerWitness;

/// Current whole-config validation, bound to the peer and both file identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NextRestartProof {
    pub(super) peer: PeerSnapshot,
    pub(super) file: FileToken,
}

impl NextRestartProof {
    pub(crate) fn witness(&self) -> &ServerWitness {
        &self.peer.witness
    }

    /// A final fence rechecks evidence without granting authority to a different peer.
    pub(crate) fn revalidate(&self, source: &dyn PeerSource) -> Result<(), Unverified> {
        if source.snapshot()? != self.peer {
            return Err(Unverified::Changed);
        }
        self.file.unchanged()?;
        self.peer.image.unchanged()?;
        if source.snapshot()? != self.peer {
            return Err(Unverified::Changed);
        }
        Ok(())
    }
}

/// Historical evidence; later config edits do not erase how this peer started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupProof {
    pub(super) evidence: NextRestartProof,
}

impl StartupProof {
    pub(crate) fn witness(&self) -> &ServerWitness {
        self.evidence.witness()
    }
}
