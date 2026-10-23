//! Retention adds age and runtime-idle checks to the existing scoped retirement.

use super::transition::Job;

pub(super) async fn ready(job: &mut Job, key: &str) -> bool {
    let Some(idle) = &job.automatic else {
        return true;
    };
    let last = job.probes.get(key).and_then(|probe| probe.last_dialogue);
    let old = super::automatic_age::old_enough(last, (job.wall)());
    #[cfg(test)]
    if super::transition::test_hooks::retention_mutant("old") {
        return false;
    }
    #[cfg(test)]
    let old = old
        || (last.is_some() && super::transition::test_hooks::retention_mutant("recent"))
        || (last.is_none() && super::transition::test_hooks::retention_mutant("unknown"));
    if !old {
        return false;
    }
    let idle = (idle.0)().await;
    #[cfg(test)]
    let idle = idle || super::transition::test_hooks::retention_mutant("active");
    idle
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
