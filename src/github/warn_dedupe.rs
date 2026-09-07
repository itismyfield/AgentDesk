//! Bounded, process-local dedupe for recurring sync and triage warnings.
//!
//! A first or changed condition warns; repeats remain visible at DEBUG. Each
//! warning expires 24 hours after WARN, even if it keeps recurring. Observed
//! recovery clears terminal-card and unknown-agent warnings immediately.
//! Missing issues and successful rotating reconcile batches do not prove that
//! earlier warnings resolved, so they never clear an entire repository scope.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const WARN_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ENTRIES: usize = 4096;
const MAX_IDENTITY_BYTES: usize = 4096;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum WarningKey {
    TerminalOpen(i64, String),
    UnknownAgent(i64, String, &'static str),
    StaleReconcile(String, usize),
}

impl WarningKey {
    fn text_bytes(&self) -> usize {
        match self {
            Self::TerminalOpen(_, card) => card.len(),
            Self::UnknownAgent(_, agent, source) => agent.len() + source.len(),
            Self::StaleReconcile(error, _) => error.len(),
        }
    }
}

#[derive(Default)]
struct RepeatWarnRegistry {
    entries: Mutex<HashMap<(String, WarningKey), Instant>>,
}

impl RepeatWarnRegistry {
    fn first_occurrence(&self, repo: &str, key: WarningKey, now: Instant) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(old_repo, old_key), warned_at| {
            let replaced_agent = matches!(
                (&key, old_key),
                (WarningKey::UnknownAgent(issue, ..), WarningKey::UnknownAgent(old_issue, ..))
                    if repo == old_repo && issue == old_issue && key != *old_key
            );
            !replaced_agent && now.saturating_duration_since(*warned_at) < WARN_TTL
        });
        // Oversized identities still WARN; they must not consume unbounded
        // retained memory or collide through truncation/hashing.
        if repo.len().saturating_add(key.text_bytes()) > MAX_IDENTITY_BYTES {
            return true;
        }
        let identity = (repo.to_string(), key);
        if entries.contains_key(&identity) {
            return false;
        }
        if entries.len() >= MAX_ENTRIES {
            let oldest = entries
                .iter()
                .min_by_key(|(_, warned_at)| **warned_at)
                .map(|(identity, _)| identity.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(identity, now);
        true
    }

    fn clear(&self, repo: &str, matches: impl Fn(&WarningKey) -> bool) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(old_repo, key), _| old_repo != repo || !matches(key));
    }
}

static GITHUB_REPEAT_WARNINGS: LazyLock<RepeatWarnRegistry> =
    LazyLock::new(RepeatWarnRegistry::default);

fn log_warning(first: bool, message: &str) -> bool {
    if first {
        tracing::warn!("{message}");
    } else {
        tracing::debug!("{message} (repeat; first occurrence already warned)");
    }
    first
}

pub(super) fn terminal_open(repo: &str, issue: i64, card: &str, inconsistent: bool) -> bool {
    let key = WarningKey::TerminalOpen(issue, card.to_string());
    if !inconsistent {
        GITHUB_REPEAT_WARNINGS.clear(repo, |old_key| *old_key == key);
        return false;
    }
    log_warning(
        GITHUB_REPEAT_WARNINGS.first_occurrence(repo, key, Instant::now()),
        &format!("[github-sync] {repo}#{issue}: card {card} is terminal but issue is OPEN"),
    )
}

pub(super) fn unknown_agent(
    repo: &str,
    issue: i64,
    agent: Option<&str>,
    source: &'static str,
) -> bool {
    let Some(agent) = agent else {
        GITHUB_REPEAT_WARNINGS.clear(
            repo,
            |key| matches!(key, WarningKey::UnknownAgent(old_issue, ..) if *old_issue == issue),
        );
        return false;
    };
    log_warning(
        GITHUB_REPEAT_WARNINGS.first_occurrence(
            repo,
            WarningKey::UnknownAgent(issue, agent.to_string(), source),
            Instant::now(),
        ),
        &format!(
            "[triage] Ignoring unknown agent '{agent}' from {source} for {repo} issue #{issue}"
        ),
    )
}

