//! The domain half of a raw EOF proof: exact custody and lineage, one bounded tail read, and the
//! execution's own end. It only reads; nothing here closes, retires or deletes.
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
};
use crate::services::discord::herdr_terminate::{ExecutionEnd, execution_end};
use crate::services::tui_input::human_frame::history::v2::Source;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::{Fingerprint, MAX_PARTIAL_BYTES};
use crate::services::tui_o::writer::input_facts::native::Registration;
use crate::services::tui_o::writer::input_facts::native::terminal::{RawWitness, Tail, raw_closer};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Take};

/// The opened file's stamp, only while the source path still names that same file.
pub(super) fn stamp(source: &Source, file: &std::fs::File) -> Result<Fingerprint, String> {
    let id = SourceId {
        session_id: source.session_id.clone(),
        path: source.transcript.clone(),
        dev: source.file[0],
        ino: source.file[1],
    };
    let path = Fingerprint::path(&id).map_err(|(_, detail)| detail)?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let open = Fingerprint::of(&meta).map_err(|e| e.to_string())?;
    if path != open {
        return Err("native tail path names another file".into());
    }
    Ok(open)
}

/// Reads exactly `through..EOF` once; a stamp or byte that differs from the capture discards it.
fn read_tail(source: &Source, tail: &Tail) -> Result<(Vec<u8>, std::fs::File), String> {
    read_tail_with(source, tail, |reader, bytes| {
        #[cfg(test)]
        {
            read_probe::read(reader, bytes)
        }
        #[cfg(not(test))]
        {
            reader.read_to_end(bytes)
        }
    })
}

#[cfg(test)]
pub(in crate::services::discord::turn_finalizer) mod read_probe {
    use super::*;
    use std::cell::RefCell;

    type AfterRead = Box<dyn FnOnce(&[u8]) -> io::Result<()>>;

    thread_local! {
        static AFTER_READ: RefCell<Option<AfterRead>> = const { RefCell::new(None) };
    }

    pub(in crate::services::discord::turn_finalizer) fn after_next_read(
        inspect: impl FnOnce(&[u8]) -> io::Result<()> + 'static,
    ) {
        AFTER_READ.with(|hook| assert!(hook.replace(Some(Box::new(inspect))).is_none()));
    }

    pub(super) fn read(reader: &mut Take<&File>, bytes: &mut Vec<u8>) -> io::Result<usize> {
        let count = reader.read_to_end(bytes)?;
        if let Some(inspect) = AFTER_READ.with(RefCell::take) {
            inspect(bytes)?;
        }
        Ok(count)
    }
}

pub(in crate::services::discord::turn_finalizer) fn read_tail_with(
    source: &Source,
    tail: &Tail,
    read: impl FnOnce(&mut Take<&File>, &mut Vec<u8>) -> io::Result<usize>,
) -> Result<(Vec<u8>, File), String> {
    let mut file = File::open(&source.transcript).map_err(|e| e.to_string())?;
    let before = stamp(source, &file)?;
    let want = before.len.saturating_sub(tail.through);
    if before != tail.fingerprint || want == 0 || want > MAX_PARTIAL_BYTES as u64 {
        return Err("native tail differs from the capture".into());
    }
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(tail.through))
        .map_err(|e| e.to_string())?;
    read(&mut (&file).take(want + 1), &mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 != want
        || blake3::hash(&bytes).to_hex().as_str() != tail.digest
        || stamp(source, &file)? != before
    {
        return Err("native tail changed during its read".into());
    }
    Ok((bytes, file))
}

impl Port {
    /// The stored execution of this exact target reads as ended now; a retired flag is no proof.
    async fn ended(&self, target: &serde_json::Value) -> Result<(), String> {
        let unconfirmed = || "native execution end unconfirmed".to_string();
        let pool = self.shared.pg_pool.as_ref().ok_or_else(unconfirmed)?;
        let channel = self.channel.to_string();
        let identity = CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &self.shared.token_hash,
            channel_id: &channel,
        };
        let key = HostedLookupKey::Canonical {
            provider: "claude",
            identity,
        };
        let HostedLookup::Found(found) = load_hosted_execution_pg(pool, key).await else {
            return Err(unconfirmed());
        };
        let HostedRecord::Known(record) = found.record else {
            return Err(unconfirmed());
        };
        let at = record.location.as_ref().ok_or_else(unconfirmed)?;
        if target["execution_nonce"] != record.execution_nonce
            || target["owner"] != record.owner.logical_key
            || target["root"] != at.socket_addr
            || at.host != "herdr"
            || record.owner.provider != "claude"
        {
            return Err("native terminal execution superseded".into());
        }
        let host = crate::services::session_host::herdr_endpoints()
            .target(&record)
            .ok_or_else(unconfirmed)?;
        match execution_end(host, record).await {
            ExecutionEnd::Ended => Ok(()),
            _ => Err(unconfirmed()),
        }
    }

    pub(super) async fn terminal_proof(
        &self,
        observed: Observation,
        ticket: Registration,
        tail: Tail,
        origin: NoticeOrigin,
    ) -> Result<RawWitness, String> {
        self.origin(&origin)?;
        let source = ticket.ticket.source.as_ref();
        let source = source.ok_or("native terminal source missing")?;
        check_target(&source.target, &origin)?;
        if observed.binding.channel_id != self.channel
            || observed.binding.source != origin.record.source
            || observed.binding_seq != origin.binding_seq
            || observed.generation != self.shared.restart.current_generation
            || observed.through != tail.through
            || !ticket.matches(&observed.binding, observed.proven)
        {
            return Err("native terminal stamp superseded".into());
        }
        let (live, boot) = self.custody().await?;
        let held = |namespace: &str, held: &Ticket| {
            namespace == ticket.namespace && held.key == ticket.ticket.key
        };
        let (history, current) = match live.iter().find(|p| held(&p.namespace, &p.ticket)) {
            Some(input) if input.key.generation != observed.generation => {
                return Err("native terminal cannot adopt old-generation debt".into());
            }
            Some(input) => (input.history.clone(), input.bound_ticket(&input.history)?),
            None => {
                let input = boot.iter().find(|p| held(p.identity().0, p.identity().1));
                let input = input.ok_or("native terminal has no prepared custody")?;
                let history = Registry::runtime()?;
                let current = input.refreshed(&history)?.identity().1.clone();
                (history, current)
            }
        };
        let saved = history.native_settlement(&ticket.namespace, &current)?;
        if current != ticket.ticket
            || saved.outcome.is_some()
            || saved.extra.get("any_key") != Some(&serde_json::Value::Bool(true))
        {
            return Err("native terminal ticket changed, settled or unkeyed".into());
        }
        let (bytes, file) = read_tail(source, &tail)?;
        let aborted = raw_closer(&observed.binding, &bytes)?;
        self.ended(&source.target).await?;
        // The tail was whole before its execution read as ended, and is the same file after.
        if stamp(source, &file)? != tail.fingerprint {
            return Err("native tail changed after its execution ended".into());
        }
        Ok(RawWitness {
            version: 1,
            tail,
            aborted,
            execution_nonce: origin.execution_nonce,
            registrations_fingerprint: observed.registrations_fingerprint,
        })
    }
}
