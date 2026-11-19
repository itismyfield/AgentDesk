use std::collections::BTreeSet;

use super::adk_observe::ci::{CiObservation, CiVerdict, CompletedJobs};
use crate::domain::campaign::landing::CrossRunJob;

/// The coordinator collects a check's log and a trusted digest of the job's inputs.
pub struct JobInputWitness {
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: u64,
    pub head_sha: String,
    pub digest: String,
    pub reference: String,
}

impl JobInputWitness {
    fn binds(&self, run: &CompletedJobs, job_id: u64) -> bool {
        (self.run_id, self.run_attempt, self.job_id) == (run.run_id, run.run_attempt, job_id)
            && self.head_sha == run.head
            && !self.reference.trim().is_empty()
    }
}

pub struct RunnerLoss {
    pub failed_job: u64,
    pub diagnostic: String,
    pub log_ref: String,
    pub failed_inputs: JobInputWitness,
    pub prior: CompletedJobs,
    pub successful_job: u64,
    pub successful_inputs: JobInputWitness,
}

pub struct RunnerException {
    pub current: CompletedJobs,
    pub losses: Vec<RunnerLoss>,
}

/// Every failing job needs its own proof. Unknown or absent required checks cannot use the exception.
pub(super) fn runner_exception(
    ci: &CiObservation,
    evidence: &RunnerException,
) -> Option<Vec<CrossRunJob>> {
    let observation = &evidence.current;
    if ci.run_conclusion.as_deref() != Some("failure")
        || ci.tests().is_none()
        || !matches!(ci.verdict(), CiVerdict::Failed { .. })
        || !observation.matches(ci)
    {
        return None;
    }
    let failed: BTreeSet<_> = observation
        .jobs
        .iter()
        .filter(|job| job.conclusion == "failure")
        .map(|job| job.id)
        .collect();
    if failed.is_empty()
        || evidence.losses.len() != failed.len()
        || observation
            .jobs
            .iter()
            .any(|job| !matches!(job.conclusion.as_str(), "success" | "failure" | "skipped"))
    {
        return None;
    }
    for context in ci.required.contexts().ok()? {
        let mut checks = ci.checks.iter().filter(|check| check.context == context);
        let check = checks.next()?;
        if checks.next().is_some() || check.status != "completed" {
            return None;
        }
        if check.conclusion.as_deref() != Some("success")
            && !(check.conclusion.as_deref() == Some("failure")
                && observation.jobs.iter().any(|job| {
                    job.check_run_id == check.check_run_id
                        && job.name == check.context
                        && failed.contains(&job.id)
                }))
        {
            return None;
        }
    }

    let mut used = BTreeSet::new();
    evidence
        .losses
        .iter()
        .map(|proof| {
            let job = observation.job(proof.failed_job)?;
            let earlier = proof.prior.job(proof.successful_job)?;
            let digest = &proof.failed_inputs.digest;
            let job_url = format!(
                "https://github.com/{}/actions/runs/{}/job/{}",
                observation.repository, observation.run_id, job.id
            );
            if !used.insert(job.id)
                || !failed.contains(&job.id)
                || proof.diagnostic != "lost communication with the server"
                || proof.log_ref != job_url
                || proof.prior.repository != observation.repository
                || proof.prior.workflow.path.split('@').next()
                    != observation.workflow.path.split('@').next()
                || proof.prior.workflow.event != observation.workflow.event
                || proof.prior.workflow.name != observation.workflow.name
                || proof.prior.run_id == 0
                || proof.prior.run_attempt == 0
                || proof.prior.run_id == observation.run_id
                || earlier.name != job.name
                || earlier.conclusion != "success"
                || !proof.failed_inputs.binds(observation, job.id)
                || !proof.successful_inputs.binds(&proof.prior, earlier.id)
                || digest.len() != 64
                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                || digest != &proof.successful_inputs.digest
            {
                return None;
            }
            Some(CrossRunJob {
                job_id: job.id,
                log_ref: proof.log_ref.clone(),
                run_id: proof.prior.run_id,
                run_attempt: proof.prior.run_attempt,
                successful_job_id: earlier.id,
                inputs_digest: digest.clone(),
                input_references: [
                    proof.failed_inputs.reference.clone(),
                    proof.successful_inputs.reference.clone(),
                ],
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "ci_exception_tests.rs"]
mod tests;
