//! Pending work is revisited only after an observable dependency change or a bounded retry.
use super::*;
use crate::services::tui_o::store::ledger::input_notices::RecordId;
use crate::services::tui_prompt_dedupe::binding_events;
use tokio::time::{Duration, Instant};

pub(crate) fn fingerprint(path: &Path) -> Option<Fingerprint> {
    Fingerprint::of(&std::fs::metadata(path).ok()?).ok()
}

#[derive(Clone, PartialEq, Eq)]
struct Dependencies {
    history: Option<Fingerprint>,
    native: Option<Fingerprint>,
    binding: Option<Fingerprint>,
    through: u64,
    prefix: String,
    contexts: Vec<(PathBuf, Option<Fingerprint>)>,
}

struct Kept {
    dependencies: Dependencies,
    revision: u64,
    retry: Option<Instant>,
    quick_retry: bool,
}

#[derive(Default)]
pub(crate) struct WorkCache {
    records: HashMap<RecordId, Kept>,
    binding: Option<PathBuf>,
}

impl WorkCache {
    pub(crate) fn select(&mut self, store: &ChannelStore, owners: &Batch) -> HashSet<RecordId> {
        if self.binding.is_none() {
            self.binding = binding_events::log_path(store.init().channel)
                .ok()
                .flatten();
        }
        let history = path().ok().as_deref().and_then(fingerprint);
        let binding = self.binding.as_deref().and_then(fingerprint);
        let state = store.ledger().input_notices();
        self.records.retain(|id, _| {
            state
                .evidence
                .get(id)
                .is_some_and(|e| !state.terminal(store.ledger(), id, e))
        });
        let mut sources = HashMap::new();
        let mut selected = HashSet::new();
        for (id, evidence) in &state.evidence {
            if state.terminal(store.ledger(), id, evidence) {
                continue;
            }
            let deps = sources.entry(&id.0).or_insert_with(|| {
                let cursor = store.cursor(&id.0);
                let mut contexts: Vec<_> = owners
                    .scopes
                    .iter()
                    .filter(|s| s.origin.record.source == id.0)
                    .flat_map(|s| s.executions.keys())
                    .map(|p| (p.clone(), fingerprint(p)))
                    .collect();
                contexts.sort_by(|a, b| a.0.cmp(&b.0));
                contexts.dedup_by(|a, b| a.0 == b.0);
                Dependencies {
                    history: history.clone(),
                    native: fingerprint(&id.0.path),
                    binding: binding.clone(),
                    through: cursor.map_or(0, |c| c.captured_through),
                    prefix: cursor.map_or(String::new(), |c| c.prefix_hash.clone()),
                    contexts,
                }
            });
            let revision = state.revision(id);
            let same = self
                .records
                .get(id)
                .is_some_and(|kept| kept.dependencies == *deps && kept.revision == revision);
            let quick_retry = same && self.records.get(id).is_some_and(|kept| kept.quick_retry);
            let unchanged = same
                && self
                    .records
                    .get(id)
                    .is_some_and(|kept| kept.retry.is_none_or(|at| Instant::now() < at));
            if !unchanged {
                selected.insert(id.clone());
                self.records.insert(
                    id.clone(),
                    Kept {
                        dependencies: deps.clone(),
                        revision,
                        retry: None,
                        quick_retry,
                    },
                );
            }
        }
        selected
    }

    pub(crate) fn track_contexts(
        &mut self,
        owners: &Batch,
        selected: &HashSet<RecordId>,
        state: &crate::services::tui_o::store::ledger::input_notices::NoticeState,
    ) {
        for id in selected {
            if let Some(kept) = self.records.get_mut(id) {
                let mut contexts: Vec<_> = owners
                    .scopes
                    .iter()
                    .filter(|s| s.origin.record.source == id.0)
                    .flat_map(|s| s.executions.keys())
                    .map(|p| (p.clone(), fingerprint(p)))
                    .collect();
                contexts.sort_by(|a, b| a.0.cmp(&b.0));
                contexts.dedup_by(|a, b| a.0 == b.0);
                kept.dependencies.contexts = contexts;
                kept.revision = state.revision(id);
            }
        }
    }

    pub(crate) fn retry(&mut self, id: &RecordId, transient: bool) {
        if let Some(kept) = self.records.get_mut(id) {
            let delay = if transient && !kept.quick_retry {
                kept.quick_retry = true;
                0
            } else {
                5
            };
            kept.retry = Some(Instant::now() + Duration::from_secs(delay));
        }
    }
}
