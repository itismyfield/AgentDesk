const BENIGN_BLOCKED_REASON_PREFIXES: &[&str] = &[
    "ci:waiting",
    "ci:running",
    "ci:rerunning",
    "ci:rework",
    "deploy:waiting",
    "deploy:deploying:",
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
