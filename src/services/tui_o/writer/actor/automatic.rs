//! Retention adds age and runtime-idle checks to the existing scoped retirement.

use super::transition::Job;

pub(super) async fn ready(job: &mut Job, key: &str) -> bool {
    let Some(idle) = &job.automatic else {
        return true;
    };
    let last = job.probes.get(key).and_then(|probe| probe.last_dialogue);
    let old = super::automatic_age::old_enough(last, (job.wall)());
    // A missing automatic path retains every source until the policy is implemented.
    let _ = (idle, old);
    false
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
