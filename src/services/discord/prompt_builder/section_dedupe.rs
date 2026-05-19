//! Prompt-section dedup + size telemetry (issue #2659).
//!
//! Audit observation: large content blocks (`shared_knowledge`,
//! `longterm_catalog`, role prompt, etc.) can be appended to a single
//! Discord turn's system prompt by *several* code paths, occasionally
//! resulting in the same ~25KB body appearing 2–6× per session in extreme
//! cases (chunk-02 §3, chunk-09 s1/s7/s9/s11/s18/s19).
//!
//! The actual large skill catalog Claude prints during a session is owned
//! by the Claude CLI, not by AgentDesk; the issue's literal `attachment.
//! skill_listing` envelope is a Claude transcript event type we only
//! *observe*. What AgentDesk *can* do is guarantee that any content block
//! it owns is appended to the system prompt **at most once per build** and
//! that any oversized block trips a structured warning early enough to
//! catch regressions.
//!
//! This module provides a lightweight tracker that the system-prompt
//! builder routes through whenever it wants to append a large, externally
//! sourced section. Behavior is conservative: dedup is by SHA-256 of the
//! trimmed content, and dropping a duplicate emits a `WARN` log so the
//! root-cause source can be located instead of being silently swallowed.

use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// Threshold above which a single appended section is considered "large"
/// and logged at WARN. Roughly matches the 25.8KB skill_listing observed
/// in the audit. We pick a slightly lower limit so the very first regress
/// trips the warning.
pub(super) const LARGE_SECTION_BYTES: usize = 20 * 1024;

/// Per-build appendage tracker. Owns *only* hashes — never the original
/// content — so it can be cheaply cloned/passed by reference without
/// duplicating prompt bytes.
#[derive(Default)]
pub(super) struct PromptSectionTracker {
    seen: HashSet<[u8; 32]>,
    /// Cumulative size of *uniquely* appended sections. Useful for the
    /// "build done — final length" telemetry call site.
    appended_bytes: usize,
}

impl PromptSectionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether `content` would be a duplicate without consuming the
    /// tracker. Returns `true` if a section with the same SHA-256 has
    /// already been recorded.
    #[cfg(test)]
    pub fn would_be_duplicate(&self, content: &str) -> bool {
        self.seen.contains(&hash_content(content))
    }

    /// Record an appendage. Returns `true` if the content is *new* (caller
    /// should perform the actual `push_str`). Returns `false` if it is a
    /// byte-for-byte duplicate of something already appended in this build
    /// and the caller should skip the push.
    ///
    /// `label` is included in log lines so operators can tell which code
    /// path tried to inject the duplicate.
    pub fn record(&mut self, label: &str, content: &str) -> bool {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return false;
        }
        let digest = hash_content(content);
        if !self.seen.insert(digest) {
            tracing::warn!(
                target: "agentdesk.prompt_section_dedupe",
                section = %label,
                bytes = content.len(),
                "dropping duplicate prompt section — same hash already appended this build"
            );
            return false;
        }
        if content.len() >= LARGE_SECTION_BYTES {
            tracing::warn!(
                target: "agentdesk.prompt_section_dedupe",
                section = %label,
                bytes = content.len(),
                threshold = LARGE_SECTION_BYTES,
                "large prompt section appended — investigate trimming or moving to attachment"
            );
        }
        self.appended_bytes += content.len();
        true
    }

    /// Cumulative size of unique sections recorded so far.
    pub fn appended_bytes(&self) -> usize {
        self.appended_bytes
    }
}

fn hash_content(content: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_whitespace_never_records() {
        let mut t = PromptSectionTracker::new();
        assert!(!t.record("a", ""));
        assert!(!t.record("a", "   \n  "));
        assert_eq!(t.appended_bytes(), 0);
    }

    #[test]
    fn first_append_returns_true_duplicate_returns_false() {
        let mut t = PromptSectionTracker::new();
        assert!(t.record("shared_knowledge", "alpha"));
        assert!(!t.record("longterm_catalog", "alpha"));
        assert_eq!(t.appended_bytes(), 5);
    }

    #[test]
    fn distinct_content_each_records() {
        let mut t = PromptSectionTracker::new();
        assert!(t.record("a", "alpha"));
        assert!(t.record("b", "beta"));
        assert_eq!(t.appended_bytes(), 9);
    }

    #[test]
    fn would_be_duplicate_does_not_mutate() {
        let mut t = PromptSectionTracker::new();
        t.record("a", "alpha");
        assert!(t.would_be_duplicate("alpha"));
        assert!(!t.would_be_duplicate("beta"));
        // still recorded once
        assert!(!t.record("a2", "alpha"));
    }

    #[test]
    fn large_section_still_records_first_time() {
        let mut t = PromptSectionTracker::new();
        let big = "x".repeat(LARGE_SECTION_BYTES + 1);
        assert!(t.record("skill_listing", &big));
        // second time is still a duplicate
        assert!(!t.record("skill_listing", &big));
    }

    #[test]
    fn hash_differs_for_distinct_content() {
        let a = hash_content("alpha");
        let b = hash_content("beta");
        assert_ne!(a, b);
    }
}
