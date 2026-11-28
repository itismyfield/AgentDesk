use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::file_sync::fsync_parent_dir;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::{MAX_READ_BYTES, ShadowProvider, UnitKey, UnitKind};

use super::{CHECKPOINT, Failure, SEGMENT, STORE_DIR_NAME};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgyTarget {
    pub(crate) destination: u64,
    pub(crate) logical_channel: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct AgyTurnKey {
    pub(crate) id: u64,
    pub(crate) nonce: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct AgyOpen {
    ingress: String,
    message_id: u64,
    pub(crate) logical_channel: u64,
    pub(crate) writer_generation: NonZeroU64,
    pub(crate) resume: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgyReplay {
    pub(crate) key: AgyTurnKey,
    pub(crate) session_key: String,
    pub(crate) open: AgyOpen,
    pub(crate) frontier: AgyFrontier,
    pub(crate) outcome: AgyOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgyOutcome {
    Pending,
    Failed(Failure),
    Sealed(AgySeal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgySeal {
    pub(crate) conversation_id: Uuid,
    pub(crate) body: String,
    pub(crate) body_sha256: String,
    pub(crate) split_version: u32,
    pub(crate) reply: UnitKey,
    pub(crate) footer: UnitKey,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct AgyFrontier {
    pub(crate) path: PathBuf,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) bytes: u64,
    pub(crate) last_seq: u64,
    pub(crate) prefix_hash: String,
}

#[derive(Deserialize)]
struct Checkpoint {
    segment: String,
    committed_bytes: u64,
    last_seq: u64,
    prefix_hash: String,
}

#[derive(Deserialize)]
struct Record {
    v: u32,
    provider: ShadowProvider,
    destination: u64,
    session_key: String,
    attempt: u32,
    seq: u64,
    turn_key: AgyTurnKey,
    #[serde(flatten)]
    data: Data,
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum Data {
    Open(AgyOpen),
    Init {
        conversation_id: Uuid,
    },
    Delta {
        text: String,
    },
    BodyPart {
        start: usize,
        end: usize,
        text: String,
    },
    Done {
        conversation_id: Uuid,
        body_first_seq: u64,
        body_end_seq: u64,
        body_bytes: usize,
        body_sha256: String,
        exit_code: i32,
        split_version: u32,
    },
    Failed {
        reason: Failure,
    },
}

/// Replays only the published v1 prefix. A prior frontier pins the inode and the bytes already read.
pub(crate) fn replay(
    root: &Path,
    target: AgyTarget,
    message: u64,
    previous: Option<&AgyFrontier>,
) -> io::Result<AgyReplay> {
    let version = version_dir(root, target.destination)?;
    if read_retired_marker(&version, target)? {
        return Err(invalid("agy spool belongs to a retired child"));
    }
    let turn = checked_dir(version.join(message.to_string()))?;
    let checkpoint_path = turn.join(CHECKPOINT);
    let checkpoint_file = open_regular(&checkpoint_path)?;
    let checkpoint: Checkpoint = serde_json::from_reader(checkpoint_file.take(4096))?;
    if checkpoint.segment != SEGMENT || checkpoint.committed_bytes == 0 {
        return Err(invalid("invalid agy checkpoint"));
    }
    // A checkpoint rename can be visible before the producer flushes its parent directory.
    fsync_parent_dir(&checkpoint_path)?;

    let path = turn.join(SEGMENT);
    let file = open_regular(&path)?;
    let meta = file.metadata()?;
    let (dev, ino) = file_identity(&meta);
    if ino == 0 {
        return Err(invalid("agy spool file identity is unavailable"));
    }
    let frontier = AgyFrontier {
        path,
        dev,
        ino,
        bytes: checkpoint.committed_bytes,
        last_seq: checkpoint.last_seq,
        prefix_hash: checkpoint.prefix_hash,
    };
    if meta.len() < frontier.bytes
        || previous.is_some_and(|old| {
            old.path != frontier.path
                || old.ino != frontier.ino
                || old.bytes == 0
                || old.bytes > frontier.bytes
        })
    {
        return Err(invalid("agy segment identity or committed length changed"));
    }

    let mut records = BufReader::new(file.take(frontier.bytes));
    let mut digest = Sha256::new();
    let mut offset = 0;
    let mut seq = 0;
    let mut line = Vec::new();
    let mut state = None::<ReplayState>;
    let mut checked_previous = previous.is_none();
    while offset < frontier.bytes {
        line.clear();
        let size = records
            .by_ref()
            .take(MAX_READ_BYTES)
            .read_until(b'\n', &mut line)?;
        if size == 0 || line.last() != Some(&b'\n') {
            return Err(invalid("agy checkpoint ends outside a complete record"));
        }
        digest.update(&line);
        offset += size as u64;
        if let Some(old) = previous
            && old.bytes == offset
        {
            checked_previous = old.last_seq == seq && old.prefix_hash == hex(digest.clone());
        }
        let record: Record = serde_json::from_slice(&line)?;
        if record.v != 1
            || record.provider != ShadowProvider::Antigravity
            || record.destination != target.destination
            || record.seq != seq
            || record.attempt != 1
            || record.turn_key.id != message
            || !crate::services::tui_input::input_key::is_discord_key(message)
            || record.turn_key.nonce.is_nil()
        {
            return Err(invalid(
                "agy record does not name the selected source and sequence",
            ));
        }
        match &mut state {
            Some(state) => state.apply(record, target)?,
            None => state = Some(ReplayState::open(record, target)?),
        }
        seq += 1;
    }
    let current = fs::symlink_metadata(&frontier.path)?;
    if !checked_previous
        || frontier.last_seq != seq - 1
        || frontier.prefix_hash != hex(digest)
        || file_identity(&current) != (frontier.dev, frontier.ino)
        || current.len() < frontier.bytes
        || read_retired_marker(&version, target)?
    {
        return Err(invalid("agy committed prefix does not match its frontier"));
    }
    let state = state.ok_or_else(|| invalid("agy source has no open"))?;
    Ok(AgyReplay {
        key: state.key,
        session_key: state.session_key,
        open: state.open,
        frontier,
        outcome: state.outcome,
    })
}

pub(crate) fn version_dir(root: &Path, destination: u64) -> io::Result<PathBuf> {
    let store = checked_dir(checked_dir(root.to_owned())?.join(STORE_DIR_NAME))?;
    let destination = checked_dir(store.join(destination.to_string()))?;
    checked_dir(checked_dir(destination.join("agy"))?.join("v1"))
}

pub(crate) fn checked_dir(path: PathBuf) -> io::Result<PathBuf> {
    if !fs::symlink_metadata(&path)?.is_dir() {
        return Err(invalid("agy spool path is not a directory"));
    }
    Ok(path)
}

fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK,
    );
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("agy spool file is not regular"));
    }
    Ok(file)
}

struct ReplayState {
    key: AgyTurnKey,
    session_key: String,
    open: AgyOpen,
    conversation: Option<Uuid>,
    body: String,
    first_part: Option<u64>,
    outcome: AgyOutcome,
}

impl ReplayState {
    fn open(record: Record, target: AgyTarget) -> io::Result<Self> {
        let Data::Open(open) = record.data else {
            return Err(invalid("agy first record is not open"));
        };
        if open.ingress != "discord_foreground"
            || open.logical_channel != target.logical_channel
            || open.message_id != record.turn_key.id
            || record.session_key.is_empty()
            || open.resume.is_some_and(|id| id.is_nil())
        {
            return Err(invalid(
                "agy open does not match the selected destination and ingress",
            ));
        }
        Ok(Self {
            key: record.turn_key,
            session_key: record.session_key,
            conversation: open.resume,
            open,
            first_part: None,
            body: String::new(),
            outcome: AgyOutcome::Pending,
        })
    }

    fn apply(&mut self, record: Record, target: AgyTarget) -> io::Result<()> {
        if self.key != record.turn_key
            || self.session_key != record.session_key
            || self.outcome != AgyOutcome::Pending
        {
            return Err(invalid(
                "agy identity changed or record followed a terminal",
            ));
        }
        match record.data {
            Data::Init { conversation_id } if self.first_part.is_none() => {
                self.check_conversation(conversation_id)?;
                self.conversation = Some(conversation_id);
            }
            Data::Delta { text } if self.first_part.is_none() && !text.is_empty() => {}
            Data::BodyPart { start, end, text }
                if start == self.body.len()
                    && end.checked_sub(start) == Some(text.len())
                    && !text.is_empty() =>
            {
                self.first_part.get_or_insert(record.seq);
                self.body.push_str(&text);
            }
            Data::Done {
                conversation_id,
                body_first_seq,
                body_end_seq,
                body_bytes,
                body_sha256,
                exit_code,
                split_version,
            } => {
                self.check_conversation(conversation_id)?;
                if exit_code != 0
                    || split_version != 1
                    || self.body.is_empty()
                    || self.first_part != Some(body_first_seq)
                    || body_end_seq != record.seq
                    || self.body.len() != body_bytes
                    || body_sha256 != format!("{:x}", Sha256::digest(self.body.as_bytes()))
                {
                    return Err(invalid("agy Done does not seal the canonical body"));
                }
                let unit = |kind, suffix| UnitKey {
                    channel_id: target.destination,
                    provider: ShadowProvider::Antigravity,
                    native_key: format!("{}:{}:{suffix}", self.key.id, self.key.nonce),
                    kind,
                };
                self.outcome = AgyOutcome::Sealed(AgySeal {
                    conversation_id,
                    body: std::mem::take(&mut self.body),
                    body_sha256,
                    split_version,
                    reply: unit(UnitKind::Body, "reply"),
                    footer: unit(UnitKind::Recap, "footer"),
                });
            }
            Data::Failed { reason } => self.outcome = AgyOutcome::Failed(reason),
            _ => {
                return Err(invalid(
                    "agy record is out of order or has an invalid body range",
                ));
            }
        }
        Ok(())
    }

    fn check_conversation(&self, id: Uuid) -> io::Result<()> {
        if id.is_nil() || self.conversation.is_some_and(|known| known != id) {
            return Err(invalid("agy conversation id changed"));
        }
        Ok(())
    }
}

fn hex(digest: Sha256) -> String {
    format!("{:x}", digest.finalize())
}

fn invalid(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

const RETIRED: &str = "retired.json";

pub(crate) fn child_retired(root: &Path, target: AgyTarget) -> io::Result<bool> {
    match version_dir(root, target.destination) {
        Ok(dir) => read_retired_marker(&dir, target),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_retired_marker(version: &Path, target: AgyTarget) -> io::Result<bool> {
    let marker = match open_regular(&version.join(RETIRED)) {
        Ok(marker) => marker,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let parent: u64 = serde_json::from_reader(marker.take(32))?;
    if target.logical_channel != parent || target.destination == parent {
        return Err(invalid("agy retirement names another child relationship"));
    }
    Ok(true)
}

pub(crate) fn mark_child_retired(root: &Path, target: AgyTarget) -> io::Result<()> {
    let dir = super::ensure_version_dir(root, target.destination)?;
    super::durable::replace(
        &dir.join(RETIRED),
        &serde_json::to_vec(&target.logical_channel)?,
    )
}
