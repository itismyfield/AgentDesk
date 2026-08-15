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

/// Run states an activate request may still act on.
///
/// This is deliberately NOT [`LIVE_RUN_STATUSES`]. Activate's primary job is to
/// promote a freshly generated run: `route_generate` inserts `status =
/// 'generated'`, the CLI `auto-queue add` path inserts `'pending'`, and
/// activate's own `promote_run_and_clear_inactive_slots` flips them with `WHERE
/// status IN ('generated', 'pending')`. Gating activate on the live set alone
/// would reject exactly the runs activate exists to start.
///
/// `restoring` is included on two grounds. The downstream canonical choke point
/// `gate_dispatched_entry_run_on_pg_tx` admits an entry dispatch whenever
/// [`is_live_run_status`] holds, and that set contains `restoring`; excluding it
/// upstream would refuse an activate the authoritative gate would accept.
/// Promotion is also silent on it — the promote SQL matches only
/// `generated`/`pending`, so a `restoring` run passes through unchanged.
///
/// Everything outside this set is terminal for activate. Measured against the
/// statuses production writes to `auto_queue_runs.status` — `generated`,
/// `pending`, `active`, `paused`, `restoring`, `completed`, `cancelled` — the
/// rejected remainder is `completed` and `cancelled`. The predicate is a
/// deny-by-default allowlist, so an unknown or future status is refused too.
///
/// The observation is point-in-time, not a latch: an entry reopen can revive a
/// run just after a terminal status is read here. That race costs the caller one
/// 409 and is recovered by a retry; safety comes from the downstream choke point
/// re-checking under the run advisory lock, not from this predicate.
pub const ACTIVATE_ELIGIBLE_RUN_STATUSES: &[&str] =
    &["active", "paused", "restoring", "generated", "pending"];

/// Run states an activate request may still act on.
pub fn is_activate_eligible_run_status(status: &str) -> bool {
    ACTIVATE_ELIGIBLE_RUN_STATUSES.contains(&status.trim())
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

    #[test]
    fn activate_eligible_status_contract_admits_promotable_runs() {
        // The pre-promotion states activate exists to promote, plus the live set.
        for status in ["generated", "pending", "active", "paused", "restoring"] {
            assert!(
                is_activate_eligible_run_status(status),
                "{status} must be activate-eligible"
            );
        }
        // `completed` / `cancelled` are the terminal run statuses production
        // actually writes; the rest are non-run statuses that must still be
        // refused, because the predicate is a deny-by-default allowlist.
        for status in ["completed", "cancelled", "failed", "user_cancelled", ""] {
            assert!(
                !is_activate_eligible_run_status(status),
                "{status:?} must not be activate-eligible"
            );
        }
        // The live set must remain a subset: the downstream dispatch choke point
        // admits every live status, so an upstream refusal of one would deny an
        // activate the authoritative gate accepts.
        for status in LIVE_RUN_STATUSES {
            assert!(
                is_activate_eligible_run_status(status),
                "live status {status} must stay activate-eligible"
            );
        }
    }
}
