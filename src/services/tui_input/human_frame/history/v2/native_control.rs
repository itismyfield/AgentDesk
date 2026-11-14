//! A release admission never waits for another registry owner or loses an unflushed candidate.
use super::*;
use crate::services::discord::runtime_store::{
    AtomicWriteContext,
    durable_publish::{self, FlushStatus},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Intent {
    pub(crate) namespace: String,
    pub(crate) ticket: Ticket,
    pub(crate) disposition: Disposition,
    pub(crate) row: Option<Value>,
}

impl Intent {
    pub(crate) fn owns_row(&self, row: &Value) -> bool {
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

    pub(crate) fn runtime_matches(&self, row: &Value) -> bool {
        self.owns_row(row)
            && self.row.as_ref().is_some_and(|saved| {
                saved["started_at"] == row["started_at"]
                    && saved["turn_start_offset"] == row["turn_start_offset"]
            })
    }

    pub(crate) fn validates(&self, namespace: &str, ticket: &Ticket, outcome: &Outcome) -> bool {
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
    pub(crate) fn same_control_registry(&self, other: &Self) -> bool {
        self.path == other.path
    }
    pub(crate) fn try_native_not_sent(
        &self,
        namespace: &str,
        ticket: &Ticket,
    ) -> Result<(), String> {
        self.try_control_transaction(|state| {
            let (registration, _) = state
                .locate(namespace, &ticket.key)
                .ok_or("native no-key registration missing")?;
            registration
                .native_settlement
                .as_ref()
                .ok_or("native no-key ticket missing")?
                .validate(registration)?;
            let native = settlement_mut(state, namespace, &ticket.key)?;
            if native.ticket != *ticket
                || native.outcome.is_some()
                || native.extra.get("any_key") == Some(&Value::Bool(true))
            {
                return Err("native no-key witness superseded".into());
            }
            native.extra.insert("not_sent".into(), Value::Bool(true));
            state.version = 3;
            Ok(())
        })
    }

    pub(crate) fn try_native_control(
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

    fn try_control_transaction<T>(
        &self,
        apply: impl FnOnce(&mut Snapshot) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut cached = try_mutex(&self.cached, "native control cache busy")?;
        std::fs::create_dir_all(self.path.parent().ok_or("history parent missing")?)
            .map_err(|e| e.to_string())?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        lock.try_lock().map_err(|_| "native control history busy")?;
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => migration::MISSING.into(),
            Err(e) => return Err(e.to_string()),
        };
        let context = || AtomicWriteContext::new("injected_frames_v2");
        let mut pending = try_mutex(&self.pending, "native control publication busy")?;
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
        if cached.as_ref().is_none_or(|(bytes, _)| *bytes != raw) {
            *cached = Some((raw.clone(), migration::restore(&raw)?));
        }
        let (_, state) = cached.as_mut().ok_or("history cache unavailable")?;
        let encoded = apply(state).and_then(|result| {
            if self.encode_fault.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("history encoding failed".to_string());
            }
            Ok((result, state.encode()?))
        });
        let (result, candidate) = match encoded {
            Ok(value) => value,
            Err(error) => {
                *cached = None;
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
                cached.as_mut().ok_or("history cache disappeared")?.0 = candidate;
                Ok(result)
            }
            status => {
                *cached = None;
                if status.is_ok() {
                    *pending = Some((raw, candidate));
                }
                Err("history publication unconfirmed; preserve registration".into())
            }
        }
    }
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
