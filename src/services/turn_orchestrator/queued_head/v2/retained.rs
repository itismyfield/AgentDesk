use super::*;
use crate::db::turns::TerminalOutcome;

impl Owner {
    /// Positive no-content ends this attempt; the original keeps its queue slot and receipt.
    /// Only the distinct successor may acquire a new effect permit.
    #[cfg(test)]
    pub(crate) fn successor_not_sent(
        &mut self,
        key: &AttemptRef,
        proof: crate::services::claude_tui::owner_input::NotSentProof,
        nonce: String,
    ) -> Result<AttemptRef, String> {
        proof.into_reason(&serde_json::to_value(key).map_err(|e| e.to_string())?)?;
        if nonce.is_empty() || nonce == key.attempt.nonce {
            return Err("successor needs a distinct nonce".into());
        }
        let mut next = self.snapshot.clone();
        let head = next.head_injection.as_mut().ok_or("no live head")?;
        if !key.matches(head) || !super::front_holds(&next.items, head) {
            return Err("stale no-content successor".into());
        }
        let mut successor = key.clone();
        successor.attempt.nonce = nonce;
        head.attempt = Some(successor.attempt.clone());
        head.phase = HeadPhase::Reserved;
        head.confirmation_deadline_ms = None;
        head.payload_window = None;
        head.resolved_source = None;
        head.resolved_target = None;
        head.preparation_hold = None;
        // Every member keeps its ticket and moves to the successor together.
        for bind in &mut next.admission_binds {
            if let BindAt::Attempt { attempt, .. } = &mut bind.at
                && *attempt == key.attempt
            {
                *attempt = successor.attempt.clone();
            }
        }
        self.commit(next)?;
        Ok(successor)
    }

    /// When `tick` next retains the head: its confirmation deadline, at once (0) for a head it
    /// retains without one, `None` while the head is reserved, bootstrapping or absent.
    pub(crate) fn settle_due(&self) -> Option<u64> {
        let head = self.snapshot.head_injection.as_ref()?;
        if head.phase == HeadPhase::Reserved {
            return None;
        }
        let bootstrapping = matches!(head.payload_window, Some(PayloadWindow::Bootstrap));
        match head.confirmation_deadline_ms {
            Some(deadline) => Some(deadline),
            None => (head.phase == HeadPhase::Unknown || !bootstrapping).then_some(0),
        }
    }

