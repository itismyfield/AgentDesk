//! Dormant strict presence runtime: typing goes out only on the client whose bot id was read through
//! itself, after an approval's first poll under every fence. No production path builds one yet.

use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};

use async_trait::async_trait;
use poise::serenity_prelude::{ChannelId, Http};
use tokio::task::{AbortHandle, JoinHandle};

use super::activity::{Reading, presence_reading_now};
use super::entrypoints::{Fence, Installation, install};
use super::lifecycle::{Lifetime, Runtime, Shown, Ticket};
use super::supervisor::Presences;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

/// The Discord calls typing needs; the bot id is read through the same client that sends.
#[async_trait]
pub(in crate::services::discord) trait Transport:
    Send + Sync + 'static
{
    async fn bot_id(&self) -> Result<u64, String>;
    async fn typing(&self, channel: u64) -> Result<(), String>;
}

#[async_trait]
impl Transport for Http {
    async fn bot_id(&self) -> Result<u64, String> {
        let user = self.get_current_user().await;
        user.map(|user| user.id.get())
            .map_err(|error| error.to_string())
    }

    async fn typing(&self, channel: u64) -> Result<(), String> {
        let typing = ChannelId::new(channel).broadcast_typing(self);
        typing.await.map_err(|error| error.to_string())
    }
}

/// A client and the bot id read through it; no other client sends this runtime's typing.
pub(super) struct Client {
    transport: Arc<dyn Transport>,
    bot_id: u64,
}

impl Client {
    /// An unreadable or zero bot id leaves no client, and so no typing at all.
    pub(super) async fn verify(transport: Arc<dyn Transport>) -> Result<Self, String> {
        match transport.bot_id().await? {
            0 => Err("bot id unreadable".to_string()),
            bot_id => Ok(Self { transport, bot_id }),
        }
    }
}

pub(in crate::services::discord) struct Strict {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    client: Client,
    runtime: Arc<Runtime>,
    presences: Arc<Presences>,
}

/// Keeps a strict runtime open and installed; dropping it closes the runtime first, then takes down
/// what it still shows.
pub(super) struct Owner {
    lifetime: Option<Lifetime>,
    _installed: Installation,
    presences: Arc<Presences>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        drop(self.lifetime.take());
        self.presences.retire_all();
    }
}

impl Strict {
    /// Opens a runtime for `shared` and installs it behind the entry-point hooks.
    pub(super) fn open(
        shared: Arc<SharedData>,
        provider: ProviderKind,
        client: Client,
    ) -> (Arc<Self>, Owner) {
        let presences = Arc::new(Presences::default());
        let shown = presences.clone() as Arc<dyn Shown>;
        let runtime = Arc::new(Runtime::showing(Some(shown)));
        let lifetime = runtime.own();
        let installed = install(&shared, Arc::downgrade(&runtime) as Weak<dyn Fence>);
        let owner = Owner {
            lifetime: Some(lifetime),
            _installed: installed,
            presences: presences.clone(),
        };
        let strict = Self {
            shared,
            provider,
            client,
            runtime,
            presences,
        };
        (Arc::new(strict), owner)
    }

    pub(super) fn presences(&self) -> &Presences {
        &self.presences
    }

    /// A read under the channel's current registration, adopted for this client's bot.
    pub(super) async fn read(&self, channel: u64) -> Option<Reading> {
        let ticket = self.runtime.register(channel)?;
        let id = ChannelId::new(channel);
        let mut reading = presence_reading_now(&self.shared, &self.provider, id, &ticket).await?;
        if let Some(incarnation) = ticket.incarnation() {
            incarnation.adopt_reading(&mut reading, self.client.bot_id);
        }
        Some(reading)
    }

    /// The channel's current registration; none while the runtime is closed.
    pub(super) fn ticket(&self, channel: u64) -> Option<Ticket> {
        self.runtime.register(channel)
    }

    /// One send under `ticket`: a fresh reading approved for this client, first polled under the
    /// fences. `None`: refused before any request was polled.
    pub(super) async fn send(self: Arc<Self>, ticket: Ticket) -> Option<Result<(), String>> {
        let channel = ticket.channel();
        let id = ChannelId::new(channel);
        let reading = presence_reading_now(&self.shared, &self.provider, id, &ticket).await?;
        let approval = ticket.incarnation()?.approve(reading)?;
        let client = &self.client;
        let started = approval.start(client.bot_id, channel, || client.transport.typing(channel));
        Some(started.await?.finish().await)
    }

    /// Owns `work` under the channel's registration; nothing runs while the runtime is closed.
    pub(super) fn spawn(&self, channel: u64, work: impl Future<Output = ()> + Send + 'static) {
        if let Some(ticket) = self.runtime.register(channel) {
            ticket.spawn_child(work);
        }
    }

    pub(super) fn prune(&self) {
        self.runtime.prune();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A strict supervisor's task, keyed by the shared data it serves.
type Running = Vec<(Weak<SharedData>, AbortHandle)>;

/// Strict supervisors by the runtime they serve, so a second start closes the first.
static RUNNING: LazyLock<Mutex<Running>> = LazyLock::new(Default::default);

/// Until handed off, dropping this closes the runtime it started, even before its first poll.
#[must_use]
pub(in crate::services::discord) struct Boot(Option<JoinHandle<()>>);

impl Boot {
    /// From here on the runtime lives as long as its task.
    pub(in crate::services::discord) fn hand_off(mut self) {
        self.0.take();
    }
}

impl Drop for Boot {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// Starts a strict supervisor for `shared` once `transport` names its bot. An earlier one for the
/// same `shared` is closed before the new runtime is installed.
pub(in crate::services::discord) async fn spawn(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    transport: Arc<dyn Transport>,
) -> Result<Boot, String> {
    let client = Client::verify(transport).await?;
    super::entrypoints::suspend(&shared);
    lock(&RUNNING).retain(|(owner, task)| {
        let earlier = std::ptr::eq(owner.as_ptr(), Arc::as_ptr(&shared));
        if earlier {
            task.abort();
        }
        !earlier && owner.strong_count() > 0 && !task.is_finished()
    });
    let (strict, owner) = Strict::open(shared.clone(), provider.clone(), client);
    let run = super::supervisor::strict(shared.clone(), provider, strict);
    let task = tokio::spawn(async move {
        let _owner = owner;
        run.await;
    });
    lock(&RUNNING).push((Arc::downgrade(&shared), task.abort_handle()));
    Ok(Boot(Some(task)))
}
