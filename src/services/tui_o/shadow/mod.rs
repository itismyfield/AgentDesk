//! Read-only shadow of TUI output: derives what O would post and diffs it against Legacy.
//! Its only write target is `root::ShadowRoot`; `scripts/check_o_shadow_write_zero.py` enforces that.

pub mod binding_reader;
pub mod capture;
pub mod root;

// Derive-side modules (identity, seal, derive, unit_plan) are declared below.
pub mod derive;
pub mod identity;
pub mod seal;
pub mod unit_plan;

// Observe-side modules (tap, diff, report, metrics) are declared below.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_READ_BYTES: u64 = 1024 * 1024;
pub const TAP_CAPACITY: usize = 1024;
pub const DISK_CAP_BYTES: u64 = 1024 * 1024 * 1024;
pub const MATCH_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Raised when unit identity, turn extraction or historical rules change; old samples do not mix.
pub const IDENTITY_VERSION: u32 = 2;

/// `tui_o.shadow` settings; disabled unless explicitly enabled.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShadowConfig {
    pub enabled: bool,
    pub channel_allowlist: Vec<u64>,
}

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowUnit {
    pub unit_key: UnitKey,
    pub kind: UnitKind,
    pub source_range: SourceRange,
    pub sealed_at: DateTime<Utc>,
    pub pieces: Vec<PieceDigest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyEdit {
    pub at: DateTime<Utc>,
    pub content_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyMsg {
    pub msg_id: u64,
    pub channel_id: u64,
    pub created_at: DateTime<Utc>,
    pub edits: Vec<LegacyEdit>,
    pub deleted: bool,
    pub content_sha256: String,
}

/// Bot-authored gateway event copied by the tap; plain ids keep serenity out of this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LegacyTapEvent {
    Created {
        channel_id: u64,
        msg_id: u64,
        at: DateTime<Utc>,
        content: String,
    },
    Updated {
        channel_id: u64,
        msg_id: u64,
        at: DateTime<Utc>,
        content: Option<String>,
    },
    Deleted {
        channel_id: u64,
        msg_id: u64,
        at: DateTime<Utc>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffClass {
    Match,
    FormatOnly,
    LegacyMissing,
    LegacyDuplicate,
    LegacyExtra,
    OrderDiff,
    OUnsealed,
    OSchemaBlocked,
    OExcluded,
    TapGap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DiffCause {
    #[serde(rename = "O_defect")]
    ODefect,
    #[serde(rename = "Legacy_defect")]
    LegacyDefect,
    Expected,
    Unknown,
}

/// `unit_key` is absent for rows with no O unit (Legacy extras, tap gaps).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRecord {
    pub channel_id: u64,
    pub unit_key: Option<UnitKey>,
    pub class: DiffClass,
    pub legacy_msg_ids: Vec<u64>,
    pub cause: DiffCause,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAnomalyKind {
    Replaced,
    Shrunk,
    PrefixMismatch,
    Unreadable,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingChange {
    pub channel_id: u64,
    pub old: Option<SourceBinding>,
    pub new: Option<SourceBinding>,
    pub at: DateTime<Utc>,
}

/// Result of deriving one unit from captured records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeriveOutput {
    Sealed(ShadowUnit),
    Excluded {
        unit_key: UnitKey,
        reason: String,
    },
    SchemaBlocked {
        channel_id: u64,
        source_range: SourceRange,
        reason: String,
    },
}

/// One persisted line under the shadow root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShadowRecord {
    Derived { output: DeriveOutput },
    Legacy { msg: LegacyMsg },
    Diff { diff: DiffRecord },
    Anomaly { anomaly: SourceAnomaly },
    Binding { change: BindingChange },
    TapGap { dropped: u64 },
}

/// capture -> derive boundary: yields complete records from one source.
pub trait CaptureSource: Send {
    fn source(&self) -> &SourceId;
    fn poll(&mut self, max_bytes: u64) -> CaptureOutcome;
}

/// derive boundary: turns captured records into sealed, excluded or blocked units.
pub trait ShadowDerive: Send {
    fn derive(&mut self, binding: &SourceBinding, batch: &CaptureBatch) -> Vec<DeriveOutput>;
    fn unsealed(&self) -> Vec<UnitKey>;
}

/// derive + tap -> diff boundary: correlates O units with Legacy messages in `MATCH_WINDOW`.
pub trait ShadowDiff: Send {
    fn observe_derived(&mut self, output: &DeriveOutput, at: DateTime<Utc>);
    fn observe_legacy(&mut self, event: &LegacyTapEvent);
    fn observe_tap_gap(&mut self, dropped: u64, at: DateTime<Utc>);
    fn drain_ready(&mut self, now: DateTime<Utc>) -> Vec<DiffRecord>;
}

/// diff/report boundary: the single persistence sink, implemented by `root::ShadowStore`.
pub trait ShadowSink: Send {
    fn append(&mut self, record: &ShadowRecord) -> std::io::Result<()>;
}

/// Everything the shadow receives; no HTTP client, shared runtime data or tmux handle.
pub struct ShadowInputs {
    pub binding_reader: binding_reader::BindingReader,
    pub gateway_rx: tokio::sync::mpsc::Receiver<LegacyTapEvent>,
}
