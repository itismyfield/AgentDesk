//! Restore execution authority before any local watcher or actor can resume.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::binding::{BindingEvents, ChannelBindingLog};
use crate::services::tui_o::{cutover, ownership::execution};

struct RefreshDiagnostic {
    held_since: std::time::Instant,
    retirement: Option<&'static str>,
}

// Diagnostic history never authorizes admission or changes a refresh decision.
static REFRESH_DIAGNOSTICS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<(String, u64), RefreshDiagnostic>>,
> = std::sync::LazyLock::new(Default::default);

fn record_hold(provider: &str, channel: u64) {
    REFRESH_DIAGNOSTICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry((provider.into(), channel))
        .or_insert_with(|| RefreshDiagnostic {
            held_since: std::time::Instant::now(),
            retirement: None,
        });
}

fn record_retirement(provider: &str, channel: u64, outcome: &'static str) {
    let changed = {
        let mut diagnostics = REFRESH_DIAGNOSTICS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(prior) = diagnostics.get_mut(&(provider.into(), channel)) else {
            return;
        };
        prior.retirement.replace(outcome) != Some(outcome)
    };
    if changed {
        tracing::info!(
            provider,
            channel_id = channel,
            outcome,
            "execution owner refresh retirement outcome"
        );
    }
}

fn record_readmission(provider: &str, channel: u64) {
    if cutover::intake_route::route(provider, channel)
        != cutover::intake_route::IntakeRoute::Gateway
    {
        return;
    }
    let prior = REFRESH_DIAGNOSTICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(provider.into(), channel));
    if let Some(prior) = prior {
        let held_ms = prior.held_since.elapsed().as_millis();
        tracing::info!(
            provider,
            channel_id = channel,
            held_ms,
            "execution owner refresh admission resumed"
        );
    }
}

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
                    legacy_present(provider, channel, shadow)
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
                record_hold(provider.as_str(), channel);
                tracing::warn!(provider=provider.as_str(),channel_id=channel,%reason,"execution restore held");
                Some(Err(reason))
            }
        };
        snapshot.install_execution(channel, view);
    }
}

#[cfg(all(test, unix))]
mod child_view_tests;
#[cfg(test)]
mod tests;

/// Transport restart fences Legacy; PG execution owners and their native sessions stay intact.
pub(super) fn gateway_transport_lost(shared: &SharedData) {
    shared.restart.legacy_lease_lost();
}

fn legacy_present(
    provider: &ProviderKind,
    channel: u64,
    shadow: ShadowProvider,
) -> Result<bool, String> {
    use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
    if super::super::inflight::inflight_state_file_exists(provider, channel) {
        return Ok(true);
    }
    let events = ChannelBindingLog::new(channel, shadow).binding_events_since(channel, 0)?;
    let Some(event) = events.last() else {
        return Ok(false);
    };
    match binding_context::observe_spawn_nonce_marker(&event.tmux_session) {
        SpawnNonceMarker::Absent => Ok(false),
        SpawnNonceMarker::Known(_) => Ok(true),
        SpawnNonceMarker::Unreadable => Err("local native marker unreadable".into()),
    }
}

/// A failed observation pauses admission without discarding the last verified execution.
pub(super) async fn refresh(provider: &ProviderKind, pool: Option<&sqlx::PgPool>) {
    let Some(snapshot) = cutover::boot_snapshot() else {
        return;
    };
    snapshot.refresh_child_records();
    let (shadow, kind) = match provider {
        ProviderKind::Claude => (ShadowProvider::Claude, RuntimeHandoffKind::ClaudeTui),
        ProviderKind::Codex => (ShadowProvider::Codex, RuntimeHandoffKind::CodexTui),
        _ => return,
    };
    let local =
        super::o_writer_host::local_id(snapshot.configured_id(), Duration::from_secs(10)).await;
    for &channel in snapshot.channels() {
        if snapshot.kind(channel) != Some(kind) {
            continue;
        }
        let result = match (pool, &local) {
            (Some(pool), Ok(local)) => tokio::time::timeout(
                Duration::from_secs(3),
                execution::read_observation(pool, provider.as_str(), channel, local, || {
                    legacy_present(provider, channel, shadow)
                }),
            )
            .await
            .map_err(|_| execution::ReadError::temporary("execution owner PG read timed out"))
            .and_then(|result| result),
            (None, _) => Err(execution::ReadError::temporary(
                "execution owner PG pool unavailable",
            )),
            (_, Err(reason)) => Err(execution::ReadError::temporary(reason)),
        };
        let permit = match result {
            Err(error) => {
                record_hold(provider.as_str(), channel);
                // The loop runs every 2 s, so only a new or changed reason is logged.
                let held = snapshot
                    .execution(channel)
                    .and_then(|view| view.permit.err());
                if held.as_deref() != Some(error.reason.as_str()) {
                    tracing::warn!(
                        provider = provider.as_str(),
                        channel_id = channel,
                        reason = %error.reason,
                        temporary = error.temporary,
                        "execution owner refresh read failed"
                    );
                }
                snapshot.suspend_execution(channel, error.reason, error.temporary);
                continue;
            }
            Ok(permit) => permit,
        };
        if snapshot.resume_execution(channel, permit.as_ref()) {
            record_readmission(provider.as_str(), channel);
            continue;
        }
        if let Some(view) = snapshot.execution(channel) {
            record_hold(provider.as_str(), channel);
            const CHANGED: &str = "execution identity changed";
            if view.permit.as_ref().err().map(String::as_str) != Some(CHANGED) {
                tracing::warn!(
                    provider = provider.as_str(),
                    channel_id = channel,
                    prior_permit = view.verified().is_some(),
                    next_permit = permit.is_some(),
                    "execution identity changed; admission suspended"
                );
            }
            snapshot.suspend_execution(channel, CHANGED.into(), false);
            let readiness = crate::services::tui_o::writer::host::process_readiness();
            // A writer hosted under the gateway lease never retires on an execution gate close.
            let lease = crate::services::tui_o::ownership::gate(provider.as_str());
            if readiness.hosted_under(channel, &lease) {
                record_retirement(provider.as_str(), channel, "lease_host_install");
            } else if tokio::time::timeout(Duration::from_secs(3), async {
                while readiness.is_hosted(channel) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_err()
            {
                record_retirement(provider.as_str(), channel, "wait_timeout_continue");
                continue;
            } else {
                record_retirement(provider.as_str(), channel, "retired_host_install");
            }
        }
        snapshot.install_execution(channel, permit.map(Ok));
        record_readmission(provider.as_str(), channel);
    }
}
