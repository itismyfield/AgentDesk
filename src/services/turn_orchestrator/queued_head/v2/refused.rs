//! An input refused for good leaves the owner without its body. The record that stays blocks a
//! second admission of its sources and owes one notice; nothing puts it back in the queue.
use super::*;
use crate::db::turns::TerminalOutcome;

/// The notice a readiness wait owes once it ended without evidence that the pane takes input.
const READINESS_UNPROVEN: &str = "readiness_unproven";

impl Owner {
    /// Ends the head for good on a proof that no key was sent: every member leaves the queue, its
    /// origin's execution fails, and only identities, fingerprints and the notice are kept.
    pub(crate) fn refuse_not_sent(
        &mut self,
        key: &AttemptRef,
        proof: crate::services::claude_tui::owner_input::NotSentProof,
    ) -> Result<(), String> {
        let reason = proof.into_reason(&serde_json::to_value(key).map_err(|e| e.to_string())?)?;
        let mut next = self.snapshot.clone();
        let head = next.head_injection.take_if(|head| key.matches(head));
        let input = head.ok_or("stale refused attempt")?;
        retained::remove_members(&mut next, &input)?;
        let now = origin::now_ms();
        next.owe_attempt(&input, TerminalOutcome::Failed, now);
        let members: Vec<PendingQueueItem> = input.members().cloned().collect();
        if let Some(intent) = next.effort_restart.as_mut()
            && intent.continuation.as_ref().is_some_and(|c| {
                members
                    .iter()
                    .any(|item| source_admission::same_original(item, &c.original))
            })
        {
            intent.cancelled = true;
            intent.continuation = None;
        }
        // The attempt's tickets end with it; their receipts keep a retry of them a duplicate.
        let attempt = &key.attempt;
        (next.admission_binds)
            .retain(|b| !matches!(&b.at, BindAt::Attempt { attempt: a, .. } if a == attempt));
        // The fingerprint is the digest a receipt carries for the same original.
        let member = |item: &PendingQueueItem| RefusedMember {
            identity: sources::identity(item),
            fingerprint: source_admission::digest(&source_admission::canonical(item)),
        };
        next.refused_inputs.push(RefusedInput {
            input: key.clone(),
            members: members.iter().map(member).collect(),
            reason,
            notice: NoticeIntent {
                kind: READINESS_UNPROVEN.into(),
                enqueued: false,
            },
            refused_at_ms: now,
        });
        self.commit(next)?;
        // A proven-unsent paste can never be observed, so its ledger entry is forgotten.
        let (provider, owner) = (&attempt.provider, &attempt.target.owner);
        crate::services::tui_prompt_dedupe::withdraw_injected_steer(
            provider,
            owner,
            &attempt.nonce,
        );
        self.project_slots();
        Ok(())
    }

    /// Marks a refusal's notice sent; `None` when `key` names no refusal.
    pub(super) fn refusal_told(&mut self, key: &AttemptRef) -> Option<Result<(), String>> {
        let mut next = self.snapshot.clone();
        let entry = next.refused_inputs.iter_mut().find(|r| r.input == *key)?;
        if entry.notice.enqueued {
            return Some(Ok(()));
        }
        entry.notice.enqueued = true;
        Some(self.commit(next))
    }
}

impl SnapshotV2 {
    /// A told refusal is forgotten after the window a delivered identity is kept for.
    pub(super) fn forget_told_refusals(&mut self, now: u64) {
        self.refused_inputs
            .retain(|r| !r.notice.enqueued || now.saturating_sub(r.refused_at_ms) < 86_400_000);
    }
}