    pub(crate) fn tick(&mut self, now: u64) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        if self.settle_due().is_some_and(|due| now >= due) {
            let input = next
                .head_injection
                .take()
                .ok_or("active head disappeared")?;
            remove_members(&mut next, &input)?;
            next.retained_inputs.push(RetainedInput {
                input,
                reason: RetainedReason::Unconfirmed,
                disposition: None,
                notice: NoticeIntent {
                    kind: "unconfirmed".into(),
                    enqueued: false,
                },
            });
        }
        super::migration::anchor_terminals(&mut next, now);
        super::source_admission::expire_binds(&mut next, now);
        next.terminal_identities
            .retain(|terminal| now.saturating_sub(terminal.settled_at_ms) < 86_400_000);
        self.commit(next)?;
        self.project_slots();
        Ok(())
    }

    pub(crate) fn settle(
        &mut self,
        key: &AttemptRef,
        delivered: bool,
        now: u64,
    ) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        let active = next
            .head_injection
            .as_ref()
            .is_some_and(|head| key.matches(head));
        let retained = next
            .retained_inputs
            .iter()
            .position(|entry| key.matches(&entry.input));
        if active
            && next
                .head_injection
                .as_ref()
                .is_some_and(|head| head.phase == HeadPhase::Reserved)
        {
            return Err("effect not armed".into());
        }
        if !active && retained.is_none() {
            return Err("stale exact attempt".into());
        }
        if delivered {
            super::source_admission::settle_binds(&mut next, key, now);
            let mut members = Vec::new();
            if active && let Some(input) = next.head_injection.take() {
                remove_members(&mut next, &input)?;
                members.extend(input.members().map(sources::identity));
            }
            if let Some(at) = retained {
                let entry = next.retained_inputs.remove(at);
                members.extend(entry.input.members().map(sources::identity));
            }
            // A local submit settles the whole unit in this one transition.
            for identity in members {
                next.terminal_identities.push(TerminalIdentity {
                    identity,
                    settled_at_ms: now,
                });
            }
        } else if active {
            next.head_injection
                .as_mut()
                .ok_or("active head disappeared")?
                .phase = HeadPhase::Unknown;
        }
        self.commit(next)?;
        self.project_slots();
        Ok(())
    }

    /// A refusal before any key that a later look may clear: the attempt ends unretained and the
    /// original keeps its queue slot and ticket for the next wake.
    pub(crate) fn hold_not_sent(
        &mut self,
        key: &AttemptRef,
        proof: crate::services::claude_tui::owner_input::NotSentProof,
    ) -> Result<(), String> {
        proof.into_reason(&serde_json::to_value(key).map_err(|e| e.to_string())?)?;
        let mut next = self.snapshot.clone();
        let head = next.head_injection.take_if(|head| key.matches(head));
        let message = head.ok_or("stale held attempt")?.original.message_id;
        let attempt = &key.attempt;
        (next.admission_binds)
            .retain(|b| !matches!(&b.at, BindAt::Attempt { attempt: a, .. } if a == attempt));
        self.commit(next)?;
        self.held = Some(message);
        // A proven-unsent paste can never be observed, so its ledger entry is forgotten.
        let (provider, owner) = (&attempt.provider, &attempt.target.owner);
        crate::services::tui_prompt_dedupe::withdraw_injected_steer(
            provider,
            owner,
            &attempt.nonce,
        );
        Ok(())
    }

    pub(crate) fn retain_not_sent(
        &mut self,
        key: &AttemptRef,
        proof: crate::services::claude_tui::owner_input::NotSentProof,
    ) -> Result<(), String> {
        let reason = proof.into_reason(&serde_json::to_value(key).map_err(|e| e.to_string())?)?;
        let mut next = self.snapshot.clone();
        if !next.head_injection.as_ref().is_some_and(|h| key.matches(h)) {
            return Err("stale NotSent attempt".into());
        }
        let mut input = next.head_injection.take().ok_or("missing NotSent input")?;
        remove_members(&mut next, &input)?;
        input.phase = HeadPhase::Reserved;
        // No later look submits a permanent refusal, so its origin's execution has failed.
        let permanent = PERMANENT.contains(&reason.as_str());
        if permanent {
            next.owe_attempt(&input, TerminalOutcome::Failed, super::origin::now_ms());
        }
        let members: Vec<PendingQueueItem> = input.members().cloned().collect();
        next.retained_inputs.push(RetainedInput {
            input,
            reason: RetainedReason::Rejected {
                reason: format!("known_not_sent: {reason}"),
                start_failure: None,
            },
            disposition: Some(RetainedDisposition::KnownNotSent),
            notice: NoticeIntent {
                kind: "retained_retry_required".into(),
                enqueued: false,
            },
        });
        if permanent {
            let dropped = super::effort_continue::Dropped::Only(&members);
            super::effort_continue::close_dropped(&mut next, dropped);
        }
        self.commit(next)?;
        self.project_slots();
        Ok(())
    }
    pub(crate) fn project_slots(&self) {
        if self.fault == Fault::BeforeProjection {
            return;
        }
        for entry in &self.snapshot.retained_inputs {
            let Some(attempt) = &entry.input.attempt else {
                continue;
            };
            let (provider, owner) = (&attempt.provider, &attempt.target.owner);
            // A proven-unsent paste can never be observed, so its ledger entry is forgotten.
            if matches!(entry.disposition, Some(RetainedDisposition::KnownNotSent)) {
                crate::services::tui_prompt_dedupe::withdraw_injected_steer(
                    provider,
                    owner,
                    &attempt.nonce,
                );
            } else {
                crate::services::tui_prompt_dedupe::retain_injected_steer(
                    provider,
                    owner,
                    &attempt.nonce,
                );
            }
        }
    }

    pub(crate) fn notice_enqueued(&mut self, key: &AttemptRef) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        let entry = next
            .retained_inputs
            .iter_mut()
            .find(|entry| key.matches(&entry.input))
            .ok_or("stale notice attempt")?;
        if entry.notice.enqueued {
            return Ok(());
        }
        entry.notice.enqueued = true;
        // A told retryable proven-unsent input is never resubmitted; a permanent one owed at retain.
        let known = matches!(entry.disposition, Some(RetainedDisposition::KnownNotSent));
        let input = entry.input.clone();
        if known && !permanently_refused(entry) {
            next.owe_attempt(&input, TerminalOutcome::Failed, super::origin::now_ms());
        }
        if known {
            // A told proven-unsent continuation is never resubmitted either: its barrier closes.
            let members: Vec<PendingQueueItem> = input.members().cloned().collect();
            let dropped = super::effort_continue::Dropped::Only(&members);
            super::effort_continue::close_dropped(&mut next, dropped);
        }
        self.commit(next)
    }
}

