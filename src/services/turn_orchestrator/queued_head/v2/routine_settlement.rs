//! Settles a routine's exact admission at the delivery deadline or on lease recovery.
use super::{source_admission as admission, *};
use crate::db::turns::TerminalOutcome;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Withdrawn,
    Unconfirmed,
    Submitted,
}

impl Owner {
    pub(super) fn routine_context(&self, key: &AttemptRef) -> Option<&Value> {
        self.snapshot.admission_binds.iter().find_map(|bind| {
            let BindAt::Attempt { identity, attempt } = &bind.at else {
                return None;
            };
            let context = bind.context.as_ref()?.execution.as_ref()?;
            (*identity == key.identity && *attempt == key.attempt && context["origin"] == "routine")
                .then_some(context)
        })
    }

    /// Commits the approval before the leaf can send a key, so recovery can distinguish a
    /// previously approved writer from a bootstrap that never received permission.
    pub(super) fn approve_routine_first_key(
        &mut self,
        key: &AttemptRef,
        now: &dyn Fn() -> u64,
    ) -> Result<(), String> {
        let head = self
            .snapshot
            .head_injection
            .as_ref()
            .filter(|head| key.matches(head))
            .ok_or("stale first key attempt")?;
        if head.routine_delivery != Some(RoutineDelivery::AwaitingKey) {
            return Ok(());
        }
        if self
            .routine_context(key)
            .and_then(|context| context[ROUTINE_DELIVERY_DEADLINE_FIELD].as_u64())
            .is_some_and(|deadline| now() >= deadline)
        {
            return Err("routine_delivery_deadline".into());
        }
        let mut next = self.snapshot.clone();
        next.head_injection
            .as_mut()
            .ok_or("missing routine attempt")?
            .routine_delivery = Some(RoutineDelivery::Approved);
        self.commit(next)
    }

    pub(super) fn routine_delivery_closed(&self, key: &AttemptRef) -> bool {
        self.snapshot.head_injection.as_ref().is_some_and(|head| {
            key.matches(head) && head.routine_delivery == Some(RoutineDelivery::Closed)
        })
    }

