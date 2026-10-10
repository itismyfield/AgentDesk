//! Gateway side of the O writer host: the gateway's own HTTP client and bot id, the shared
//! delivery lease cells, the process alarm router and the facts a first activation checks. Off the
//! gateway, a delegated channel's writer posts through the bot token's REST client instead.

use super::*;

use std::future::Future;
use std::time::Duration;

use crate::services::discord::outbound::o_writer_io::{ChannelLeases, GatewayPort};
use crate::services::discord::outbound::o_writer_legacy::LegacyRelay;
use crate::services::tui_o::alarm::AlarmRouter;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_o::turn_mode::TurnConfig;
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::actor::POLL_INTERVAL;
use crate::services::tui_o::writer::adoption::LegacyView;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::{self, Custody, FencedFacts, HostIo};

/// How long a first activation waits for the cluster bootstrap to publish this node's id.
const SELF_ID_WAIT: Duration = Duration::from_secs(10);

struct GatewayHost {
    shared: Arc<SharedData>,
    alarms: Arc<AlarmRouter>,
    self_id_wait: Duration,
    /// `cluster.instance_id` as the home judgement read it; `None` without clustering.
    configured_id: Option<String>,
    /// The boot turn selection a newly adopted channel is confirmed against.
    turn: TurnConfig,
    /// Off the gateway: the bot token's REST client every hosted channel posts through.
    rest: Option<Arc<serenity::Http>>,
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

/// A port over the REST client once it names its bot; a failed lookup is retried.
async fn rest_port(http: Arc<serenity::Http>) -> Arc<GatewayPort> {
    loop {
        match http.get_current_user().await {
            Ok(user) => return Arc::new(GatewayPort::new(Arc::clone(&http), user.id.get())),
            Err(error) => {
                tracing::warn!(%error, "[tui_o] REST writer port: bot user lookup failed")
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

impl HostIo for GatewayHost {
    type Port = GatewayPort;
    type Lease = ChannelLeases;
    type Alarms = Arc<AlarmRouter>;
    type Bindings = ChannelBindingLog;

    /// Waits for the context the gateway caches on ready, so no token fallback stands in for it;
    /// off the gateway, for the REST client to name its bot.
    fn port(&self) -> impl Future<Output = Arc<GatewayPort>> + Send {
        let shared = Arc::clone(&self.shared);
        let rest = self.rest.clone();
        async move {
            if let Some(http) = rest {
                return rest_port(http).await;
            }
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

    /// Reads other pool-backed policy before reserving the intake fence's connection.
    async fn intake_fence(
        &self,
        channel: u64,
        _provider: ShadowProvider,
    ) -> Result<FencedFacts, String> {
        let pool = self.shared.pg_pool.as_ref().ok_or("no PG pool")?;
        let id = channel.to_string();
        let local = local_id(self.configured_id.as_deref(), self.self_id_wait).await?;
        // Finish pool lookups before the fence takes its connection, including a one-slot pool.
        let agent_node = crate::services::cluster::agent_execution_node::for_channel(pool, &id)
            .await
            .map_err(|error| format!("agent node: {error}"))?;
        let node_override =
            super::super::commands::channel_node_override(&self.shared, ChannelId::new(channel));
        let (hold, rows, queued_bodies) =
            crate::db::o_channel_activation::fenced_activation_rows(pool, &id, &local)
                .await
                .map_err(|error| format!("activation fence: {error}"))?;
        Ok(FencedFacts {
            hold,
            facts: ActivationFacts {
                open_intake: rows.open_intake,
                runner_sessions: rows.foreign_sessions,
                node_override: node_override.or(agent_node),
            },
            queued_bodies,
        })
    }

    /// Legacy inflight, delivery custody and pending starts, durable or in memory: all local files.
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
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
        if pending_start || super::super::terminal_delivery_custody::retains_channel(channel)? {
            return Ok(Custody::Active);
        }
        let row = super::super::inflight::inflight_state_file_exists(&kind, channel);
        Ok(if row { Custody::Row } else { Custody::Free })
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        Arc::new(LegacyRelay::new(Arc::clone(&self.shared)))
    }

    /// An active turn, a queued intervention or a dispatch not yet started.
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        let shared = Arc::clone(&self.shared);
        async move {
            let mailbox = super::super::mailbox_snapshot(&shared, ChannelId::new(channel)).await;
            mailbox.cancel_token.is_some()
                || !mailbox.intervention_queue.is_empty()
                || mailbox.pending_user_dispatch.is_some()
        }
    }

    fn relaying(&self, channel: u64) -> bool {
        self.shared
            .relay_emission_in_flight(ChannelId::new(channel))
    }

    fn adopted(&self, channel: u64, provider: ShadowProvider) {
        let kind = match provider {
            ShadowProvider::Claude => ProviderKind::Claude,
            ShadowProvider::Codex => ProviderKind::Codex,
        };
        let confirm =
            super::super::tui_direct_pending_start::turn_retirement::confirm_turn_channels;
        let confirmed = confirm(&kind, Some(&self.turn), || vec![channel]);
        super::super::turn_presence::supervisor::register(&kind, &confirmed);
    }
}

fn shadow_of(provider: &ProviderKind) -> Option<ShadowProvider> {
    match provider.as_str() {
        "claude" => Some(ShadowProvider::Claude),
        "codex" => Some(ShadowProvider::Codex),
        _ => None,
    }
}

fn parts(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    config: Option<&TuiOConfig>,
    rest: Option<Arc<serenity::Http>>,
) -> host::HostParts<GatewayHost> {
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
            turn: config.map(|c| c.turn.clone()).unwrap_or_default(),
            rest,
        }),
        runtime_root: crate::config::runtime_root(),
        gate: crate::services::tui_o::ownership::gate(provider.as_str()),
        readiness: host::process_readiness(),
    }
}

/// Starts the writer host for this provider's gateway runtime; it never waits on the gateway.
pub(super) fn spawn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    config: Option<&TuiOConfig>,
    pg_gateway: bool,
) {
    let Some(shadow) = shadow_of(provider) else {
        return;
    };
    // Nothing below runs unless this bot may adopt a channel, so an off or empty writer takes no lock.
    host::start(shadow, pg_gateway, || parts(shared, provider, config, None));
}

/// Starts the writer for each delegated channel whose home gate is registered here and whose Herdr
/// endpoint runs on this node, off the gateway; with none, nothing is built.
pub(super) fn spawn_delegated(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    config: Option<&TuiOConfig>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some(shadow) = shadow_of(provider) else {
        return Vec::new();
    };
    let delegated = crate::services::cluster::channel_home_boot::delegated_boot_ownership(
        super::herdr_runs_here,
    );
    host::start_delegated(shadow, delegated, || {
        let rest = shared.serenity_http_or_token_fallback();
        parts(shared, provider, config, rest)
    })
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
            turn: TurnConfig::default(),
            rest: None,
        }
    }

    /// The host a standby writer gets: every port posts over `http`.
    pub(in crate::services::discord::runtime_bootstrap) fn rest(
        shared: &Arc<SharedData>,
        http: Arc<serenity::Http>,
    ) -> Arc<impl HostIo<Port = GatewayPort>> {
        parts(shared, &ProviderKind::Claude, None, Some(http)).io
    }
}

