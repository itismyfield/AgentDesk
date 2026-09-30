//! Gateway side of the O writer host: the gateway's own HTTP client and bot id, the shared
//! delivery lease cells, the process alarm router and the facts a first activation checks.

use super::*;

use std::future::Future;
use std::time::Duration;

use crate::services::discord::outbound::o_writer_io::{ChannelLeases, GatewayPort};
use crate::services::tui_o::alarm::AlarmRouter;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::actor::POLL_INTERVAL;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::{self, HostIo};

/// How long a first activation waits for the cluster bootstrap to publish this node's id.
const SELF_ID_WAIT: Duration = Duration::from_secs(10);

struct GatewayHost {
    shared: Arc<SharedData>,
    alarms: Arc<AlarmRouter>,
    self_id_wait: Duration,
    /// `cluster.instance_id` as the home judgement read it; `None` without clustering.
    configured_id: Option<String>,
}

/// With clustering, the configured id the home judgement used, refused when bootstrap published
/// another; otherwise the published id. The hostname-PID fallback is never taken.
async fn local_id(configured: Option<&str>, max_wait: Duration) -> Result<String, String> {
    let published = crate::services::cluster::node_registry::SELF_INSTANCE_ID.get();
    match (configured, published) {
        (Some(configured), Some(published)) if configured != published => Err(format!(
            "published instance id {published} differs from configured {configured}"
        )),
        (Some(configured), _) => Ok(configured.to_owned()),
        (None, _) => published_self_id(max_wait)
            .await
            .ok_or_else(|| "this node's instance id is not published yet".into()),
    }
}

/// The id the cluster bootstrap published; the hostname-PID fallback is never taken for it.
async fn published_self_id(max_wait: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if let Some(id) = crate::services::cluster::node_registry::SELF_INSTANCE_ID.get() {
            return Some(id.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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

    /// Open intake, sessions of other nodes and node overrides.
    fn activation_facts(
        &self,
        channel: u64,
        _provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        let (shared, self_id_wait) = (Arc::clone(&self.shared), self.self_id_wait);
        let configured = self.configured_id.clone();
        async move {
            let pool = shared.pg_pool.clone().ok_or("no PG pool")?;
            let id = channel.to_string();
            let local = local_id(configured.as_deref(), self_id_wait).await?;
            let rows = crate::db::o_channel_activation::activation_rows(&pool, &id, &local).await;
            let rows = rows.map_err(|error| format!("activation rows: {error}"))?;
            let agent_node =
                crate::services::cluster::agent_execution_node::for_channel(&pool, &id);
            let agent_node = agent_node
                .await
                .map_err(|error| format!("agent node: {error}"))?;
            let node_override =
                super::super::commands::channel_node_override(&shared, ChannelId::new(channel));
            Ok(ActivationFacts {
                open_intake: rows.open_intake,
                runner_sessions: rows.foreign_sessions,
                node_override: node_override.or(agent_node),
            })
        }
    }

    /// Legacy inflight, delivery custody and pending starts, durable or in memory: all local files.
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<bool, String> {
        let kind = match provider {
            ShadowProvider::Claude => ProviderKind::Claude,
            ShadowProvider::Codex => ProviderKind::Codex,
        };
        let pending = |record: &super::super::tui_direct_pending_start::TuiDirectPendingStart| {
            record.channel_id == channel && record.provider.eq_ignore_ascii_case(kind.as_str())
        };
        let pending_start = super::super::tui_direct_pending_start::load_all()
            .iter()
            .any(pending)
            || super::super::tui_direct_pending_start::pending_synthetic_start_present(
                kind.as_str(),
                channel,
            );
        Ok(pending_start
            || super::super::inflight::inflight_state_file_exists(&kind, channel)
            || super::super::terminal_delivery_custody::retains_channel(channel)?)
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
    // Nothing below runs unless this bot may adopt a channel, so an off or empty writer takes no lock.
    host::start(shadow, pg_gateway, || {
        let alarms = Arc::new(AlarmRouter::for_process(config, shared.pg_pool.clone()));
        let boot = crate::services::tui_o::channel_policy::boot();
        let configured_id = boot
            .and_then(|boot| boot.configured_id())
            .map(str::to_owned);
        host::HostParts {
            io: Arc::new(GatewayHost {
                shared: Arc::clone(shared),
                alarms,
                self_id_wait: SELF_ID_WAIT,
                configured_id,
            }),
            runtime_root: crate::config::runtime_root(),
            gate: crate::services::tui_o::ownership::gate(provider.as_str()),
            readiness: host::process_readiness(),
        }
    });
}

#[cfg(test)]
pub(super) mod test_host {
    use super::*;

    /// The gateway host over `shared`, waiting `self_id_wait` for the published self id unless
    /// clustering configured `configured_id`.
    pub(in crate::services::discord::runtime_bootstrap) fn over(
        shared: Arc<SharedData>,
        self_id_wait: Duration,
        configured_id: Option<&str>,
    ) -> impl HostIo {
        let alarms = Arc::new(AlarmRouter::for_process(None, None));
        GatewayHost {
            shared,
            alarms,
            self_id_wait,
            configured_id: configured_id.map(str::to_owned),
        }
    }
}
