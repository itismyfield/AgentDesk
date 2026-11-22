use std::sync::{Arc, atomic::Ordering};

use poise::serenity_prelude::ChannelId;

use super::HealthRegistry;
use super::runtime_resolve::resolve_direct_channel_runtime;
use crate::db::dispatched_sessions::hosted_execution::{HostedLookup, HostedRecord, HostedState};
use crate::domain::human_input::HumanInputError;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::session_host::HostKind;

pub(crate) struct ThreadTurnRuntime {
    pub(super) shared: Arc<SharedData>,
    parent: ChannelId,
    pub(super) channel: ChannelId,
}

impl ThreadTurnRuntime {
    pub(crate) fn parent_id(&self) -> u64 {
        self.parent.get()
    }

    pub(crate) async fn read(
        registry: &HealthRegistry,
        parent: ChannelId,
        provider: &ProviderKind,
        channel: ChannelId,
    ) -> Result<(Option<u64>, Self), String> {
        let (http, shared) = resolve_direct_channel_runtime(registry, parent, provider).await?;
        if shared.provider != *provider {
            return Err("Discord runtime provider differs from the binding".into());
        }
        use crate::services::session_host::surface_policy::VerifiedSurface;
        use poise::serenity_prelude::Channel;
        let observed = match channel
            .to_channel(&http)
            .await
            .map_err(|error| error.to_string())?
        {
            Channel::Guild(guild) if guild.id == channel => VerifiedSurface::checked_parent(&guild),
            other if other.id() == channel => None,
            _ => return Err("Discord returned a different channel".to_owned()),
        };
        Ok((
            observed,
            Self {
                shared,
                parent,
                channel,
            },
        ))
    }

    pub(super) fn scope_metadata(&self, metadata: Option<serde_json::Value>) -> serde_json::Value {
        let parent = serde_json::Value::String(self.parent.to_string());
        match metadata {
            Some(serde_json::Value::Object(mut object)) => {
                object.insert("parent_channel_id".to_owned(), parent);
                serde_json::Value::Object(object)
            }
            Some(value) => {
                serde_json::json!({"trigger_metadata": value, "parent_channel_id": parent})
            }
            None => serde_json::json!({"parent_channel_id": parent}),
        }
    }

    pub(crate) fn parent_consistent(&self) -> bool {
        let policy = crate::services::tui_o::cutover::policy_channel(self.channel.get());
        policy == self.channel.get() || policy == self.parent.get()
    }

    pub(crate) async fn author_allowed(&self, author_id: u64) -> Result<(), HumanInputError> {
        super::turn_deliver::authorize_on_shared(&self.shared, author_id).await
    }

