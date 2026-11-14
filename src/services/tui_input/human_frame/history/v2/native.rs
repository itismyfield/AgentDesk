//! Native input evidence stays attached to its original registration through projection.
use super::*;

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
        .map(|bound| &bound.registration)
        .chain(snapshot.planned.iter())
        .chain(
            snapshot
                .passive
                .iter()
                .filter_map(|passive| passive.registration.as_ref()),
        );
    if snapshot.version == 2 {
        return match registrations.any(|r| r.native_settlement.is_some()) {
            true => Err("native frame history requires a v3 envelope".into()),
            false => Ok(vec![]),
        };
    }
    registrations
        .filter(|r| r.key["attempt"]["provider"] == provider)
        .filter_map(|registration| {
            registration
                .native_settlement
                .as_ref()
                .filter(|s| !s.cleanup_complete)
                .map(|s| (registration, s))
        })
        .map(|(registration, settlement)| {
            settlement.validate(registration)?;
            serde_json::to_value(BootRecord {
                namespace: &registration.namespace,
                settlement,
                registration,
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

    #[cfg(test)]
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

    #[cfg(all(test, unix))]
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
