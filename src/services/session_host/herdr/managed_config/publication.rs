use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::super::super::observe::RestoreUnverified;
use super::{invalid, open_regular, publish, synced_temp, unchanged};
use crate::services::discord::runtime_store::fsync_parent_dir;

pub(crate) const STATE_FILE: &str = ".agentdesk-config-state";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum State {
    Preparing,
    Committed(SystemTime),
}

pub(super) fn state(config: &Path) -> io::Result<Option<State>> {
    let path = config.with_file_name(STATE_FILE);
    let Some(mut file) = open_regular(&path)? else {
        return Ok(None);
    };
    let before = file.metadata()?;
    let mut text = String::new();
    (&mut file).take(257).read_to_string(&mut text)?;
    unchanged(&file, &path, &before)?;
    if text.len() > 256 {
        return Err(invalid("publication record exceeds its bound"));
    }
    let fields: Vec<_> = text.split_whitespace().collect();
    let result = match fields.as_slice() {
        ["v1", "preparing"] => State::Preparing,
        ["v1", "committed", seconds, nanos] => {
            let seconds = seconds
                .parse::<u64>()
                .map_err(|_| invalid("invalid publication seconds"))?;
            let nanos = nanos
                .parse::<u32>()
                .map_err(|_| invalid("invalid publication nanos"))?;
            if nanos >= 1_000_000_000 {
                return Err(invalid("invalid publication nanos"));
            }
            State::Committed(
                UNIX_EPOCH
                    .checked_add(Duration::new(seconds, nanos))
                    .ok_or_else(|| invalid("publication time overflow"))?,
            )
        }
        _ => return Err(invalid("unknown publication record")),
    };
    Ok(Some(result))
}

pub(crate) fn read(config: &Path) -> Result<Option<SystemTime>, RestoreUnverified> {
    match state(config).map_err(|_| RestoreUnverified::ConfigUnreadable)? {
        None => Ok(None),
        Some(State::Preparing) => Err(RestoreUnverified::ConfigChanged),
        Some(State::Committed(at)) => Ok(Some(at)),
    }
}

pub(super) fn store(config: &Path, state: State) -> io::Result<()> {
    let bytes = match state {
        State::Preparing => "v1 preparing\n".to_string(),
        State::Committed(at) => {
            let stamp = at.duration_since(UNIX_EPOCH).map_err(io::Error::other)?;
            format!(
                "v1 committed {} {}\n",
                stamp.as_secs(),
                stamp.subsec_nanos()
            )
        }
    };
    let path = config.with_file_name(STATE_FILE);
    let tmp = synced_temp(&path, bytes.as_bytes())?;
    publish(tmp, &path)?;
    fsync_parent_dir(&path)
}
