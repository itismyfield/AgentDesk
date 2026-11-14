//! Native input evidence stays attached to its original registration through projection.
use super::*;
#[cfg(unix)]
#[path = "native_control.rs"]
pub(crate) mod control;

#[cfg(all(test, unix))]
pub(crate) fn frame_hashes(text: &str) -> Vec<String> {
    super::super::class_hashes(text)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Episode {
    pub(crate) user_message_id: u64,
    pub(crate) turn_nonce: String,
    pub(crate) generation: u64,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Intent {
    Primary(Episode),
    Follower,
}

impl Intent {
    pub(crate) fn attribution(&self, sources: &[String]) -> serde_json::Value {
        serde_json::json!({"sources": sources, "native_intent": self})
    }

    pub(crate) fn episode(&self, primary: u64) -> Result<Option<Episode>, String> {
        match self {
            Self::Primary(episode)
                if episode.user_message_id != primary
                    || primary == 0
                    || episode.turn_nonce.is_empty()
                    || episode.generation == 0 =>
            {
                Err("native primary intent mismatch".into())
            }
            Self::Primary(episode) => Ok(Some(episode.clone())),
            Self::Follower => Ok(None),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ticket {
    pub(crate) key: Value,
    pub(crate) channel_id: u64,
    pub(crate) source: Option<Source>,
    pub(crate) owning_episode: Option<Episode>,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Executed,
    WithdrawnNotExecuted,
    TransferredUnverified,
    Cancelled,
    OperatorReleased,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Outcome {
    pub(crate) disposition: Disposition,
    pub(crate) range: Option<[u64; 2]>,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settlement {
    pub(crate) ticket: Ticket,
    pub(crate) outcome: Option<Outcome>,
    #[serde(default)]
    pub(crate) withdrawal_notice: bool,
    #[serde(default)]
    pub(crate) cleanup_complete: bool,
    #[serde(default)]
    pub(crate) warned: bool,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

impl Outcome {
    fn valid(&self) -> bool {
        self.range.map_or_else(
            || {
                !matches!(
                    self.disposition,
                    Disposition::Executed | Disposition::WithdrawnNotExecuted
                )
            },
            |range| range[0] < range[1],
        )
    }
}

impl Settlement {
    fn validate(&self, registration: &Registration) -> Result<(), String> {
        let ticket = &self.ticket;
        if ticket.key != registration.key
            || ticket.source != registration.source
            || ticket.channel_id == 0
            || ticket.owning_episode.as_ref().is_some_and(|episode| {
                episode.user_message_id == 0
                    || episode.turn_nonce.is_empty()
                    || ticket.key["identity"]["primary"] != episode.user_message_id
            })
            || self
                .outcome
                .as_ref()
                .is_some_and(|outcome| !outcome.valid())
        {
            return Err("native input ticket does not match registration".into());
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct BootRecord<'a> {
    namespace: &'a str,
    settlement: &'a Settlement,
    registration: &'a Registration,
    entry: Option<&'a Entry>,
}

#[cfg(unix)]
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RestoredRecord {
    pub(crate) namespace: String,
    pub(crate) settlement: Settlement,
    registration: Registration,
    #[serde(default)]
    entry: Option<Entry>,
}

#[cfg(unix)]
impl RestoredRecord {
    pub(crate) fn decode(value: Value) -> Result<Self, String> {
        let record: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        record.settlement.validate(&record.registration)?;
        if record.namespace.is_empty()
            || record.namespace != record.registration.namespace
            || record.registration.native_settlement.as_ref() != Some(&record.settlement)
            || (record.settlement.cleanup_complete && record.settlement.outcome.is_none())
        {
            return Err("native custody registration mismatch".into());
        }
        if let Some(entry) = &record.entry {
            let source = record
                .registration
                .source
                .as_ref()
                .ok_or("native receipt source absent")?;
            let expected = record
                .registration
                .frame
                .bind(&record.registration.key, source);
            same_entry(&expected, entry)?;
            merge_entry(None, Some(entry))?;
        }
        Ok(record)
    }
}

/// Returns no work for historical envelopes; a v3 read must succeed before any boot deletion.
pub(crate) fn boot_records(path: &Path, provider: &str) -> Result<Vec<Value>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.to_string()),
    };
    let snapshot = match migration::decode(&raw)? {
        migration::Decoded::Legacy(_) => return Ok(vec![]),
        migration::Decoded::V2(snapshot) => snapshot,
    };
    let mut registrations = snapshot
        .live
        .iter()
        .map(|bound| (&bound.registration, Some(&bound.entry)))
        .chain(snapshot.planned.iter().map(|r| (r, None)))
        .chain(
            snapshot
                .passive
                .iter()
                .filter_map(|p| p.registration.as_ref().map(|r| (r, p.entry.as_ref()))),
        );
    if snapshot.version == 2 {
        return match registrations.any(|(r, _)| r.native_settlement.is_some()) {
            true => Err("native frame history requires a v3 envelope".into()),
            false => Ok(vec![]),
        };
    }
    registrations
        .filter(|(r, _)| r.key["attempt"]["provider"] == provider)
        .filter_map(|(registration, entry)| {
            registration
                .native_settlement
                .as_ref()
                .filter(|s| !s.cleanup_complete)
                .map(|s| (registration, s, entry))
        })
        .map(|(registration, settlement, entry)| {
            settlement.validate(registration)?;
            serde_json::to_value(BootRecord {
                namespace: &registration.namespace,
                settlement,
                registration,
                entry,
            })
            .map_err(|error| error.to_string())
        })
        .collect()
}

#[cfg(all(test, unix))]
fn evidence_registrations(
    state: &Snapshot,
) -> Result<Vec<crate::services::tui_o::writer::input_facts::native::Registration>, String> {
    let registrations = state
        .live
        .iter()
        .map(|b| (&b.registration, Some(&b.entry)))
        .chain(state.planned.iter().map(|r| (r, None)))
        .chain(
            state
                .passive
                .iter()
                .filter(|p| p.state != collision::ABANDONED)
                .filter_map(|p| p.registration.as_ref().map(|r| (r, p.entry.as_ref()))),
        );
    registrations
        .filter_map(|(registration, entry)| {
            registration
                .native_settlement
                .as_ref()
                .filter(|s| s.outcome.is_none() && !s.cleanup_complete)
                .map(|s| (registration, entry, s))
        })
        .map(|(registration, entry, settlement)| {
            settlement.validate(registration)?;
            Ok(
                crate::services::tui_o::writer::input_facts::native::Registration {
                    namespace: registration.namespace.clone(),
                    ticket: settlement.ticket.clone(),
                    frame_hash: registration.frame.class_hash.clone(),
                    receipts: entry
                        .into_iter()
                        .flat_map(|e| e.events.iter())
                        .filter(|e| matches!(e.kind.as_str(), "user" | "command"))
                        .filter_map(|e| e.row.clone())
                        .collect(),
                },
            )
        })
        .collect()
}

impl Registry {
    #[cfg(unix)]
    pub(crate) fn native_boot_record(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<RestoredRecord, String> {
        self.inspect(|snapshot| {
            let (registration, entry) = snapshot
                .locate(namespace, &ticket.key)
                .ok_or("native registration missing")?;
            let settlement = registration
                .native_settlement
                .clone()
                .ok_or("native ticket missing")?;
            settlement.validate(registration)?;
            if settlement.ticket != *ticket {
                return Err("native boot record ticket changed".into());
            }
            Ok(RestoredRecord {
                namespace: namespace.into(),
                settlement,
                registration: registration.clone(),
                entry: entry.cloned(),
            })
        })
    }

    #[cfg(unix)]
    pub(crate) fn adopt_native_records(
        &self,
        records: &[RestoredRecord],
    ) -> Result<Vec<RestoredRecord>, String> {
        self.transaction(|snapshot| {
            let mut adopted = Vec::new();
            for record in records {
                let mut incoming = record.registration.clone();
                let mut receipts = record.entry.clone();
                let original = snapshot.locate(&record.namespace, &incoming.key);
                incoming.source = merge_source(
                    &incoming.key,
                    original.and_then(|(r, _)| r.source.as_ref()),
                    incoming.source.as_ref(),
                )?;
                incoming
                    .native_settlement
                    .as_mut()
                    .ok_or("native ticket missing")?
                    .ticket
                    .source = incoming.source.clone();
                if let Some((current, entry)) = original {
                    receipts = merge_entry(entry, receipts.as_ref())?;
                    let mut normalized = current.clone();
                    normalized.source = incoming.source.clone();
                    let mut a = serde_json::to_value(&normalized).map_err(|e| e.to_string())?;
                    let mut b = serde_json::to_value(&incoming).map_err(|e| e.to_string())?;
                    // Accept only the display compaction already performed by Passive::compact.
                    let display = ["author", "body_hash", "header", "screen_prefix"];
                    let compacted = [&a, &b].iter().any(|value| {
                        display
                            .iter()
                            .all(|key| value["frame"][*key].as_str() == Some(""))
                    });
                    for value in [&mut a, &mut b] {
                        value
                            .as_object_mut()
                            .ok_or("native registration object missing")?
                            .remove("native_settlement");
                        if compacted {
                            for key in display {
                                value["frame"]
                                    .as_object_mut()
                                    .ok_or("native frame missing")?
                                    .remove(key);
                            }
                        }
                    }
                    if a != b {
                        return Err("native custody conflicts with current registration".into());
                    }
                    let mut settled = current
                        .native_settlement
                        .clone()
                        .ok_or("native ticket missing")?;
                    settled.ticket.source = incoming.source.clone();
                    if settled.ticket
                        != incoming
                            .native_settlement
                            .as_ref()
                            .ok_or("native ticket missing")?
                            .ticket
                        || settled
                            .outcome
                            .as_ref()
                            .zip(record.settlement.outcome.as_ref())
                            .is_some_and(|(a, b)| a != b)
                    {
                        return Err("native custody conflicts with current result".into());
                    }
                    if settled.outcome.is_none() {
                        settled.outcome = record.settlement.outcome.clone();
                    }
                    settled.cleanup_complete |= record.settlement.cleanup_complete;
                    settled.withdrawal_notice |= record.settlement.withdrawal_notice;
                    settled.warned |= record.settlement.warned;
                    for (key, value) in &record.settlement.extra {
                        if settled
                            .extra
                            .get(key)
                            .is_some_and(|current| current != value)
                        {
                            return Err("native custody extension changed".into());
                        }
                        settled.extra.insert(key.clone(), value.clone());
                    }
                    normalized.native_settlement = Some(settled);
                    incoming = normalized;
                }
                let key = &incoming.key;
                if let Some(entry) = &receipts {
                    same_entry(
                        &incoming.frame.bind(
                            key,
                            incoming
                                .source
                                .as_ref()
                                .ok_or("native receipt source absent")?,
                        ),
                        entry,
                    )?;
                    for (owner, other) in snapshot.entries() {
                        if owner.is_some_and(|r| r.namespace == record.namespace && r.key == *key) {
                            continue;
                        }
                        if other.path == entry.path
                            && other.file == entry.file
                            && entry
                                .events
                                .iter()
                                .any(|event| other.events.iter().any(|old| old.key == event.key))
                        {
                            return Err("native receipt belongs to another registration".into());
                        }
                    }
                }
                let recovered_entry = || {
                    receipts.clone().or_else(|| {
                        incoming
                            .source
                            .as_ref()
                            .map(|source| incoming.frame.bind(key, source))
                    })
                };
                if let Some(bound) = snapshot.live.iter_mut().find(|b| {
                    b.registration.namespace == record.namespace && b.registration.key == *key
                }) {
                    bound.registration = incoming.clone();
                    if let Some(entry) = recovered_entry() {
                        bound.entry = entry;
                    }
                } else if let Some(planned) = snapshot
                    .planned
                    .iter_mut()
                    .find(|r| r.namespace == record.namespace && r.key == *key)
                {
                    *planned = incoming.clone();
                    if incoming.source.is_some() {
                        snapshot
                            .planned
                            .retain(|r| !(r.namespace == record.namespace && r.key == *key));
                        snapshot.passive.push(Passive {
                            registration: Some(incoming.clone()),
                            entry: recovered_entry(),
                            state: "retained".into(),
                            encoded: None,
                        });
                    }
                } else if let Some(passive) = snapshot.passive.iter_mut().find(|p| {
                    p.registration
                        .as_ref()
                        .is_some_and(|r| r.namespace == record.namespace && r.key == *key)
                }) {
                    passive.entry = recovered_entry();
                    passive.registration = Some(incoming.clone());
                    passive.encoded = None;
                } else {
                    snapshot.passive.push(Passive {
                        registration: Some(incoming.clone()),
                        entry: recovered_entry(),
                        state: if incoming
                            .native_settlement
                            .as_ref()
                            .is_some_and(|s| s.cleanup_complete)
                        {
                            "settled"
                        } else {
                            "retained"
                        }
                        .into(),
                        encoded: None,
                    });
                }
                adopted.push(RestoredRecord {
                    namespace: record.namespace.clone(),
                    settlement: incoming
                        .native_settlement
                        .clone()
                        .ok_or("native ticket missing")?,
                    entry: snapshot
                        .locate(&record.namespace, key)
                        .and_then(|(_, entry)| entry.cloned()),
                    registration: incoming,
                });
            }
            if !records.is_empty() {
                snapshot.version = 3;
            }
            Ok(adopted)
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn native_evidence_registrations(
        &self,
    ) -> Result<Vec<crate::services::tui_o::writer::input_facts::native::Registration>, String>
    {
        self.inspect(evidence_registrations)
    }

    /// The cold effect calls this before arming only when its payload carries explicit intent.
    pub(crate) fn prepare_native_intent(
        &self,
        namespace: &str,
        key: &Value,
        channel: u64,
        intent: &Intent,
    ) -> Result<(), String> {
        let episode = intent.episode(
            key["identity"]["primary"]
                .as_u64()
                .ok_or("missing primary")?,
        )?;
        self.native_transaction(namespace, key, |registration| {
            let ticket = Ticket {
                key: key.clone(),
                channel_id: channel,
                source: registration.source.clone(),
                owning_episode: episode,
                extra: Default::default(),
            };
            prepare_ticket(registration, &ticket)
        })
    }

    #[cfg(test)]
    pub(crate) fn prepare_native(&self, namespace: &str, ticket: &Ticket) -> Result<(), String> {
        self.native_transaction(namespace, &ticket.key, |registration| {
            prepare_ticket(registration, ticket)
        })
    }

    #[cfg(test)]
    pub(crate) fn record_native_outcome(
        &self,
        namespace: &str,
        ticket: &Ticket,
        outcome: &Outcome,
    ) -> Result<bool, String> {
        self.native_transaction(namespace, &ticket.key, |registration| {
            let current = registration
                .native_settlement
                .as_mut()
                .ok_or("native ticket missing")?;
            if current.ticket != *ticket || !outcome.valid() {
                return Err("native result ticket mismatch".into());
            }
            match &current.outcome {
                Some(existing) if existing == outcome => Ok(false),
                Some(_) => Err("native result already settled".into()),
                None => {
                    current.outcome = Some(outcome.clone());
                    Ok(true)
                }
            }
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn record_native_outcome_at_stamp(
        &self,
        namespace: &str,
        ticket: &Ticket,
        outcome: &Outcome,
        expected: &str,
    ) -> Result<bool, String> {
        self.native_transaction_checked(
            namespace,
            &ticket.key,
            |snapshot| {
                let current = snapshot
                    .locate(namespace, &ticket.key)
                    .and_then(|(r, _)| r.native_settlement.as_ref())
                    .ok_or("native ticket missing")?;
                if current.ticket != *ticket {
                    return Err("native result ticket mismatch".into());
                }
                if current.outcome.as_ref() == Some(outcome) {
                    return Ok(());
                }
                let source = ticket.source.as_ref().ok_or("native source missing")?;
                let registrations: Vec<_> = evidence_registrations(snapshot)?
                    .into_iter()
                    .filter(|r| {
                        r.ticket.channel_id == ticket.channel_id
                            && r.ticket.source.as_ref().is_some_and(|s| {
                                s.provider == source.provider
                                    && s.session_id == source.session_id
                                    && s.transcript == source.transcript
                                    && s.file == source.file
                            })
                    })
                    .collect();
                let fingerprint =
                    crate::services::tui_o::writer::input_facts::native::registrations_fingerprint(
                        &registrations,
                    )?;
                if fingerprint != expected {
                    return Err("native registrations superseded".into());
                }
                if outcome.disposition == Disposition::Executed {
                    let row = outcome
                        .extra
                        .get("native_proof")
                        .and_then(|p| p["membership_row"].as_str())
                        .ok_or("native executed receipt missing")?;
                    if !registrations.iter().any(|r| {
                        r.namespace == namespace
                            && r.ticket == *ticket
                            && r.receipts.iter().any(|known| known == row)
                    }) {
                        return Err("native executed receipt unconfirmed".into());
                    }
                }
                Ok(())
            },
            |registration| {
                let current = registration
                    .native_settlement
                    .as_mut()
                    .ok_or("native ticket missing")?;
                if current.ticket != *ticket || !outcome.valid() {
                    return Err("native result ticket mismatch".into());
                }
                match &current.outcome {
                    Some(existing) if existing == outcome => Ok(false),
                    Some(_) => Err("native result already settled".into()),
                    None => {
                        current.outcome = Some(outcome.clone());
                        Ok(true)
                    }
                }
            },
        )
    }

    #[cfg(unix)]
    pub(crate) fn ack_native_withdrawal_notice(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<bool, String> {
        self.native_transaction(namespace, &ticket.key, |registration| {
            let current = registration
                .native_settlement
                .as_mut()
                .ok_or("native ticket missing")?;
            if current.ticket != *ticket {
                return Err("native notice ticket mismatch".into());
            }
            let withdrawn = current
                .outcome
                .as_ref()
                .is_some_and(|o| o.disposition == Disposition::WithdrawnNotExecuted);
            let first = withdrawn && !current.withdrawal_notice;
            current.withdrawal_notice |= first;
            Ok(first)
        })
    }

    #[cfg(unix)]
    pub(crate) fn native_ticket(&self, namespace: &str, key: &Value) -> Result<Ticket, String> {
        self.inspect(|snapshot| {
            let registration = snapshot
                .live
                .iter()
                .map(|b| &b.registration)
                .chain(snapshot.planned.iter())
                .chain(
                    snapshot
                        .passive
                        .iter()
                        .filter_map(|p| p.registration.as_ref()),
                )
                .find(|r| r.namespace == namespace && r.key == *key)
                .ok_or("native registration missing")?;
            let current = registration
                .native_settlement
                .as_ref()
                .ok_or("native ticket missing")?;
            current.validate(registration)?;
            Ok(current.ticket.clone())
        })
    }

    #[cfg(unix)]
    pub(crate) fn note_native_any_key(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<(), String> {
        self.native_transaction(namespace, &ticket.key, |registration| {
            let current = registration
                .native_settlement
                .as_mut()
                .ok_or("native ticket missing")?;
            if current.ticket != *ticket || current.outcome.is_some() {
                return Err("native key receipt ticket mismatch".into());
            }
            current.extra.insert("any_key".into(), Value::Bool(true));
            Ok(())
        })
    }

    #[cfg(unix)]
    pub(crate) fn native_settlement(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<Settlement, String> {
        self.inspect(|snapshot| {
            let registration = snapshot
                .live
                .iter()
                .map(|b| &b.registration)
                .chain(snapshot.planned.iter())
                .chain(
                    snapshot
                        .passive
                        .iter()
                        .filter_map(|p| p.registration.as_ref()),
                )
                .find(|r| r.namespace == namespace && r.key == ticket.key)
                .ok_or("native registration missing")?;
            let current = registration
                .native_settlement
                .as_ref()
                .ok_or("native ticket missing")?;
            current.validate(registration)?;
            if current.ticket != *ticket {
                return Err("native settlement ticket mismatch".into());
            }
            Ok(current.clone())
        })
    }

    #[cfg(unix)]
    pub(crate) fn ack_native_cleanup(
        &self,
        namespace: &str,
        ticket: &Ticket,
        outcome: &Outcome,
    ) -> Result<bool, String> {
        self.native_transaction(namespace, &ticket.key, |registration| {
            let current = registration
                .native_settlement
                .as_mut()
                .ok_or("native ticket missing")?;
            if current.ticket != *ticket
                || current.outcome.as_ref() != Some(outcome)
                || (outcome.disposition == Disposition::WithdrawnNotExecuted
                    && !current.withdrawal_notice)
            {
                return Err("native cleanup admission missing".into());
            }
            let first = !current.cleanup_complete;
            current.cleanup_complete = true;
            Ok(first)
        })
    }

    fn native_transaction<T>(
        &self,
        namespace: &str,
        key: &Value,
        apply: impl FnOnce(&mut Registration) -> Result<T, String>,
    ) -> Result<T, String> {
        self.native_transaction_checked(namespace, key, |_| Ok(()), apply)
    }

    fn native_transaction_checked<T>(
        &self,
        namespace: &str,
        key: &Value,
        check: impl FnOnce(&Snapshot) -> Result<(), String>,
        apply: impl FnOnce(&mut Registration) -> Result<T, String>,
    ) -> Result<T, String> {
        self.transaction(|snapshot| {
            check(snapshot)?;
            let mut active = snapshot
                .live
                .iter_mut()
                .map(|b| &mut b.registration)
                .chain(snapshot.planned.iter_mut());
            let result = if let Some(registration) =
                active.find(|r| r.namespace == namespace && r.key == *key)
            {
                apply(registration)?
            } else {
                let passive = snapshot
                    .passive
                    .iter_mut()
                    .find(|p| {
                        p.registration
                            .as_ref()
                            .is_some_and(|r| r.namespace == namespace && r.key == *key)
                    })
                    .ok_or("native registration missing")?;
                let result = apply(
                    passive
                        .registration
                        .as_mut()
                        .ok_or("native registration missing")?,
                )?;
                passive.encoded = None;
                result
            };
            snapshot.version = 3;
            Ok(result)
        })
    }
}

fn prepare_ticket(registration: &mut Registration, ticket: &Ticket) -> Result<(), String> {
    let candidate = Settlement {
        ticket: ticket.clone(),
        outcome: None,
        withdrawal_notice: false,
        cleanup_complete: false,
        warned: false,
        extra: Default::default(),
    };
    candidate.validate(registration)?;
    if let Some(existing) = &registration.native_settlement {
        return (existing.ticket == *ticket)
            .then_some(())
            .ok_or("native ticket changed".into());
    }
    registration.native_settlement = Some(candidate);
    Ok(())
}

#[cfg(unix)]
pub(crate) fn scoped_ticket() -> Result<(Registry, String, Ticket), String> {
    let context = super::context().ok_or("native attempt scope missing")?;
    if context.receipt_only {
        return Err("native receipt scope cannot prepare input".into());
    }
    let ticket = context
        .registry
        .native_ticket(&context.namespace, &context.key)?;
    Ok((context.registry, context.namespace, ticket))
}

#[cfg(unix)]
fn merge_source(
    key: &Value,
    a: Option<&Source>,
    b: Option<&Source>,
) -> Result<Option<Source>, String> {
    if a.zip(b).is_some_and(|(a, b)| a != b) {
        return Err("native bound source conflicts".into());
    }
    let Some(source) = a.or(b) else {
        return Ok(None);
    };
    let expected = &key["attempt"]["source"];
    let valid = match expected["kind"].as_str() {
        Some("existing") => {
            expected["session_id"] == source.session_id
                && expected["transcript"] == json!(source.transcript)
                && expected["file"] == json!(source.file)
                && expected["offset"] == source.offset
        }
        Some("pre_source") => [
            (&expected["expected_session_id"], json!(source.session_id)),
            (&expected["expected_transcript"], json!(source.transcript)),
            (
                &expected["binding_watermark"],
                json!(source.binding_watermark),
            ),
        ]
        .iter()
        .all(|(wanted, actual)| wanted.is_null() || *wanted == actual),
        _ => false,
    };
    if !valid
        || source.target != key["attempt"]["target"]
        || source.provider != key["attempt"]["provider"]
        || source.session_id.is_empty()
        || source.file == [0, 0]
        || source.target["execution_nonce"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err("native source expectation mismatch".into());
    }
    Ok(Some(source.clone()))
}

#[cfg(unix)]
fn same_entry(a: &Entry, b: &Entry) -> Result<(), String> {
    let mut a = serde_json::to_value(a).map_err(|e| e.to_string())?;
    let mut b = serde_json::to_value(b).map_err(|e| e.to_string())?;
    let display = ["author", "body_hash", "header", "screen_prefix"];
    let compact = [&a, &b]
        .iter()
        .any(|v| display.iter().all(|k| v[*k].as_str() == Some("")));
    for v in [&mut a, &mut b] {
        let object = v.as_object_mut().ok_or("native receipt object missing")?;
        object.remove("events");
        object.remove("append");
        if compact {
            for key in display {
                object.remove(key);
            }
        }
    }
    if a != b {
        return Err("native receipt identity mismatch".into());
    }
    Ok(())
}

#[cfg(unix)]
fn merge_entry(a: Option<&Entry>, b: Option<&Entry>) -> Result<Option<Entry>, String> {
    let Some(mut entry) = a.or(b).cloned() else {
        return Ok(None);
    };
    if let Some(other) = b {
        same_entry(&entry, other)?;
        for event in &other.events {
            if event.kind.is_empty()
                || !event.key.starts_with(&format!("{}:", event.kind))
                || event.row.as_ref().is_some_and(|row| {
                    row.is_empty() || event.key != format!("{}:{row}", event.kind)
                })
            {
                return Err("native receipt event malformed".into());
            }
            if let Some(old) = entry.events.iter().find(|old| old.key == event.key) {
                if serde_json::to_value(old).map_err(|e| e.to_string())?
                    != serde_json::to_value(event).map_err(|e| e.to_string())?
                {
                    return Err("native receipt event conflicts".into());
                }
            } else {
                entry.events.push(event.clone());
            }
        }
        if entry.append.is_none() {
            entry.append = other.append.clone();
        } else if other.append.is_some()
            && serde_json::to_value(&entry.append).map_err(|e| e.to_string())?
                != serde_json::to_value(&other.append).map_err(|e| e.to_string())?
        {
            return Err("native receipt append conflicts".into());
        }
    }
    Ok(Some(entry))
}
