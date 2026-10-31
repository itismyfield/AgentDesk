use super::*;

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

    /// A modal refused the input before any key: the attempt ends unretained and the original
    /// keeps its queue slot and ticket for the release wake.
    pub(crate) fn hold_not_sent(
        &mut self,
        key: &AttemptRef,
        proof: crate::services::claude_tui::owner_input::NotSentProof,
    ) -> Result<(), String> {
        proof.into_reason(&serde_json::to_value(key).map_err(|e| e.to_string())?)?;
        let mut next = self.snapshot.clone();
        (next.head_injection.take_if(|head| key.matches(head))).ok_or("stale held attempt")?;
        let attempt = &key.attempt;
        (next.admission_binds)
            .retain(|b| !matches!(&b.at, BindAt::Attempt { attempt: a, .. } if a == attempt));
        self.commit(next)
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
        next.retained_inputs.push(RetainedInput {
            input,
            reason: RetainedReason::Rejected {
                reason: format!("known_not_sent: {reason}"),
            },
            disposition: Some(RetainedDisposition::KnownNotSent),
            notice: NoticeIntent {
                kind: "retained_retry_required".into(),
                enqueued: false,
            },
        });
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
        entry.notice.enqueued = true;
        self.commit(next)
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
