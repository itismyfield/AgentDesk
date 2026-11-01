//! Boot evidence is independent of output authority; failed observation cannot authorize a send.
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

mod history;
mod manifest;
#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
mod writer;
#[cfg(test)]
pub(crate) use tests::mutant;

type Shared = Arc<Mutex<State>>;

struct State {
    header: Value,
    ready: bool,
    invalid: bool,
    reasons: Vec<String>,
}

impl State {
    fn new(boot: &crate::services::process_boot::Boot) -> Self {
        let probes = manifest::snapshot();
        Self {
            header: json!({"boot_id":boot.id,"started_at_ms":boot.started_at_ms,
                "census_hash":probes["census_hash"],"probe_coverage":probes["coverage"],
                "pending_probes":probes["pending"]}),
            ready: false,
            invalid: false,
            reasons: Vec::new(),
        }
    }

    fn mark(&mut self, reason: &str, invalid: bool) {
        self.invalid |= invalid;
        if !self.reasons.iter().any(|old| old == reason) {
            self.reasons.push(reason.into());
        }
    }
}

fn note(state: &Shared, reason: &str) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mark(reason, false);
}

fn invalidate(state: &Shared, reason: &str) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mark(reason, true);
}

fn reasons(state: &Shared) -> Vec<String> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reasons
        .clone()
}

pub(crate) struct Journal(writer::Writer);

impl Journal {
    pub(crate) async fn append(&self, record: Value) -> bool {
        self.0.append(record).await
    }

    pub(crate) fn snapshot(&self) -> Value {
        let state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut value = state.header.clone();
        let complete = value["probe_coverage"] == true;
        value["status"] =
            json!(
                if state.ready && !state.invalid && state.reasons.is_empty() && complete {
                    "READY"
                } else {
                    "INCONCLUSIVE"
                }
            );
        value["journal_ready"] = json!(state.ready);
        value["journal_invalid"] = json!(state.invalid);
        value["reasons"] = json!(state.reasons);
        value
    }

    #[cfg(test)]
    async fn open(
        root: &std::path::Path,
        boot: &crate::services::process_boot::Boot,
        doctor: Option<String>,
        timeout: Duration,
        before_write: writer::Hook,
    ) -> Self {
        let journal = Self(writer::Writer::spawn(
            Some(root.into()),
            boot.clone(),
            doctor,
            timeout,
            before_write,
        ));
        journal.append(json!({"kind":"boot_ready"})).await;
        journal
    }
}

static JOURNAL: OnceLock<Journal> = OnceLock::new();
static READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

pub(crate) fn start() {
    JOURNAL.get_or_init(|| {
        let root: Option<PathBuf> = crate::config::runtime_root();
        Journal(writer::Writer::spawn(
            root,
            crate::services::process_boot::initialize().clone(),
            None,
            Duration::from_secs(2),
            Arc::new(|| {}),
        ))
    });
}

pub(crate) async fn ensure_ready() {
    start();
    READY
        .get_or_init(|| async {
            if let Some(journal) = JOURNAL.get() {
                journal.append(json!({"kind":"boot_ready"})).await;
            }
        })
        .await;
}

pub(crate) fn health_json() -> Value {
    let mut value = JOURNAL.get().map_or_else(
        || {
            let probes = manifest::snapshot();
            json!({"status":"INCONCLUSIVE","journal_ready":false,"journal_invalid":false,
            "boot_id":crate::services::process_boot::initialize().id,
            "probe_coverage":probes["coverage"],"census_hash":probes["census_hash"],
            "pending_probes":probes["pending"],"reasons":["JOURNAL_NOT_STARTED"]})
        },
        Journal::snapshot,
    );
    value["uptime_ms"] = json!(
        crate::services::process_boot::initialize()
            .monotonic
            .elapsed()
            .as_millis()
    );
    value
}
