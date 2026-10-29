use super::*;

pub(super) fn identity(item: &PendingQueueItem) -> QueueIdentity {
    let item = super::super::super::pending_queue_persistence::pending_queue_item_to_intervention(
        item.clone(),
        std::time::SystemTime::now(),
        std::time::Instant::now(),
    );
    QueueIdentity::of(&item)
}

impl Owner {
    pub(super) fn fenced(&self, item: &PendingQueueItem) -> bool {
        let wanted = identity(item);
        let mut known = self
            .snapshot
            .items
            .iter()
            .map(identity)
            .chain(
                (self.snapshot.retained_inputs.iter())
                    .flat_map(|e| e.input.members().skip(1).map(identity))
                    .chain(
                        self.snapshot
                            .retained_inputs
                            .iter()
                            .map(|e| e.input.identity.clone()),
                    ),
            )
            .chain(
                self.snapshot
                    .terminal_identities
                    .iter()
                    .map(|e| e.identity.clone()),
            );
        known.any(|old| {
            old.sources.iter().any(|a| {
                wanted.sources.iter().any(|b| {
                    a.message_id == b.message_id
                        && crate::services::tui_input::input_key::is_discord_key(a.message_id)
                })
            })
        })
    }
}

pub(super) fn restore(raw: &str, namespace: &str) -> Result<SnapshotV2, String> {
    let mut snapshot = match super::super::compat::inspect(raw)? {
        super::super::compat::SnapshotRead::ReadOnlyV2(snapshot) => *snapshot,
        super::super::compat::SnapshotRead::Legacy(items, meta) => {
            super::migration::legacy(items, meta)
        }
    };
    if snapshot
        .namespace
        .as_deref()
        .is_some_and(|old| old != namespace)
    {
        return Err("queue namespace mismatch".into());
    }
    for input in snapshot
        .head_injection
        .iter()
        .chain(snapshot.retained_inputs.iter().map(|entry| &entry.input))
    {
        if identity(&input.original) != input.identity {
            return Err("original source identity mismatch".into());
        }
    }
    // Every member of the head, not only the first, must be the queue front in order.
    if let Some(head) = &snapshot.head_injection
        && !super::front_holds(&snapshot.items, head)
    {
        return Err("active original differs from queue head".into());
    }
    snapshot.namespace = Some(namespace.into());
    // Released unarmed, every member still holds the queued bind its reserve kept.
    snapshot
        .head_injection
        .take_if(|head| head.phase == HeadPhase::Reserved);
    Ok(snapshot)
}

/// Parked without positive evidence of no attempt is retained, never rejected or retried.
pub(super) fn migrate_parked(
    owner: &mut Owner,
    item: &PendingQueueItem,
    proven_unattempted: bool,
) -> Result<(), AdmitError> {
    if owner.pending.is_some() {
        return Err(AdmitError::NotCommitted("owner recovery required".into()));
    }
    if owner.fenced(item) {
        return Ok(());
    }
    if proven_unattempted {
        owner.enqueue(item)?;
        return Ok(());
    }
    let mut next = owner.snapshot.clone();
    next.retained_inputs.push(RetainedInput {
        input: InputAttempt {
            identity: identity(item),
            original: item.clone(),
            phase: HeadPhase::Unknown,
            attempt: None,
            payload: None,
            confirmation_deadline_ms: None,
            resolved_source: None,
            payload_window: None,
            resolved_target: None,
            preparation_hold: None,
            legacy_attempt: None,
        },
        reason: RetainedReason::Unconfirmed,
        disposition: None,
        notice: NoticeIntent {
            kind: "unconfirmed".into(),
            enqueued: false,
        },
    });
    owner.admit_commit(next, None)
}

impl Owner {
    pub(crate) fn prepare_history(
        &self,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
    ) -> Result<(), String> {
        let input = self
            .snapshot
            .head_injection
            .as_ref()
            .ok_or("no selected input")?;
        history.prepare(
            self.snapshot
                .namespace
                .as_deref()
                .ok_or("missing namespace")?,
            &serde_json::to_value(input).map_err(|e| e.to_string())?,
        )
    }
    pub(crate) fn bind_source(
        &mut self,
        key: &super::AttemptRef,
        source: &crate::services::tui_input::human_frame::history::v2::Source,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
    ) -> Result<(), String> {
        let encoded = serde_json::to_value(key).map_err(|e| e.to_string())?;
        source.validate(&encoded)?;
        let mut next = self.snapshot.clone();
        let input = if next.head_injection.as_ref().is_some_and(|h| key.matches(h)) {
            next.head_injection.as_mut().ok_or("head disappeared")?
        } else {
            &mut next
                .retained_inputs
                .iter_mut()
                .find(|r| key.matches(&r.input))
                .ok_or("stale source bind")?
                .input
        };
        let value = serde_json::to_value(source).map_err(|e| e.to_string())?;
        if input
            .resolved_source
            .as_ref()
            .is_some_and(|old| *old != value)
        {
            return Err("source already bound".into());
        }
        input.resolved_source = Some(value);
        self.commit(next)?;
        history.bind(
            self.snapshot
                .namespace
                .as_deref()
                .ok_or("missing namespace")?,
            &encoded,
            source,
        )
    }
    pub(crate) fn sync_history(
        &self,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
    ) -> Result<(), String> {
        for input in self
            .snapshot
            .head_injection
            .iter()
            .chain(self.snapshot.retained_inputs.iter().map(|r| &r.input))
        {
            if input.attempt.is_some() && input.payload.is_some() {
                history.prepare(
                    self.snapshot
                        .namespace
                        .as_deref()
                        .ok_or("missing namespace")?,
                    &serde_json::to_value(input).map_err(|e| e.to_string())?,
                )?;
            }
        }
        history.project(&self.json()?)
    }
}
