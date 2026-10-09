//! Gateway IO fixtures for the real writer host.
use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::{PostOutcome, SeenMessage};

#[derive(Default)]
pub(crate) struct Posts(Mutex<Vec<(u64, SeenMessage)>>);

impl Posts {
    pub(crate) fn to(&self, channel: u64) -> Vec<String> {
        let posts = locked(&self.0);
        let to = posts.iter().filter(|(c, _)| *c == channel);
        to.map(|(_, message)| message.content.clone()).collect()
    }
}

impl DiscordPort for Posts {
    fn bot_id(&self) -> u64 {
        42
    }

    fn post(
        &self,
        channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        let mut posts = locked(&self.0);
        let id = 101 + posts.len() as u64;
        let author_id = 42;
        let receipt = SeenMessage {
            id,
            author_id,
            content,
        };
        posts.push((channel, receipt.clone()));
        std::future::ready(PostOutcome::Created(receipt))
    }

    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        let posts = locked(&self.0);
        let page = posts.iter().filter(|(c, m)| *c == channel && m.id > after);
        std::future::ready(Ok(page.map(|(_, m)| m.clone()).collect()))
    }

    fn history_readable(&self, _: u64) -> bool {
        true
    }
}

pub(crate) struct AnyLease;

impl DeliveryLease for AnyLease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
        Some(())
    }
}

#[derive(Clone, Default)]
pub(crate) struct Alarms(pub(crate) Arc<Mutex<Vec<(u64, WriterAlarm)>>>);

impl AlarmSink for Alarms {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        locked(&self.0).push((channel, alarm));
    }
}

pub(crate) struct Startup {
    event: BindingEvent,
    notice: watch::Sender<u64>,
}

impl BindingEvents for Startup {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        let due = channel == self.event.channel_id && after < self.event.seq;
        Ok(due.then(|| self.event.clone()).into_iter().collect())
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

/// Reports `facts` for every channel (none blocking by default); the store's own checks and
/// the adoption still apply. `on_facts` runs once as the next facts are read.
pub(crate) struct TestHost {
    pub(crate) posts: Arc<Posts>,
    pub(crate) alarms: Alarms,
    sources: BTreeMap<u64, SourceId>,
    pub(crate) facts: Mutex<ActivationFacts>,
    pub(crate) on_facts: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Legacy's relay state for channels that already hold output; fails closed when unset.
    pub(crate) legacy: Mutex<Option<Arc<dyn LegacyView>>>,
    /// The tmux session each channel's binding names, `host-<channel>` when unset.
    pub(crate) sessions: Mutex<BTreeMap<u64, String>>,
    /// Legacy's custody of a channel as the gateway reads it, as an inflight row; none when unset.
    pub(crate) custody: Mutex<Option<fn(u64) -> bool>>,
    /// Legacy's mailbox work and watcher emission for every channel; idle by default.
    pub(crate) busy: std::sync::atomic::AtomicBool,
    pub(crate) relaying: std::sync::atomic::AtomicBool,
}

impl TestHost {
    pub(crate) fn new(sources: impl IntoIterator<Item = (u64, SourceId)>) -> Arc<Self> {
        Arc::new(Self {
            posts: Arc::default(),
            alarms: Alarms::default(),
            sources: sources.into_iter().collect(),
            facts: Mutex::default(),
            on_facts: Mutex::default(),
            legacy: Mutex::default(),
            sessions: Mutex::default(),
            custody: Mutex::default(),
            busy: Default::default(),
            relaying: Default::default(),
        })
    }
}

impl HostIo for TestHost {
    type Port = Posts;
    type Lease = AnyLease;
    type Alarms = Alarms;
    type Bindings = Startup;

    fn port(&self) -> impl Future<Output = Arc<Posts>> + Send {
        std::future::ready(Arc::clone(&self.posts))
    }

    fn lease(&self) -> AnyLease {
        AnyLease
    }

    fn alarms(&self) -> Alarms {
        self.alarms.clone()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Startup> {
        let source = self.sources.get(&channel).cloned();
        let source = source.unwrap_or_else(|| panic!("no source for channel {channel}"));
        let received_at = chrono::Utc::now();
        let evidence = BindingEvidence {
            hook_event: "SessionStart".into(),
            received_at,
            reclaims: false,
        };
        let record = BindingRecord::Bound {
            old: None,
            new: BindingTarget::Source(source),
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence,
        };
        let event = BindingEvent {
            seq: 1,
            channel_id: channel,
            provider,
            tmux_session: locked(&self.sessions)
                .get(&channel)
                .cloned()
                .unwrap_or_else(|| format!("host-{channel}")),
            execution_nonce: "host".into(),
            record,
            committed_at: received_at,
        };
        let notice = watch::channel(1).0;
        Arc::new(Startup { event, notice })
    }

    fn activation_facts(
        &self,
        _: u64,
        _: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        if let Some(hook) = locked(&self.on_facts).take() {
            hook();
        }
        std::future::ready(Ok(locked(&self.facts).clone()))
    }

    fn local_custody(&self, channel: u64, _: ShadowProvider) -> Result<Custody, String> {
        let custody = *locked(&self.custody);
        let row = custody.is_some_and(|custody| custody(channel));
        Ok(if row { Custody::Row } else { Custody::Free })
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        let set = locked(&self.legacy).clone();
        set.unwrap_or_else(|| Arc::new(crate::services::tui_o::writer::adoption::NoLegacy))
    }

    fn legacy_busy(&self, _: u64) -> impl Future<Output = bool> + Send {
        std::future::ready(self.busy.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn relaying(&self, _: u64) -> bool {
        self.relaying.load(std::sync::atomic::Ordering::SeqCst)
    }
}
