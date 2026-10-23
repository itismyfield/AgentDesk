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
                self.snapshot
                    .retained_inputs
                    .iter()
                    .map(|e| e.input.identity.clone()),
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
            let head = meta.as_ref().and_then(|meta| meta.head_injection.as_ref());
            let input = head
                .filter(|head| head.phase != HeadPhase::Reserved)
                .map(|head| InputAttempt {
                    identity: head.identity.clone(),
                    original: items
                        .first()
                        .cloned()
                        .unwrap_or_else(|| head.original.clone()),
                    phase: HeadPhase::Unknown,
                    attempt: head.attempt.as_ref().map(|a| AttemptIdentity {
                        nonce: a.nonce.clone(),
                        provider: "claude".into(),
                        target: TargetIdentity {
                            host: "tmux".into(),
                            owner: namespace.into(),
                            root: String::new(),
                            execution_nonce: String::new(),
                        },
                        source: AttemptSource::Existing {
                            session_id: a.session_id.clone(),
                            transcript: a.transcript.clone(),
                            file: [a.file.0, a.file.1],
                            offset: a.offset,
                        },
                    }),
                    payload: None,
                    confirmation_deadline_ms: None,
                });
            SnapshotV2 {
                version: 2,
                items,
                head_injection: input,
                retained_inputs: vec![],
                terminal_identities: meta
                    .map(|meta| {
                        meta.terminal_identities
                            .into_iter()
                            .map(|identity| TerminalIdentity {
                                identity,
                                settled_at_ms: u64::MAX,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                namespace: None,
            }
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
    if let Some(head) = &snapshot.head_injection {
        let first = snapshot.items.first().ok_or("active original missing")?;
        if serde_json::to_value(first).map_err(|error| error.to_string())?
            != serde_json::to_value(&head.original).map_err(|error| error.to_string())?
        {
            return Err("active original differs from queue head".into());
        }
    }
    snapshot.namespace = Some(namespace.into());
    if snapshot
        .head_injection
        .as_ref()
        .is_some_and(|head| head.phase == HeadPhase::Reserved)
    {
        snapshot.head_injection = None;
    }
    Ok(snapshot)
}

/// Parked without positive evidence of no attempt is retained, never rejected or retried.
pub(crate) fn migrate_parked(
    owner: &mut Owner,
    item: PendingQueueItem,
    proven_unattempted: bool,
) -> Result<(), String> {
    if owner.fenced(&item) {
        return Ok(());
    }
    if proven_unattempted {
        owner.enqueue(item)?;
        return Ok(());
    }
    let mut next = owner.snapshot.clone();
    next.retained_inputs.push(RetainedInput {
        input: InputAttempt {
            identity: identity(&item),
            original: item,
            phase: HeadPhase::Unknown,
            attempt: None,
            payload: None,
            confirmation_deadline_ms: None,
        },
        reason: RetainedReason::Unconfirmed,
        notice: NoticeIntent {
            kind: "unconfirmed".into(),
            enqueued: false,
        },
    });
    owner.commit(next)
}
