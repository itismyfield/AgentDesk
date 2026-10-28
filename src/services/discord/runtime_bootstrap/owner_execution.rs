//! Restore execution authority before any local watcher or actor can resume.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::binding::{BindingEvents, ChannelBindingLog};
use crate::services::tui_o::{cutover, ownership::execution};

pub(super) async fn prepare(provider: &ProviderKind, pool: Option<&sqlx::PgPool>) {
    let Some(snapshot) = cutover::boot_snapshot() else {
        return;
    };
    let (shadow, kind) = match provider {
        ProviderKind::Claude => (ShadowProvider::Claude, RuntimeHandoffKind::ClaudeTui),
        ProviderKind::Codex => (ShadowProvider::Codex, RuntimeHandoffKind::CodexTui),
        _ => return,
    };
    let channels: Vec<_> = snapshot
        .channels()
        .iter()
        .copied()
        .filter(|ch| snapshot.kind(*ch) == Some(kind))
        .collect();
    if channels.is_empty() {
        return;
    }
    let local =
        super::o_writer_host::local_id(snapshot.configured_id(), Duration::from_secs(10)).await;
    for channel in channels {
        let result = match (pool, &local) {
            (Some(pool), Ok(local)) => {
                let read = execution::read(pool, provider.as_str(), channel, local, || {
                    use crate::services::tui_prompt_dedupe::binding_context::{
                        self, SpawnNonceMarker,
                    };
                    if super::super::inflight::inflight_state_file_exists(provider, channel) {
                        return Ok(true);
                    }
                    let events =
                        ChannelBindingLog::new(channel, shadow).binding_events_since(channel, 0)?;
                    let Some(event) = events.last() else {
                        return Ok(false);
                    };
                    match binding_context::observe_spawn_nonce_marker(&event.tmux_session) {
                        SpawnNonceMarker::Absent => Ok(false),
                        SpawnNonceMarker::Known(_) => Ok(true),
                        SpawnNonceMarker::Unreadable => {
                            Err("local native marker unreadable".into())
                        }
                    }
                });
                tokio::time::timeout(Duration::from_secs(3), read)
                    .await
                    .map_err(|_| "execution owner PG read timed out".to_owned())
                    .and_then(|r| r)
            }
            (None, _) => Err("execution owner PG pool unavailable".into()),
            (_, Err(reason)) => Err(reason.clone()),
        };
        let view = match result {
            Ok(None) => None,
            Ok(Some(permit)) => Some(Ok(permit)),
            Err(reason) => {
                tracing::warn!(provider=provider.as_str(),channel,%reason,"execution restore held");
                Some(Err(reason))
            }
        };
        snapshot.install_execution(channel, view);
    }
}

#[cfg(test)]
mod tests;

/// Transport restart fences Legacy; PG execution owners and their native sessions stay intact.
pub(super) fn gateway_transport_lost(shared: &SharedData) {
    shared.restart.legacy_lease_lost();
}
