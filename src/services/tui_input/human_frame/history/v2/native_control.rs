//! A release admission never waits for another registry owner or loses an unflushed candidate.
use super::*;

const CACHE_BUSY: &str = "native control cache busy";
const HISTORY_BUSY: &str = "native control history busy";
const PUBLICATION_BUSY: &str = "native control publication busy";
use crate::services::discord::runtime_store::{
    AtomicWriteContext,
    durable_publish::{self, FlushStatus},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub namespace: String,
    pub ticket: Ticket,
    pub disposition: Disposition,
    pub row: Option<Value>,
}

impl Intent {
    pub fn owns_row(&self, row: &Value) -> bool {
        let Some(source) = &self.ticket.source else {
            return false;
        };
        let Some(episode) = &self.ticket.owning_episode else {
            return false;
        };
        row["provider"] == "claude"
            && row["channel_id"] == self.ticket.channel_id
            && row["user_msg_id"] == episode.user_message_id
            && row["born_generation"] == episode.generation
            && row["turn_nonce"] == episode.turn_nonce
            && row["session_id"] == source.session_id
            && row["output_path"].as_str() == source.transcript.to_str()
            && row["hosted_execution_nonce"] == source.target["execution_nonce"]
    }

    pub fn runtime_matches(&self, row: &Value) -> bool {
        self.owns_row(row)
            && self.row.as_ref().is_some_and(|saved| {
                saved["started_at"] == row["started_at"]
                    && saved["turn_start_offset"] == row["turn_start_offset"]
            })
    }

    pub fn validates(&self, namespace: &str, ticket: &Ticket, outcome: &Outcome) -> bool {
        self.namespace == namespace
            && self.ticket == *ticket
            && ticket.source.as_ref().is_some_and(|source| {
                source.provider == "claude"
                    && !source.session_id.is_empty()
                    && source.file != [0, 0]
                    && source.target == ticket.key["attempt"]["target"]
            })
            && ticket.owning_episode.is_some()
            && self.row.as_ref().is_none_or(|row| self.owns_row(row))
            && ticket.source.is_some()
            && matches!(
                self.disposition,
                Disposition::Cancelled | Disposition::OperatorReleased
            )
            && outcome.disposition == self.disposition
            && outcome.range.is_none()
            && outcome.extra.get("control_proof") == serde_json::to_value(self).ok().as_ref()
    }
}

