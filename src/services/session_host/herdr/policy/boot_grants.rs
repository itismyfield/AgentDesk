//! Async boot owns disk reads; mutation gates receive only restored memory evidence.
use super::{
    Unverified,
    grant_store::{FileGrantStore, GrantRestoration},
};
use crate::config::session_hosts::{BootSessionHosts, EndpointMode};
use crate::services::herdr_launch::HerdrLaunchEndpoint;
use std::sync::OnceLock;

static PREPARED: OnceLock<Result<GrantRestoration, Unverified>> = OnceLock::new();

pub(crate) fn store() -> FileGrantStore {
    FileGrantStore::new(
        crate::config::runtime_root().map(|root| root.join("runtime/herdr_policy/grants")),
    )
}

async fn load(
    config: &crate::config::session_hosts::SessionHostsConfig,
    local_node: Option<&str>,
    store: &FileGrantStore,
) -> Result<GrantRestoration, Unverified> {
    let endpoints = config
        .herdr
        .endpoints
        .iter()
        .filter(|(_, e)| {
            e.mode == EndpointMode::UserDefault && Some(e.execution_node.trim()) == local_node
        })
        .map(|(key, e)| HerdrLaunchEndpoint {
            execution_node: e.execution_node.trim().into(),
            config_key: key.clone(),
            socket_addr: e.socket_path.to_string_lossy().into_owned(),
            herdr_session: e.herdr_session.clone(),
        })
        .collect::<Vec<_>>();
    if endpoints.is_empty() {
        return Ok(GrantRestoration::default());
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), store.load_all(&endpoints)).await
        .map_err(|_| Unverified::Cache)?.map_err(|reason| {
            tracing::warn!(%reason, "herdr grant restoration refused; existing mutation requires verified cache");
            Unverified::Cache
        })
}

pub(crate) async fn prepare(boot: &BootSessionHosts) {
    let loaded = load(boot.config(), boot.local_node(), &store()).await;
    let _ = PREPARED.set(loaded);
}
#[cfg(unix)]
pub(crate) fn prepared() -> Result<GrantRestoration, Unverified> {
    PREPARED
        .get()
        .cloned()
        .unwrap_or_else(|| Ok(GrantRestoration::default()))
}

#[cfg(all(test, unix))]
#[path = "boot_grants_tests.rs"]
mod tests;
