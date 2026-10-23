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
