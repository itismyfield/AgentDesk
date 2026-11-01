use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::super::contract::ServerWitness;
use super::{ConfigChecker, NextRestartProof, PeerSource, StartupProof, Unverified, file};

#[derive(Default)]
struct State {
    startup: HashMap<ServerWitness, StartupProof>,
    current: HashMap<PathBuf, NextRestartProof>,
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
        source: Arc<dyn PeerSource>,
        checker: &dyn ConfigChecker,
    ) -> Result<NextRestartProof, Unverified> {
        let observed = source.clone();
        let peer = tokio::task::spawn_blocking(move || observed.snapshot())
            .await
            .map_err(|_| Unverified::Peer)??;
        let key = peer.witness.socket.clone();
        let cached = self
            .state
            .lock()
            .map_err(|_| Unverified::Cache)?
            .current
            .get(&key)
            .cloned();
        let prepared = tokio::task::spawn_blocking(move || {
            if let Some(proof) = cached
                && proof.fast_revalidate(&peer).is_ok()
            {
                return Ok((proof, false));
            }
            peer.image.fast_unchanged()?;
            let read = file::read(&peer.config)?;
            explicit_false(&read.bytes)?;
            Ok((
                NextRestartProof {
                    peer,
                    file: read.token,
                },
                true,
            ))
        })
        .await
        .map_err(|_| Unverified::File)?;
        let result = async {
            let (proof, check) = prepared?;
            if check {
                checker.check(&proof.peer).await?;
            }
            let validated = proof.clone();
            let startup = tokio::task::spawn_blocking(move || {
                validated.fast_revalidate(&source.snapshot()?)?;
                Ok::<_, Unverified>(
                    validated.file.predates(validated.peer.started)
                        && validated.file.parent_identity()? == validated.peer.socket_dir,
                )
            })
            .await
            .map_err(|_| Unverified::Peer)??;
            let mut state = self.state.lock().map_err(|_| Unverified::Cache)?;
            if startup {
                state
                    .startup
                    .entry(proof.witness().clone())
                    .or_insert_with(|| StartupProof {
                        evidence: proof.clone(),
                    });
            }
            state.current.insert(key.clone(), proof.clone());
            Ok(proof)
        }
        .await;
        if result.is_err() {
            self.state
                .lock()
                .map_err(|_| Unverified::Cache)?
                .current
                .remove(&key);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        let _guard = self.state.lock().expect("test cache lock");
        panic!("injected cache failure");
    }
}

/// `[session] resume_agents_on_restore = false`, written out; anything else may resume.
pub(crate) fn explicit_false(bytes: &[u8]) -> Result<(), Unverified> {
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
