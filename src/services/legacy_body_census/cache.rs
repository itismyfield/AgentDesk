use super::collector::Collector;
use serde_json::{Value, json};
use std::{path::Path, sync::RwLock, time::Instant};

#[derive(Default)]
pub(crate) struct ProjectionCache {
    value: RwLock<Option<(Instant, Value)>>,
}

impl ProjectionCache {
    pub(super) fn refresh(
        &self,
        collector: &mut Collector,
        root: &Path,
        current: &str,
        now: i64,
        state: &super::Shared,
    ) {
        let captured = Instant::now();
        let value = collector.collect(root, current, now, Some(state));
        *self
            .value
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((captured, value));
    }

    pub(super) fn snapshot(&self, now: i64) -> Value {
        match self.value.try_read().ok().and_then(|value| value.clone()) {
            Some((at, mut data)) => {
                let age = u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX);
                data["served_at_ms"] = json!(now);
                data["monotonic_age_ms"] = json!(age);
                let clock_unknown = data["cut_ms"].as_i64().is_none_or(|cut| {
                    u64::try_from(now.saturating_sub(cut))
                        .ok()
                        .is_none_or(|wall_age| wall_age.abs_diff(age) > 5000)
                });
                let stale = age > 120_000
                    || data["cut_ms"]
                        .as_i64()
                        .is_none_or(|cut| now < cut || now.saturating_sub(cut) > 120_000);
                data["clock_unknown"] = json!(clock_unknown);
                if stale || clock_unknown {
                    let reason = if clock_unknown {
                        "SNAPSHOT_CLOCK_UNKNOWN"
                    } else {
                        "STALE_OR_REVERSED_SNAPSHOT"
                    };
                    if let Some(errors) = data["errors"].as_array_mut() {
                        errors.push(json!(reason));
                    }
                    data["stale_snapshot"] = json!(stale);
                    if data["local_evaluation"]["status"] != "FAIL" {
                        data["local_evaluation"]["status"] = json!("INCONCLUSIVE");
                    }
                    data["local_evaluation"]["complete"] = json!(false);
                    if let Some(reasons) = data["local_evaluation"]["reasons"].as_array_mut() {
                        reasons.push(json!(reason));
                    }
                    if let Some(sites) = data["local_evaluation"]["sites"].as_object_mut() {
                        for site in sites.values_mut() {
                            site["status"] = json!("DELETE_NOT_READY");
                            site["reason"] = json!(reason);
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
        Self::fixture_at_age(root, current, now, 0)
    }

    #[cfg(test)]
    pub(crate) fn fixture_at_age(
        root: &Path,
        current: &str,
        now: i64,
        age: u64,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            value: RwLock::new(Some((
                Instant::now()
                    .checked_sub(std::time::Duration::from_millis(age))
                    .unwrap(),
                Collector::default().fixture(root, current, now),
            ))),
        })
    }

    #[cfg(test)]
    pub(crate) fn block_for_test(&self, action: impl FnOnce()) {
        let _guard = self.value.write().unwrap();
        action();
    }
}
