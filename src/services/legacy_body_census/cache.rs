use super::collector::Collector;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::RwLock,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct ProjectionCache {
    value: RwLock<Option<(Instant, Value)>>,
}

impl ProjectionCache {
    pub(super) fn refresh(&self, collector: &mut Collector, root: &Path, current: &str, now: i64) {
        let value = collector.collect(root, current, now);
        *self
            .value
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), value));
    }

    pub(super) fn snapshot(&self, now: i64) -> Value {
        match self.value.try_read().ok().and_then(|value| value.clone()) {
            Some((at, mut data)) => {
                if at.elapsed() > Duration::from_secs(120)
                    || data["cut_ms"]
                        .as_i64()
                        .is_none_or(|cut| now < cut || now.saturating_sub(cut) > 120_000)
                {
                    if let Some(errors) = data["errors"].as_array_mut() {
                        errors.push(json!("STALE_OR_REVERSED_SNAPSHOT"));
                    }
                    data["stale_snapshot"] = json!(true);
                    if data["local_evaluation"]["status"] != "FAIL" {
                        data["local_evaluation"]["status"] = json!("INCONCLUSIVE");
                    }
                    data["local_evaluation"]["complete"] = json!(false);
                    data["local_evaluation"]["reasons"] = data["errors"].clone();
                    if let Some(sites) = data["local_evaluation"]["sites"].as_object_mut() {
                        for site in sites.values_mut() {
                            site["status"] = json!("DELETE_NOT_READY");
                            site["reason"] = json!("STALE_SNAPSHOT");
                        }
                    }
                }
                data
            }
            None => json!({"schema_version":1,"policy_version":1,"status":"INCONCLUSIVE",
                "inventory_source":{"available":false},"errors":["COLLECTOR_PENDING_BUSY_OR_STALE"],"evidence_truncated":false}),
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture(root: &Path, current: &str, now: i64) -> std::sync::Arc<Self> {
        let cache = std::sync::Arc::new(Self::default());
        cache.refresh(&mut Collector::default(), root, current, now);
        cache
    }

    #[cfg(test)]
    pub(crate) fn block_for_test(&self, action: impl FnOnce()) {
        let _guard = self.value.write().unwrap();
        action();
    }
}
