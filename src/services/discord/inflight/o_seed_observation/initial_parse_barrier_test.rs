use super::*;

struct InitialParseGate {
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

static INITIAL_PARSE_GATES: LazyLock<Mutex<HashMap<Key, std::sync::Arc<InitialParseGate>>>> =
    LazyLock::new(Mutex::default);

pub(in crate::services::discord) struct InitialParseBarrier {
    key: Key,
    gate: std::sync::Arc<InitialParseGate>,
}

impl InitialParseBarrier {
    pub(in crate::services::discord) fn arm(provider: &ProviderKind, channel: u64) -> Self {
        let key = (provider.as_str().to_owned(), channel);
        let gate = std::sync::Arc::new(InitialParseGate {
            entered: tokio::sync::Notify::new(),
            released: tokio::sync::Notify::new(),
        });
        assert!(
            INITIAL_PARSE_GATES
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
        .expect("initial nonterminal parse barrier");
    }

    pub(in crate::services::discord) fn release(&self) {
        self.gate.released.notify_one();
    }
}

impl Drop for InitialParseBarrier {
    fn drop(&mut self) {
        INITIAL_PARSE_GATES.lock().unwrap().remove(&self.key);
        self.release();
    }
}

pub(in crate::services::discord) async fn initial_parse_checkpoint(
    provider: &ProviderKind,
    channel: u64,
) {
    record_event(provider, channel, "parsed_nonterminal");
    let gate = INITIAL_PARSE_GATES
        .lock()
        .unwrap()
        .get(&(provider.as_str().to_owned(), channel))
        .cloned();
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.released.notified().await;
    }
}
