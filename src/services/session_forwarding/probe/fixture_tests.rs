//! Readiness cache fixture for tests that need a node already probed.
use super::{CACHE, CachedProbe};
use serde_json::json;

pub(crate) struct ScopedReadinessProbe {
    id: String,
    previous: Option<CachedProbe>,
}

impl ScopedReadinessProbe {
    pub(crate) fn verified(id: &str, origin: &str, expires_at_ms: i64) -> Self {
        let previous = CACHE
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.to_owned(),
                CachedProbe {
                    origin: Some(origin.to_owned()),
                    report: json!({
                        "expires_at_ms": expires_at_ms,
                        "reachability_verified": true,
                        "reachability_status": "verified",
                    }),
                },
            );
        Self {
            id: id.to_owned(),
            previous,
        }
    }

    pub(crate) fn expire(&self) {
        if let Some(probe) = CACHE
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&self.id)
        {
            probe.report["expires_at_ms"] = json!(0);
        }
    }
}

impl Drop for ScopedReadinessProbe {
    fn drop(&mut self) {
        let mut cache = CACHE
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = self.previous.take() {
            cache.insert(self.id.clone(), previous);
        } else {
            cache.remove(&self.id);
        }
    }
}
