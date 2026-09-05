//! Process-lifetime dedupe for GitHub sync/triage warnings that recur on
//! every periodic cycle while the underlying condition persists.
//!
//! The periodic sync runs every `github.sync_interval_minutes` (10 min in
//! production) and re-discovers the same conditions each time: a card that is
//! terminal while its issue is still OPEN, a stale-reconcile GraphQL error
//! for an issue that no longer resolves, or an `agent:<id>` label naming an
//! agent this instance does not know. Each of those emitted a WARN per cycle
//! (3,346 lines over three days for a handful of distinct keys), burying
//! genuinely new problems.
//!
//! Semantics:
//! * The first observation of a `(scope, key)` pair returns `true` → caller
//!   logs at WARN. Later observations return `false` → caller logs at DEBUG.
//! * At the end of a successful cycle the caller passes the set of keys it
//!   observed in that cycle via [`RepeatWarnRegistry::retain`]; keys that were
//!   not observed are dropped so a condition that resolves and then recurs
//!   warns again.
//! * Purely in-memory; a process restart warns once more, which is the
//!   desired "one WARN per process lifetime" contract.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

/// Scope-partitioned set of already-warned keys.
pub(crate) struct RepeatWarnRegistry {
    scopes: Mutex<HashMap<String, HashSet<String>>>,
}

impl RepeatWarnRegistry {
    pub(crate) fn new() -> Self {
        Self {
            scopes: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `true` when `key` has not been seen in `scope` since the last
    /// [`retain`](Self::retain) that dropped it (or ever), and records it.
    pub(crate) fn first_occurrence(&self, scope: &str, key: &str) -> bool {
        let mut scopes = self
            .scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        scopes
            .entry(scope.to_string())
            .or_default()
            .insert(key.to_string())
    }

    /// Keeps only the keys still `live` for `scope`; everything else is
    /// forgotten so it warns again on recurrence.
    pub(crate) fn retain(&self, scope: &str, live: &HashSet<String>) {
        let mut scopes = self
            .scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(keys) = scopes.get_mut(scope) {
            keys.retain(|key| live.contains(key));
            if keys.is_empty() {
                scopes.remove(scope);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn is_tracked(&self, scope: &str, key: &str) -> bool {
        self.scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(scope)
            .is_some_and(|keys| keys.contains(key))
    }
}

/// Registry shared by `github::sync` (terminal-but-OPEN, stale reconcile)
/// and `github::triage` (unknown agent label). Scopes are prefixed per
/// producer so the two never collide.
pub(crate) static GITHUB_REPEAT_WARNINGS: LazyLock<RepeatWarnRegistry> =
    LazyLock::new(RepeatWarnRegistry::new);

pub(crate) fn sync_scope(repo: &str) -> String {
    format!("sync:{repo}")
}

pub(crate) fn triage_scope(repo: &str) -> String {
    format!("triage:{repo}")
}

/// Emits `message` at WARN on the first observation of `(scope, key)` and at
/// DEBUG afterwards. Returns whether this call warned.
pub(crate) fn warn_once_else_debug(scope: &str, key: &str, message: &str) -> bool {
    if GITHUB_REPEAT_WARNINGS.first_occurrence(scope, key) {
        tracing::warn!("{message}");
        true
    } else {
        tracing::debug!("{message} (repeat; first occurrence already warned)");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_occurrence_warns_once_per_scope_key() {
        let registry = RepeatWarnRegistry::new();
        assert!(registry.first_occurrence("sync:o/r", "#1:terminal-open"));
        assert!(!registry.first_occurrence("sync:o/r", "#1:terminal-open"));
        assert!(!registry.first_occurrence("sync:o/r", "#1:terminal-open"));
        // Same key in another scope (another repo) is independent.
        assert!(registry.first_occurrence("sync:o/other", "#1:terminal-open"));
        // A different key in the same scope warns.
        assert!(registry.first_occurrence("sync:o/r", "#2:terminal-open"));
    }

    #[test]
    fn retain_drops_resolved_keys_so_recurrence_warns_again() {
        let registry = RepeatWarnRegistry::new();
        assert!(registry.first_occurrence("sync:o/r", "#1"));
        assert!(registry.first_occurrence("sync:o/r", "#2"));

        // Cycle where only #2 is still observed: #1 resolved.
        let live: HashSet<String> = ["#2".to_string()].into_iter().collect();
        registry.retain("sync:o/r", &live);
        assert!(!registry.is_tracked("sync:o/r", "#1"));
        assert!(registry.is_tracked("sync:o/r", "#2"));

        // #2 stays deduped; #1 recurring warns again.
        assert!(!registry.first_occurrence("sync:o/r", "#2"));
        assert!(registry.first_occurrence("sync:o/r", "#1"));
    }

    #[test]
    fn retain_with_empty_live_set_clears_scope() {
        let registry = RepeatWarnRegistry::new();
        assert!(registry.first_occurrence("triage:o/r", "#186:td"));
        registry.retain("triage:o/r", &HashSet::new());
        assert!(!registry.is_tracked("triage:o/r", "#186:td"));
        assert!(registry.first_occurrence("triage:o/r", "#186:td"));
    }

    #[test]
    fn retain_on_unknown_scope_is_noop() {
        let registry = RepeatWarnRegistry::new();
        registry.retain("sync:never-seen", &HashSet::new());
        assert!(registry.first_occurrence("sync:never-seen", "k"));
    }

    #[test]
    fn scopes_are_producer_prefixed() {
        assert_eq!(sync_scope("o/r"), "sync:o/r");
        assert_eq!(triage_scope("o/r"), "triage:o/r");
        assert_ne!(sync_scope("o/r"), triage_scope("o/r"));
    }
}
