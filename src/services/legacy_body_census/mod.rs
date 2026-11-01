//! Boot evidence is independent of output authority; failed observation cannot authorize a send.
use serde_json::{Value, json};
#[cfg(test)]
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

mod history;
mod manifest;
#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
mod writer;

type Shared = Arc<Mutex<State>>;

struct State {
    header: Value,
    ready: bool,
    invalid: bool,
    own_reasons: Vec<String>,
    history_problems: Vec<Value>,
}

impl State {
    fn new(boot: &crate::services::process_boot::Boot, host: Option<String>) -> Self {
        let probes = manifest::snapshot();
        let mut state = Self {
            header: json!({"boot_id":boot.id,"host":host,"started_at_ms":boot.started_at_ms,
                "census_hash":probes["census_hash"],"probe_coverage":probes["coverage"],
                "pending_probes":probes["pending"],"manifest_error":probes["error"]}),
            ready: false,
            invalid: false,
            own_reasons: Vec::new(),
            history_problems: Vec::new(),
        };
        if state.header["host"]
            .as_str()
            .is_none_or(|host| host.trim().is_empty())
        {
            state.mark("HOST_IDENTITY_UNKNOWN");
        }
        if probes["error"].is_string() {
            state.mark("MANIFEST_INVALID");
        }
        state
    }

    fn mark(&mut self, reason: &str) {
        self.invalid = true;
        if !self.own_reasons.iter().any(|old| old == reason) {
            self.own_reasons.push(reason.into());
        }
    }

    fn reasons(&self) -> Vec<String> {
        let mut reasons = self.own_reasons.clone();
        for problem in &self.history_problems {
            if let Some(reason) = problem["reason"].as_str()
                && !reasons.iter().any(|old| old == reason)
            {
                reasons.push(reason.into());
            }
        }
        reasons
    }
}

fn invalidate(state: &Shared, reason: &str) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mark(reason);
}

fn inherited(state: &Shared, boot: Option<&str>, reason: &str) {
    let problem = json!({"boot_id":boot,"reason":reason});
    let mut status = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !status.history_problems.contains(&problem) {
        status.history_problems.push(problem);
    }
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
        let reasons = state.reasons();
        value["status"] = json!(if state.ready
            && !state.invalid
            && reasons.is_empty()
            && value["probe_coverage"] == true
        {
            "READY"
        } else {
            "INCONCLUSIVE"
        });
        value["journal_ready"] = json!(state.ready);
        value["journal_invalid"] = json!(state.invalid);
        value["reasons"] = json!(reasons);
        value["history_problems"] = json!(state.history_problems);
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
            writer::Options {
                doctor,
                host: Some("test-node".into()),
                timeout,
                before_write,
                ..Default::default()
            },
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
            Default::default(),
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

pub(crate) async fn record_selection(config: &crate::config::Config, installed: bool) {
    use crate::services::tui_o::channel_policy;
    let writer = &channel_policy::tui_o_of(config).writer;
    let record = json!({"kind":"selection", "installed":installed,
        "host":config.cluster.instance_id,
        "requested":{"all_tui":writer.all_tui,"channels":writer.channels},
        "effective":channel_policy::boot().filter(|_| installed).map(|boot| boot.selected()),
        "dormant":channel_policy::boot().filter(|_| installed).map(|boot| boot.dormant_channels())});
    if let Some(journal) = JOURNAL.get() {
        journal.append(record).await;
    }
}

pub(crate) fn health_json() -> Value {
    let mut value = JOURNAL.get().map_or_else(
        || {
            let probes = manifest::snapshot();
            json!({"status":"INCONCLUSIVE","journal_ready":false,"journal_invalid":false,
            "boot_id":crate::services::process_boot::initialize().id,
            "probe_coverage":probes["coverage"],"census_hash":probes["census_hash"],
            "pending_probes":probes["pending"],"manifest_error":probes["error"],
            "reasons":["JOURNAL_NOT_STARTED"]})
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
