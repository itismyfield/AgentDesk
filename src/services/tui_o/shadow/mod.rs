//! Shared transcript capture, identity and derivation support for the output track.

pub mod binding_reader;
pub mod capture;

pub mod identity;
pub mod seal;
pub mod unit_plan;

pub mod tap;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const MAX_READ_BYTES: u64 = 1024 * 1024;
/// Raised when unit identity, turn extraction or historical rules change; old samples do not mix.
pub const IDENTITY_VERSION: u32 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowProvider {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Body,
    Tool,
    ToolResult,
}

/// Unit identity; session id is deliberately absent so forked sources inherit keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnitKey {
    pub channel_id: u64,
    pub provider: ShadowProvider,
    pub native_key: String,
    pub kind: UnitKind,
}

/// Transcript identity: a path is the same source only while dev/ino stay equal.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId {
    pub session_id: String,
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub source: SourceId,
    pub start: u64,
    pub end: u64,
}

/// One split piece of a sealed unit; `units` counts UTF-16 code units.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceDigest {
    pub index: u32,
    pub units: u32,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAnomalyKind {
    Missing,
    Replaced,
    Shrunk,
    PrefixMismatch,
    Unreadable,
    Oversized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAnomaly {
    pub source: SourceId,
    pub kind: SourceAnomalyKind,
    pub captured_through: u64,
    pub detail: String,
}

/// One newline-terminated record; `line` excludes the newline and `end` includes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedRecord {
    pub start: u64,
    pub end: u64,
    pub line: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureBatch {
    pub source: SourceId,
    pub records: Vec<CapturedRecord>,
    pub captured_through: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    Batch(CaptureBatch),
    Anomaly(SourceAnomaly),
}

/// Which channel/provider a captured source currently feeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBinding {
    pub channel_id: u64,
    pub provider: ShadowProvider,
    pub source: SourceId,
}

/// capture -> derive boundary: yields complete records from one source.
pub trait CaptureSource: Send {
    fn source(&self) -> &SourceId;
    fn poll(&mut self, max_bytes: u64) -> CaptureOutcome;
}
