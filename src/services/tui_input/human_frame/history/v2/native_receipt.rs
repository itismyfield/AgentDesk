//! Membership confirms delivery only; terminal evidence still needs its separate result CAS.
use super::*;
impl Registry {
    #[cfg(any(test, feature = "test-support"))]
    pub fn try_native_receipt(
        &self,
        namespace: &str,
        ticket: &Ticket,
        receipt: (&str, &str, (u64, u64), u64, &str),
        expected: &str,
    ) -> Result<(), String> {
        self.try_scoped_native_receipt(namespace, ticket, receipt, (expected, &[]))
    }

    pub fn try_scoped_native_receipt(
        &self,
        namespace: &str,
        ticket: &Ticket,
        (kind, row, range, through, observed_hash): (&str, &str, (u64, u64), u64, &str),
        (expected, devices): (&str, &[u64]),
    ) -> Result<(), String> {
        let source = ticket
            .source
            .as_ref()
            .ok_or("native receipt source missing")?;
        source.validate(&ticket.key)?;
        if !matches!(kind, "user" | "command")
            || row.is_empty()
            || range.0 < source.offset
            || range.0 >= range.1
            || range.1 > through
        {
            return Err("native receipt range/kind mismatch".into());
        }
        self.try_control_transaction_checked(
            |state| {
                let (registration, entry) = state
                    .locate(namespace, &ticket.key)
                    .ok_or("native registration missing")?;
                let current = registration
                    .native_settlement
                    .as_ref()
                    .ok_or("native ticket missing")?;
                current.validate(registration)?;
                if current.ticket != *ticket
                    || registration.source.as_ref() != Some(source)
                    || current.extra.get("any_key") != Some(&Value::Bool(true))
                    || registration.frame.hash != observed_hash
                    || state.abandoned(namespace, &ticket.key)
                {
                    return Err("native receipt identity/hash/key mismatch".into());
                }
                let entry = entry.ok_or("native receipt entry missing")?;
                if entry.hash != observed_hash
                    || entry.path != source.transcript
                    || entry.file != source.file
                    || entry.offset != source.offset
                {
                    return Err("native receipt entry changed".into());
                }
                let registrations = scoped_registrations(state, ticket, devices)?;
                if registrations_fingerprint(&registrations)? != expected {
                    return Err("native receipt registrations superseded".into());
                }
                let event_key = format!("{kind}:{row}");
                if let Some((owner, existing)) = state.entries().find(|(_, e)| {
                    e.path == entry.path
                        && same_registered_file(e.file, entry.file, devices)
                        && e.events.iter().any(|event| event.key == event_key)
                }) {
                    return if owner.is_some_and(|r| r.namespace == namespace && r.key == ticket.key)
                        && existing.events.iter().any(|event| {
                            event.key == event_key
                                && event.kind == kind
                                && event.row.as_deref() == Some(row)
                        }) {
                        Ok(None)
                    } else {
                        Err("native receipt row occupied".into())
                    };
                }
                if collision::contested(
                    state,
                    (namespace, &ticket.key, registration, entry),
                    kind,
                    range.0,
                ) {
                    return Err("native receipt ownership unresolved".into());
                }
                Ok(Some(Event {
                    key: event_key,
                    row: Some(row.into()),
                    kind: kind.into(),
                }))
            },
            |state, event| {
                let Some(event) = event else { return Ok(()) };
                if let Some(bound) = state.live.iter_mut().find(|b| {
                    b.registration.namespace == namespace && b.registration.key == ticket.key
                }) {
                    bound.entry.events.push(event);
                } else if let Some(passive) = state.passive.iter_mut().find(|p| {
                    p.registration
                        .as_ref()
                        .is_some_and(|r| r.namespace == namespace && r.key == ticket.key)
                }) {
                    passive
                        .entry
                        .as_mut()
                        .ok_or("native receipt passive entry missing")?
                        .events
                        .push(event);
                    passive.encoded = None;
                } else {
                    return Err("native receipt bound entry missing".into());
                }
                state.version = 3;
                Ok(())
            },
        )
    }
}
