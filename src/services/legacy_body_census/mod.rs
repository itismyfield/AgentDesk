//! Boot evidence is independent of output authority; failed observation cannot authorize a send.
use serde_json::{Value, json};
#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

mod cache;
mod collector;
mod evaluation;
mod evidence;
mod history;
mod manifest;
pub mod request_context;
#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
pub mod transport;
mod window;
mod writer;

type Shared = Arc<Mutex<State>>;

// Defined in a private child so default builds name it only inside this module, as before.
mod state {
    use serde_json::Value;

    pub(crate) struct State {
        pub(super) header: Value,
        pub(super) ready: bool,
        pub(super) invalid: bool,
        pub(super) own_reasons: Vec<String>,
        pub(super) history_problems: Vec<Value>,
        pub(super) window: super::window::Window,
        pub(super) predecessor_window: Value,
    }
}
#[cfg(not(any(test, feature = "test-support")))]
use state::State;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use state::State;

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
            window: window::Window::new(boot.id.clone()),
            predecessor_window: json!({"status":"INCONCLUSIVE","reason":"PREDECESSOR_UNKNOWN"}),
        };
        if probes["error"].is_string() {
            state.mark("MANIFEST_INVALID");
        }
        state
    }

    fn mark(&mut self, reason: &str) {
        self.invalid = true;
        self.window.invalid = true;
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
        let candidate = if state.ready
            && !state.invalid
            && reasons.is_empty()
            && value["probe_coverage"] == true
        {
            "READY"
        } else {
            "INCONCLUSIVE"
        };
        value["status"] = json!(state.window.verdict(Some(&state.window.boot_id), candidate));
        value["window"] = state.window.report(Some(&state.window.boot_id));
        value["predecessor_window"] = state.predecessor_window.clone();
        value["journal_ready"] = json!(state.ready);
        value["journal_invalid"] = json!(state.invalid);
        value["reasons"] = json!(reasons);
        value["history_problems"] = json!(state.history_problems);
        value
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn open(
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

pub(crate) fn producer_identity() -> Value {
    let host = JOURNAL.get().map_or(Value::Null, |journal| {
        journal
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .header["host"]
            .clone()
    });
    json!({"boot_id":crate::services::process_boot::initialize().id,"host":host})
}

pub fn observation_unknown(reason: &str) {
    if let Some(journal) = JOURNAL.get() {
        invalidate(&journal.0.state, reason);
        transport::try_record(json!({"kind":"invalid","reasons":[reason]}));
    }
}

pub fn start() {
    JOURNAL.get_or_init(|| {
        let root: Option<PathBuf> = crate::config::runtime_root();
        Journal(writer::Writer::spawn(
            root,
            crate::services::process_boot::initialize().clone(),
            Default::default(),
        ))
    });
}

pub async fn ensure_ready() {
    let host = crate::config::snapshot().and_then(|config| config.cluster.instance_id.clone());
    ensure_ready_with(host.as_deref()).await;
}

pub async fn ensure_ready_with(host: Option<&str>) {
    start();
    READY
        .get_or_init(|| async {
            if let Some(journal) = JOURNAL.get() {
                journal.0.prepare(host.map(str::to_owned)).await;
            }
        })
        .await;
}

pub async fn record_selection(config: &crate::config::Config, installed: bool) {
    use crate::services::tui_o::channel_policy;
    let writer = &channel_policy::tui_o_of(config).writer;
    let bindings: Option<std::collections::BTreeSet<u64>> =
        crate::services::provider_hosting::tui_binding_ids(config).collect();
    let selection_source = if writer.all_tui == Some(false) && writer.channels.is_empty() {
        "explicit_off"
    } else if !writer.channels.is_empty() {
        "explicit_list"
    } else if writer.all_tui == Some(true) {
        "explicit_all_tui"
    } else {
        "default_all_tui"
    };
    let record = json!({"kind":"selection", "installed":installed,
        "host":config.cluster.instance_id,"tui_bindings":bindings,"selection_source":selection_source,
        "owner":channel_policy::boot().and_then(|boot| boot.configured_id()),
        "requested":{"all_tui":writer.all_tui,"channels":writer.channels},
        "effective":channel_policy::boot().filter(|_| installed).map(|boot| boot.selected()),
        "dormant":channel_policy::boot().filter(|_| installed).map(|boot| boot.dormant_channels())});
    if let Some(journal) = JOURNAL.get() {
        journal.append(record).await;
    }
}

pub fn health_json() -> Value {
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
    value["o_created"] = transport::snapshot();
    value["uptime_ms"] = json!(
        crate::services::process_boot::initialize()
            .monotonic
            .elapsed()
            .as_millis()
    );
    value
}

pub fn detail_json() -> Value {
    let now = chrono::Utc::now().timestamp_millis();
    #[cfg(any(test, feature = "test-support"))]
    if let Ok(value) = TEST_PROJECTION.try_with(|(cache, now)| cache.snapshot(*now)) {
        return value;
    }
    let mut value = JOURNAL.get().map_or_else(
        || {
            json!({"schema_version":1,"policy_version":1,"status":"INCONCLUSIVE",
            "inventory_source":{"available":false},"errors":["JOURNAL_NOT_STARTED"]})
        },
        |journal| journal.0.projection.snapshot(now),
    );
    value["boot_id"] = json!(crate::services::process_boot::initialize().id);
    value
}

#[cfg(any(test, feature = "test-support"))]
pub use cache::ProjectionCache;
#[cfg(any(test, feature = "test-support"))]
pub use collector::Collector;
pub use evaluation::evaluate;
#[cfg(any(test, feature = "test-support"))]
tokio::task_local! { static TEST_PROJECTION: (Arc<ProjectionCache>, i64); }
#[cfg(any(test, feature = "test-support"))]
pub async fn with_projection_for_test<T>(
    cache: Arc<ProjectionCache>,
    now: i64,
    next: impl std::future::Future<Output = T>,
) -> T {
    TEST_PROJECTION.scope((cache, now), next).await
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) use history::{append_line_for_test, prepare_with_sync_for_test};
#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) use manifest::packaged_for_test;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use manifest::{inspect_for_test, inspect_with_evidence_for_test};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use window::{Window, valid_chain_for_test};

#[cfg(any(test, feature = "test-support"))]
impl State {
    pub(crate) fn new_for_test(
        boot: &crate::services::process_boot::Boot,
        host: Option<String>,
    ) -> Self {
        Self::new(boot, host)
    }

    pub(crate) fn mark_for_test(&mut self, reason: &str) {
        self.mark(reason);
    }

    pub(crate) fn window_for_test(&self) -> &window::Window {
        &self.window
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn invalidate_for_test(state: &Shared, reason: &str) {
    invalidate(state, reason);
}

#[cfg(any(test, feature = "test-support"))]
impl Journal {
    pub(crate) fn spawn_for_test(
        root: Option<PathBuf>,
        boot: crate::services::process_boot::Boot,
        doctor: Option<String>,
        host: Option<String>,
        sync_parent: Option<writer::SyncParent>,
        after_ack: Option<writer::Hook>,
    ) -> Self {
        let mut options = writer::Options {
            doctor,
            host,
            sync_parent,
            ..Default::default()
        };
        if let Some(after_ack) = after_ack {
            options.after_ack = after_ack;
        }
        Self(writer::Writer::spawn(root, boot, options))
    }

    pub(crate) fn state_for_test(&self) -> &Shared {
        &self.0.state
    }

    pub(crate) fn try_append_for_test(&self, record: Value) {
        self.0.try_append(record);
    }
}
