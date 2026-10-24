//! Persistence preserves historical server identity without minting a live proof.
use super::super::{contract::ServerWitness, provenance::StartIdentity};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    socket: PathBuf,
    pid: u32,
    start: Start,
}
#[derive(Serialize, Deserialize)]
enum Start {
    Darwin { seconds: u64, micros: u64 },
    LinuxTicks(u64),
}
pub(super) fn serialize<S: Serializer>(
    w: &ServerWitness,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    Wire {
        socket: w.socket.clone(),
        pid: w.pid,
        start: match w.start {
            StartIdentity::Darwin { seconds, micros } => Start::Darwin { seconds, micros },
            StartIdentity::LinuxTicks(ticks) => Start::LinuxTicks(ticks),
        },
    }
    .serialize(serializer)
}
pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<ServerWitness, D::Error> {
    let w = Wire::deserialize(deserializer)?;
    let start = match w.start {
        Start::Darwin { seconds, micros } if seconds != 0 && micros < 1_000_000 => {
            StartIdentity::Darwin { seconds, micros }
        }
        Start::LinuxTicks(ticks) if ticks != 0 => StartIdentity::LinuxTicks(ticks),
        _ => return Err(serde::de::Error::custom("invalid process start identity")),
    };
    if w.pid == 0 || !w.socket.is_absolute() {
        return Err(serde::de::Error::custom("invalid server witness"));
    }
    Ok(ServerWitness {
        socket: w.socket,
        pid: w.pid,
        start,
    })
}
