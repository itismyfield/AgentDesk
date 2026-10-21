//! Execution authority is historical; a current file edit cannot revoke its exact witness.

use super::super::contract::ServerWitness;
use super::super::model::HerdrEndpoint;
use super::{NextRestartProof, PeerSource, StartupProof, Unverified};
use crate::services::herdr_launch::HerdrLaunchEndpoint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreatePermit {
    endpoint: HerdrLaunchEndpoint,
    proof: NextRestartProof,
}

impl CreatePermit {
    pub(crate) fn new(
        endpoint: HerdrLaunchEndpoint,
        proof: NextRestartProof,
    ) -> Result<Self, Unverified> {
        if proof.witness().socket != std::path::Path::new(&endpoint.socket_addr) {
            return Err(Unverified::Peer);
        }
        Ok(Self { endpoint, proof })
    }

    pub(crate) fn witness(&self) -> &ServerWitness {
        self.proof.witness()
    }

    pub(crate) fn fence(
        &self,
        endpoint: &HerdrLaunchEndpoint,
        source: &dyn PeerSource,
    ) -> Result<(), Unverified> {
        if endpoint != &self.endpoint {
            return Err(Unverified::Peer);
        }
        self.proof.revalidate(source)
    }

    pub(crate) fn grant(&self, nonce: &str) -> LaunchAdmissionGrant {
        LaunchAdmissionGrant {
            endpoint: self.endpoint.clone(),
            nonce: nonce.into(),
            proof: self.proof.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchAdmissionGrant {
    endpoint: HerdrLaunchEndpoint,
    nonce: String,
    proof: NextRestartProof,
}

impl LaunchAdmissionGrant {
    pub(crate) fn endpoint(&self) -> &HerdrLaunchEndpoint {
        &self.endpoint
    }
    pub(crate) fn nonce(&self) -> &str {
        &self.nonce
    }

    pub(crate) fn witness_for(
        &self,
        endpoint: &HerdrLaunchEndpoint,
        nonce: &str,
    ) -> Result<&ServerWitness, Unverified> {
        if endpoint != &self.endpoint || nonce != self.nonce || nonce.is_empty() {
            return Err(Unverified::Peer);
        }
        Ok(self.proof.witness())
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExistingMutationEvidence {
    pub(crate) startup: Option<StartupProof>,
    pub(crate) grant: Option<LaunchAdmissionGrant>,
}

pub(crate) struct ExistingMutationPermit(ServerWitness);

impl ExistingMutationPermit {
    pub(crate) fn issue(
        endpoint: &HerdrEndpoint,
        nonce: &str,
        current: ServerWitness,
        evidence: &ExistingMutationEvidence,
    ) -> Result<Self, Unverified> {
        if current.socket != endpoint.socket_path() {
            return Err(Unverified::Peer);
        }
        let launch = HerdrLaunchEndpoint {
            execution_node: endpoint.execution_node().into(),
            config_key: endpoint.config_key().into(),
            socket_addr: endpoint.socket_path().to_string_lossy().into_owned(),
            herdr_session: endpoint.herdr_session().into(),
        };
        let admitted = evidence
            .startup
            .as_ref()
            .is_some_and(|p| p.witness() == &current)
            || evidence
                .grant
                .as_ref()
                .is_some_and(|g| g.witness_for(&launch, nonce) == Ok(&current));
        if !admitted {
            return Err(Unverified::Identity);
        }
        #[cfg(test)]
        EVENTS.with(|events| events.borrow_mut().push("ExistingMutationPermit"));
        Ok(Self(current))
    }

    pub(crate) fn witness(&self) -> &ServerWitness {
        &self.0
    }
}

#[cfg(test)]
pub(crate) fn grant_for_test(
    endpoint: &HerdrEndpoint,
    nonce: &str,
    witness: ServerWitness,
) -> LaunchAdmissionGrant {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, b"[session]\nresume_agents_on_restore=false\n").unwrap();
    let file = super::file::read(&config).unwrap().token;
    LaunchAdmissionGrant {
        endpoint: HerdrLaunchEndpoint {
            execution_node: endpoint.execution_node().into(),
            config_key: endpoint.config_key().into(),
            socket_addr: endpoint.socket_path().to_string_lossy().into_owned(),
            herdr_session: endpoint.herdr_session().into(),
        },
        nonce: nonce.into(),
        proof: NextRestartProof {
            peer: super::peer::PeerSnapshot {
                started: std::time::SystemTime::now(),
                home: dir.path().into(),
                config: config.clone(),
                checker: config,
                image: file.clone(),
                witness,
                socket_dir: (0, 0),
            },
            file,
        },
    }
}

#[cfg(test)]
pub(crate) fn create_for_test(
    endpoint: HerdrLaunchEndpoint,
    witness: ServerWitness,
) -> CreatePermit {
    let configured = HerdrEndpoint::new(
        &endpoint.execution_node,
        &endpoint.config_key,
        std::path::Path::new(&endpoint.socket_addr),
        &endpoint.herdr_session,
    )
    .unwrap();
    let grant = grant_for_test(&configured, "fixture", witness);
    CreatePermit::new(endpoint, grant.proof).unwrap()
}

#[cfg(test)]
thread_local! {
    pub(crate) static EVENTS: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}
