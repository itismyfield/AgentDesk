//! Reevaluate the original execution without renewing its total deadline or resubmitting input.
use crate::services::tui_o::channel_policy::output_disposition::{Output, Phase, Reason};
use crate::services::tui_o::channel_policy::{Adoption, Candidate, FirstHold, FirstProof};
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

/// Looks for the admitted launch's trust menu until a rollout proves the admission, the
/// admission ends or its one repair is spent; the menu action is never adoption evidence.
pub(crate) async fn watch_trust(
    candidate: Candidate,
    context: crate::services::tui_prompt_dedupe::binding_context::BindingContext,
    cwd: std::path::PathBuf,
) {
    use crate::services::codex_tui::{host_input::trust::TrustHost, normalize};
    while let Some(output) = candidate.first_trust_window(&context) {
        let (look, view, at) = (output.clone(), context.clone(), cwd.clone());
        let looked = tokio::task::spawn_blocking(move || {
            normalize::repair_if_shown(&look, &view, &at, &mut TrustHost)
        });
        if let Ok(Some(result)) = looked.await {
            tracing::info!(
                channel = output.id().channel,
                ?result,
                "codex trust repair ran"
            );
            return;
        }
        let pause = output.remaining().min(TRUST_LOOK);
        if pause.is_zero() {
            return;
        }
        tokio::time::sleep(pause).await;
    }
}

/// How often the admitted launch's pane is looked at for a trust menu.
const TRUST_LOOK: tokio::time::Duration = tokio::time::Duration::from_millis(500);

/// Holds a body claim of the admitted first turn until its output is adopted or rejected: the
/// takeover's own commit adopts it, and the one budget from the admission ends in a rejection.
pub(crate) async fn hold(candidate: &Candidate, nonce: Option<[u8; 32]>) {
    let Some(nonce) = nonce else { return };
    let mut changes = candidate.first_changes();
    loop {
        let until = match candidate.first_hold(nonce) {
            None => return,
            Some(FirstHold::Until(deadline)) => deadline,
            Some(FirstHold::Output(held)) => {
                let (output, proof) = *held;
                let root = crate::config::runtime_root();
                if let Some(store) = root.as_deref().and_then(OStore::existing) {
                    let _ = run(&output, candidate, &proof, &store).await;
                    return;
                }
                tokio::time::Instant::now() + output.remaining()
            }
        };
        // No rollout or store can adopt it yet: either may arrive within the same budget.
        if until <= tokio::time::Instant::now() {
            return candidate.expire_first(nonce);
        }
        tokio::select! {
            _ = changes.changed() => {}
            () = tokio::time::sleep_until(until) => {}
        }
    }
}