    fn settle_routine(
        &mut self,
        (namespace, ticket): (&str, Ticket),
        original: &PendingQueueItem,
        context: &Value,
        committed: bool,
        now: u64,
    ) -> Result<Disposition, String> {
        if self.pending.is_some() {
            self.recover()?;
        }
        if self.snapshot.namespace.as_deref() != Some(namespace) {
            return Err("owner namespace mismatch".into());
        }
        let receipt = ticket.receipt(original);
        let context_digest = admission::digest(context);
        let binds = || {
            self.snapshot
                .admission_binds
                .iter()
                .filter(|bind| bind.receipt == receipt)
        };
        if let Some(saved) = self.snapshot.routine_withdrawals.get(&receipt) {
            if saved.context_digest != context_digest {
                return Err("withdrawal context mismatch".into());
            }
            let disposition = if saved.unconfirmed {
                Disposition::Unconfirmed
            } else {
                Disposition::Withdrawn
            };
            if self
                .front_grant
                .as_ref()
                .or(self.front_claim.as_ref())
                .is_some_and(|front| admission::same_original(&front.1, original))
            {
                self.revoke_front();
                self.held = None;
            }
            return Ok(disposition);
        }
        let turn = self.exact_turn(&ticket, original)?;
        if let Some(saved) = Self::context_in(&self.snapshot, &receipt) {
            if saved.execution.as_ref() != Some(context) {
                return Err("owner execution context mismatch".into());
            }
        } else if !matches!(
            turn,
            admission::TurnBind::Absent | admission::TurnBind::Unknown
        ) {
            return Err("owner execution context missing".into());
        }
        if matches!(turn, admission::TurnBind::Settled(_)) {
            return Ok(Disposition::Submitted);
        }

        let queued = binds().find_map(|bind| match &bind.at {
            BindAt::Queued { index, item }
                if self.snapshot.queued_digest(*index).as_ref() == Some(item) =>
            {
                Some(*index)
            }
            _ => None,
        });
        let matches = |head: &InputAttempt| admission::same_original(&head.original, original);
        if let Some(head) = self
            .snapshot
            .head_injection
            .as_ref()
            .filter(|head| matches(head))
        {
            if head.members().count() != 1 || queued != Some(0) {
                return Err("routine attempt does not own its exact original".into());
            }
            if head.phase != HeadPhase::Reserved
                && head.routine_delivery != Some(RoutineDelivery::AwaitingKey)
            {
                let mut next = self.snapshot.clone();
                let head = next
                    .head_injection
                    .as_mut()
                    .ok_or("missing routine attempt")?;
                if head.routine_delivery != Some(RoutineDelivery::Closed) {
                    head.routine_delivery = Some(RoutineDelivery::Closed);
                    self.commit(next)?;
                }
                return Ok(Disposition::Unconfirmed);
            }
            let mut next = self.snapshot.clone();
            let head = next
                .head_injection
                .take()
                .ok_or("missing routine attempt")?;
            next.rebind_queued(&head);
            next.take_cancelled(0, now);
            self.finish_routine_withdrawal(next, receipt, (context_digest, false), true)?;
            if let Some(attempt) = head.attempt {
                crate::services::tui_prompt_dedupe::withdraw_injected_steer(
                    &attempt.provider,
                    &attempt.target.owner,
                    &attempt.nonce,
                );
            }
            return Ok(Disposition::Withdrawn);
        }

        let retained = self
            .snapshot
            .retained_inputs
            .iter()
            .position(|entry| matches(&entry.input));
        if let Some(index) = retained {
            let entry = &self.snapshot.retained_inputs[index];
            if !matches!(
                entry.disposition,
                Some(RetainedDisposition::KnownNotSent | RetainedDisposition::OpaqueUnsent)
            ) {
                return Ok(Disposition::Unconfirmed);
            }
            if entry.input.members().count() != 1
                || Self::context_in(&self.snapshot, &receipt).is_none()
            {
                return Err("retained routine does not own its exact original".into());
            }
            let mut next = self.snapshot.clone();
            let entry = next.retained_inputs.remove(index);
            if control::owes_cancelled(&entry) {
                next.owe_attempt(&entry.input, TerminalOutcome::Cancelled, now);
            }
            next.rebind_queued(&entry.input);
            if let Some(index) = next
                .items
                .iter()
                .position(|item| admission::same_original(item, original))
            {
                next.take_cancelled(index, now);
            }
            self.finish_routine_withdrawal(
                next,
                receipt,
                (context_digest, false),
                queued == Some(0),
            )?;
            return Ok(Disposition::Withdrawn);
        }

        let mut next = self.snapshot.clone();
        if let Some(index) = queued {
            if index < control::untried(self)
                || !admission::same_original(&next.items[index], original)
            {
                return Err("queued routine does not own its exact original".into());
            }
            next.take_cancelled(index, now);
        } else if self
            .snapshot
            .items
            .iter()
            .any(|item| admission::same_original(item, original))
        {
            return Err("routine original without exact admission bind".into());
        }
        let unconfirmed =
            queued.is_none() && (committed || !matches!(turn, admission::TurnBind::Absent));
        self.finish_routine_withdrawal(
            next,
            receipt,
            (context_digest, unconfirmed),
            queued == Some(0),
        )?;
        if unconfirmed {
            // History may have aged out or been removed independently. The sealed ticket can no
            // longer admit, but absence supplies no positive proof about a previous write.
            return Ok(Disposition::Unconfirmed);
        }
        Ok(Disposition::Withdrawn)
    }

    fn finish_routine_withdrawal(
        &mut self,
        mut next: SnapshotV2,
        receipt: String,
        (context_digest, unconfirmed): (String, bool),
        revoke: bool,
    ) -> Result<(), String> {
        next.prune_readiness_holds();
        next.admission_binds.retain(|bind| bind.receipt != receipt);
        if !next.admission_receipts.contains(&receipt) {
            next.admission_receipts.push(receipt.clone());
        }
        next.routine_withdrawals.insert(
            receipt,
            RoutineWithdrawal {
                context_digest,
                unconfirmed,
            },
        );
        self.commit(next)?;
        if revoke {
            self.revoke_front();
            self.held = None;
        }
        Ok(())
    }
}

impl ChannelMailboxHandle {
    pub(crate) const ROUTINE_DELIVERY_DEADLINE: &str = ROUTINE_DELIVERY_DEADLINE_FIELD;

    pub(crate) async fn settle_routine_input(
        &self,
        (namespace, ticket): (String, String),
        original: PendingQueueItem,
        context: Value,
        committed: bool,
    ) -> Result<Disposition, String> {
        let ticket = Ticket::new(&ticket)?;
        self.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("owner not adopted")?;
            owner.settle_routine(
                (&namespace, ticket),
                &original,
                &context,
                committed,
                origin::now_ms(),
            )
        })
        .await
    }
}