pub(super) fn stale_reconcile(repo: &str, error_count: usize, errors: &[String]) -> bool {
    if error_count == 0 {
        return false;
    }
    let now = Instant::now();
    let mut first = false;
    for error in errors {
        // Evaluate every error, including when an earlier one was new.
        first |= GITHUB_REPEAT_WARNINGS.first_occurrence(
            repo,
            WarningKey::StaleReconcile(error.clone(), error_count),
            now,
        );
    }
    // An incomplete error report must remain visible, even without text.
    if errors.is_empty() {
        first = GITHUB_REPEAT_WARNINGS.first_occurrence(
            repo,
            WarningKey::StaleReconcile(String::new(), error_count),
            now,
        );
    }
    log_warning(
        first,
        &format!(
            "[github-sync] {repo}: stale card reconcile had {error_count} non-fatal GraphQL error(s): {}",
            errors.join("; ")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn terminal(issue: i64) -> WarningKey {
        WarningKey::TerminalOpen(issue, "card".to_string())
    }

    #[test]
    fn repeat_warnings_preserve_repository_issue_card_and_category_identity() {
        let registry = RepeatWarnRegistry::default();
        let now = Instant::now();
        assert!(registry.first_occurrence("owner/repo", terminal(1), now));
        assert!(!registry.first_occurrence("owner/repo", terminal(1), now));
        assert!(registry.first_occurrence("other/repo", terminal(1), now));
        assert!(registry.first_occurrence("owner/repo", terminal(2), now));
        assert!(registry.first_occurrence(
            "owner/repo",
            WarningKey::TerminalOpen(1, "other-card".to_string()),
            now
        ));
        assert!(registry.first_occurrence(
            "owner/repo",
            WarningKey::StaleReconcile("card".to_string(), 1),
            now
        ));
    }

    #[test]
    fn repeat_observations_do_not_extend_warning_ttl() {
        let registry = RepeatWarnRegistry::default();
        let now = Instant::now();
        assert!(registry.first_occurrence("ttl/repo", terminal(1), now));
        assert!(!registry.first_occurrence(
            "ttl/repo",
            terminal(1),
            now + WARN_TTL - Duration::from_secs(1)
        ));
        assert!(registry.first_occurrence("ttl/repo", terminal(1), now + WARN_TTL));
        assert!(!registry.first_occurrence("ttl/repo", terminal(1), now + WARN_TTL));
    }

    #[test]
    fn partial_or_aborted_cycles_cannot_clear_unobserved_issues() {
        let registry = RepeatWarnRegistry::default();
        let now = Instant::now();
        assert!(registry.first_occurrence("partial/repo", terminal(1), now));
        assert!(registry.first_occurrence("partial/repo", terminal(2), now));
        registry.clear("partial/repo", |key| *key == terminal(2));
        assert!(!registry.first_occurrence("partial/repo", terminal(1), now));
        assert!(registry.first_occurrence("partial/repo", terminal(2), now));
        assert!(RepeatWarnRegistry::default().first_occurrence("partial/repo", terminal(1), now));
    }

    #[test]
    fn concurrent_duplicate_observations_warn_exactly_once() {
        let registry = Arc::new(RepeatWarnRegistry::default());
        let barrier = Arc::new(Barrier::new(8));
        let now = Instant::now();
        let handles = (0..8)
            .map(|_| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.first_occurrence("concurrent/repo", terminal(1), now)
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            handles
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn poisoned_registry_keeps_new_warnings_visible() {
        let registry = Arc::new(RepeatWarnRegistry::default());
        let poisoned = registry.clone();
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoned.entries.lock().unwrap();
                panic!("simulate interrupted registry update");
            })
            .join()
            .is_err()
        );
        let now = Instant::now();
        assert!(registry.first_occurrence("poisoned/repo", terminal(1), now));
        assert!(!registry.first_occurrence("poisoned/repo", terminal(1), now));
        assert!(registry.first_occurrence("poisoned/repo", terminal(2), now));
    }

    #[test]
    fn capacity_and_identity_size_are_bounded_without_silencing_new_warnings() {
        let registry = RepeatWarnRegistry::default();
        let now = Instant::now();
        for issue in 0..MAX_ENTRIES + 2 {
            assert!(registry.first_occurrence("capacity/repo", terminal(issue as i64), now));
        }
        assert_eq!(registry.entries.lock().unwrap().len(), MAX_ENTRIES);
        let huge = WarningKey::StaleReconcile("x".repeat(MAX_IDENTITY_BYTES), 1);
        assert!(registry.first_occurrence("capacity/repo", huge.clone(), now));
        assert!(registry.first_occurrence("capacity/repo", huge, now));
        assert_eq!(registry.entries.lock().unwrap().len(), MAX_ENTRIES);
        assert!(registry.first_occurrence("capacity/repo", terminal(-1), now + WARN_TTL));
        assert_eq!(registry.entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn terminal_warning_recovery_is_limited_to_observed_card_and_repo() {
        let repo = "terminal-recovery/repo";
        assert!(terminal_open(repo, 1, "a", true));
        assert!(!terminal_open(repo, 1, "a", true));
        assert!(terminal_open(repo, 1, "b", true));
        assert!(!terminal_open("terminal-recovery/other", 1, "a", false));
        assert!(!terminal_open(repo, 1, "a", true));
        assert!(!terminal_open(repo, 1, "a", false));
        assert!(terminal_open(repo, 1, "a", true));
        assert!(!terminal_open(repo, 1, "b", true));
    }

    #[test]
    fn unknown_agent_changes_and_recovery_warn_again_without_clearing_other_issues() {
        let repo = "unknown-recovery/repo";
        assert!(unknown_agent(repo, 1, Some("td"), "explicit label"));
        assert!(!unknown_agent(repo, 1, Some("td"), "explicit label"));
        assert!(unknown_agent(repo, 2, Some("td"), "explicit label"));
        assert!(unknown_agent(repo, 1, Some("td"), "inferred routing"));
        assert!(unknown_agent(repo, 1, Some("other"), "explicit label"));
        assert!(unknown_agent(repo, 1, Some("td"), "explicit label"));
        assert!(!unknown_agent(repo, 1, None, "resolved"));
        assert!(unknown_agent(repo, 1, Some("td"), "explicit label"));
        assert!(!unknown_agent(repo, 2, Some("td"), "explicit label"));
    }

    #[test]
    fn stale_errors_preserve_changed_errors_counts_and_rotating_batches() {
        let repo = "stale-errors/repo";
        let errors = vec!["issue 1 missing".to_string(), "issue 2 denied".to_string()];
        assert!(stale_reconcile(repo, 2, &errors));
        assert!(!stale_reconcile(repo, 2, &errors));
        let mut reversed = errors.clone();
        reversed.reverse();
        assert!(!stale_reconcile(repo, 2, &reversed));
        assert!(stale_reconcile(
            repo,
            2,
            &[errors[0].clone(), "issue 2 missing".to_string()]
        ));
        assert!(stale_reconcile(repo, 1, &errors[..1]));
        assert!(!stale_reconcile(repo, 0, &[]));
        assert!(!stale_reconcile(repo, 1, &errors[..1]));
        assert!(stale_reconcile("stale-errors/other", 1, &errors[..1]));
    }

    #[test]
    fn missing_error_details_still_warn_and_dedupe() {
        assert!(stale_reconcile("empty-errors/repo", 1, &[]));
        assert!(!stale_reconcile("empty-errors/repo", 1, &[]));
        assert!(stale_reconcile("empty-errors/repo", 2, &[]));
    }

    #[test]
    fn first_and_changed_warnings_are_warn_and_repeats_remain_debug() {
        let capture = crate::github::test_support::LogCapture::new();
        tracing::dispatcher::with_default(&capture.dispatch, || {
            assert!(unknown_agent(
                "log-level/repo",
                1,
                Some("td"),
                "explicit label"
            ));
            assert!(!unknown_agent(
                "log-level/repo",
                1,
                Some("td"),
                "explicit label"
            ));
            assert!(unknown_agent(
                "log-level/repo",
                1,
                Some("new-agent"),
                "explicit label"
            ));
        });
        let logs = capture.take();
        assert_eq!(
            logs.lines().filter(|line| line.contains("WARN")).count(),
            2,
            "{logs}"
        );
        assert_eq!(
            logs.lines().filter(|line| line.contains("DEBUG")).count(),
            1,
            "{logs}"
        );
        assert!(logs.contains("repeat; first occurrence already warned"));
        assert!(logs.contains("new-agent"));
    }
}
