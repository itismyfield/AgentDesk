//! Record-only output from a single admitted Antigravity child.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::file_sync::{PARENT_DIR_FSYNC_FLUSHES, fsync_parent_dir};

use super::{STORE_DIR_NAME, durable};

pub(crate) mod replay;

pub(crate) const SEGMENT: &str = "0000.jsonl";
pub(crate) const CHECKPOINT: &str = "committed.json";
/// An iMessage turn slot is this prefix and the digest of its authenticated original.
pub(crate) const IMESSAGE_SLOT_PREFIX: &str = "imsg-";
// JSON escaping can expand a single input byte to six bytes.
const TEXT_BYTES: usize = 4096;

pub(crate) struct AgyReservation<'a> {
    pub destination: u64,
    pub logical_channel: u64,
    pub message: u64,
    pub nonce: Uuid,
    pub session_key: &'a str,
    pub writer_generation: std::num::NonZeroU64,
    pub resume: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImessageSource(String);

impl ImessageSource {
    pub(crate) fn from_guid(guid: &str) -> Option<Self> {
        if guid.is_empty()
            || guid.len() > 256
            || guid.trim() != guid
            || guid.chars().any(char::is_control)
        {
            return None;
        }
        let mut digest = Sha256::new();
        digest.update(b"agentdesk:agy:imessage-http:v1\0");
        digest.update(guid.as_bytes());
        Some(Self(format!("{:x}", digest.finalize())))
    }

    fn slot(&self) -> String {
        format!("{IMESSAGE_SLOT_PREFIX}{}", self.0)
    }

    pub(crate) fn digest(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Failure {
    Preparation,
    Spawn,
    Child,
    Protocol,
    Cancelled,
}

pub(crate) struct AgySpool {
    reservation: Value,
    turn_dir: PathBuf,
    segment: File,
    seq: u64,
    bytes: u64,
    prefix: Sha256,
    blocked: bool,
    terminal: bool,
    resume: Option<Uuid>,
}

impl AgySpool {
    pub(crate) fn reserve(root: &Path, input: AgyReservation<'_>) -> io::Result<Self> {
        Self::reserve_source(root, input, None)
    }

    pub(crate) fn reserve_imessage(
        root: &Path,
        input: AgyReservation<'_>,
        source: &ImessageSource,
    ) -> io::Result<Self> {
        Self::reserve_source(root, input, Some(source))
    }

    fn reserve_source(
        root: &Path,
        input: AgyReservation<'_>,
        source: Option<&ImessageSource>,
    ) -> io::Result<Self> {
        if !PARENT_DIR_FSYNC_FLUSHES {
            return Err(io::Error::other("agy spool requires directory sync"));
        }
        if input.destination == 0
            || input.logical_channel == 0
            || input.message == 0
            || input.nonce.is_nil()
            || input.session_key.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid agy spool identity",
            ));
        }
        let version = ensure_version_dir(root, input.destination)?;

        // One create-once directory per source. A new mailbox nonce cannot replace its obligation.
        let turn_dir =
            version.join(source.map_or_else(|| input.message.to_string(), ImessageSource::slot));
        private_dir(&turn_dir, false)?;
        let path = turn_dir.join(SEGMENT);
        let mut options = OpenOptions::new();
        options.append(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let segment = options.open(&path)?;
        fsync_parent_dir(&path)?;
        let mut spool = Self {
            reservation: json!({
                "v": 1, "provider": "antigravity", "destination": input.destination,
                "session_key": input.session_key, "attempt": 1,
                "turn_key": {"id": input.message, "nonce": input.nonce},
            }),
            turn_dir,
            segment,
            seq: 0,
            bytes: 0,
            prefix: Sha256::new(),
            blocked: false,
            terminal: false,
            resume: input.resume,
        };
        let mut open = json!({
            "ingress": "discord_foreground", "message_id": input.message,
            "logical_channel": input.logical_channel, "writer_generation": input.writer_generation,
            "resume": input.resume,
        });
        if let Some(source) = source {
            open["ingress"] = json!("imessage_http");
            open["original_input_sha256"] = json!(source.0);
        }
        spool.append("open", open)?;
        Ok(spool)
    }

