//! Native input evidence stays attached to its original registration through projection.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Episode {
    pub(crate) user_message_id: u64,
    pub(crate) turn_nonce: String,
    pub(crate) generation: u64,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
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

impl Registry {
    // Writers stay test-only until native input activation installs their production caller.
    #[cfg(test)]
    pub(crate) fn prepare_native(&self, namespace: &str, ticket: &Ticket) -> Result<(), String> {
        self.native_transaction(namespace, &ticket.key, |registration| {
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

    #[cfg(test)]
    fn native_transaction<T>(
        &self,
        namespace: &str,
        key: &Value,
        apply: impl FnOnce(&mut Registration) -> Result<T, String>,
    ) -> Result<T, String> {
        self.transaction(|snapshot| {
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
