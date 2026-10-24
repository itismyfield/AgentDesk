//! Only the executing node resolves its user's implicit socket; remote defaults stay logical.

use std::path::PathBuf;

use super::{Config, EndpointMode};

pub(super) fn resolve(
    config: &mut Config,
    home: impl FnOnce() -> Option<PathBuf>,
) -> anyhow::Result<()> {
    let local = config.cluster.instance_id.as_deref().map(str::trim);
    let needs_home = config
        .session_hosts
        .herdr
        .endpoints
        .iter()
        .find_map(|(key, endpoint)| {
            (endpoint.mode == EndpointMode::UserDefault
                && Some(endpoint.execution_node.trim()) == local
                && endpoint.socket_path.as_os_str().is_empty())
            .then(|| key.clone())
        });
    let home = if let Some(key) = needs_home {
        let home = home().filter(|p| p.is_absolute()).ok_or_else(|| anyhow::anyhow!(
            "session_hosts.herdr.endpoints.{key}.socket_path: local user_default requires an absolute execution-user HOME or an explicit absolute socket_path"
        ))?;
        Some(home)
    } else {
        None
    };
    for endpoint in config.session_hosts.herdr.endpoints.values_mut() {
        if endpoint.mode != EndpointMode::UserDefault {
            continue;
        }
        if endpoint.herdr_session.is_empty() {
            endpoint.herdr_session = "default".into();
        }
        if endpoint.socket_path.as_os_str().is_empty()
            && Some(endpoint.execution_node.trim()) == local
            && let Some(home) = &home
        {
            endpoint.socket_path = home.join(".config/herdr/herdr.sock");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "user_default_tests.rs"]
mod tests;
