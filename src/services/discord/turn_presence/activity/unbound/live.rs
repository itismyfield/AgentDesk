//! Concrete Unbound reads and their publication identity; no start or cleanup authority.
use super::super::{Activity, LivePorts, Observed, Watch, lock, observed};
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{HostedLookupKey, load_hosted_execution_pg};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::{SharedData, inflight, org_schema, runtime_store};
use crate::services::platform::tmux::session_inventory::{Observation, observe_inventory};
use crate::services::tmux_common;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use poise::serenity_prelude::ChannelId;
use std::sync::{Arc, Mutex, Weak};

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalTrace {
    marker: HostKindMarker,
    runtime: Result<Option<RuntimeHandoffKind>, String>,
    process: Option<u32>,
}
fn marker_path(name: &str, extension: &str) -> std::path::PathBuf {
    let legacy = std::path::PathBuf::from(tmux_common::legacy_tmp_session_path(name, extension));
    match (tmux_common::persistent_sessions_dir(), legacy.file_name()) {
        (Some(dir), Some(filename)) => dir.join(filename),
        _ => legacy,
    }
}
fn local_trace(name: &str) -> LocalTrace {
    let path = marker_path(name, tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT);
    let runtime = match std::fs::read_to_string(path) {
        Ok(raw) => RuntimeHandoffKind::from_str(&raw)
            .map(Some)
            .ok_or_else(|| "unrecognized runtime marker".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    };
    let marker = match std::fs::read_to_string(marker_path(name, "host_kind")) {
        Ok(raw) => match HostKind::from_persisted(raw.trim()) {
            Some(kind) => HostKindMarker::Known(kind),
            None => HostKindMarker::Unrecognized(raw),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HostKindMarker::Absent,
        Err(error) => HostKindMarker::ReadFailed(error.to_string()),
    };
    LocalTrace {
        marker,
        runtime,
        process: crate::services::session_backend::process_session_pid(name),
    }
}
fn read_optional(path: Option<std::path::PathBuf>) -> Result<Option<Vec<u8>>, String> {
    let path = path.ok_or("runtime path unavailable")?;
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}
// Preserve registration precedence without config hardening or recovery-catalog mutation.
fn registrations(config: &Config) -> Result<Vec<RegisteredChannelBinding>, String> {
    let mut bindings = BTreeMap::new();
    if let Some(bytes) = read_optional(runtime_store::role_map_path())? {
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if let Some(by_id) = value.get("byChannelId").and_then(|v| v.as_object()) {
            for (id, entry) in by_id {
                let Some(channel_id) = id.parse::<u64>().ok().filter(|_| {
                    entry.get("roleId").and_then(|v| v.as_str()).is_some()
                        && entry.get("promptFile").and_then(|v| v.as_str()).is_some()
                }) else {
                    continue;
                };
                if let Some(owner_provider) = entry
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .and_then(ProviderKind::from_str)
                    .filter(ProviderKind::is_supported)
                {
                    bindings.insert(
                        channel_id,
                        RegisteredChannelBinding {
                            channel_id,
                            owner_provider,
                            fallback_name: None,
                        },
                    );
                }
            }
        }
    }
    if let Some(bytes) = read_optional(runtime_store::org_schema_path())? {
        let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        let schema = org_schema::parse_org_schema(text)?;
        if let Some(by_id) = schema
            .channels
            .as_ref()
            .and_then(|channels| channels.by_id.as_ref())
        {
            for (id, binding) in by_id {
                let Ok(channel_id) = id.parse::<u64>() else {
                    continue;
                };
                if let Some(owner_provider) = binding
                    .provider
                    .as_deref()
                    .or_else(|| {
                        schema
                            .agents
                            .get(&binding.agent)
                            .and_then(|agent| agent.provider.as_deref())
                    })
                    .and_then(ProviderKind::from_str)
                    .filter(ProviderKind::is_supported)
                {
                    bindings.insert(
                        channel_id,
                        RegisteredChannelBinding {
                            channel_id,
                            owner_provider,
                            fallback_name: None,
                        },
                    );
                }
            }
        }
    }
    let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
    for path in [
        crate::runtime_layout::config_file_path(&root),
        crate::runtime_layout::legacy_config_file_path(&root),
    ] {
        let Some(bytes) = read_optional(Some(path))? else {
            continue;
        };
        let disk: Config = serde_yaml::from_slice(&bytes).map_err(|e| e.to_string())?;
        let host = BootSessionHosts::from_config(&disk).map_err(|e| e.to_string())?;
        if host != BootSessionHosts::from_config(config).map_err(|e| e.to_string())? {
            return Err("host config awaits restart".into());
        }
        for agent in &disk.agents {
            for (key, channel) in agent.channels.iter() {
                if crate::services::provider::intern_provider_id(key).is_none() {
                    continue;
                }
                let Some(id) = channel.channel_id().and_then(|id| id.parse::<u64>().ok()) else {
                    continue;
                };
                let provider = channel.provider().unwrap_or_else(|| agent.provider.clone());
                let Some(owner_provider) =
                    ProviderKind::from_str(&provider).filter(ProviderKind::is_supported)
                else {
                    continue;
                };
                let fallback_name = channel
                    .channel_name()
                    .or_else(|| channel.aliases().into_iter().next());
                bindings.insert(
                    id,
                    RegisteredChannelBinding {
                        channel_id: id,
                        owner_provider,
                        fallback_name,
                    },
                );
            }
        }
        break;
    }
    Ok(bindings.into_values().collect())
}
#[derive(Clone)]
struct Files {
    fallback: Vec<RegisteredChannelBinding>,
    candidates: Result<Candidates, String>,
    row: Result<Option<InflightTurnState>, String>,
    row_identity: String,
    seq: u64,
    seq_watch: tokio::sync::watch::Receiver<u64>,
    traces: BTreeMap<String, LocalTrace>,
}
impl Files {
    fn collect(
        provider: &ProviderKind,
        channel: u64,
        core: Option<&str>,
        config: &Config,
        ports: &LivePorts,
    ) -> Result<Self, String> {
        ports.scoped(|| {
            let fallback = registrations(config)?;
            let shadow = match provider {
                ProviderKind::Claude => ShadowProvider::Claude,
                _ => ShadowProvider::Codex,
            };
            let log = ChannelBindingLog::new(channel, shadow);
            let candidates = observe_candidates(channel, provider, core, &fallback, &log);
            let row = inflight::load_inflight_state_read_only_result(provider, channel);
            let row_identity = serde_json::to_string(&row).map_err(|e| e.to_string())?;
            let traces = candidates
                .as_ref()
                .map(|c| {
                    c.names
                        .iter()
                        .map(|c| (c.name.clone(), local_trace(&c.name)))
                        .collect()
                })
                .unwrap_or_default();
            let seq_watch =
                crate::services::tui_prompt_dedupe::binding_events::subscribe_binding_events(
                    channel,
                )
                .map_err(|e| e.to_string())?;
            let seq = *seq_watch.borrow();
            Ok(Self {
                fallback,
                candidates,
                row,
                row_identity,
                seq,
                seq_watch,
                traces,
            })
        })
    }
    fn same(&self, other: &Self) -> bool {
        self.fallback == other.fallback
            && self.candidates == other.candidates
            && self.row_identity == other.row_identity
            && self.seq == other.seq
            && self.traces == other.traces
    }
}
#[derive(Clone)]
struct Snapshot {
    shared: Weak<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    basis: Option<String>,
    runtime_root: Option<std::path::PathBuf>,
    config: Arc<Config>,
    files: Files,
    exact: HostedLookup,
    canonical: HostedLookup,
    owners: BTreeMap<String, Option<u64>>,
}
impl Snapshot {
    async fn collect(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        ports: Arc<LivePorts>,
    ) -> Result<Self, String> {
        let config = crate::config_live_reload::current().ok_or("host config unavailable")?;
        let boot = BootSessionHosts::from_config(&config).map_err(|e| e.to_string())?;
        if !crate::config::session_hosts::with_boot(|actual| {
            actual.map_or_else(|| boot.channels().is_empty(), |actual| actual == &boot)
        }) {
            return Err("host config awaits restart".into());
        }
        let runtime_root = crate::config::runtime_root();
        let basis = shared
            .core
            .lock()
            .await
            .sessions
            .get(&channel)
            .and_then(|s| s.channel_name.clone());
        let (worker_ports, worker_provider, worker_basis, worker_config) = (
            ports.clone(),
            provider.clone(),
            basis.clone(),
            config.clone(),
        );
        let files = tokio::task::spawn_blocking(move || {
            Files::collect(
                &worker_provider,
                channel.get(),
                worker_basis.as_deref(),
                &worker_config,
                &worker_ports,
            )
        })
        .await
        .map_err(|e| e.to_string())??;
        let pool = shared.pg_pool.as_ref().ok_or("database unavailable")?;
        let name = files
            .candidates
            .as_ref()
            .map_err(Clone::clone)?
            .current
            .as_deref();
        let exact = match name {
            Some(name) => {
                let key =
                    adk_session::build_namespaced_session_key(&shared.token_hash, provider, name);
                load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await
            }
            None => HostedLookup::Missing,
        };
        let channel_text = channel.to_string();
        let identity = CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &shared.token_hash,
            channel_id: &channel_text,
        };
        let canonical = load_hosted_execution_pg(
            pool,
            HostedLookupKey::Canonical {
                provider: provider.as_str(),
                identity,
            },
        )
        .await;
        Ok(Self {
            shared: Arc::downgrade(shared),
            provider: provider.clone(),
            channel,
            basis,
            runtime_root,
            config,
            files,
            exact,
            canonical,
            owners: BTreeMap::new(),
        })
    }
    fn current(&self) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        if shared
            .tmux_watchers
            .channel_binding(&self.channel)
            .is_some()
            || crate::config::runtime_root() != self.runtime_root
        {
            return false;
        }
        let Ok(core) = shared.core.try_lock() else {
            return false;
        };
        if core
            .sessions
            .get(&self.channel)
            .and_then(|s| s.channel_name.as_ref())
            != self.basis.as_ref()
        {
            return false;
        }
        drop(core);
        if !crate::config_live_reload::current()
            .is_some_and(|config| Arc::ptr_eq(&config, &self.config))
        {
            return false;
        }
        *self.files.seq_watch.borrow() == self.files.seq
            && self.owners.iter().all(|(name, owner)| {
                shared
                    .tmux_watchers
                    .owner_channel_for_tmux_session(name)
                    .map(|id| id.get())
                    == *owner
            })
    }
    fn scope(&self, shared: &SharedData) -> HostScope {
        let candidates = self.files.candidates.as_ref().ok();
        let name = candidates.and_then(|c| c.current.as_deref());
        let hold = |reason| HostScope {
            channel: self.channel.get(),
            provider: self.provider.clone(),
            name: name.unwrap_or_default().into(),
            result: Scope::Hold(reason),
        };
        let Some(name) = name else {
            let registered =
                self.files.fallback.iter().any(|r| {
                    r.channel_id == self.channel.get() && r.owner_provider == self.provider
                });
            return if registered
                && candidates.is_some_and(|c| c.names.is_empty() && !c.inflight)
                && matches!(self.files.row, Ok(None))
                && self.exact == HostedLookup::Missing
                && self.canonical == HostedLookup::Missing
                && local_registration(&self.config, self.channel.get(), &self.provider)
            {
                HostScope {
                    result: Scope::LocalNameless,
                    ..hold("unused")
                }
            } else {
                hold("identity_unresolved")
            };
        };
        let Some(trace) = self.files.traces.get(name) else {
            return hold("probe_failed");
        };
        let Ok(runtime) = trace.runtime else {
            return hold("probe_failed");
        };
        let Ok(boot) = BootSessionHosts::from_config(&self.config) else {
            return hold("probe_failed");
        };
        let key =
            adk_session::build_namespaced_session_key(&shared.token_hash, &self.provider, name);
        let owner = HostedOwner {
            provider: self.provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: self.channel.to_string(),
            logical_key: name.into(),
            owner_node: boot.local_node().unwrap_or_default().into(),
            runtime_root: crate::config::runtime_root()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        };
        host_scope(HostReads {
            channel: self.channel.get(),
            provider: &self.provider,
            token_hash: &shared.token_hash,
            name,
            config: &self.config,
            expected_owner: &owner,
            exact_key: &key,
            exact: &self.exact,
            canonical: &self.canonical,
            evidence: Ok(raw_evidence(
                Some(key.clone()),
                Some(name.into()),
                &self.exact,
                trace.marker.clone(),
                runtime,
                trace.process.is_some(),
            )),
            marker: trace.marker.clone(),
            inflight: &self.files.row,
        })
    }
}