#[cfg(test)]
mod fence_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::tui_o::shadow::ShadowProvider::Claude;

    /// Delegates host facts while retaining HostIo's fail-closed intake fence default.
    struct Unfenced<I: HostIo>(Arc<I>);

    impl<I: HostIo> HostIo for Unfenced<I> {
        type Port = I::Port;
        type Lease = I::Lease;
        type Alarms = I::Alarms;
        type Bindings = I::Bindings;

        fn port(&self) -> impl Future<Output = Arc<Self::Port>> + Send {
            self.0.port()
        }

        fn lease(&self) -> Self::Lease {
            self.0.lease()
        }

        fn alarms(&self) -> Self::Alarms {
            self.0.alarms()
        }

        fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Self::Bindings> {
            self.0.bindings(channel, provider)
        }

        fn activation_facts(
            &self,
            channel: u64,
            provider: ShadowProvider,
        ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
            self.0.activation_facts(channel, provider)
        }

        fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
            self.0.local_custody(channel, provider)
        }

        fn legacy(&self) -> Arc<dyn LegacyView> {
            self.0.legacy()
        }

        fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
            self.0.legacy_busy(channel)
        }

        fn relaying(&self, channel: u64) -> bool {
            self.0.relaying(channel)
        }

        fn adopted(&self, channel: u64, provider: ShadowProvider) {
            self.0.adopted(channel, provider);
        }
    }

    #[tokio::test]
    async fn gateway_intake_fence_keeps_fresh_facts_with_one_pool_connection_pg() {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate_with_max_connections(1).await;
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, default_execution_node_id)
             VALUES ('fence-agent', 'fence', '673702', 'worker-override')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, channel_id, instance_id)
             VALUES ('fence-foreign', 'claude', 'idle', '673702', 'foreign')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let host = GatewayHost {
            shared,
            alarms: Arc::new(AlarmRouter::for_process(None, None)),
            self_id_wait: Duration::ZERO,
            configured_id: crate::services::cluster::node_registry::SELF_INSTANCE_ID
                .get()
                .cloned()
                .or_else(|| Some("fence-local".into())),
            turn: TurnConfig::default(),
            rest: None,
        };
        let fenced =
            tokio::time::timeout(Duration::from_secs(3), host.intake_fence(673702, Claude))
                .await
                .expect("a fence must not wait for a second pool connection")
                .unwrap();
        assert_eq!(fenced.facts.runner_sessions, 1);
        assert_eq!(fenced.facts.open_intake, 0);
        assert_eq!(
            fenced.facts.node_override.as_deref(),
            Some("worker-override")
        );
        assert_eq!(fenced.queued_bodies, 0);
        assert_eq!(
            pool.num_idle(),
            0,
            "the returned hold retains its connection"
        );
        fenced.hold.release().await.unwrap();
        assert_eq!(
            host.activation_facts(673702, Claude)
                .await
                .unwrap()
                .runner_sessions,
            1
        );
        pool.close().await;
        fixture.drop().await;
    }

    #[tokio::test]
    async fn a_host_without_an_intake_fence_refuses_deferred_activation_facts() {
        let host = crate::services::tui_o::writer::host::test_io::TestHost::new([]);
        let result = Unfenced(Arc::clone(&host))
            .intake_fence(673702, Claude)
            .await;
        assert!(matches!(result, Err(detail) if detail == "this host has no intake fence"));
        assert!(host.posts.to(673702).is_empty());
    }
}
