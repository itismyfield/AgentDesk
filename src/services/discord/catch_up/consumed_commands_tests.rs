//! The consumed-command record is bounded: past the cap its oldest entries go,
//! with a warning, so those commands lose their no-replay guarantee.

use super::super::consumed_commands::{self, MAX_CONSUMED_PER_CHANNEL};
use super::*;

#[derive(Clone)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_record_drops_its_oldest_commands_past_the_cap_with_a_warning() {
    let _root = scoped_runtime_root();
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(4_655_401);
    let ids: Vec<MessageId> = (1..=MAX_CONSUMED_PER_CHANNEL as u64 + 1)
        .map(MessageId::new)
        .collect();
    let (oldest, newest) = (ids[0], ids[ids.len() - 1]);
    for id in &ids[..ids.len() - 1] {
        consumed_commands::record(&provider, channel_id, *id).await;
    }
    let full = consumed_commands::read(&provider, channel_id);
    assert!(
        full.settles("!stop", oldest.get()),
        "the cap holds exactly its size"
    );

    let buffer = Captured(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(buffer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    {
        let _default = tracing::subscriber::set_default(subscriber);
        consumed_commands::record(&provider, channel_id, newest).await;
    }

    let record = consumed_commands::read(&provider, channel_id);
    assert!(
        !record.settles("!stop", oldest.get()),
        "the oldest command is dropped"
    );
    assert!(record.settles("!stop", ids[1].get()));
    assert!(record.settles("!stop", newest.get()));
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("consumed-command record full") && logs.contains(&oldest.get().to_string()),
        "{logs}"
    );
}