pub(in crate::services::discord::turn_presence::activity) struct Stamp {
    shared: Weak<SharedData>,
    channel: ChannelId,
    basis: Option<String>,
    config: Option<Arc<Config>>,
    runtime_root: Option<std::path::PathBuf>,
    generation: u64,
    revision: u64,
    snapshot: Option<Snapshot>,
    namespace: Option<Observation>,
}
impl Stamp {
    pub(in crate::services::discord::turn_presence::activity) fn publish<R>(
        &self,
        watch: &Mutex<Watch>,
        publish: impl FnOnce() -> R,
    ) -> Option<R> {
        let guard = lock(watch);
        let shared = self.shared.upgrade()?;
        if shared
            .tmux_watchers
            .channel_binding(&self.channel)
            .is_some()
            || crate::config::runtime_root() != self.runtime_root
        {
            return None;
        }
        let core = shared.core.try_lock().ok()?;
        if core
            .sessions
            .get(&self.channel)
            .and_then(|s| s.channel_name.as_ref())
            != self.basis.as_ref()
        {
            return None;
        }
        drop(core);
        let config = crate::config_live_reload::current();
        if !match (&self.config, &config) {
            (Some(old), Some(now)) => Arc::ptr_eq(old, now),
            (None, None) => true,
            _ => false,
        } {
            return None;
        }
        (guard.key.is_none()
            && (guard.generation, guard.revision) == (self.generation, self.revision)
            && self.snapshot.as_ref().is_none_or(Snapshot::current)
            && self.namespace.as_ref().is_none_or(Observation::current))
        .then(publish)
    }
}

