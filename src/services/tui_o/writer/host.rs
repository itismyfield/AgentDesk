//! Starts the O writer on the gateway runtime: one actor per channel the boot snapshot hands to
//! O, creating a new channel's first store. A channel is ready only while its actor has resumed
//! and the gateway is Owned.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::activation::{self, ActivationFacts};
use super::actor;
use super::binding::BindingEvents;
use super::deliver::ChannelWriter;
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm, WriterConfig};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::cutover;
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::{ChannelStore, OStore, StoreConfig};

/// What the gateway runtime supplies; asked only for channels O owns.
pub trait HostIo: Send + Sync + 'static {
    type Port: DiscordPort;
    type Lease: DeliveryLease + 'static;
    type Alarms: AlarmSink + Clone + 'static;
    type Bindings: BindingEvents;
    /// Resolves once the gateway's HTTP client and the bot's own id are known.
    fn port(&self) -> impl Future<Output = Arc<Self::Port>> + Send;
    fn lease(&self) -> Self::Lease;
    fn alarms(&self) -> Self::Alarms;
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Self::Bindings>;
    /// Asked only for a channel with no store yet, before its first `init`, while the gate is Owned.
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send;
}

/// Channels with a hosted actor and those ready to take work.
#[derive(Default)]
pub struct Readiness {
    hosted: Mutex<BTreeSet<u64>>,
    ready: Mutex<BTreeSet<u64>>,
    live: Mutex<BTreeMap<u64, Live>>,
}

/// What a hosted channel's readiness is derived from, kept so intake can read it directly.
struct Live {
    gate: Arc<OwnershipGate>,
    resumed: watch::Receiver<bool>,
}

fn locked<T>(set: &Mutex<T>) -> MutexGuard<'_, T> {
    set.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Readiness {
    pub fn is_ready(&self, channel: u64) -> bool {
        locked(&self.ready).contains(&channel)
    }

    fn set(&self, channel: u64, ready: bool) {
        let mut set = locked(&self.ready);
        if ready {
            set.insert(channel);
        } else {
            set.remove(&channel);
        }
    }

    /// Only the first claim of a channel hosts it, so a channel never has two actors.
    fn claim(&self, channel: u64) -> bool {
        locked(&self.hosted).insert(channel)
    }

    fn track(&self, channel: u64, gate: Arc<OwnershipGate>, resumed: watch::Receiver<bool>) {
        locked(&self.live).insert(channel, Live { gate, resumed });
    }

    /// Ready and, read now rather than from the published flag that trails them, the gate is Owned
    /// and the actor is still running resumed.
    pub fn accepts(&self, channel: u64) -> bool {
        let live = locked(&self.live);
        let Some(live) = live.get(&channel).filter(|_| self.is_ready(channel)) else {
            return false;
        };
        let owned = matches!(live.gate.current(), GatewayOwnership::Owned { .. });
        owned && live.resumed.has_changed().is_ok() && *live.resumed.borrow()
    }
}

static PROCESS: LazyLock<Arc<Readiness>> = LazyLock::new(Arc::default);

pub(crate) fn process_readiness() -> Arc<Readiness> {
    Arc::clone(&PROCESS)
}

/// Whether this process's writer can take work for `channel`; false for any channel O does not own.
pub(crate) fn channel_accepts(channel: u64) -> bool {
    PROCESS.accepts(channel)
}

/// What hosting needs once a channel is owned; built only then, so an off or empty writer takes nothing.
pub struct HostParts<I> {
    pub io: Arc<I>,
    pub runtime_root: Option<PathBuf>,
    pub gate: Arc<OwnershipGate>,
    pub readiness: Arc<Readiness>,
}

/// Spawns one host task per channel this provider's bot owns. Without a PG gateway lease the gate
/// never becomes Owned, so those channels are held with an alarm and get no actor.
pub fn start<I: HostIo>(
    provider: ShadowProvider,
    pg_gateway: bool,
    prepare: impl FnOnce() -> HostParts<I>,
) -> Vec<JoinHandle<()>> {
    let kind = match provider {
        ShadowProvider::Claude => RuntimeHandoffKind::ClaudeTui,
        ShadowProvider::Codex => RuntimeHandoffKind::CodexTui,
    };
    let ours = |&(_, channel_kind, owned): &(u64, _, bool)| owned && channel_kind == Some(kind);
    let channels: Vec<_> = cutover::boot_ownership().into_iter().filter(ours).collect();
    if channels.is_empty() {
        return Vec::new();
    }
    let HostParts {
        io,
        runtime_root,
        gate,
        readiness,
    } = prepare();
    let mut tasks = Vec::new();
    for (channel, _, owned) in channels {
        if !readiness.claim(channel) {
            continue;
        }
        let root = match (pg_gateway, &runtime_root) {
            (false, _) => Err("no PG gateway lease"),
            (true, None) => Err("runtime root unresolved"),
            (true, Some(root)) => Ok(root.clone()),
        };
        let root = match root {
            Ok(root) => root,
            Err(detail) => {
                hold(&io.alarms(), channel, detail);
                continue;
            }
        };
        let (gate, readiness) = (Arc::clone(&gate), Arc::clone(&readiness));
        let host = host_channel(
            Arc::clone(&io),
            channel,
            owned,
            provider,
            root,
            gate,
            readiness,
        );
        tasks.push(tokio::spawn(host));
    }
    tasks
}

