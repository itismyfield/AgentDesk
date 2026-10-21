//! Each grant is immutable, keyed by its complete endpoint and execution nonce.

use super::{
    durable::{self, Directory},
    permit::LaunchAdmissionGrant,
};
use crate::services::herdr_launch::{HerdrLaunchEndpoint, policy::LaunchGrantStore};
use std::path::PathBuf;

pub(crate) struct FileGrantStore(Directory);
impl FileGrantStore {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self(Directory(root))
    }
    pub(crate) async fn load_all(
        &self,
        endpoints: &[HerdrLaunchEndpoint],
    ) -> Result<Vec<LaunchAdmissionGrant>, String> {
        if endpoints.is_empty() {
            return Ok(Vec::new());
        }
        let (directory, endpoints) = (self.0.clone(), endpoints.to_vec());
        tokio::task::spawn_blocking(move || {
            let mut grants = Vec::new();
            for key in directory.keys()? {
                let bytes = directory
                    .read(&key)?
                    .ok_or_else(|| directory.failure(&key, "grant disappeared"))?;
                let grant = LaunchAdmissionGrant::decode_any(&bytes)
                    .map_err(|reason| directory.failure(&key, &reason))?;
                if durable::key(&(grant.endpoint(), grant.nonce()))? != key {
                    return Err(directory.failure(&key, "grant filename mismatch"));
                }
                if endpoints.contains(grant.endpoint()) {
                    grants.push(grant);
                }
            }
            Ok(grants)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[async_trait::async_trait]
impl LaunchGrantStore for FileGrantStore {
    async fn load(
        &self,
        endpoint: &HerdrLaunchEndpoint,
        nonce: &str,
    ) -> Result<Option<LaunchAdmissionGrant>, String> {
        let (directory, endpoint, nonce) = (self.0.clone(), endpoint.clone(), nonce.to_owned());
        tokio::task::spawn_blocking(move || {
            let key = durable::key(&(&endpoint, &nonce))?;
            directory
                .read(&key)?
                .map(|bytes| {
                    LaunchAdmissionGrant::decode(&bytes, &endpoint, &nonce)
                        .map_err(|reason| directory.failure(&key, &reason))
                })
                .transpose()
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn save(&self, grant: LaunchAdmissionGrant) -> Result<(), String> {
        let directory = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let key = durable::key(&(grant.endpoint(), grant.nonce()))?;
            directory
                .change(&key, |previous| {
                    if let Some(bytes) = previous {
                        if LaunchAdmissionGrant::decode(&bytes, grant.endpoint(), grant.nonce())?
                            != grant
                        {
                            return Err("conflicting admission grant".into());
                        }
                        return Ok(bytes);
                    }
                    grant.encode()
                })
                .map(|_| ())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
#[path = "grant_store_tests.rs"]
mod tests;
