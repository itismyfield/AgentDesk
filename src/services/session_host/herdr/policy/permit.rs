//! Execution authority is historical; a current file edit cannot revoke its exact witness.

use super::super::contract::ServerWitness;
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
            witness: self.proof.witness().clone(),
            config: (
                self.proof.file.logical_path().into(),
                self.proof.file.digest(),
            ),
            checker: (
                self.proof.peer.image.logical_path().into(),
                self.proof.peer.image.digest(),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchAdmissionGrant {
    endpoint: HerdrLaunchEndpoint,
    nonce: String,
    witness: ServerWitness,
    config: (std::path::PathBuf, [u8; 32]),
    checker: (std::path::PathBuf, [u8; 32]),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireGrant {
    schema: u32,
    endpoint: HerdrLaunchEndpoint,
    nonce: String,
    #[serde(with = "super::witness_codec")]
    witness: ServerWitness,
    config: (std::path::PathBuf, [u8; 32]),
    checker: (std::path::PathBuf, [u8; 32]),
}

impl LaunchAdmissionGrant {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&WireGrant {
            schema: 1,
            endpoint: self.endpoint.clone(),
            nonce: self.nonce.clone(),
            witness: self.witness.clone(),
            config: self.config.clone(),
            checker: self.checker.clone(),
        })
        .map_err(|e| e.to_string())
    }

    pub(crate) fn decode_any(bytes: &[u8]) -> Result<Self, String> {
        let version: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if version
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|v| v > 1)
        {
            return Err("admission_grant_newer: use a compatible AgentDesk version".into());
        }
        let wire: WireGrant = serde_json::from_value(version).map_err(|e| e.to_string())?;
        if wire.schema != 1
            || wire.nonce.is_empty()
            || wire.witness.pid == 0
            || wire.witness.socket != std::path::Path::new(&wire.endpoint.socket_addr)
            || !std::path::Path::new(&wire.endpoint.socket_addr).is_absolute()
            || !wire.config.0.is_absolute()
            || !wire.checker.0.is_absolute()
            || [
                &wire.endpoint.execution_node,
                &wire.endpoint.config_key,
                &wire.endpoint.herdr_session,
            ]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            return Err("incompatible admission grant".into());
        }
        Ok(Self {
            endpoint: wire.endpoint,
            nonce: wire.nonce,
            witness: wire.witness,
            config: wire.config,
            checker: wire.checker,
        })
    }

    pub(crate) fn decode(
        bytes: &[u8],
        endpoint: &HerdrLaunchEndpoint,
        nonce: &str,
    ) -> Result<Self, String> {
        let grant = Self::decode_any(bytes)?;
        grant
            .witness_for(endpoint, nonce)
            .map_err(|_| "grant endpoint or nonce mismatch")?;
        Ok(grant)
    }

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
        Ok(&self.witness)
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
        endpoint: &HerdrLaunchEndpoint,
        nonce: &str,
        current: ServerWitness,
        evidence: &ExistingMutationEvidence,
    ) -> Result<Self, Unverified> {
        if current.socket != std::path::Path::new(&endpoint.socket_addr) {
            return Err(Unverified::Peer);
        }
        let admitted = evidence
            .startup
            .as_ref()
            .is_some_and(|p| p.witness() == &current)
            || evidence
                .grant
                .as_ref()
                .is_some_and(|g| g.witness_for(endpoint, nonce) == Ok(&current));
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
    endpoint: &HerdrLaunchEndpoint,
    nonce: &str,
    witness: ServerWitness,
) -> LaunchAdmissionGrant {
    LaunchAdmissionGrant {
        endpoint: endpoint.clone(),
        nonce: nonce.into(),
        witness,
        config: ("/fixture/config.toml".into(), [1; 32]),
        checker: ("/fixture/checker".into(), [2; 32]),
    }
}

#[cfg(test)]
pub(crate) fn create_for_test(
    endpoint: HerdrLaunchEndpoint,
    witness: ServerWitness,
) -> CreatePermit {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, b"[session]\nresume_agents_on_restore=false\n").unwrap();
    let file = super::file::read(&config).unwrap().token;
    let proof = NextRestartProof {
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
    };
    CreatePermit::new(endpoint, proof).unwrap()
}

#[cfg(test)]
thread_local! {
    pub(crate) static EVENTS: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}