fn hold(alarms: &impl AlarmSink, channel: u64, detail: &str) {
    tracing::error!(channel, detail, "[tui_o] writer host held the channel");
    let detail = format!("writer host: {detail}");
    alarms.raise(channel, WriterAlarm::Halted { detail });
}

async fn host_channel<I: HostIo>(
    io: Arc<I>,
    channel: u64,
    owned: bool,
    provider: ShadowProvider,
    runtime_root: PathBuf,
    gate: Arc<OwnershipGate>,
    readiness: Arc<Readiness>,
) {
    let alarms = io.alarms();
    let mut bindings = None;
    let store = match recover(&runtime_root, channel, owned) {
        Ok(Recovered::Store(store)) => store,
        Ok(Recovered::Fresh(fresh)) => {
            let log = bindings.insert(io.bindings(channel, provider));
            let created = loop {
                until_owned(&gate).await;
                let facts = io.activation_facts(channel, provider).await;
                // Facts read before a lost gate are not acted on; wait for Owned and read again.
                if !matches!(gate.current(), GatewayOwnership::Owned { .. }) {
                    continue;
                }
                break facts
                    .and_then(|facts| activation::activate(&fresh, channel, &facts, &**log));
            };
            if let Err(detail) = created {
                return hold(&alarms, channel, &format!("first activation: {detail}"));
            }
            match recover(&runtime_root, channel, owned) {
                Ok(Recovered::Store(store)) => store,
                Ok(Recovered::Fresh(_)) => {
                    return hold(&alarms, channel, "init missing after activation");
                }
                Err(detail) => return hold(&alarms, channel, &detail),
            }
        }
        Err(detail) => return hold(&alarms, channel, &detail),
    };
    let port = io.port().await;
    let writer = ChannelWriter::new(store, Arc::clone(&gate), port, io.lease(), alarms);
    let (stop_tx, stop) = watch::channel(false);
    let (resumed_tx, resumed) = watch::channel(false);
    let config = WriterConfig { enabled: owned };
    let bindings = bindings.unwrap_or_else(|| io.bindings(channel, provider));
    let spawned = actor::spawn_if_enabled(&config, writer, provider, bindings, stop, resumed_tx);
    let Some(actor) = spawned else { return };
    readiness.track(channel, Arc::clone(&gate), resumed.clone());
    publish(channel, &readiness, gate.subscribe(), resumed, actor).await;
    drop(stop_tx);
}

/// Returns once the gate is Owned; a gate not yet acquired at startup is waited on, not held.
async fn until_owned(gate: &OwnershipGate) {
    let mut watch = gate.subscribe();
    while !matches!(*watch.borrow_and_update(), GatewayOwnership::Owned { .. }) {
        if watch.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

enum Recovered {
    Store(ChannelStore),
    /// Neither the era nor an `init` names the channel: only a first activation may create it.
    Fresh(OStore),
}

/// Recovers the channel's store. Damage, a foreign init, an era channel without init or an init
/// without era holds it.
fn recover(runtime_root: &Path, channel: u64, owned: bool) -> Result<Recovered, String> {
    let config = StoreConfig { enabled: owned };
    let store = OStore::open_if_enabled(&config, runtime_root)
        .map_err(|error| format!("store: {error}"))?
        .ok_or_else(|| "store disabled".to_string())?;
    let era = store
        .read_era()
        .map_err(|error| format!("era: {error:?}"))?;
    let Some(era) = era else {
        return match store.read_init(channel) {
            Ok(None) => Ok(Recovered::Fresh(store)),
            Ok(Some(_)) => Err("no writer era".into()),
            Err(error) => Err(format!("init: {error:?}")),
        };
    };
    let opened = store.open_channel(&era, channel);
    let opened = opened.map_err(|halt| format!("recovery: {halt:?}"))?;
    let Some(opened) = opened else {
        return Ok(Recovered::Fresh(store));
    };
    match opened.init().channel {
        stored if stored != channel => Err(format!("store names channel {stored}")),
        _ => Ok(Recovered::Store(opened)),
    }
}

/// Ready only while the actor has resumed and the gate is Owned; cleared once the actor ends.
async fn publish(
    channel: u64,
    readiness: &Readiness,
    mut gate: watch::Receiver<GatewayOwnership>,
    mut resumed: watch::Receiver<bool>,
    mut actor: JoinHandle<()>,
) {
    loop {
        let owned = matches!(*gate.borrow_and_update(), GatewayOwnership::Owned { .. });
        readiness.set(channel, owned && *resumed.borrow_and_update());
        tokio::select! {
            changed = gate.changed() => if changed.is_err() { break },
            changed = resumed.changed() => if changed.is_err() { break },
            _ = &mut actor => break,
        }
    }
    readiness.set(channel, false);
}