pub(in crate::services::discord::turn_presence::activity) async fn read(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    watch: Arc<Mutex<Watch>>,
) -> (Observed, Stamp) {
    // Invalidate before the first core, DB or tmux await; old workers cannot install afterwards.
    let (generation, revision) = {
        let mut guard = lock(&watch);
        guard.key = None;
        guard.outcome = super::super::Outcome::Rebuilding;
        guard.generation += 1;
        guard.revision += 1;
        (guard.generation, guard.revision)
    };
    let config = crate::config_live_reload::current();
    let runtime_root = crate::config::runtime_root();
    let basis = shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|s| s.channel_name.clone());
    let ports = Arc::new(LivePorts {
        #[cfg(test)]
        root: crate::services::tui_prompt_dedupe::binding_events::test_root(),
    });
    let result = collect(shared, provider, channel, ports).await;
    let (mut answer, snapshot, namespace) = match result {
        Ok((answer, snapshot, namespace)) => (answer, Some(snapshot), Some(namespace)),
        Err(_) => (
            observed(Activity::Unknown, "session_probe_failed"),
            None,
            None,
        ),
    };
    let stamp = Stamp {
        shared: Arc::downgrade(shared),
        channel,
        basis,
        config,
        runtime_root,
        generation,
        revision,
        snapshot,
        namespace,
    };
    if stamp.publish(&watch, || ()).is_none() {
        answer = observed(Activity::Unknown, "superseded");
    }
    (answer, stamp)
}
async fn collect(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    ports: Arc<LivePorts>,
) -> Result<(Observed, Snapshot, Observation), String> {
    let mut snapshot = Snapshot::collect(shared, provider, channel, ports.clone()).await?;
    let namespace = tokio::task::spawn_blocking(move || observe_inventory(channel.get()))
        .await
        .map_err(|e| e.to_string())?;
    let candidates = snapshot.files.candidates.clone();
    let mut names: Vec<_> = candidates
        .as_ref()
        .map_err(Clone::clone)?
        .names
        .iter()
        .map(|c| c.name.clone())
        .collect();
    if let Inventory::Sessions(live) = &namespace.inventory {
        names.extend(live.iter().cloned());
    }
    snapshot.owners = names
        .into_iter()
        .map(|name| {
            let owner = shared
                .tmux_watchers
                .owner_channel_for_tmux_session(&name)
                .map(|id| id.get());
            (name, owner)
        })
        .collect();
    let scope = snapshot.scope(shared);
    let verdict = decide_unbound(candidates, scope, &namespace.inventory, &snapshot.owners);
    tracing::debug!(channel = channel.get(), candidates = ?snapshot.files.candidates, inventory = ?namespace.inventory, ?verdict, "unbound activity observation");
    let next = Snapshot::collect(shared, provider, channel, ports).await?;
    if snapshot.runtime_root != next.runtime_root
        || snapshot.basis != next.basis
        || !Arc::ptr_eq(&snapshot.config, &next.config)
        || !snapshot.files.same(&next.files)
        || snapshot.exact != next.exact
        || snapshot.canonical != next.canonical
    {
        return Ok((
            observed(Activity::Unknown, "superseded"),
            snapshot,
            namespace,
        ));
    }
    let answer = match verdict {
        Verdict::Absent(_) => observed(Activity::Idle, "no_session"),
        Verdict::Hold("watcher_unbound") => observed(Activity::Unknown, "watcher_unbound"),
        Verdict::Hold("probe_failed") => observed(Activity::Unknown, "session_probe_failed"),
        Verdict::Hold(_) => observed(Activity::Unknown, "session_identity_unresolved"),
    };
    Ok((answer, snapshot, namespace))
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
