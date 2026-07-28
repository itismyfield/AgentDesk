/// Run states that still own live dispatch and slot obligations.
pub const LIVE_RUN_STATUSES: &[&str] = &["active", "paused", "restoring"];

/// Build a SQL literal list from the canonical live-run state set.
pub fn live_run_statuses_sql() -> String {
    LIVE_RUN_STATUSES
        .iter()
        .map(|status| format!("'{status}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Run states that can still own live dispatch and slot obligations.
pub fn is_live_run_status(status: &str) -> bool {
    LIVE_RUN_STATUSES.contains(&status.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_run_status_contract_includes_restore_window() {
        for status in ["active", "paused", "restoring"] {
            assert!(is_live_run_status(status), "{status} must remain live");
        }
        for status in ["generated", "pending", "cancelled", "completed"] {
            assert!(!is_live_run_status(status), "{status} must not be live");
        }
        assert_eq!(live_run_statuses_sql(), "'active', 'paused', 'restoring'");
    }
}