    /// Reads the child's current execution before the routes can reserve an original.
    pub(crate) async fn check_live(&self) -> Result<HostKind, &'static str> {
        let shared = &self.shared;
        let channel = self.channel.get();
        let restart = &shared.restart;
        if !restart.reconcile_done.load(Ordering::Acquire)
            || restart.restart_pending.load(Ordering::Acquire)
        {
            return Err("thread_boot_unverified");
        }
        if shared.http.cached_serenity_ctx.get().is_none()
            || shared.http.cached_bot_token.get().is_none()
        {
            return Err("thread_runtime_unavailable");
        }
        if !self.parent_consistent() {
            return Err("thread_parent_conflict");
        }
        if matches!(
            crate::services::tui_o::cutover::intake_route::route(shared.provider.as_str(), channel),
            crate::services::tui_o::cutover::intake_route::IntakeRoute::Hold(_)
        ) {
            return Err("thread_output_unready");
        }
        let pool = shared.pg_pool.as_ref().ok_or("thread_owner_unverified")?;
        self.check_owner(pool).await?;
        let (logical, session_id) = {
            let core = shared.core.lock().await;
            let session = core.sessions.get(&self.channel).ok_or("thread_cold")?;
            let name = session
                .channel_name
                .as_deref()
                .ok_or("thread_boot_unverified")?;
            let session = session
                .session_id
                .clone()
                .filter(|id| !id.is_empty())
                .ok_or("thread_boot_unverified")?;
            (shared.provider.build_tmux_session_name(name), session)
        };
        let lookup = super::super::host_key_derivation::derive_hosted_lookup(
            pool,
            std::slice::from_ref(&shared.token_hash),
            &shared.provider,
            channel,
            &logical,
        )
        .await;
        let record = match lookup {
            HostedLookup::Found(row) => row.record,
            HostedLookup::Missing => HostedRecord::Legacy,
            _ => return Err("thread_host_unverified"),
        };
        let key = super::super::session_locator::current_row(
            pool,
            &shared.token_hash,
            &shared.provider,
            channel,
        )
        .await
        .map_err(|_| "thread_host_unverified")?
        .map(|(_, key)| key)
        .unwrap_or_else(|| {
            super::super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &shared.provider,
                &logical,
            )
        });
        let host =
            crate::services::turn_host::for_turn(Some(pool), &shared.provider, channel, Some(&key))
                .await;
        let live = match (record, host) {
            #[cfg(unix)]
            (HostedRecord::Known(row), crate::services::turn_host::TurnHost::Herdr(plan)) => {
                let local = crate::config::session_hosts::local_node();
                let root = crate::config::runtime_root();
                let ours = row.state == HostedState::Bound
                    && row.owner.logical_key == logical
                    && row.owner.channel_id == channel.to_string()
                    && row.owner.discord_token_hash == shared.token_hash
                    && row.owner.provider == shared.provider.as_str()
                    && root.as_deref() == Some(std::path::Path::new(&row.owner.runtime_root))
                    && local.as_deref() == Some(row.owner.owner_node.as_str())
                    && row.location.as_ref().is_some_and(|at| {
                        local.as_deref() == Some(at.execution_node.as_str())
                            && at.endpoint_config_key == plan.endpoint.config_key
                            && at.socket_addr == plan.endpoint.socket_addr
                            && at.named_session == plan.endpoint.herdr_session
                    });
                let same = !plan.bootstrap
                    && plan.row.is_some_and(|observed| {
                        observed.record == HostedRecord::Known(row.clone())
                    });
                if !ours
                    || !same
                    || !crate::services::turn_host::herdr_turn_switched_on_for(&shared.provider)
                {
                    return Err("thread_execution_unverified");
                }
                let target = crate::services::session_host::herdr_endpoints().target(&row);
                tokio::task::spawn_blocking(move || {
                    target.is_some_and(|target| target.execution_alive())
                })
                .await
                .unwrap_or(false)
                .then_some(HostKind::Herdr)
            }
            (record, crate::services::turn_host::TurnHost::Tmux)
                if matches!(&record, HostedRecord::Legacy)
                    || matches!(&record, HostedRecord::Known(row)
                        if row.state == HostedState::Retired) =>
            {
                live_tmux(channel, logical, &session_id)
                    .await
                    .then_some(HostKind::Tmux)
            }
            _ => None,
        };
        live.ok_or("thread_execution_unverified")
    }

    async fn check_owner(&self, pool: &sqlx::PgPool) -> Result<(), &'static str> {
        use crate::services::cluster::intake_router_hook::owner_record::{
            OwnerIdentity, read_latest_owner_in_tx,
        };
        let identity = OwnerIdentity::new(self.shared.provider.as_str(), &self.channel.to_string());
        let mut tx = pool.begin().await.map_err(|_| "thread_owner_unverified")?;
        let owner = read_latest_owner_in_tx(&mut tx, &identity)
            .await
            .map_err(|_| "thread_owner_unverified")?;
        tx.rollback().await.map_err(|_| "thread_owner_unverified")?;
        if let Some(owner) = owner
            && (owner.status != "active"
                || crate::config::session_hosts::local_node().as_deref()
                    != Some(owner.owner_instance_id.as_str()))
        {
            return Err("thread_owner_unavailable");
        }
        Ok(())
    }
}

async fn live_tmux(channel: u64, logical: String, session_id: &str) -> bool {
    use crate::services::tui_prompt_dedupe::{
        owner_channel_for_tmux_session, peek_tmux_runtime_binding,
    };
    let maps_to_child = owner_channel_for_tmux_session(&logical) == Some(channel)
        && peek_tmux_runtime_binding(&logical)
            .is_some_and(|binding| binding.session_id.as_deref() == Some(session_id));
    maps_to_child
        && tokio::task::spawn_blocking(move || {
            use crate::services::session_host::{HostLiveness, HostSessionRef, TmuxHost};
            crate::services::session_host::operation_policy::marker_is_tmux(
                &crate::services::tmux_common::host_marker::read_host_kind_marker(&logical),
            ) && TmuxHost.liveness_within(
                HostSessionRef::tmux(&logical),
                std::time::Duration::from_secs(2),
            ) == HostLiveness::Live
        })
        .await
        .unwrap_or(false)
}
