const BENIGN_BLOCKED_REASON_PREFIXES: &[&str] = &[
    "ci:waiting",
    "ci:running",
    "ci:rerunning",
    "ci:rework",
    // #743: pr:creating marks a card whose create-pr dispatch is in flight;
    // it is a benign progress state, not manual intervention.
    "pr:creating",
];

pub(crate) fn is_benign_blocked_reason(reason: &str) -> bool {
    BENIGN_BLOCKED_REASON_PREFIXES
        .iter()
        .any(|prefix| reason.starts_with(prefix))
}

pub(crate) fn manual_intervention_fingerprint(
    review_status: Option<&str>,
    blocked_reason: Option<&str>,
) -> Option<String> {
    if review_status == Some("dilemma_pending") {
        return Some("review:dilemma_pending".to_string());
    }

    blocked_reason
        .filter(|reason| !reason.trim().is_empty() && !is_benign_blocked_reason(reason))
        .map(|reason| format!("blocked:{reason}"))
}

pub(crate) fn requires_manual_intervention(
    review_status: Option<&str>,
    blocked_reason: Option<&str>,
) -> bool {
    manual_intervention_fingerprint(review_status, blocked_reason).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_creating_is_benign_not_manual_intervention() {
        // #743: `pr:creating` marks a benign in-flight create-pr handoff.
        assert!(is_benign_blocked_reason("pr:creating"));
        assert_eq!(
            manual_intervention_fingerprint(None, Some("pr:creating")),
            None
        );
    }

    #[test]
    fn pr_create_failed_is_not_benign() {
        // #743: `pr:create_failed*` marks a real failure needing retry/intervention.
        assert!(!is_benign_blocked_reason(
            "pr:create_failed:dispatch_failed"
        ));
        assert!(!is_benign_blocked_reason(
            "pr:create_failed_escalated:max_retries"
        ));
        assert!(requires_manual_intervention(
            None,
            Some("pr:create_failed:some_error")
        ));
    }
}
