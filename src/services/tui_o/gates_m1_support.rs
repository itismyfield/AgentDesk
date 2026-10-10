use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm,
};

#[derive(Default)]
pub(super) struct TestPort {
    posts: Mutex<Vec<String>>,
}

impl TestPort {
    pub(super) fn posts(&self) -> Vec<String> {
        self.posts.lock().unwrap().clone()
    }
}

impl DiscordPort for TestPort {
    fn bot_id(&self) -> u64 {
        42
    }

    fn post(
        &self,
        _channel: u64,
        content: String,
    ) -> impl std::future::Future<Output = PostOutcome> + Send + 'static {
        let mut posts = self.posts.lock().unwrap();
        posts.push(content.clone());
        let id = 10_000 + posts.len() as u64;
        let author_id = self.bot_id();
        async move {
            PostOutcome::Created(SeenMessage {
                id,
                author_id,
                content,
            })
        }
    }

    async fn history_after(&self, _channel: u64, _after: u64) -> Result<Vec<SeenMessage>, String> {
        Ok(Vec::new())
    }

    fn history_readable(&self, _channel: u64) -> bool {
        true
    }
}

pub(super) struct TestLease;

impl DeliveryLease for TestLease {
    type Held = ();

    fn try_acquire(&self, _channel: u64, _serial: u64) -> Option<()> {
        Some(())
    }
}

pub(super) struct TestAlarms;

impl AlarmSink for TestAlarms {
    fn raise(&self, _channel: u64, alarm: WriterAlarm) {
        panic!("unexpected writer alarm: {alarm:?}");
    }
}

pub(super) type TestWriter = ChannelWriter<TestPort, TestLease, TestAlarms>;

pub(super) fn writer(store: ChannelStore) -> (TestWriter, Arc<TestPort>) {
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let port = Arc::new(TestPort::default());
    let writer = ChannelWriter::new(store, gate, port.clone(), TestLease, TestAlarms);
    (writer, port)
}

pub(super) struct EmptyBindings;

impl BindingEvents for EmptyBindings {
    fn binding_events_since(
        &self,
        _channel: u64,
        _after: u64,
    ) -> Result<Vec<BindingEvent>, String> {
        Ok(Vec::new())
    }

    fn subscribe(&self, _channel: u64) -> watch::Receiver<u64> {
        watch::channel(0).1
    }
}
