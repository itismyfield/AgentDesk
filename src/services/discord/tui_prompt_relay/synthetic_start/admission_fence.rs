//! A synthetic actor keeps the channel transition and its original hosted execution together.

use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use tokio::sync::OwnedMutexGuard;

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedObservation, HostedOwner, HostedRecord, HostedState,
    load_hosted_execution_pg,
};
use crate::services::discord::SharedData;
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::provider::ProviderKind;
use crate::services::tui_prompt_dedupe::{self, ExternalInputRelayLease};

pub(super) struct AdmissionFence<'a> {
    transition: SessionTransitionLease<'a>,
    shared: &'a Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    logical: String,
    lease: ExternalInputRelayLease,
    observed: Option<HostedObservation>,
}

impl<'a> AdmissionFence<'a> {
    pub(super) async fn capture(
        shared: &'a Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        logical: &str,
        lease: &ExternalInputRelayLease,
        borrowed: Option<&'a OwnedMutexGuard<()>>,
    ) -> Result<Self, String> {
        let transition = match borrowed {
            Some(guard) => SessionTransitionLease::borrow(shared, channel, guard),
            None => SessionTransitionLease::try_acquire(shared, channel),
        }
        .map_err(|_| "synthetic session transition busy or mismatched".to_string())?;
        let mut fence = Self {
            transition,
            shared,
            provider: provider.clone(),
            channel,
            logical: logical.to_owned(),
            lease: lease.clone(),
            observed: None,
        };
        fence.observed = fence.read_current().await?;
        Ok(fence)
    }

    pub(super) async fn revalidate(&self) -> Result<(), String> {
        if !self.transition.matches(self.shared, self.channel) {
            return Err("synthetic session transition changed".into());
        }
        if self.read_current().await? != self.observed {
            return Err("synthetic hosted row changed under admission".into());
        }
        Ok(())
    }

    pub(super) fn hosted_identity(&self) -> Option<(i64, String)> {
        let observed = self.observed.as_ref()?;
        let HostedRecord::Known(record) = &observed.record else {
            return None;
        };
        Some((observed.session_id(), record.execution_nonce.clone()))
    }

    /// A source alias is accepted only while it still names the pinned physical file and session.
    pub(super) async fn matches_source(&self, path: &Path, session: Option<&str>) -> bool {
        let Some(pin) = self.lease.hosted_source.as_ref() else {
            return self.observed.is_none();
        };
        let pin = pin.source.clone();
        let path = path.to_path_buf();
        let session = session.map(str::to_owned);
        tokio::task::spawn_blocking(move || source_matches(&pin, &path, session.as_deref()))
            .await
            .unwrap_or(false)
    }

