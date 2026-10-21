//! Versioned, secret-free registry state; decoding never consults current agent registrations.

use serde::{Deserialize, Serialize};

use super::{BootSessionHosts, SessionHostsConfig, projected_channels};

pub(super) const SCHEMA: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Projection {
    pub hosts: SessionHostsConfig,
    pub local_node: Option<String>,
}

impl Projection {
    pub(crate) fn of(boot: &BootSessionHosts) -> Self {
        let mut hosts = boot.config.clone();
        for endpoint in hosts.herdr.endpoints.values_mut() {
            endpoint.execution_node = endpoint.execution_node.trim().to_owned();
        }
        hosts.herdr.channels = boot
            .channels
            .iter()
            .map(|(id, endpoint)| (id.to_string(), endpoint.key.clone()))
            .collect();
        Self {
            hosts,
            local_node: boot.local_node.clone(),
        }
    }

    pub(super) fn into_hosts(self) -> anyhow::Result<BootSessionHosts> {
        let channels = projected_channels(&self.hosts)?;
        anyhow::ensure!(
            self.local_node
                .as_deref()
                .is_none_or(|id| !id.trim().is_empty()),
            "empty local node"
        );
        Ok(BootSessionHosts {
            config: self.hosts,
            channels,
            local_node: self.local_node.map(|node| node.trim().to_owned()),
            ..BootSessionHosts::default()
        })
    }
}

pub(super) enum Decoded {
    Known(BootSessionHosts),
    Newer,
}

pub(super) fn decode(bytes: &[u8]) -> anyhow::Result<Decoded> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("approval snapshot has no schema"))?;
    if schema > SCHEMA {
        return Ok(Decoded::Newer);
    }
    // Schema zero had no mode; serde's default converts it to LegacyDedicated.
    let projection: Projection = serde_json::from_value(
        value
            .get("projection")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("approval snapshot has no projection"))?,
    )?;
    Ok(Decoded::Known(projection.into_hosts()?))
}
