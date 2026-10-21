use std::collections::HashMap;
use std::sync::Mutex;

use super::super::super::contract::ServerWitness;
use super::{ConfigChecker, NextRestartProof, PeerSource, StartupProof, Unverified, file};

#[derive(Default)]
struct State {
    startup: HashMap<ServerWitness, StartupProof>,
    current: Option<NextRestartProof>,
}

/// Historical startup evidence and reusable current validation have different lifetimes.
#[derive(Default)]
pub(crate) struct ProofCache {
    state: Mutex<State>,
}

impl ProofCache {
    pub(crate) fn startup_for(
        &self,
        witness: &ServerWitness,
    ) -> Result<Option<StartupProof>, Unverified> {
        let state = self.state.lock().map_err(|_| Unverified::Cache)?;
        Ok(state.startup.get(witness).cloned())
    }

    pub(crate) async fn next_restart(
        &self,
        source: &dyn PeerSource,
        checker: &dyn ConfigChecker,
    ) -> Result<NextRestartProof, Unverified> {
        let result = self.verify(source, checker).await;
        if result.is_err() {
            self.state.lock().map_err(|_| Unverified::Cache)?.current = None;
        }
        result
    }

    async fn verify(
        &self,
        source: &dyn PeerSource,
        checker: &dyn ConfigChecker,
    ) -> Result<NextRestartProof, Unverified> {
        let peer = source.snapshot()?;
        peer.image.unchanged()?;
        let read = file::read(&peer.config)?;
        let proof = NextRestartProof {
            peer,
            file: read.token,
        };
        let cached = self
            .state
            .lock()
            .map_err(|_| Unverified::Cache)?
            .current
            .as_ref()
            == Some(&proof);
        if !cached {
            explicit_false(&read.bytes)?;
            checker.check(&proof.peer).await?;
        }
        proof.revalidate(source)?;
        let mut state = self.state.lock().map_err(|_| Unverified::Cache)?;
        if proof.file.predates(proof.peer.started) {
            state
                .startup
                .entry(proof.witness().clone())
                .or_insert_with(|| StartupProof {
                    evidence: proof.clone(),
                });
        }
        state.current = Some(proof.clone());
        Ok(proof)
    }

    #[cfg(test)]
    pub(super) fn poison_for_test(&self) {
        let _guard = self.state.lock().expect("test cache lock");
        panic!("injected cache failure");
    }
}

fn explicit_false(bytes: &[u8]) -> Result<(), Unverified> {
    let text = std::str::from_utf8(bytes).map_err(|_| Unverified::ConfigInvalid)?;
    let document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| Unverified::ConfigInvalid)?;
    match document
        .get("session")
        .and_then(|s| s.get("resume_agents_on_restore"))
        .and_then(toml_edit::Item::as_bool)
    {
        Some(false) => Ok(()),
        Some(true) => Err(Unverified::ResumeEnabled),
        None => Err(Unverified::ConfigInvalid),
    }
}