    async fn read_current(&self) -> Result<Option<HostedObservation>, String> {
        let endpoint = crate::config::session_hosts::herdr_endpoint(self.channel.get());
        let pin = self.lease.hosted_source.as_ref();
        let herdr = tui_prompt_dedupe::hosted_source::herdr_evidence(
            self.channel.get(),
            &self.logical,
            None,
        )
        .map_err(str::to_owned)?
            || pin.is_some_and(|pin| pin.hosted_admission_required);
        let (pool, key) = match (
            self.shared.pg_pool.as_ref(),
            self.lease.session_key.as_deref(),
        ) {
            (Some(pool), Some(key)) => (pool, key),
            _ if !herdr => return Ok(None),
            _ => return Err("synthetic Herdr admission has no database or session key".into()),
        };
        if !herdr {
            let exact = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await;
            match exact {
                HostedLookup::Missing => return Ok(None),
                HostedLookup::Found(observed) => {
                    if !tui_prompt_dedupe::hosted_source::herdr_evidence(
                        self.channel.get(),
                        &self.logical,
                        Some(&observed.record),
                    )
                    .map_err(str::to_owned)?
                    {
                        return Ok(None);
                    }
                }
                other => return Err(format!("synthetic exact hosted lookup: {other:?}")),
            }
        }
        if let Some(pin) = pin {
            let original = pin.clone();
            #[cfg(test)]
            let log_root = tui_prompt_dedupe::binding_events::test_root();
            let historical = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                let previous_root = tui_prompt_dedupe::binding_events::test_root();
                #[cfg(test)]
                tui_prompt_dedupe::binding_events::set_test_root(log_root.as_deref());
                let valid = tui_prompt_dedupe::hosted_source::validate(&original);
                #[cfg(test)]
                tui_prompt_dedupe::binding_events::set_test_root(previous_root.as_deref());
                valid
            })
            .await
            .map_err(|error| format!("synthetic source history read failed: {error}"))?;
            if !historical {
                return Err("synthetic original source history differs".into());
            }
            let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(&self.logical)
                .ok_or("synthetic pinned source has no runtime binding")?;
            if !self
                .matches_source(
                    Path::new(&binding.output_path),
                    binding.session_id.as_deref(),
                )
                .await
            {
                return Err("synthetic original source changed".into());
            }
        }
        // Re-read after source I/O so the saved identity uses a fresh exact observation.
        let exact = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await;
        let observed = match exact {
            HostedLookup::Found(observed) => observed,
            other => return Err(format!("synthetic exact hosted lookup: {other:?}")),
        };
        let HostedRecord::Known(record) = &observed.record else {
            return Err("synthetic hosted execution is unreadable".into());
        };
        let pin = pin.ok_or("synthetic Herdr claim has no original source pin")?;
        let endpoint = endpoint.ok_or("synthetic Herdr endpoint is unavailable")?;
        let node = crate::config::session_hosts::local_node()
            .ok_or("synthetic Herdr local node is unavailable")?;
        let root =
            crate::config::runtime_root().ok_or("synthetic Herdr runtime root is unavailable")?;
        let owner = HostedOwner {
            provider: self.provider.as_str().into(),
            discord_token_hash: self.shared.token_hash.clone(),
            channel_id: self.channel.to_string(),
            logical_key: self.logical.clone(),
            owner_node: node.clone(),
            runtime_root: root.display().to_string(),
        };
        let location_matches = record.location.as_ref().is_some_and(|location| {
            location.host == "herdr"
                && location.execution_node == node
                && endpoint.execution_node == node
                && location.endpoint_config_key == endpoint.key
                && Path::new(&location.socket_addr) == endpoint.socket_path
                && location.named_session == endpoint.herdr_session
                && !location.pane_id.is_empty()
        });
        if record.state != HostedState::Bound
            || record.owner != owner
            || pin.provider != owner.provider
            || pin.channel_id != self.channel.get()
            || pin.logical_key != self.logical
            || pin.execution_nonce != record.execution_nonce
            || !location_matches
        {
            return Err("synthetic original hosted execution differs".into());
        }
        let channel = self.channel.to_string();
        let canonical = HostedLookupKey::Canonical {
            provider: self.provider.as_str(),
            identity: CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &self.shared.token_hash,
                channel_id: &channel,
            },
        };
        if load_hosted_execution_pg(pool, canonical).await != HostedLookup::Found(observed.clone())
        {
            return Err("synthetic exact and canonical hosted rows differ".into());
        }
        // The map guard is a leaf: no database, file or actor await runs while it is held.
        let admitted = tui_prompt_dedupe::herdr_claim_admission(&self.logical, true)
            .ok()
            .flatten()
            .is_some_and(|map| {
                map.get(&self.logical)
                    .is_some_and(|(nonce, admitted)| nonce == &pin.execution_nonce && *admitted)
            });
        if !admitted {
            return Err("synthetic original Herdr execution is not admitted".into());
        }
        Ok(Some(observed))
    }
}

fn source_matches(
    source: &tui_prompt_dedupe::binding_events::SourceId,
    path: &Path,
    session: Option<&str>,
) -> bool {
    if session != Some(source.session_id.as_str()) || source.session_id.is_empty() {
        return false;
    }
    let identity = |path: &Path| {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| crate::services::tui_o::shadow::capture::file_identity(&metadata))
    };
    let expected = (source.dev, source.ino);
    identity(&source.path) == Some(expected)
        && identity(path) == Some(expected)
        && (expected != (0, 0)
            || std::fs::canonicalize(&source.path)
                .ok()
                .zip(std::fs::canonicalize(path).ok())
                .is_some_and(|(original, current)| original == current))
}
