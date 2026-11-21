use std::collections::BTreeSet;
use std::sync::Arc;

use poise::serenity_prelude::{Channel, ChannelId, Http};

use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover;
use crate::services::tui_o::ownership;
use crate::services::tui_o::writer::host::agy_host::{AgyShadowHost, spool_destinations};

pub(super) async fn observe(http: &Http, provider: &ProviderKind) {
    if *provider != ProviderKind::Antigravity {
        return;
    }
    let Some(snapshot) = cutover::boot_snapshot() else {
        return;
    };
    let gate = ownership::gate(provider.as_str());
    if snapshot.site() != &crate::services::tui_o::channel_policy::Site::Home
        || gate.current().epoch().is_none()
    {
        return;
    }
    let Some(root) = crate::config::runtime_root() else {
        return;
    };
    let scan_root = root.clone();
    let destinations =
        match tokio::task::spawn_blocking(move || spool_destinations(&scan_root)).await {
            Ok(Ok(destinations)) => destinations,
            Ok(Err(error)) => {
                tracing::warn!(%error, "[tui_o] agy shadow spool index is held");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "[tui_o] agy shadow spool index task stopped");
                return;
            }
        };
    let destinations: BTreeSet<_> = destinations
        .into_iter()
        .chain(snapshot.agy_channels().iter().copied())
        .collect();
    for channel in destinations {
        if !snapshot.configured_binding(channel) {
            match ChannelId::new(channel).to_channel(http).await {
                Ok(Channel::Guild(read)) if read.id.get() == channel => {
                    snapshot.record_agy_thread(&read);
                }
                _ => {
                    tracing::warn!(
                        channel,
                        "[tui_o] agy shadow child relationship could not be checked"
                    );
                    continue;
                }
            }
        }
        let Some(target) = snapshot.agy_target(channel) else {
            tracing::warn!(
                channel,
                "[tui_o] agy shadow spool has no verified selected destination"
            );
            continue;
        };
        let root = root.clone();
        let gate = Arc::clone(&gate);
        let prior_legacy_settled =
            super::super::super::inflight::load_inflight_state_read_only_result(provider, channel)
                .is_ok_and(|inflight| inflight.is_none());
        let observation = tokio::task::spawn_blocking(move || {
            AgyShadowHost::start(root, target, gate)?
                .map(|host| host.adopt(&Default::default(), prior_legacy_settled))
                .transpose()
        })
        .await;
        match observation {
            Ok(Ok(Some((permit, turns)))) => tracing::debug!(
                channel,
                ready = permit.is_some(),
                turns = turns.len(),
                "[tui_o] agy shadow custody restored"
            ),
            Ok(Ok(None)) => break,
            Ok(Err(error)) => tracing::warn!(channel, %error, "[tui_o] agy shadow custody held"),
            Err(error) => tracing::warn!(channel, %error, "[tui_o] agy shadow replay task stopped"),
        }
    }
}
