//! Each grant is immutable, keyed by its complete endpoint and execution nonce.

use super::{
    Unverified,
    durable::{self, Directory},
    permit::LaunchAdmissionGrant,
};
use crate::services::herdr_launch::{HerdrLaunchEndpoint, policy::LaunchGrantStore};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GrantRestoration {
    pub(crate) by_endpoint:
        HashMap<HerdrLaunchEndpoint, Result<Vec<LaunchAdmissionGrant>, Unverified>>,
    pub(crate) unknown_failure: bool,
}

impl GrantRestoration {
    #[cfg(test)]
    pub(crate) fn from_grants(grants: Vec<LaunchAdmissionGrant>) -> Self {
        let mut grouped: HashMap<HerdrLaunchEndpoint, Vec<LaunchAdmissionGrant>> = HashMap::new();
        for grant in grants {
            grouped
                .entry(grant.endpoint().clone())
                .or_default()
                .push(grant);
        }
        Self {
            by_endpoint: grouped
                .into_iter()
                .map(|(endpoint, grants)| (endpoint, Ok(grants)))
                .collect(),
            unknown_failure: false,
        }
    }
}

pub(crate) struct FileGrantStore(Directory);
impl FileGrantStore {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self(Directory(root))
    }
    pub(crate) async fn load_all(
        &self,
        endpoints: &[HerdrLaunchEndpoint],
    ) -> Result<GrantRestoration, String> {
        if endpoints.is_empty() {
            return Ok(GrantRestoration::default());
        }
        let (directory, endpoints) = (self.0.clone(), endpoints.to_vec());
        tokio::task::spawn_blocking(move || {
            let mut restored = GrantRestoration::default();
            for key in directory.keys()? {
                let owned: Result<(HerdrLaunchEndpoint, Vec<u8>), String> = (|| {
                    let bytes = directory
                        .read(&key)?
                        .ok_or("grant disappeared")?;
                    // A failed codec belongs to an endpoint only when its complete key agrees.
                    #[derive(serde::Deserialize)]
                    struct Owner {
                        endpoint: HerdrLaunchEndpoint,
                        nonce: String,
                    }
                    let owner: Owner =
                        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                    if owner.nonce.is_empty()
                        || !std::path::Path::new(&owner.endpoint.socket_addr).is_absolute()
                        || [&owner.endpoint.execution_node, &owner.endpoint.config_key, &owner.endpoint.herdr_session].iter().any(|s| s.trim().is_empty())
                        || durable::key(&(&owner.endpoint, &owner.nonce))? != key
                    {
                        return Err("grant filename mismatch".into());
                    }
                    Ok((owner.endpoint, bytes))
                })();
                let (endpoint, bytes) = match owned {
                    Ok(owned) => owned,
                    Err(reason) => {
                        tracing::warn!(reason = %directory.failure(&key, reason), "herdr grant owner is unverified");
                        restored.unknown_failure = true;
                        continue;
                    }
                };
                if !endpoints.contains(&endpoint) {
                    continue;
                }
                let entry = restored
                    .by_endpoint
                    .entry(endpoint)
                    .or_insert_with(|| Ok(Vec::new()));
                match LaunchAdmissionGrant::decode_any(&bytes) {
                    Ok(grant) => {
                        if let Ok(grants) = entry {
                            grants.push(grant);
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(reason = %directory.failure(&key, reason), "herdr grant restoration refused for endpoint");
                        *entry = Err(Unverified::Cache);
                    }
                }
            }
            Ok(restored)
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
                        .map_err(|reason| directory.failure(&key, reason))
                })
                .transpose()
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn save(&self, grant: LaunchAdmissionGrant) -> Result<(), String> {
        let directory = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let bytes = grant.encode()?;
            if LaunchAdmissionGrant::decode(&bytes, grant.endpoint(), grant.nonce())? != grant {
                return Err("admission grant does not round-trip".into());
            }
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
                    Ok(bytes)
                })
                .map(|_| ())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(all(test, unix))]
#[path = "grant_store_tests.rs"]
mod tests;
