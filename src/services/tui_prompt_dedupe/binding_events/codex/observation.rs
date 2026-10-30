//! Bounded whole-channel observation; partial reads cannot prove an absent held source.

use super::*;
use sha2::{Digest, Sha256};
use std::io::Read;

const BYTE_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelObservation {
    pub events: Vec<BindingEvent>,
    pub settled_companions: SettledCompanions,
    pub tail: u64,
    pub digest: String,
}

pub(crate) fn observe_channel(channel: u64, limit: usize) -> Result<ChannelObservation, String> {
    let read = || -> io::Result<ChannelObservation> {
        let path = log_path(channel)?.ok_or_else(|| invalid("Codex binding log unavailable"))?;
        let logs = lock_logs();
        if logs
            .get(&path)
            .and_then(|log| log.writer.as_ref())
            .is_some_and(|writer| writer.poisoned || writer.tainted)
        {
            return Err(invalid("Codex binding writer has incomplete evidence"));
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take(BYTE_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > BYTE_LIMIT || (!bytes.is_empty() && !bytes.ends_with(b"\n")) {
            return Err(invalid(
                "Codex binding log exceeds byte budget or has a torn tail",
            ));
        }
        let mut records = Vec::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            if records.len() == limit {
                #[cfg(test)]
                if std::env::var("ADK_CODEX_OBSERVATION_MUTANT")
                    .is_ok_and(|name| name == "log_budget")
                {
                    break;
                }
                return Err(invalid("Codex binding log exceeds record budget"));
            }
            records.push(serde_json::from_slice::<Logged>(line).map_err(io::Error::other)?);
        }
        let records = checked_ownership_records(channel, records)?;
        let settled_companions = super::terminal::settled_companions(&records);
        let tail = records.last().map_or(0, |record| record.event.seq);
        let mut digest = Sha256::new();
        digest.update(b"adk-codex-p5-observation-v1\0");
        digest.update(&bytes);
        Ok(ChannelObservation {
            events: records.into_iter().map(|record| record.event).collect(),
            settled_companions,
            tail,
            digest: format!("sha256:{}", hex::encode(digest.finalize())),
        })
    };
    read().map_err(|error| error.to_string())
}

#[cfg(all(test, unix))]
#[path = "observation_tests.rs"]
mod tests;
