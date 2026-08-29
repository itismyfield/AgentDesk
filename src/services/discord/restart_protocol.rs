//! Dormant P1 contract for durable, versioned restart attempts.
//!
//! This module has no production caller and does not grant claim, send, cleanup,
//! or activation authority. Legacy `RestartCompletionReport` remains separate
//! compatibility data and is never decoded as an attempt record.

#![allow(dead_code)] // #5575: consumed only by tests until later protocol slices wire adapters.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use super::runtime_store::fsync_parent_dir;

pub(crate) const RESTART_PROTOCOL_VERSION: RestartProtocolVersion = RestartProtocolVersion(2);
const RESTART_PROTOCOL_NAMESPACE: &str = "v2";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RestartProtocolVersion(pub(crate) u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RestartAttemptId(String);

impl RestartAttemptId {
    pub(crate) fn new_random() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let parsed = Uuid::parse_str(value).map_err(|error| error.to_string())?;
        let canonical = parsed.to_string();
        if value != canonical {
            return Err("restart attempt ID is not canonical lowercase UUID text".to_string());
        }
        Ok(Self(canonical))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for RestartAttemptId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RestartAttemptId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartAttemptTarget {
    pub(crate) provider: String,
    pub(crate) channel_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FrozenReportPrincipal {
    pub(crate) provider: String,
    pub(crate) token_hash: String,
    pub(crate) bot_user_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProcessGenerationAllocationRoute {
    AdvancedWithSyncedRename,
    ParentSyncFailed,
    CounterReadFailed,
    Saturated,
    WriteFailed,
    LockFailed,
    PathUnavailable,
    Unwitnessed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessGenerationProvenance {
    pub(crate) generation: u64,
    pub(crate) allocation_route: ProcessGenerationAllocationRoute,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartTerminalProofRef {
    relative_path: String,
}

impl RestartTerminalProofRef {
    pub(crate) fn new(relative_path: impl Into<String>) -> Result<Self, String> {
        let value = Self {
            relative_path: relative_path.into(),
        };
        validate_relative_ref(&value.relative_path)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartDurableReceiptRef {
    relative_path: String,
}

impl RestartDurableReceiptRef {
    pub(crate) fn new(relative_path: impl Into<String>) -> Result<Self, String> {
        let value = Self {
            relative_path: relative_path.into(),
        };
        validate_relative_ref(&value.relative_path)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestartTerminalOutcome {
    Completed,
    RolledBack,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(crate) enum RestartAttemptPhase {
    Requested,
    Bound {
        process: ProcessGenerationProvenance,
    },
    Running {
        process: ProcessGenerationProvenance,
    },
    Terminal {
        process: ProcessGenerationProvenance,
        outcome: RestartTerminalOutcome,
        terminal_proof: RestartTerminalProofRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        durable_receipt: Option<RestartDurableReceiptRef>,
    },
}

impl RestartAttemptPhase {
    fn ordinal(&self) -> u8 {
        match self {
            Self::Requested => 0,
            Self::Bound { .. } => 1,
            Self::Running { .. } => 2,
            Self::Terminal { .. } => 3,
        }
    }

    fn process(&self) -> Option<ProcessGenerationProvenance> {
        match self {
            Self::Requested => None,
            Self::Bound { process }
            | Self::Running { process }
            | Self::Terminal { process, .. } => Some(*process),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartAttemptRecord {
    pub(crate) schema_version: RestartProtocolVersion,
    pub(crate) attempt_id: RestartAttemptId,
    pub(crate) target: RestartAttemptTarget,
    pub(crate) request_nonce: String,
    pub(crate) requested_generation: u64,
    pub(crate) principal: FrozenReportPrincipal,
    pub(crate) phase: RestartAttemptPhase,
}

impl RestartAttemptRecord {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != RESTART_PROTOCOL_VERSION {
            return Err("restart attempt record has unsupported schema version".to_string());
        }
        validate_path_component("provider", &self.target.provider)?;
        validate_path_component("principal provider", &self.principal.provider)?;
        if self.request_nonce.trim().is_empty() {
            return Err("restart request nonce is missing".to_string());
        }
        if self.principal.token_hash.trim().is_empty() || self.principal.bot_user_id == 0 {
            return Err("frozen report principal is incomplete".to_string());
        }
        match &self.phase {
            RestartAttemptPhase::Terminal {
                terminal_proof,
                durable_receipt,
                ..
            } => {
                validate_relative_ref(&terminal_proof.relative_path)?;
                if let Some(receipt) = durable_receipt {
                    validate_relative_ref(&receipt.relative_path)?;
                }
            }
            RestartAttemptPhase::Requested
            | RestartAttemptPhase::Bound { .. }
            | RestartAttemptPhase::Running { .. } => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestartTransitionError {
    TerminalIsImmutable,
    NonMonotonic,
    SkippedPhase,
    GenerationChanged,
    InvalidRecord(String),
}

pub(crate) fn reduce_restart_attempt(
    current: &RestartAttemptRecord,
    next: RestartAttemptPhase,
) -> Result<RestartAttemptRecord, RestartTransitionError> {
    if matches!(current.phase, RestartAttemptPhase::Terminal { .. }) {
        return Err(RestartTransitionError::TerminalIsImmutable);
    }
    let current_ordinal = current.phase.ordinal();
    let next_ordinal = next.ordinal();
    if next_ordinal <= current_ordinal {
        return Err(RestartTransitionError::NonMonotonic);
    }
    if next_ordinal != current_ordinal + 1 {
        return Err(RestartTransitionError::SkippedPhase);
    }
    if let (Some(current_process), Some(next_process)) = (current.phase.process(), next.process())
        && current_process != next_process
    {
        return Err(RestartTransitionError::GenerationChanged);
    }
    let mut reduced = current.clone();
    reduced.phase = next;
    reduced
        .validate()
        .map_err(RestartTransitionError::InvalidRecord)?;
    Ok(reduced)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestartAttemptDecode {
    Current(RestartAttemptRecord),
    UnsupportedVersion {
        version: u64,
        bytes: Vec<u8>,
    },
    Corrupt {
        error: RestartAttemptDecodeError,
        bytes: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestartAttemptDecodeError {
    MalformedJson,
    MissingSchemaVersion,
    InvalidSchemaVersion,
    InvalidRecord(String),
}

pub(crate) fn decode_restart_attempt(bytes: &[u8]) -> RestartAttemptDecode {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => {
            return RestartAttemptDecode::Corrupt {
                error: RestartAttemptDecodeError::MalformedJson,
                bytes: bytes.to_vec(),
            };
        }
    };
    let Some(version_value) = value.get("schema_version") else {
        return RestartAttemptDecode::Corrupt {
            error: RestartAttemptDecodeError::MissingSchemaVersion,
            bytes: bytes.to_vec(),
        };
    };
    let Some(version) = version_value.as_u64() else {
        return RestartAttemptDecode::Corrupt {
            error: RestartAttemptDecodeError::InvalidSchemaVersion,
            bytes: bytes.to_vec(),
        };
    };
    if version != u64::from(RESTART_PROTOCOL_VERSION.0) {
        return RestartAttemptDecode::UnsupportedVersion {
            version,
            bytes: bytes.to_vec(),
        };
    }
    let record: RestartAttemptRecord = match serde_json::from_value(value) {
        Ok(record) => record,
        Err(error) => {
            return RestartAttemptDecode::Corrupt {
                error: RestartAttemptDecodeError::InvalidRecord(error.to_string()),
                bytes: bytes.to_vec(),
            };
        }
    };
    match record.validate() {
        Ok(()) => RestartAttemptDecode::Current(record),
        Err(error) => RestartAttemptDecode::Corrupt {
            error: RestartAttemptDecodeError::InvalidRecord(error),
            bytes: bytes.to_vec(),
        },
    }
}

pub(crate) fn encode_restart_attempt(record: &RestartAttemptRecord) -> Result<Vec<u8>, String> {
    record.validate()?;
    serde_json::to_vec_pretty(record).map_err(|error| error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestartAttemptPathIdentity {
    pub(crate) target: RestartAttemptTarget,
    pub(crate) attempt_id: RestartAttemptId,
}

pub(crate) fn restart_attempt_path(
    root: &Path,
    target: &RestartAttemptTarget,
    attempt_id: &RestartAttemptId,
) -> Result<PathBuf, String> {
    validate_path_component("provider", &target.provider)?;
    Ok(root
        .join(RESTART_PROTOCOL_NAMESPACE)
        .join(&target.provider)
        .join(target.channel_id.to_string())
        .join(format!("{}.json", attempt_id.as_str())))
}

pub(crate) fn restart_attempt_identity_from_path(
    root: &Path,
    path: &Path,
) -> Result<RestartAttemptPathIdentity, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "restart attempt path is outside its store root".to_string())?;
    let components: Vec<_> = relative.components().collect();
    let [
        Component::Normal(version),
        Component::Normal(provider),
        Component::Normal(channel),
        Component::Normal(file),
    ] = components.as_slice()
    else {
        return Err("restart attempt path has an unexpected shape".to_string());
    };
    if version != RESTART_PROTOCOL_NAMESPACE {
        return Err("restart attempt path has an unsupported namespace".to_string());
    }
    let provider = provider
        .to_str()
        .ok_or_else(|| "restart attempt provider is not UTF-8".to_string())?;
    validate_path_component("provider", provider)?;
    let channel_id = channel
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| "restart attempt channel is invalid".to_string())?;
    let file = file
        .to_str()
        .and_then(|value| value.strip_suffix(".json"))
        .ok_or_else(|| "restart attempt filename is invalid".to_string())?;
    Ok(RestartAttemptPathIdentity {
        target: RestartAttemptTarget {
            provider: provider.to_string(),
            channel_id,
        },
        attempt_id: RestartAttemptId::parse(file)?,
    })
}

fn validate_path_component(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 64
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(format!("{label} is not a safe path component"));
    }
    Ok(())
}

fn validate_relative_ref(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("durable reference must be a non-empty relative path".to_string());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestartAttemptPublicationStep {
    PrepareParent,
    WriteTemp,
    SyncTemp,
    PublishCanonical,
    SyncParent,
}

pub(crate) trait RestartAttemptPublicationOps {
    fn prepare_parent(&mut self, parent: &Path) -> std::io::Result<()>;
    fn write_temp(&mut self, temp: &Path, bytes: &[u8]) -> std::io::Result<()>;
    fn sync_temp(&mut self, temp: &Path) -> std::io::Result<()>;
    fn publish_canonical(&mut self, temp: &Path, canonical: &Path) -> std::io::Result<()>;
    fn sync_parent(&mut self, canonical: &Path) -> std::io::Result<()>;
}

struct FilesystemPublicationOps;

impl RestartAttemptPublicationOps for FilesystemPublicationOps {
    fn prepare_parent(&mut self, parent: &Path) -> std::io::Result<()> {
        fs::create_dir_all(parent)
    }

    fn write_temp(&mut self, temp: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp)?;
        file.write_all(bytes)
    }

    fn sync_temp(&mut self, temp: &Path) -> std::io::Result<()> {
        fs::OpenOptions::new().read(true).open(temp)?.sync_all()
    }

    fn publish_canonical(&mut self, temp: &Path, canonical: &Path) -> std::io::Result<()> {
        fs::rename(temp, canonical)
    }

    fn sync_parent(&mut self, canonical: &Path) -> std::io::Result<()> {
        fsync_parent_dir(canonical)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishedRestartAttempt {
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestartAttemptPublicationError {
    Invalid(String),
    Operation {
        step: RestartAttemptPublicationStep,
        error: String,
    },
}

pub(crate) fn publish_restart_attempt_in_root_with_ops(
    root: &Path,
    record: &RestartAttemptRecord,
    ops: &mut impl RestartAttemptPublicationOps,
) -> Result<PublishedRestartAttempt, RestartAttemptPublicationError> {
    let bytes = encode_restart_attempt(record).map_err(RestartAttemptPublicationError::Invalid)?;
    let canonical = restart_attempt_path(root, &record.target, &record.attempt_id)
        .map_err(RestartAttemptPublicationError::Invalid)?;
    let parent = canonical
        .parent()
        .ok_or_else(|| RestartAttemptPublicationError::Invalid("missing parent".to_string()))?;
    run_step(RestartAttemptPublicationStep::PrepareParent, || {
        ops.prepare_parent(parent)
    })?;
    let temp = canonical.with_file_name(format!(
        ".{}.{}.tmp",
        canonical
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("attempt"),
        Uuid::new_v4().simple()
    ));
    if let Err(error) = run_step(RestartAttemptPublicationStep::WriteTemp, || {
        ops.write_temp(&temp, &bytes)
    }) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = run_step(RestartAttemptPublicationStep::SyncTemp, || {
        ops.sync_temp(&temp)
    }) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = run_step(RestartAttemptPublicationStep::PublishCanonical, || {
        ops.publish_canonical(&temp, &canonical)
    }) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    run_step(RestartAttemptPublicationStep::SyncParent, || {
        ops.sync_parent(&canonical)
    })?;
    // The returned token exists only after directory sync succeeds. This is an
    // atomic record update contract, not the no-replace claim authority owned by P2.
    Ok(PublishedRestartAttempt { path: canonical })
}

fn run_step(
    step: RestartAttemptPublicationStep,
    operation: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), RestartAttemptPublicationError> {
    operation().map_err(|error| RestartAttemptPublicationError::Operation {
        step,
        error: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process() -> ProcessGenerationProvenance {
        ProcessGenerationProvenance {
            generation: 42,
            allocation_route: ProcessGenerationAllocationRoute::AdvancedWithSyncedRename,
        }
    }

    fn fixture(attempt_id: RestartAttemptId) -> RestartAttemptRecord {
        RestartAttemptRecord {
            schema_version: RESTART_PROTOCOL_VERSION,
            attempt_id,
            target: RestartAttemptTarget {
                provider: "claude".to_string(),
                channel_id: 123,
            },
            request_nonce: "request-nonce".to_string(),
            requested_generation: 41,
            principal: FrozenReportPrincipal {
                provider: "claude".to_string(),
                token_hash: "frozen-token-hash".to_string(),
                bot_user_id: 456,
            },
            phase: RestartAttemptPhase::Requested,
        }
    }

    #[test]
    fn two_attempts_for_the_same_channel_have_distinct_canonical_paths() {
        let root = Path::new("store");
        let first = fixture(RestartAttemptId::new_random());
        let second = fixture(RestartAttemptId::new_random());
        let first_path = restart_attempt_path(root, &first.target, &first.attempt_id).unwrap();
        let second_path = restart_attempt_path(root, &second.target, &second.attempt_id).unwrap();
        assert_ne!(first_path, second_path);
        assert!(
            first_path
                .to_string_lossy()
                .contains(first.attempt_id.as_str())
        );
        assert!(
            second_path
                .to_string_lossy()
                .contains(second.attempt_id.as_str())
        );
    }

    #[test]
    fn terminal_attempt_cannot_regress_to_pending() {
        let requested = fixture(RestartAttemptId::new_random());
        let bound = reduce_restart_attempt(
            &requested,
            RestartAttemptPhase::Bound { process: process() },
        )
        .unwrap();
        let running =
            reduce_restart_attempt(&bound, RestartAttemptPhase::Running { process: process() })
                .unwrap();
        let terminal = reduce_restart_attempt(
            &running,
            RestartAttemptPhase::Terminal {
                process: process(),
                outcome: RestartTerminalOutcome::Completed,
                terminal_proof: RestartTerminalProofRef::new("proofs/terminal.json").unwrap(),
                durable_receipt: Some(
                    RestartDurableReceiptRef::new("receipts/discord.json").unwrap(),
                ),
            },
        )
        .unwrap();
        assert_eq!(
            reduce_restart_attempt(&terminal, RestartAttemptPhase::Requested),
            Err(RestartTransitionError::TerminalIsImmutable)
        );
    }

    #[test]
    fn future_version_fails_closed_and_preserves_bytes() {
        let bytes = br#"{"schema_version":999,"attempt_id":"ignored"}"#;
        assert_eq!(
            decode_restart_attempt(bytes),
            RestartAttemptDecode::UnsupportedVersion {
                version: 999,
                bytes: bytes.to_vec(),
            }
        );
    }

    #[test]
    fn corrupt_or_missing_attempt_identity_is_never_legacy_authority() {
        for bytes in [
            br#"{"schema_version":2}"#.as_slice(),
            br#"{"schema_version":2,"attempt_id":"../escape"}"#.as_slice(),
            br#"{"schema_version":2,"attempt_id":null}"#.as_slice(),
        ] {
            assert!(matches!(
                decode_restart_attempt(bytes),
                RestartAttemptDecode::Corrupt {
                    error: RestartAttemptDecodeError::InvalidRecord(_),
                    ..
                }
            ));
        }
    }

    #[test]
    fn codec_preserves_generation_allocation_route_and_terminal_references() {
        let requested = fixture(RestartAttemptId::new_random());
        let bound = reduce_restart_attempt(
            &requested,
            RestartAttemptPhase::Bound { process: process() },
        )
        .unwrap();
        let running =
            reduce_restart_attempt(&bound, RestartAttemptPhase::Running { process: process() })
                .unwrap();
        let terminal = reduce_restart_attempt(
            &running,
            RestartAttemptPhase::Terminal {
                process: process(),
                outcome: RestartTerminalOutcome::Completed,
                terminal_proof: RestartTerminalProofRef::new("proofs/terminal.json").unwrap(),
                durable_receipt: Some(
                    RestartDurableReceiptRef::new("receipts/discord.json").unwrap(),
                ),
            },
        )
        .unwrap();
        let bytes = encode_restart_attempt(&terminal).unwrap();
        assert_eq!(
            decode_restart_attempt(&bytes),
            RestartAttemptDecode::Current(terminal)
        );
    }

    #[test]
    fn generation_binding_cannot_change_after_the_actual_process_is_bound() {
        let requested = fixture(RestartAttemptId::new_random());
        let bound = reduce_restart_attempt(
            &requested,
            RestartAttemptPhase::Bound { process: process() },
        )
        .unwrap();
        let preview_instead_of_actual = ProcessGenerationProvenance {
            generation: bound.requested_generation,
            allocation_route: ProcessGenerationAllocationRoute::Unwitnessed,
        };
        assert_eq!(
            reduce_restart_attempt(
                &bound,
                RestartAttemptPhase::Running {
                    process: preview_instead_of_actual,
                },
            ),
            Err(RestartTransitionError::GenerationChanged)
        );
    }

    #[test]
    fn attempt_path_round_trips_identity_and_rejects_unsafe_components() {
        let root = Path::new("store");
        let record = fixture(RestartAttemptId::new_random());
        let path = restart_attempt_path(root, &record.target, &record.attempt_id).unwrap();
        assert_eq!(
            restart_attempt_identity_from_path(root, &path).unwrap(),
            RestartAttemptPathIdentity {
                target: record.target.clone(),
                attempt_id: record.attempt_id.clone(),
            }
        );
        let unsafe_target = RestartAttemptTarget {
            provider: "../claude".to_string(),
            channel_id: 123,
        };
        assert!(restart_attempt_path(root, &unsafe_target, &record.attempt_id).is_err());
        assert!(
            restart_attempt_identity_from_path(root, Path::new("elsewhere/v2/x/1/a.json")).is_err()
        );
    }

    struct RecordingOps {
        steps: Vec<RestartAttemptPublicationStep>,
        fail_parent_sync: bool,
    }

    impl RestartAttemptPublicationOps for RecordingOps {
        fn prepare_parent(&mut self, _: &Path) -> std::io::Result<()> {
            self.steps
                .push(RestartAttemptPublicationStep::PrepareParent);
            Ok(())
        }
        fn write_temp(&mut self, _: &Path, _: &[u8]) -> std::io::Result<()> {
            self.steps.push(RestartAttemptPublicationStep::WriteTemp);
            Ok(())
        }
        fn sync_temp(&mut self, _: &Path) -> std::io::Result<()> {
            self.steps.push(RestartAttemptPublicationStep::SyncTemp);
            Ok(())
        }
        fn publish_canonical(&mut self, _: &Path, _: &Path) -> std::io::Result<()> {
            self.steps
                .push(RestartAttemptPublicationStep::PublishCanonical);
            Ok(())
        }
        fn sync_parent(&mut self, _: &Path) -> std::io::Result<()> {
            self.steps.push(RestartAttemptPublicationStep::SyncParent);
            if self.fail_parent_sync {
                Err(std::io::Error::other("injected parent sync failure"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn parent_directory_sync_is_required_before_authority_is_returned() {
        let record = fixture(RestartAttemptId::new_random());
        let mut ops = RecordingOps {
            steps: Vec::new(),
            fail_parent_sync: true,
        };
        let result =
            publish_restart_attempt_in_root_with_ops(Path::new("store"), &record, &mut ops);
        assert!(matches!(
            result,
            Err(RestartAttemptPublicationError::Operation {
                step: RestartAttemptPublicationStep::SyncParent,
                ..
            })
        ));
        assert_eq!(
            ops.steps,
            vec![
                RestartAttemptPublicationStep::PrepareParent,
                RestartAttemptPublicationStep::WriteTemp,
                RestartAttemptPublicationStep::SyncTemp,
                RestartAttemptPublicationStep::PublishCanonical,
                RestartAttemptPublicationStep::SyncParent,
            ]
        );
    }

    #[test]
    fn legacy_restart_completion_report_cannot_be_attempt_authority() {
        let legacy = br#"{"version":1,"provider":"claude","channel_id":123,"status":"ok","summary":"done","completed_at":"2026-08-29 00:00:00"}"#;
        assert_eq!(
            decode_restart_attempt(legacy),
            RestartAttemptDecode::Corrupt {
                error: RestartAttemptDecodeError::MissingSchemaVersion,
                bytes: legacy.to_vec(),
            }
        );
    }
}
