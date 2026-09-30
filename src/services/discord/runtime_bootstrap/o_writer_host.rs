//! Gateway side of the O writer host: the gateway's own HTTP client and bot id, the shared
//! delivery lease cells, the process alarm router and the facts a first activation checks.

use super::*;

use std::future::Future;

use crate::services::discord::outbound::o_writer_io::{ChannelLeases, GatewayPort};
use crate::services::tui_o::alarm::AlarmRouter;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::actor::POLL_INTERVAL;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::{self, HostIo};

struct GatewayHost {
    shared: Arc<SharedData>,
    alarms: Arc<AlarmRouter>,
}

impl HostIo for GatewayHost {
    type Port = GatewayPort;
    type Lease = ChannelLeases;
    type Alarms = Arc<AlarmRouter>;
    type Bindings = ChannelBindingLog;

    /// Waits for the context the gateway caches on ready, so no token fallback stands in for it.
    fn port(&self) -> impl Future<Output = Arc<GatewayPort>> + Send {
        let shared = Arc::clone(&self.shared);
        async move {
            loop {
                if let Some(ctx) = shared.http.cached_serenity_ctx.get() {
                    let bot_id = ctx.cache.current_user().id.get();
                    return Arc::new(GatewayPort::new(Arc::clone(&ctx.http), bot_id));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }

    fn lease(&self) -> ChannelLeases {
        ChannelLeases::from_shared(Arc::clone(&self.shared))
    }

    fn alarms(&self) -> Arc<AlarmRouter> {
        Arc::clone(&self.alarms)
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        Arc::new(ChannelBindingLog::new(channel, provider))
    }

    /// Open intake, sessions of other nodes, node overrides and Legacy inflight or custody.
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        let shared = Arc::clone(&self.shared);
        async move {
            let pool = shared.pg_pool.clone().ok_or("no PG pool")?;
            let id = channel.to_string();
            let local =
                crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
            let rows = crate::db::o_channel_activation::activation_rows(&pool, &id, &local).await;
            let rows = rows.map_err(|error| format!("activation rows: {error}"))?;
            let agent_node =
                crate::services::cluster::agent_execution_node::for_channel(&pool, &id);
            let agent_node = agent_node
                .await
                .map_err(|error| format!("agent node: {error}"))?;
            let node_override =
                super::super::commands::channel_node_override(&shared, ChannelId::new(channel));
            let kind = match provider {
                ShadowProvider::Claude => ProviderKind::Claude,
                ShadowProvider::Codex => ProviderKind::Codex,
            };
            let legacy_custody = super::super::inflight::inflight_state_file_exists(&kind, channel)
                || super::super::terminal_delivery_custody::retains_channel(channel)?;
            Ok(ActivationFacts {
                open_intake: rows.open_intake,
                runner_sessions: rows.foreign_sessions,
                node_override: node_override.or(agent_node),
                legacy_custody,
            })
        }
    }
}

/// Starts the writer host for this provider's gateway runtime; it never waits on the gateway.
pub(super) fn spawn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    config: Option<&TuiOConfig>,
    pg_gateway: bool,
) {
    let shadow = match provider.as_str() {
        "claude" => ShadowProvider::Claude,
        "codex" => ShadowProvider::Codex,
        _ => return,
    };
    // Nothing below runs unless this bot owns a channel, so an off or empty writer takes no lock.
    host::start(shadow, pg_gateway, || {
        let alarms = Arc::new(AlarmRouter::for_process(config, shared.pg_pool.clone()));
        host::HostParts {
            io: Arc::new(GatewayHost {
                shared: Arc::clone(shared),
                alarms,
            }),
            runtime_root: crate::config::runtime_root(),
            gate: crate::services::tui_o::ownership::gate(provider.as_str()),
            readiness: host::process_readiness(),
        }
    });
}
