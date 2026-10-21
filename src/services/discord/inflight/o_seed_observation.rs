use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::provider::ProviderKind;

type Key = (String, u64);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct Snapshot {
    pub(in crate::services::discord) writable_inflight_load_calls: usize,
    pub(in crate::services::discord) readonly_inflight_load_calls: usize,
    pub(in crate::services::discord) compatibility_backfill_attempts: usize,
    pub(in crate::services::discord) legacy_prefix_restore_calls: usize,
    pub(in crate::services::discord) decoded_source_chunks: usize,
    pub(in crate::services::discord) watcher_initialization_observed: bool,
    pub(in crate::services::discord) stream_decoder_initialization_observed: bool,
    pub(in crate::services::discord) decoded_chunks: Vec<String>,
    pub(in crate::services::discord) parser_responses: Vec<String>,
    pub(in crate::services::discord) events: BTreeMap<&'static str, usize>,
}

impl Snapshot {
    pub(in crate::services::discord) fn event_count(&self, event: &'static str) -> usize {
        self.events.get(event).copied().unwrap_or_default()
    }

    fn since(&self, baseline: &Self) -> Self {
        let events: BTreeMap<_, _> = self
            .events
            .iter()
            .filter_map(|(event, count)| {
                let count = count - baseline.event_count(event);
                (count > 0).then_some((*event, count))
            })
            .collect();
        Self {
            writable_inflight_load_calls: self.writable_inflight_load_calls
                - baseline.writable_inflight_load_calls,
            readonly_inflight_load_calls: self.readonly_inflight_load_calls
                - baseline.readonly_inflight_load_calls,
            compatibility_backfill_attempts: self.compatibility_backfill_attempts
                - baseline.compatibility_backfill_attempts,
            legacy_prefix_restore_calls: self.legacy_prefix_restore_calls
                - baseline.legacy_prefix_restore_calls,
            decoded_source_chunks: self.decoded_source_chunks - baseline.decoded_source_chunks,
            watcher_initialization_observed: events.contains_key("watcher_initialized"),
            stream_decoder_initialization_observed: events.contains_key("decoder_initialized"),
            decoded_chunks: self
                .decoded_chunks
                .iter()
                .skip(baseline.decoded_chunks.len())
                .cloned()
                .collect(),
            parser_responses: self
                .parser_responses
                .iter()
                .skip(baseline.parser_responses.len())
                .cloned()
                .collect(),
            events,
        }
    }
}

#[derive(Default)]
struct Observations {
    process: Snapshot,
    keys: HashMap<Key, Snapshot>,
    sources: HashMap<Key, Option<SourceFileIdentity>>,
}

static OBSERVATIONS: LazyLock<Mutex<Observations>> = LazyLock::new(Mutex::default);

pub(in crate::services::discord) struct Guard {
    key: Key,
    key_baseline: Snapshot,
    process_baseline: Snapshot,
}

impl Guard {
    pub(in crate::services::discord) fn new(provider: &ProviderKind, channel_id: u64) -> Self {
        let key = (provider.as_str().to_owned(), channel_id);
        let mut observations = OBSERVATIONS.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !observations.sources.contains_key(&key),
            "overlapping observation guards"
        );
        observations.sources.insert(key.clone(), None);
        Self {
            key_baseline: observations.keys.get(&key).cloned().unwrap_or_default(),
            process_baseline: observations.process.clone(),
            key,
        }
    }

    pub(in crate::services::discord) fn register_source(&self, path: &std::path::Path) {
        let file = std::fs::File::open(path).expect("open observed source");
        let source = SourceFileIdentity::from_open_file(&file);
        assert_ne!(source, SourceFileIdentity::Unavailable);
        *OBSERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sources
            .get_mut(&self.key)
            .expect("active observation guard") = Some(source);
    }

    pub(in crate::services::discord) fn snapshot(&self) -> Snapshot {
        OBSERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys
            .get(&self.key)
            .cloned()
            .unwrap_or_default()
            .since(&self.key_baseline)
    }

    pub(in crate::services::discord) fn raw_process_snapshot(&self) -> Snapshot {
        raw_process_totals().since(&self.process_baseline)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        OBSERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sources
            .remove(&self.key);
    }
}

pub(in crate::services::discord) fn raw_process_totals() -> Snapshot {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .process
        .clone()
}

pub(in crate::services::discord) fn key_totals(
    provider: &ProviderKind,
    channel_id: u64,
) -> Snapshot {
    OBSERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys
        .get(&(provider.as_str().to_owned(), channel_id))
        .cloned()
        .unwrap_or_default()
}

fn update(provider: &ProviderKind, channel_id: u64, update: impl Fn(&mut Snapshot)) {
    let mut observations = OBSERVATIONS.lock().unwrap_or_else(|e| e.into_inner());
    update(&mut observations.process);
    update(
        observations
            .keys
            .entry((provider.as_str().to_owned(), channel_id))
            .or_default(),
    );
}

pub(super) fn writable_load(provider: &ProviderKind, channel_id: u64) {
    update(provider, channel_id, |s| {
        s.writable_inflight_load_calls += 1
    });
}

pub(super) fn read_only_load(provider: &ProviderKind, channel_id: u64) {
    update(provider, channel_id, |s| {
        s.readonly_inflight_load_calls += 1
    });
}

pub(super) fn backfill_attempt(provider: &ProviderKind, channel_id: u64) {
    update(provider, channel_id, |s| {
        s.compatibility_backfill_attempts += 1
    });
}

pub(in crate::services::discord) fn record_event(
    provider: &ProviderKind,
    channel_id: u64,
    event: &'static str,
) {
    update(provider, channel_id, |s| {
        *s.events.entry(event).or_default() += 1;
        s.watcher_initialization_observed |= event == "watcher_initialized";
        s.stream_decoder_initialization_observed |= event == "decoder_initialized";
    });
}

pub(in crate::services::discord) fn watcher_initialized(provider: &ProviderKind, channel_id: u64) {
    record_event(provider, channel_id, "watcher_initialized");
}

pub(in crate::services::discord) fn decoder_initialized(provider: &ProviderKind, channel_id: u64) {
    record_event(provider, channel_id, "decoder_initialized");
}

pub(in crate::services::discord) fn prefix_restore(provider: &ProviderKind, channel_id: u64) {
    update(provider, channel_id, |s| s.legacy_prefix_restore_calls += 1);
}

pub(in crate::services::discord) fn record_parser_response(
    provider: &ProviderKind,
    channel_id: u64,
    response: &str,
) {
    update(provider, channel_id, |s| {
        s.parser_responses.push(response.to_owned())
    });
}

pub(in crate::services::discord) fn source_decoded(source: SourceFileIdentity, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut observations = OBSERVATIONS.lock().unwrap_or_else(|e| e.into_inner());
    observations.process.decoded_source_chunks += 1;
    let keys: Vec<_> = observations
        .sources
        .iter()
        .filter_map(|(key, identity)| (*identity == Some(source)).then_some(key.clone()))
        .collect();
    for key in keys {
        let keyed = observations.keys.entry(key).or_default();
        keyed.decoded_source_chunks += 1;
        keyed.decoded_chunks.push(text.to_owned());
    }
}

#[path = "o_seed_observation/terminal_barrier_test.rs"]
mod terminal_barrier;
pub(in crate::services::discord) use terminal_barrier::terminal_checkpoint;