impl RetainedInput {
    /// An enqueued notice ends ownership for retry-required known/opaque unsent, disposition-free
    /// unconfirmed, or cleared disposition-free unconfirmed evidence; other inputs remain owed.
    pub(crate) fn owed_to_owner(&self) -> bool {
        !(self.notice.enqueued
            && (matches!(
                (&self.disposition, self.notice.kind.as_str()),
                (
                    Some(RetainedDisposition::KnownNotSent | RetainedDisposition::OpaqueUnsent),
                    "retained_retry_required"
                ) | (None, "unconfirmed")
            ) || (self.disposition.is_none()
                && self.notice.kind == CLEARED_NOTICE
                && matches!(self.reason, RetainedReason::Unconfirmed))))
    }

    /// Whether this unowed retention closes an attempt bind: one of its attempt naming the input or
    /// any member it carried, matched member by member as a delivered settle would settle it.
    pub(crate) fn closes_bind(&self, identity: &QueueIdentity, attempt: &AttemptIdentity) -> bool {
        !self.owed_to_owner()
            && self.input.attempt.as_ref() == Some(attempt)
            && (self.input.identity == *identity
                || (self.input.members()).any(|member| sources::identity(member) == *identity))
    }
}

/// Refusals that no later look can turn into a submit; their originals stay retained.
const PERMANENT: [&str; 2] = ["vetoed: frame_mismatch", "vetoed: InvalidInput"];

/// Whether the owner refused a retained input for a reason no later look clears.
pub(super) fn permanently_refused(entry: &RetainedInput) -> bool {
    matches!(&entry.reason, RetainedReason::Rejected { reason, .. }
        if reason.strip_prefix("known_not_sent: ").is_some_and(|r| PERMANENT.contains(&r)))
}

/// Whether a retained input was refused before any key for a reason a later look may clear. One
/// whose resend ask already went out stays retained: resending it is now the person's choice.
pub(super) fn requeueable(entry: &RetainedInput, terminal: &[TerminalIdentity]) -> bool {
    let RetainedReason::Rejected { reason, .. } = &entry.reason else {
        return false;
    };
    let refused = reason.strip_prefix("known_not_sent: ");
    !entry.notice.enqueued
        && matches!(entry.disposition, Some(RetainedDisposition::KnownNotSent))
        && entry.input.phase == HeadPhase::Reserved
        && entry.input.attempt.is_some()
        && refused.is_some_and(|refused| !PERMANENT.contains(&refused))
        && !(entry.input.members().map(sources::identity))
            .any(|member| terminal.iter().any(|t| t.identity == member))
}