impl Registry {
    pub fn same_control_registry(&self, other: &Self) -> bool {
        self.path == other.path
    }
    pub fn try_native_settlement(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<Settlement, String> {
        self.try_control_transaction(|snapshot| {
            let (registration, _) = snapshot
                .locate(namespace, &ticket.key)
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
    pub fn try_ack_native_cleanup(
        &self,
        namespace: &str,
        ticket: &Ticket,
        outcome: &Outcome,
    ) -> Result<bool, String> {
        self.try_control_transaction(|snapshot| {
            let (registration, _) = snapshot
                .locate(namespace, &ticket.key)
                .ok_or("native registration missing")?;
            registration
                .native_settlement
                .as_ref()
                .ok_or("native ticket missing")?
                .validate(registration)?;
            let current = settlement_mut(snapshot, namespace, &ticket.key)?;
            if current.ticket != *ticket
                || current.outcome.as_ref() != Some(outcome)
                || (outcome.disposition == Disposition::WithdrawnNotExecuted
                    && !current.withdrawal_notice)
            {
                return Err("native cleanup admission missing".into());
            }
            let first = !current.cleanup_complete;
            current.cleanup_complete = true;
            snapshot.version = 3;
            Ok(first)
        })
    }
    pub fn try_native_boot_record(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<RestoredRecord, String> {
        self.try_control_transaction(|snapshot| {
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

    pub(crate) fn try_native_not_sent(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<(), String> {
        self.try_control_transaction(|state| mark_not_sent(state, namespace, ticket))
    }

    /// The same no-key record, waiting for the registry lock; only off the async workers.
    pub fn native_not_sent_blocking(&self, namespace: &str, ticket: &Ticket) -> Result<(), String> {
        self.transaction(|state| mark_not_sent(state, namespace, ticket))
    }

    /// The no-key record outlasts a brief registry contention by awaiting between tries, so it
    /// never parks a worker; past the bound the busy refusal stands.
    pub async fn native_not_sent(&self, namespace: &str, ticket: &Ticket) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
        loop {
            match self.try_native_not_sent(namespace, ticket) {
                Err(error)
                    if [CACHE_BUSY, HISTORY_BUSY, PUBLICATION_BUSY].contains(&error.as_str())
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                done => return done,
            }
        }
    }

    pub fn try_native_control(
        &self,
        namespace: &str,
        ticket: &Ticket,
        disposition: Disposition,
        row: Option<Value>,
    ) -> Result<(), String> {
        if !matches!(
            disposition,
            Disposition::Cancelled | Disposition::OperatorReleased
        ) {
            return Err("native control disposition invalid".into());
        }
        self.try_control_transaction(|state| {
            let (registration, _) = state
                .locate(namespace, &ticket.key)
                .ok_or("native control registration missing")?;
            let current = registration
                .native_settlement
                .as_ref()
                .ok_or("native control ticket missing")?;
            current.validate(registration)?;
            let mut expected = ticket.clone();
            if expected.source.is_none() {
                expected.source =
                    merge_source(&expected.key, None, current.ticket.source.as_ref())?;
            }
            if current.ticket != expected
                || expected.owning_episode.is_none()
                || expected.source.is_none()
                || current.extra.get("any_key") != Some(&Value::Bool(true))
                || current.extra.get("not_sent") == Some(&Value::Bool(true))
            {
                return Err("native control ownership/key evidence missing".into());
            }
            let intent = Intent {
                namespace: namespace.into(),
                ticket: expected,
                disposition,
                row,
            };
            merge_source(&intent.ticket.key, None, intent.ticket.source.as_ref())?;
            let native = settlement_mut(state, namespace, &ticket.key)?;
            if let Some(saved) = native.extra.get("control_intent") {
                let saved: Intent =
                    serde_json::from_value(saved.clone()).map_err(|e| e.to_string())?;
                if saved.namespace != namespace
                    || saved.ticket != intent.ticket
                    || saved.row.as_ref().is_some_and(|row| !saved.owns_row(row))
                {
                    return Err("native control intent changed".into());
                }
            } else {
                if intent.row.as_ref().is_some_and(|row| !intent.owns_row(row)) {
                    return Err("native control row belongs to another execution".into());
                }
                native.extra.insert(
                    "control_intent".into(),
                    serde_json::to_value(&intent).map_err(|e| e.to_string())?,
                );
            }
            if native.outcome.is_none() {
                native.outcome = Some(Outcome {
                    disposition: intent.disposition.clone(),
                    range: None,
                    extra: serde_json::Map::from_iter([(
                        "control_proof".into(),
                        serde_json::to_value(&intent).map_err(|e| e.to_string())?,
                    )]),
                });
            }
            state.version = 3;
            Ok(())
        })
    }

    pub(in crate::services::tui_input::human_frame::history::v2) fn try_control_transaction<T>(
        &self,
        apply: impl FnOnce(&mut Snapshot) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut cached = try_mutex(&self.cached, CACHE_BUSY)?;
        std::fs::create_dir_all(self.path.parent().ok_or("history parent missing")?)
            .map_err(|e| e.to_string())?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        lock.try_lock().map_err(|_| HISTORY_BUSY)?;
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => migration::MISSING.into(),
            Err(e) => return Err(e.to_string()),
        };
        let context = || AtomicWriteContext::new("injected_frames_v2");
        let mut pending = try_mutex(&self.pending, PUBLICATION_BUSY)?;
        if let Some((old, candidate)) = pending.as_ref() {
            if raw == *candidate {
                if durable_publish::retry_published_status(&self.path, context())
                    != FlushStatus::Flushed
                {
                    return Err("native control candidate durability unconfirmed".into());
                }
            } else if raw != *old {
                return Err(
                    "history changed after an unconfirmed publication; preserve both".into(),
                );
            }
            *pending = None;
        }
        if cached.raw() != Some(raw.as_str()) {
            cached.replace(raw.clone(), migration::restore(&raw)?);
        }
        let state = cached.state_mut().ok_or("history cache unavailable")?;
        let encoded = apply(state).and_then(|result| {
            if self.encode_fault.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("history encoding failed".to_string());
            }
            Ok((result, state.encode()?))
        });
        let (result, candidate) = match encoded {
            Ok(value) => value,
            Err(error) => {
                cached.clear();
                return Err(error);
            }
        };
        if candidate == raw {
            if durable_publish::retry_published_status(&self.path, context())
                != FlushStatus::Flushed
            {
                return Err("native control durability unconfirmed".into());
            }
            return Ok(result);
        }
        match durable_publish::critical_publish_status(&self.path, &candidate, context()) {
            Ok(FlushStatus::Flushed) => {
                cached.published(candidate)?;
                Ok(result)
            }
            status => {
                cached.clear();
                if status.is_ok() {
                    *pending = Some((raw, candidate));
                }
                Err("history publication unconfirmed; preserve registration".into())
            }
        }
    }
}

fn mark_not_sent(state: &mut Snapshot, namespace: &str, ticket: &Ticket) -> Result<(), String> {
    let (registration, _) = state
        .locate(namespace, &ticket.key)
        .ok_or("native no-key registration missing")?;
    registration
        .native_settlement
        .as_ref()
        .ok_or("native no-key ticket missing")?
        .validate(registration)?;
    let native = settlement_mut(state, namespace, &ticket.key)?;
    if !native.matches_key_witness(ticket)
        || native.outcome.is_some()
        || native.extra.get("any_key") == Some(&Value::Bool(true))
    {
        return Err("native no-key witness superseded".into());
    }
    native.extra.insert("not_sent".into(), Value::Bool(true));
    state.version = 3;
    Ok(())
}

fn settlement_mut<'a>(
    state: &'a mut Snapshot,
    namespace: &str,
    key: &Value,
) -> Result<&'a mut Settlement, String> {
    state
        .live
        .iter_mut()
        .map(|b| &mut b.registration)
        .chain(state.planned.iter_mut())
        .chain(state.passive.iter_mut().filter_map(|p| {
            p.encoded = None;
            p.registration.as_mut()
        }))
        .find(|r| r.namespace == namespace && r.key == *key)
        .and_then(|r| r.native_settlement.as_mut())
        .ok_or_else(|| "native control registration missing".into())
}

fn try_mutex<'a, T>(
    lock: &'a std::sync::Mutex<T>,
    busy: &str,
) -> Result<std::sync::MutexGuard<'a, T>, String> {
    match lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Ok(error.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => Err(busy.into()),
    }
}
