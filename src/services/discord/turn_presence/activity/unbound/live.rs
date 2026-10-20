//! Concrete Unbound reads and their publication identity; no start or cleanup authority.
use super::super::{Activity, LivePorts, Observed, Watch, lock, observed};
use super::hold_leaf::{self, HoldLeaf};
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
    let runtime = {
        let extension = tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT;
        let canonical = marker_path(name, extension);
        let legacy =
            std::path::PathBuf::from(tmux_common::legacy_tmp_session_path(name, extension));
        let read = std::fs::read_to_string(&canonical).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound && canonical != legacy {
                std::fs::read_to_string(&legacy)
            } else {
                Err(error)
            }
        });
        match read {
            Ok(raw) => RuntimeHandoffKind::from_str(&raw)
                .map(Some)
                .ok_or_else(|| "unrecognized runtime marker".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
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
    bindings: Vec<BindingEvent>,
    candidates: Result<Candidates, String>,
    row: Result<Option<InflightTurnState>, String>,
    row_identity: String,
    seq: u64,
    seq_watch: tokio::sync::watch::Receiver<u64>,
    traces: BTreeMap<String, LocalTrace>,
    recovered: Option<String>,
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
            let bindings = log.binding_events_since(channel, 0)?;
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
                bindings,
                candidates,
                row,
                row_identity,
                seq,
                seq_watch,
                traces,
                recovered: None,
            })
        })
    }
    fn same(&self, other: &Self) -> bool {
        self.fallback == other.fallback
            && self.bindings == other.bindings
            && self.candidates == other.candidates
            && self.row_identity == other.row_identity
            && self.seq == other.seq
            && self.traces == other.traces
            && self.recovered == other.recovered
    }

    fn binding_only(&self) -> bool {
        matches!(self.row, Ok(None))
            && self.candidates.as_ref().is_ok_and(|candidates| {
                candidates.current.is_none()
                    && !candidates.conflict
                    && !candidates.inflight
                    && !candidates.names.is_empty()
                    && candidates
                        .names
                        .iter()
                        .all(|c| matches!(c.origin, Origin::Binding(_)))
            })
    }

    fn recover(&mut self, name: &str) -> Result<(), String> {
        if !self.binding_only() {
            return Err("canonical recovery inputs changed".into());
        }
        let candidates = self.candidates.as_mut().map_err(|error| error.clone())?;
        candidates.current = Some(name.into());
        candidates.names.push(Candidate {
            name: name.into(),
            origin: Origin::Canonical,
        });
        self.traces.insert(name.into(), local_trace(name));
        self.recovered = Some(name.into());
        Ok(())
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
    canonical_target: Option<Result<canonical_target::CanonicalTarget, String>>,
    owners: BTreeMap<String, Option<u64>>,
    ownership: Option<ownership::OwnerInputs>,
    ports: Arc<LivePorts>,
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
        let mut files = tokio::task::spawn_blocking(move || {
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
        #[cfg(test)]
        review_tests::after_files(channel.get()).await;
        let pool = shared.pg_pool.as_ref().ok_or("database unavailable")?;
        let canonical_target = if files.binding_only()
            && local_registration(&config, channel.get(), provider)
        {
            Some(canonical_target::collect(pool, provider, &shared.token_hash, channel.get()).await)
        } else {
            None
        };
        if let Some(Ok(target)) = &canonical_target {
            files.recover(&target.name)?;
        }
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
            canonical_target,
            owners: BTreeMap::new(),
            ownership: None,
            ports,
        })
    }
    async fn collect_pair(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        ports: Arc<LivePorts>,
    ) -> Result<(Self, Observation, Self), String> {
        let membership = ownership::Membership::collect(shared).await?;
        let epoch = ownership::epoch()?;
        let mut before = Self::collect(shared, provider, channel, ports.clone()).await?;
        let inventory = tokio::task::spawn_blocking(move || observe_inventory(channel.get()))
            .await
            .map_err(|e| e.to_string())?;
        let mut names: Vec<_> = before
            .files
            .candidates
            .as_ref()
            .map_err(Clone::clone)?
            .names
            .iter()
            .map(|c| c.name.clone())
            .collect();
        if let Inventory::Sessions(live) = &inventory.inventory {
            names.extend(live.iter().cloned());
        }
        names.sort();
        names.dedup();
        let first =
            ownership::OwnerInputs::collect(shared, membership.clone(), epoch, &names).await?;
        before.owners = first.effective_owners(channel.get());
        before.ownership = Some(first);
        let mut after = Self::collect(shared, provider, channel, ports).await?;
        let next = ownership::OwnerInputs::collect(shared, membership, epoch, &names).await?;
        after.owners = next.effective_owners(channel.get());
        after.ownership = Some(next);
        Ok((before, inventory, after))
    }
    fn same_inputs(&self, other: &Self) -> bool {
        self.runtime_root == other.runtime_root
            && self.basis == other.basis
            && Arc::ptr_eq(&self.config, &other.config)
            && self.files.same(&other.files)
            && self.exact == other.exact
            && self.canonical == other.canonical
            && self.canonical_target == other.canonical_target
            && match (&self.ownership, &other.ownership) {
                (Some(old), Some(new)) => old.same(new),
                (None, None) => true,
                _ => false,
            }
    }
    fn verdict(&self, shared: &SharedData, inventory: &Inventory) -> Verdict {
        let candidates = self.files.candidates.clone();
        let inventory_names = match inventory {
            Inventory::Sessions(names) => names.as_slice(),
            Inventory::Empty(_) | Inventory::ProbeFailed(_) => &[],
        };
        let conflict = self.ownership.as_ref().is_some_and(|owners| {
            inventory_names
                .iter()
                .map(String::as_str)
                .chain(
                    candidates
                        .iter()
                        .flat_map(|c| c.names.iter().map(|c| c.name.as_str())),
                )
                .any(|name| owners.local_conflict(name))
        });
        let verdict = if conflict {
            // Conflicting exact claims keep the existing watcher Hold for every consumer.
            Verdict::Hold("watcher_unbound")
        } else {
            decide_unbound(
                candidates,
                self.scope(shared),
                inventory,
                &self.effective_owners(),
            )
        };
        if matches!(verdict, Verdict::Absent(_))
            && self.files.recovered.is_some()
            && !self.ownership.as_ref().is_some_and(|owners| {
                self.files
                    .candidates
                    .as_ref()
                    .is_ok_and(|c| owners.nonces_valid(c.names.iter().map(|c| c.name.as_str())))
            })
        {
            Verdict::Hold("probe_failed")
        } else {
            verdict
        }
    }
    fn effective_owners(&self) -> BTreeMap<String, Option<u64>> {
        self.owners.clone()
    }
    fn memory_current(&self) -> bool {
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
            && self
                .ownership
                .as_ref()
                .is_none_or(ownership::OwnerInputs::current)
    }
    async fn revalidate(&self, namespace: &Observation) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        if !self.memory_current() {
            return false;
        }
        let Ok((before, fresh, next)) =
            Self::collect_pair(&shared, &self.provider, self.channel, self.ports.clone()).await
        else {
            return false;
        };
        if !before.same_inputs(&next)
            || !self.same_inputs(&next)
            || !namespace.same_namespace(&fresh)
            || namespace.inventory != fresh.inventory
            || !fresh.current()
        {
            return false;
        }
        // Re-read files after the last DB/inventory await; final publication has no await.
        let Ok(mut files) = Files::collect(
            &self.provider,
            self.channel.get(),
            self.basis.as_deref(),
            &self.config,
            &self.ports,
        ) else {
            return false;
        };
        // Reattach the pinned target after rereading original candidates; no DB await follows.
        if let Some(name) = &self.files.recovered {
            if files.recover(name).is_err() {
                return false;
            }
        }
        self.files.same(&files) && self.memory_current()
    }
    fn notice(
        &self,
        shared: &SharedData,
        answer: Observed,
        hold: Option<&HoldLeaf>,
    ) -> super::super::notice_identity::NoticeObservation {
        use super::super::notice_identity::{ExecutionIdentity, NoticeObservation};
        let label = self
            .files
            .candidates
            .as_ref()
            .ok()
            .and_then(|c| c.current.as_deref());
        let mut notice = NoticeObservation::bound(answer.reason, label);
        notice.hold_leaf = hold.map(HoldLeaf::leaf);
        if let HostedLookup::Found(found) = &self.canonical
            && let HostedRecord::Known(record) = &found.record
        {
            let owner = &record.owner;
            let node = crate::config::session_hosts::local_node();
            let root = self.runtime_root.as_ref().map(|p| p.display().to_string());
            if local_owner(
                owner,
                &self.provider,
                self.channel.get(),
                &shared.token_hash,
                &owner.logical_key,
                node.as_deref(),
            ) && root.as_deref() == Some(owner.runtime_root.as_str())
                && label.is_none_or(|label| label == owner.logical_key)
                && (self.exact == self.canonical || self.exact == HostedLookup::Missing)
            {
                notice.identity = ExecutionIdentity::Nonce(record.execution_nonce.clone());
                notice.target_label = Some(owner.logical_key.clone());
            } else {
                notice.identity = ExecutionIdentity::Unresolved;
                notice.target_label = None;
            }
        }
        notice
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
                hold("current_unresolved")
            };
        };
        for (candidate, trace) in &self.files.traces {
            if candidate == name {
                continue;
            }
            if trace.runtime.is_err() || matches!(trace.marker, HostKindMarker::ReadFailed(_)) {
                return hold("probe_failed");
            }
            if trace.process.is_some()
                || !matches!(
                    trace.marker,
                    HostKindMarker::Absent | HostKindMarker::Known(HostKind::Tmux)
                )
                || matches!(trace.runtime, Ok(Some(RuntimeHandoffKind::ProcessBackend)))
            {
                return hold("foreign_candidate_trace");
            }
        }
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
            runtime_root: self
                .runtime_root
                .as_ref()
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
    superseded: bool,
    pub(in crate::services::discord::turn_presence::activity) notice:
        super::super::notice_identity::NoticeObservation,
}
#[path = "canonical_target.rs"]
mod canonical_target;
#[path = "live_ownership.rs"]
mod ownership;
#[path = "live_publication.rs"]
mod publication;

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
    let superseded = result
        .as_ref()
        .is_err_and(|error| ownership::superseded_error(error));
    let (mut answer, snapshot, namespace, hold) = match result {
        Ok((answer, snapshot, namespace, hold)) => (answer, Some(snapshot), Some(namespace), hold),
        Err(error) => (
            observed(
                Activity::Unknown,
                if ownership::superseded_error(&error) {
                    "superseded"
                } else {
                    "session_probe_failed"
                },
            ),
            None,
            None,
            None,
        ),
    };
    let notice = snapshot.as_ref().map_or_else(
        || super::super::notice_identity::NoticeObservation::bound(answer.reason, None),
        |snapshot| snapshot.notice(shared, answer, hold.as_ref()),
    );
    let stamp = Stamp {
        notice,
        shared: Arc::downgrade(shared),
        channel,
        basis,
        config,
        runtime_root,
        generation,
        revision,
        snapshot,
        namespace,
        superseded,
    };
    if stamp.publish(&watch, || ()).await.is_none() {
        answer = observed(Activity::Unknown, "superseded");
    }
    hold_leaf::settle(channel.get(), answer.reason, hold.as_ref());
    let inputs = stamp
        .snapshot
        .as_ref()
        .map(|s| (&s.files.candidates, s.scope(shared).result));
    tracing::debug!(channel_id = channel.get(), ?inputs, namespace = ?stamp.namespace.as_ref().map(|n| n.diagnostics()), inventory = ?stamp.namespace.as_ref().map(|n| &n.inventory), result = ?answer, "unbound activity observation");
    (answer, stamp)
}
async fn collect(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    ports: Arc<LivePorts>,
) -> Result<(Observed, Snapshot, Observation, Option<HoldLeaf>), String> {
    let (snapshot, namespace, next) =
        Snapshot::collect_pair(shared, provider, channel, ports).await?;
    let candidates = snapshot.files.candidates.clone();
    let owners = snapshot.effective_owners();
    let verdict = snapshot.verdict(shared, &namespace.inventory);
    if !snapshot.same_inputs(&next) {
        return Ok((
            observed(Activity::Unknown, "superseded"),
            snapshot,
            namespace,
            None,
        ));
    }
    let (exact, canonical) = (&snapshot.exact, &snapshot.canonical);
    let (answer, hold) = match verdict {
        Verdict::Absent(_) => (observed(Activity::Idle, "no_session"), None),
        Verdict::Hold("watcher_unbound") => (observed(Activity::Unknown, "watcher_unbound"), None),
        Verdict::Hold("probe_failed") => {
            (observed(Activity::Unknown, "session_probe_failed"), None)
        }
        Verdict::Hold(leaf) => (
            observed(Activity::Unknown, "session_identity_unresolved"),
            Some(HoldLeaf::of(
                leaf,
                &candidates,
                exact,
                canonical,
                &namespace.inventory,
                &owners,
            )),
        ),
    };
    Ok((answer, snapshot, namespace, hold))
}

#[cfg(test)]
#[path = "live_tests.rs"]
pub(in crate::services::discord::turn_presence::activity) mod tests;

#[cfg(test)]
#[path = "live_review_tests.rs"]
pub(super) mod review_tests;

#[cfg(test)]
#[path = "live_owner_tests.rs"]
mod owner_tests;

#[cfg(test)]
#[path = "stale_binding_tests.rs"]
mod stale_binding_tests;
