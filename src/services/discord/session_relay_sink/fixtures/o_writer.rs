use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm, WriterConfig,
    actor,
};

#[derive(Default)]
struct FakePort(Arc<Mutex<Vec<SeenMessage>>>);

impl DiscordPort for FakePort {
    fn bot_id(&self) -> u64 {
        42
    }

    fn post(&self, _: u64, content: String) -> impl Future<Output = PostOutcome> + Send + 'static {
        let history = Arc::clone(&self.0);
        async move {
            let mut history = history.lock().unwrap();
            let receipt = SeenMessage {
                id: 101 + history.len() as u64,
                author_id: 42,
                content,
            };
            history.push(receipt.clone());
            PostOutcome::Created(receipt)
        }
    }

    fn history_after(
        &self,
        _: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        let history = self.0.lock().unwrap();
        let page = history.iter().filter(|m| m.id > after).cloned().collect();
        async move { Ok(page) }
    }

    fn history_readable(&self, _: u64) -> bool {
        true
    }
}

struct FakeLease;

impl DeliveryLease for FakeLease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<Self::Held> {
        Some(())
    }
}

#[derive(Clone, Default)]
struct Alarms(Arc<Mutex<Vec<WriterAlarm>>>);

impl AlarmSink for Alarms {
    fn raise(&self, _: u64, alarm: WriterAlarm) {
        self.0.lock().unwrap().push(alarm);
    }
}

struct NoBindings(watch::Sender<u64>);

impl BindingEvents for NoBindings {
    fn binding_events_since(&self, _: u64, _: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(Vec::new())
    }
    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}

pub(super) struct WriterFixture {
    store: OStore,
    gate: Arc<OwnershipGate>,
    port: Arc<FakePort>,
    alarms: Alarms,
    provider: ShadowProvider,
    channel: u64,
}

impl WriterFixture {
    pub(super) fn new(runtime: &Path, path: &Path, provider: ShadowProvider, channel: u64) -> Self {
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
        let config = StoreConfig { enabled: true };
        let store = OStore::open_if_enabled(&config, runtime).unwrap().unwrap();
        let source_id = source_id_for("e2e", path).unwrap();
        let source = InitSource {
            source_id,
            delivery_start: 0,
            prefix_hash: hex::encode(Sha256::digest(b"")),
        };
        store
            .begin_era(&[channel], Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![source.clone()],
                    initial_anchor: 100,
                    build_digest: "e2e-fixture".into(),
                    at: Utc::now(),
                })
            })
            .unwrap();
        Self {
            store,
            gate: Arc::new(OwnershipGate::default()),
            port: Arc::new(FakePort::default()),
            alarms: Alarms::default(),
            provider,
            channel,
        }
    }

    pub(super) fn start(&self) -> (watch::Sender<bool>, JoinHandle<()>) {
        let (stop, stopped) = watch::channel(false);
        let (gate, port, alarms) = (self.gate.clone(), self.port.clone(), self.alarms.clone());
        let writer = ChannelWriter::new(self.channel(), gate, port, FakeLease, alarms);
        let bindings = Arc::new(NoBindings(watch::channel(0).0));
        let config = WriterConfig { enabled: true };
        let task =
            actor::spawn_if_enabled(&config, writer, self.provider, bindings, stopped).unwrap();
        (stop, task)
    }

    pub(super) fn acquired(&self) {
        self.gate.acquired();
    }

    pub(super) fn posts(&self) -> Vec<String> {
        let history = self.port.0.lock().unwrap();
        history.iter().map(|m| m.content.clone()).collect()
    }

    pub(super) fn channel(&self) -> ChannelStore {
        let era = self.store.read_era().unwrap().unwrap();
        self.store
            .open_channel(&era, self.channel)
            .unwrap()
            .unwrap()
    }
}
