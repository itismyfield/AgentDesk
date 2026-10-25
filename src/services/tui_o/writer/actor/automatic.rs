//! Retention adds age and runtime-idle checks to the existing scoped retirement.

use super::transition::Job;
use crate::services::tui_o::writer::transition_port::PolicyHold;

pub(super) async fn ready(job: &mut Job, key: &str) -> Result<(), PolicyHold> {
    let Some(idle) = &job.automatic else {
        return Ok(());
    };
    let last = job.probes.get(key).and_then(|probe| probe.last_dialogue);
    let old = super::automatic_age::old_enough(last, (job.wall)());
    #[cfg(test)]
    if super::transition::test_hooks::retention_mutant("old") {
        return Err(policy_hold(PolicyHold::Young));
    }
    #[cfg(test)]
    let old = old
        || (last.is_some() && super::transition::test_hooks::retention_mutant("recent"))
        || (last.is_none() && super::transition::test_hooks::retention_mutant("unknown"));
    if !old {
        let reason = if last.is_some() {
            PolicyHold::Young
        } else {
            PolicyHold::AgeUnknown
        };
        return Err(policy_hold(reason));
    }
    let idle = (idle.0)().await;
    #[cfg(test)]
    let idle = idle || super::transition::test_hooks::retention_mutant("active");
    if idle {
        Ok(())
    } else {
        Err(policy_hold(PolicyHold::NotIdle))
    }
}

/// Re-derive the source claim under final authority and bind age to the verified second probe.
pub(super) fn codex_evidence(
    job: &Job,
    key: &str,
    snapshot: &super::super::transition_cleanup::Snapshot,
    cursor: &crate::services::tui_o::store::spool::Cursor,
) -> Option<crate::services::tui_o::store::spool::RetentionEvidenceV1> {
    use super::super::transition_cleanup::Termination;
    use crate::services::tui_o::store::spool::RetentionEvidenceV1;
    let Some(Termination::Retention(claim)) = &job.request.termination else {
        return None;
    };
    let Some(super::super::codex_snapshot::Observation::Complete(observed)) = &snapshot.codex
    else {
        return None;
    };
    let fresh = super::codex_retention::for_hop(
        observed,
        key,
        &cursor.source,
        claim.hop_seq,
        &claim.end.pane,
    )
    .ok()?;
    if fresh != **claim || job.automatic.is_none() {
        return None;
    }
    let last = job.probes.get(key)?.last_dialogue?;
    let now = (job.wall)();
    if !super::automatic_age::old_enough(Some(last), now) {
        return None;
    }
    Some(RetentionEvidenceV1 {
        claim: fresh,
        min_age_secs: 7 * 24 * 60 * 60,
        last_dialogue: chrono::DateTime::from(last),
        observed_at: chrono::DateTime::from(now),
        scope: job.scope.as_ref()?.digest.to_string(),
    })
}

fn policy_hold(reason: PolicyHold) -> PolicyHold {
    #[cfg(test)]
    if super::transition::test_hooks::retention_mutant("collapse_policy") {
        return PolicyHold::Young;
    }
    reason
}

/// Completion is recorded by the actor even when the sweep stopped waiting for its reply.
pub(super) fn log(job: &Job) {
    if job.automatic.is_none() {
        return;
    }
    let retired: Vec<_> = job
        .outcomes
        .iter()
        .filter(|(_, outcome)| {
            matches!(outcome, super::super::transition_port::Outcome::Retired(_))
        })
        .map(|(key, _)| key.as_str())
        .collect();
    tracing::info!(channel = job.request.channel, count = retired.len(), targets = ?retired,
        "[tui_o] automatic source retention");
}
