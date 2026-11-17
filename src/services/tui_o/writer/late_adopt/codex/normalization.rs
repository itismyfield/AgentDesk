//! Reevaluate the original execution without renewing its total deadline or resubmitting input.
use crate::services::tui_o::channel_policy::adoption::output_disposition::{Output, Phase, Reason};
use crate::services::tui_o::channel_policy::{Candidate, FirstProof};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::writer::late_adopt::codex::prefix::{self, Verdict};

pub(crate) async fn run(
    output: &Output,
    candidate: &Candidate,
    source: &SourceId,
) -> Result<FirstProof, Reason> {
    loop {
        if output.expire() {
            return Err(Reason::Deadline);
        }
        if output.phase() != Phase::Normalizing {
            return Err(Reason::Invalidated);
        }
        let proof = {
            let guard = candidate.lock();
            let proof = candidate.first_proof(source);
            drop(guard);
            proof
        };
        let proof = match proof {
            Ok(Some(proof)) => proof,
            _ => {
                output.reject(Reason::Invalidated);
                return Err(Reason::Invalidated);
            }
        };
        match prefix::buffered(source) {
            Verdict::Refuse(_) => {
                output.reject(Reason::SourceRejected);
                return Err(Reason::SourceRejected);
            }
            Verdict::Eligible(prefix)
                if prefix.prompt_digest.as_ref() == Some(&proof.digest)
                    && prefix.prompt_end.is_some() =>
            {
                if output.adopt(&proof).is_ok() {
                    return Ok(proof);
                }
                if output.take_repair() {
                    // This publisher rechecks launch context, owner, nonce, pinned root and source creation.
                    let _ = crate::services::codex_tui::session::publish_discovered_launch(
                        &proof.context,
                        &source.path,
                    );
                    continue;
                }
            }
            Verdict::Eligible(_) => {
                output.reject(Reason::SourceRejected);
                return Err(Reason::SourceRejected);
            }
            Verdict::Wait(_) => {}
        }
        let pause = output
            .remaining()
            .min(tokio::time::Duration::from_millis(100));
        tokio::time::sleep(pause).await;
    }
}
