use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use super::*;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone)]
struct CapturingWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn identity_mismatch_bridge_mirror_warns_about_lost_delivery_evidence() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(CapturingWriter {
            buffer: buffer.clone(),
        })
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        warn_if_bridge_terminal_delivery_evidence_lost(
            crate::services::discord::inflight::GuardedSaveOutcome::IdentityMismatch,
            &ProviderKind::Claude,
            ChannelId::new(5025),
            MessageId::new(5026),
            128,
        );
    });
    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();

    assert!(
        logs.contains(
            "turn bridge delivered the terminal answer but could not mirror terminal_delivery_committed"
        ),
        "logs={logs}"
    );
}
