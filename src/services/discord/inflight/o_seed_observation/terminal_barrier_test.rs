use super::*;
struct TerminalGate {
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

static TERMINAL_GATES: LazyLock<Mutex<HashMap<Key, std::sync::Arc<TerminalGate>>>> =
    LazyLock::new(Mutex::default);

pub(in crate::services::discord) struct TerminalBarrier {
    key: Key,
    gate: std::sync::Arc<TerminalGate>,
}

impl TerminalBarrier {
    pub(in crate::services::discord) fn arm(provider: &ProviderKind, channel: u64) -> Self {
        let key = (provider.as_str().to_owned(), channel);
        let gate = std::sync::Arc::new(TerminalGate {
            entered: tokio::sync::Notify::new(),
            released: tokio::sync::Notify::new(),
        });
        assert!(
            TERMINAL_GATES
                .lock()
                .unwrap()
                .insert(key.clone(), gate.clone())
                .is_none()
        );
        Self { key, gate }
    }
    pub(in crate::services::discord) async fn wait(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.gate.entered.notified(),
        )
        .await
        .expect("terminal checkpoint barrier");
    }
    pub(in crate::services::discord) fn release(&self) {
        self.gate.released.notify_one();
    }
}
impl Drop for TerminalBarrier {
    fn drop(&mut self) {
        TERMINAL_GATES.lock().unwrap().remove(&self.key);
        self.release();
    }
}
pub(in crate::services::discord) async fn terminal_checkpoint(
    provider: &ProviderKind,
    channel: u64,
) {
    record_event(provider, channel, "parsed_terminal");
    let gate = TERMINAL_GATES
        .lock()
        .unwrap()
        .get(&(provider.as_str().to_owned(), channel))
        .cloned();
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.released.notified().await;
    }
}