    pub(crate) fn resume(&self) -> Option<Uuid> {
        self.resume
    }

    pub(crate) fn init(&mut self, conversation: Uuid) -> io::Result<()> {
        self.append("init", json!({"conversation_id": conversation}))
    }

    pub(crate) fn delta(&mut self, text: &str) -> io::Result<()> {
        for (_, part) in text_parts(text) {
            self.append("delta", json!({"text": part}))?;
        }
        Ok(())
    }

    pub(crate) fn done(&mut self, conversation: Uuid, body: &str) -> io::Result<()> {
        let first = self.seq;
        for (start, text) in text_parts(body) {
            self.append(
                "body_part",
                json!({"start": start, "end": start + text.len(), "text": text}),
            )?;
        }
        self.append("done", json!({
            "conversation_id": conversation,
            "body_first_seq": first, "body_end_seq": self.seq,
            "body_bytes": body.len(), "body_sha256": format!("{:x}", Sha256::digest(body.as_bytes())),
            "exit_code": 0, "split_version": 1,
        }))?;
        self.terminal = true;
        Ok(())
    }

    pub(crate) fn fail(&mut self, reason: Failure) -> io::Result<()> {
        if !self.terminal && !self.blocked {
            self.append("failed", json!({"reason": reason}))?;
            self.terminal = true;
        }
        Ok(())
    }

    fn append(&mut self, kind: &str, data: Value) -> io::Result<()> {
        if self.blocked || self.terminal {
            return Err(io::Error::other(
                "agy spool cannot advance its committed prefix",
            ));
        }
        let result = self.commit(kind, data);
        self.blocked = result.is_err();
        result
    }

    fn commit(&mut self, kind: &str, data: Value) -> io::Result<()> {
        let mut record = self.reservation.clone();
        record["seq"] = json!(self.seq);
        record["kind"] = json!(kind);
        record["data"] = data;
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        self.segment.write_all(&line)?;
        retry_sync(|| self.sync_segment())?;

        let mut prefix = self.prefix.clone();
        prefix.update(&line);
        let bytes = self.bytes + line.len() as u64;
        let checkpoint = serde_json::to_vec(&json!({
            "segment": SEGMENT, "committed_bytes": bytes, "last_seq": self.seq,
            "prefix_hash": format!("{:x}", prefix.clone().finalize()),
        }))?;
        retry_sync(|| durable::replace(&self.turn_dir.join(CHECKPOINT), &checkpoint))?;
        self.prefix = prefix;
        self.bytes = bytes;
        self.seq += 1;
        Ok(())
    }

    fn sync_segment(&self) -> io::Result<()> {
        #[cfg(test)]
        durable::fault::strike(&self.turn_dir, durable::fault::Step::AgySegmentSync)?;
        self.segment.sync_data()
    }
}

fn ensure_version_dir(root: &Path, destination_id: u64) -> io::Result<PathBuf> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Err(io::Error::other(
            "agy spool runtime root is not a directory",
        ));
    }
    let store = root.join(STORE_DIR_NAME);
    durable::ensure_dir(&store)?;
    let destination = store.join(destination_id.to_string());
    durable::ensure_dir(&destination)?;
    let agy = destination.join("agy");
    private_dir(&agy, true)?;
    let version = agy.join("v1");
    private_dir(&version, true)?;

    Ok(version)
}

fn retry_sync(mut sync: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    sync().or_else(|_| sync()).or_else(|_| sync())
}

fn private_dir(path: &Path, may_exist: bool) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Err(err) if may_exist && err.kind() == io::ErrorKind::AlreadyExists => {}
        other => other?,
    }
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    if std::os::unix::fs::MetadataExt::mode(&metadata) & 0o077 != 0
        || std::os::unix::fs::MetadataExt::uid(&metadata) != unsafe { libc::geteuid() }
    {
        return Err(io::Error::other("agy spool directory is not private"));
    }
    if !metadata.is_dir() {
        return Err(io::Error::other("agy spool path is not a directory"));
    }
    fsync_parent_dir(path)
}

fn text_parts(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == text.len() {
            return None;
        }
        let end = text.floor_char_boundary((start + TEXT_BYTES).min(text.len()));
        let part = (start, &text[start..end]);
        start = end;
        Some(part)
    })
}
