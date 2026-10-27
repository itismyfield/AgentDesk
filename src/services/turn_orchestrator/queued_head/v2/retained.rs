use super::*;

impl Owner {
    pub(crate) fn tick(&mut self, now: u64) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        if next.head_injection.as_ref().is_some_and(|head| {
            head.phase != HeadPhase::Reserved
                && (head.confirmation_deadline_ms.map_or(
                    head.phase == HeadPhase::Unknown
                        || !matches!(head.payload_window, Some(PayloadWindow::Bootstrap)),
                    |deadline| now >= deadline,
                ))
        }) {
            let input = next
                .head_injection
                .take()
                .ok_or("active head disappeared")?;
            remove_first(&mut next, &input.identity)?;
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
            if active {
                next.head_injection = None;
                remove_first(&mut next, &key.identity)?;
            }
            if let Some(at) = retained {
                next.retained_inputs.remove(at);
            }
            next.terminal_identities.push(TerminalIdentity {
                identity: key.identity.clone(),
                settled_at_ms: now,
            });
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
        remove_first(&mut next, &key.identity)?;
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

fn remove_first(snapshot: &mut SnapshotV2, identity: &QueueIdentity) -> Result<(), String> {
    if !snapshot
        .items
        .first()
        .is_some_and(|item| sources::identity(item) == *identity)
    {
        return Err("queue first item differs from selected head".into());
    }
    snapshot.items.remove(0);
    Ok(())
}