impl SnapshotV2 {
    /// Puts each proven-unsent retained input `pick` names back in the queue, original then
    /// followers, behind any attempted head; its unsent notice leaves with the entry.
    pub(super) fn requeue_known_not_sent(
        &mut self,
        pick: impl Fn(&RetainedInput) -> bool,
    ) -> Vec<RetainedInput> {
        let at = self
            .head_injection
            .as_ref()
            .map_or(0, |head| head.members().count());
        let mut moved = Vec::new();
        let mut index = 0;
        while index < self.retained_inputs.len() {
            let entry = &self.retained_inputs[index];
            let slot = at + moved.iter().map(|(_, n)| n).sum::<usize>();
            let members: Vec<PendingQueueItem> = entry.input.members().cloned().collect();
            let found = |m: &PendingQueueItem| {
                let id = sources::identity(m);
                let at = self
                    .items
                    .iter()
                    .position(|item| sources::identity(item) == id);
                at.map(|at| (at, &self.items[at]))
            };
            // A member the head or an earlier requeue holds, or queued under another body, is a
            // conflict this cannot settle; an equal untried copy merges into its slot.
            let conflict = members.iter().any(|m| {
                found(m).is_some_and(|(at, q)| at < slot || !source_admission::same_original(q, m))
            });
            if !pick(entry) || !requeueable(entry, &self.terminal_identities) || conflict {
                index += 1;
                continue;
            }
            let entry = self.retained_inputs.remove(index);
            let mut tail = self.items.split_off(slot);
            for member in &members {
                let id = sources::identity(member);
                match tail.iter().position(|item| sources::identity(item) == id) {
                    Some(copy) => self.items.push(tail.remove(copy)),
                    None => self.items.push(member.clone()),
                }
            }
            self.items.append(&mut tail);
            self.rebind_queued(&entry.input);
            moved.push((entry, members.len()));
        }
        moved.into_iter().map(|(entry, _)| entry).collect()
    }
}

impl SnapshotV2 {
    /// A told entry stays retained, so an equal untried copy of its member leaves the queue from
    /// `from` on, its tickets bound to the entry's attempt; returns whether any copy left.
    fn isolate_told_copies(&mut self, from: usize) -> bool {
        let told: Vec<(PendingQueueItem, AttemptIdentity)> = (self.retained_inputs.iter())
            .filter(|entry| entry.notice.enqueued)
            .filter_map(|entry| {
                Some((entry.input.members().cloned(), entry.input.attempt.clone()?))
            })
            .flat_map(|(members, attempt)| members.map(move |m| (m, attempt.clone())))
            .collect();
        let (mut at, before) = (from, self.items.len());
        while at < self.items.len() {
            let item = &self.items[at];
            let Some((_, attempt)) = told
                .iter()
                .find(|(m, _)| source_admission::same_original(item, m))
            else {
                at += 1;
                continue;
            };
            let identity = sources::identity(item);
            for bind in &mut self.admission_binds {
                if matches!(bind.at, BindAt::Queued { index, .. } if index == at) {
                    let (identity, attempt) = (identity.clone(), attempt.clone());
                    bind.at = BindAt::Attempt { identity, attempt };
                }
            }
            self.items.remove(at);
            self.unbind_item(at);
        }
        self.items.len() != before
    }
}

impl Owner {
    /// Requeues every proven-unsent retained input, and takes a told input's queued copies out, in
    /// one commit; returns the entries it moved, and nothing moves when the commit fails.
    pub(crate) fn requeue_known_not_sent(&mut self) -> Result<Vec<RetainedInput>, String> {
        let mut next = self.snapshot.clone();
        let from = (next.head_injection.as_ref()).map_or(0, |head| head.members().count());
        let isolated = next.isolate_told_copies(from);
        let moved = next.requeue_known_not_sent(|_| true);
        if moved.is_empty() && !isolated {
            return Ok(moved);
        }
        if next.head_injection.is_none() {
            self.revoke_front();
        }
        self.commit(next)?;
        self.project_slots();
        Ok(moved)
    }
}

/// Removes every member of `input` from the queue front in one snapshot; the tail keeps its
/// originals and tickets and only moves up.
fn remove_members(snapshot: &mut SnapshotV2, input: &InputAttempt) -> Result<(), String> {
    if !super::front_holds(&snapshot.items, input) {
        return Err("queue first item differs from selected head".into());
    }
    for _ in input.members() {
        snapshot.items.remove(0);
        snapshot.unbind_item(0);
    }
    Ok(())
}

pub(super) const CLEARED_NOTICE: &str = "cleared";
