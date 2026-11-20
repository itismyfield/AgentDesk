//! Reevaluate the original execution without renewing its total deadline or resubmitting input.
use crate::services::tui_o::channel_policy::output_disposition::{Output, Phase, Reason};
use crate::services::tui_o::channel_policy::{Adoption, Candidate, FirstProof};
use crate::services::tui_o::store::OStore;
use crate::services::tui_o::writer::late_adopt::codex::prefix::{self, Verdict};

pub(crate) async fn run(
    output: &Output,
    candidate: &Candidate,
    original: &FirstProof,
    store: &OStore,
) -> Result<FirstProof, Reason> {
    let source = &original.source;
    let finish = |reason| output.finish_first(candidate, original, store, reason);
    let mut checked: Option<prefix::Prefix> = None;
    loop {
        if output.remaining().is_zero() {
            return finish(Reason::Deadline);
        }
        if output.phase() != Phase::Normalizing
            || matches!(
                candidate.peek(),
                Adoption::Committed | Adoption::Held | Adoption::Released
            )
        {
            return finish(Reason::Invalidated);
        }
        let proof = {
            let guard = candidate.lock();
            let proof = candidate.first_proof(source);
            drop(guard);
            proof
        };
        let proof = match proof {
            Ok(Some(proof)) if &proof == original => proof,
            _ => return finish(Reason::Invalidated),
        };
        if checked
            .as_ref()
            .is_some_and(|prefix| prefix.preserved_in(source).is_err())
        {
            return finish(Reason::SourceRejected);
        }
        match prefix::buffered(source) {
            Verdict::Refuse(_) => {
                return finish(Reason::SourceRejected);
            }
            Verdict::Eligible(prefix)
                if prefix.prompt_digest.as_ref() == Some(&proof.digest)
                    && prefix.prompt_end.is_some() =>
            {
                if checked.is_none() {
                    checked = Some(prefix.clone());
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
            Verdict::Eligible(prefix) if prefix.prompt_end.is_none() => {}
            Verdict::Eligible(_) => {
                return finish(Reason::SourceRejected);
            }
            Verdict::Wait(_) => {}
        }
        let pause = output
            .remaining()
            .min(tokio::time::Duration::from_millis(100));
        tokio::time::sleep(pause).await;
    }
}
