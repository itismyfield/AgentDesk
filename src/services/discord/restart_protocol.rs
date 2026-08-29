#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;
pub(crate) const RESTART_PROTOCOL_VERSION: u32 = 2;
const NAMESPACE: &str = "v2";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestartProtocolEpoch {
    V2,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RestartRequestId(String);
impl RestartRequestId {
    pub(crate) fn new(value: &str) -> Result<Self, String> {
        validate_uuid(value)?;
        Ok(Self(value.into()))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RestartAttemptId(String);
impl RestartAttemptId {
    fn mint() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}
fn validate_uuid(value: &str) -> Result<(), String> {
    if Uuid::parse_str(value)
        .map_err(|error| error.to_string())?
        .to_string()
        != value
    {
        return Err("identity is not canonical lowercase UUID text".into());
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestartAttemptTarget {
    provider: String,
    channel_id: u64,
}
impl RestartAttemptTarget {
    pub(crate) fn new(provider: impl Into<String>, channel_id: u64) -> Result<Self, String> {
        let value = Self {
            provider: provider.into(),
            channel_id,
        };
        validate_provider(&value.provider)?;
        Ok(value)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrozenReportPrincipal {
    provider: String,
    token_hash: String,
    bot_user_id: u64,
}
impl FrozenReportPrincipal {
    pub(crate) fn new(
        provider: impl Into<String>,
        token_hash: impl Into<String>,
        bot_user_id: u64,
    ) -> Result<Self, String> {
        let value = Self {
            provider: provider.into(),
            token_hash: token_hash.into(),
            bot_user_id,
        };
        validate_provider(&value.provider)?;
        if value.token_hash.trim().is_empty() || value.bot_user_id == 0 {
            return Err("frozen report principal is incomplete".into());
        }
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AllocationRoute {
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
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessAllocation {
    generation: u64,
    route: AllocationRoute,
}
impl ProcessAllocation {
    pub(crate) fn new(generation: u64, route: AllocationRoute) -> Self {
        Self { generation, route }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    request_id: RestartRequestId,
    target: RestartAttemptTarget,
    request_nonce: String,
    requested_generation: u64,
    principal: FrozenReportPrincipal,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnboundRestartRequest(Request);
impl UnboundRestartRequest {
    pub(crate) fn new(
        request_id: RestartRequestId,
        target: RestartAttemptTarget,
        request_nonce: impl Into<String>,
        requested_generation: u64,
        principal: FrozenReportPrincipal,
    ) -> Result<Self, String> {
        let request_nonce = request_nonce.into();
        if request_nonce.trim().is_empty() {
            return Err("restart request nonce is missing".into());
        }
        Ok(Self(Request {
            request_id,
            target,
            request_nonce,
            requested_generation,
            principal,
        }))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    attempt_id: RestartAttemptId,
    process: ProcessAllocation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "phase", rename_all = "snake_case")]
enum Phase {
    Bound,
    Running,
    Terminal {
        outcome: TerminalOutcome,
        terminal_proof: SafeRelativeRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        durable_receipt: Option<SafeRelativeRef>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalOutcome {
    Completed,
    RolledBack,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SafeRelativeRef(String);
impl SafeRelativeRef {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_relative_ref(&value)?;
        Ok(Self(value))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundRestartIdentity {
    epoch: RestartProtocolEpoch,
    request_id: RestartRequestId,
    attempt_id: RestartAttemptId,
    target: RestartAttemptTarget,
}
impl BoundRestartIdentity {
    pub(crate) fn epoch(&self) -> RestartProtocolEpoch {
        self.epoch
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RestartAttemptRecord {
    schema_version: u32,
    request: Request,
    binding: Binding,
    phase: Phase,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedRecord {
    schema_version: u32,
    request: Request,
    binding: Binding,
    phase: Phase,
}
impl RestartAttemptRecord {
    fn request_id(&self) -> &RestartRequestId {
        &self.request.request_id
    }
    fn attempt_id(&self) -> &RestartAttemptId {
        &self.binding.attempt_id
    }
    fn target(&self) -> &RestartAttemptTarget {
        &self.request.target
    }
    pub(crate) fn bound_identity(&self) -> BoundRestartIdentity {
        BoundRestartIdentity {
            epoch: RestartProtocolEpoch::V2,
            request_id: self.request_id().clone(),
            attempt_id: self.attempt_id().clone(),
            target: self.target().clone(),
        }
    }
    pub(crate) fn start_running(&self) -> Result<Self, TransitionError> {
        self.transition(Phase::Running)
    }
    pub(crate) fn finish(
        &self,
        outcome: TerminalOutcome,
        terminal_proof: SafeRelativeRef,
        durable_receipt: Option<SafeRelativeRef>,
    ) -> Result<Self, TransitionError> {
        self.transition(Phase::Terminal {
            outcome,
            terminal_proof,
            durable_receipt,
        })
    }
    fn transition(&self, phase: Phase) -> Result<Self, TransitionError> {
        if !matches!(
            (&self.phase, &phase),
            (Phase::Bound, Phase::Running) | (Phase::Running, Phase::Terminal { .. })
        ) {
            return Err(TransitionError::IllegalPhase);
        }
        let mut next = self.clone();
        next.phase = phase;
        next.validate().map_err(TransitionError::InvalidRecord)?;
        Ok(next)
    }
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != RESTART_PROTOCOL_VERSION {
            return Err("unsupported restart protocol version".into());
        }
        validate_uuid(&self.request.request_id.0)?;
        validate_uuid(&self.binding.attempt_id.0)?;
        validate_provider(&self.request.target.provider)?;
        validate_provider(&self.request.principal.provider)?;
        if self.request.request_nonce.trim().is_empty()
            || self.request.principal.token_hash.trim().is_empty()
            || self.request.principal.bot_user_id == 0
        {
            return Err("restart authority payload is incomplete".into());
        }
        if let Phase::Terminal {
            terminal_proof,
            durable_receipt,
            ..
        } = &self.phase
        {
            validate_relative_ref(&terminal_proof.0)?;
            if let Some(receipt) = durable_receipt {
                validate_relative_ref(&receipt.0)?;
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransitionError {
    IllegalPhase,
    InvalidRecord(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DecodeResult {
    Current(RestartAttemptRecord),
    Closed { reason: String, bytes: Vec<u8> },
}
pub(crate) fn decode_restart_attempt(bytes: &[u8]) -> DecodeResult {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => return closed(error.to_string(), bytes),
    };
    let Some(version) = value.get("schema_version").and_then(|value| value.as_u64()) else {
        return closed("missing or invalid schema_version", bytes);
    };
    if version != u64::from(RESTART_PROTOCOL_VERSION) {
        return closed(format!("unsupported version {version}"), bytes);
    }
    match serde_json::from_value::<DecodedRecord>(value).map(|wire| RestartAttemptRecord {
        schema_version: wire.schema_version,
        request: wire.request,
        binding: wire.binding,
        phase: wire.phase,
    }) {
        Ok(record) => match record.validate() {
            Ok(()) => DecodeResult::Current(record),
            Err(error) => closed(error, bytes),
        },
        Err(error) => closed(error.to_string(), bytes),
    }
}
fn closed(reason: impl Into<String>, bytes: &[u8]) -> DecodeResult {
    DecodeResult::Closed {
        reason: reason.into(),
        bytes: bytes.to_vec(),
    }
}
fn encode(record: &RestartAttemptRecord) -> Result<Vec<u8>, String> {
    record.validate()?;
    serde_json::to_vec_pretty(record).map_err(|error| error.to_string())
}
fn validate_provider(provider: &str) -> Result<(), String> {
    if provider.is_empty() || provider.len() > 64 || provider.chars().any(char::is_control) {
        return Err("provider is empty, oversized, or contains control text".into());
    }
    Ok(())
}
fn provider_component(provider: &str) -> Result<String, String> {
    validate_provider(provider)?;
    Ok(format!(
        "p-{}",
        provider
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}
fn decode_provider(component: &str) -> Result<String, String> {
    let hex = component.strip_prefix("p-").ok_or("provider prefix")?;
    if hex.is_empty()
        || hex.len() % 2 != 0
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("provider component is not canonical lowercase hex".into());
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let provider = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    validate_provider(&provider)?;
    Ok(provider)
}
fn validate_relative_ref(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains(['\\', ':'])
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err("reference is not a safe canonical relative path".into());
    }
    Ok(())
}
fn namespace(root: &Path, target: &RestartAttemptTarget) -> Result<PathBuf, String> {
    Ok(root
        .join(NAMESPACE)
        .join(provider_component(&target.provider)?)
        .join(target.channel_id.to_string()))
}
fn attempt_path(root: &Path, record: &RestartAttemptRecord) -> Result<PathBuf, String> {
    Ok(namespace(root, record.target())?
        .join("attempts")
        .join(format!("{}.json", record.attempt_id().0)))
}
fn binding_path(root: &Path, record: &RestartAttemptRecord) -> Result<PathBuf, String> {
    Ok(namespace(root, record.target())?
        .join("bindings")
        .join(format!("{}.json", record.request_id().0)))
}
fn identity_from_path(
    root: &Path,
    path: &Path,
) -> Result<(RestartAttemptTarget, RestartAttemptId), String> {
    let parts: Vec<_> = path
        .strip_prefix(root)
        .map_err(|_| "path outside store")?
        .components()
        .collect();
    let [
        Component::Normal(version),
        Component::Normal(provider),
        Component::Normal(channel),
        Component::Normal(kind),
        Component::Normal(file),
    ] = parts.as_slice()
    else {
        return Err("unexpected canonical path shape".into());
    };
    if *version != std::ffi::OsStr::new(NAMESPACE) || *kind != std::ffi::OsStr::new("attempts") {
        return Err("invalid canonical namespace".into());
    }
    let target = RestartAttemptTarget::new(
        decode_provider(provider.to_str().ok_or("provider UTF-8")?)?,
        channel
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or("invalid channel")?,
    )?;
    let attempt = file
        .to_str()
        .and_then(|value| value.strip_suffix(".json"))
        .ok_or("invalid filename")?;
    validate_uuid(attempt)?;
    Ok((target, RestartAttemptId(attempt.into())))
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadResult {
    Authoritative(RestartAttemptRecord),
    Closed { reason: String, bytes: Vec<u8> },
    Io(String),
}
pub(crate) fn load_restart_attempt_from_canonical_path(root: &Path, path: &Path) -> LoadResult {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return LoadResult::Io(error.to_string()),
    };
    let record = match decode_restart_attempt(&bytes) {
        DecodeResult::Current(record) => record,
        DecodeResult::Closed { reason, bytes } => return LoadResult::Closed { reason, bytes },
    };
    if identity_from_path(root, path) != Ok((record.target().clone(), record.attempt_id().clone()))
    {
        return LoadResult::Closed {
            reason: "path and payload identities differ".into(),
            bytes,
        };
    }
    let binding =
        binding_path(root, &record).and_then(|path| fs::read(path).map_err(|e| e.to_string()));
    if binding.as_deref() != Ok(bytes.as_slice()) {
        return LoadResult::Closed {
            reason: "request binding is absent or disagrees".into(),
            bytes,
        };
    }
    LoadResult::Authoritative(record)
}
trait PublicationOps {
    fn create_dir(&mut self, path: &Path) -> std::io::Result<bool>;
    fn sync_dir(&mut self, path: &Path) -> std::io::Result<()>;
    fn write_temp(&mut self, path: &Path, bytes: &[u8]) -> std::io::Result<()>;
    fn sync_temp(&mut self, path: &Path) -> std::io::Result<()>;
    fn publish_no_replace(&mut self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn remove_temp(&mut self, path: &Path) -> std::io::Result<()>;
}
struct FsOps;
impl PublicationOps for FsOps {
    fn create_dir(&mut self, path: &Path) -> std::io::Result<bool> {
        match fs::create_dir(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    fn sync_dir(&mut self, path: &Path) -> std::io::Result<()> {
        fs::File::open(path)?.sync_all()
    }
    fn write_temp(&mut self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?
            .write_all(bytes)
    }
    fn sync_temp(&mut self, path: &Path) -> std::io::Result<()> {
        fs::File::open(path)?.sync_all()
    }
    fn publish_no_replace(&mut self, from: &Path, to: &Path) -> std::io::Result<()> {
        fs::hard_link(from, to)
    }
    fn remove_temp(&mut self, path: &Path) -> std::io::Result<()> {
        fs::remove_file(path)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublicationError {
    Invalid(String),
    Conflict,
    Io(String),
}
pub(crate) fn publish_restart_attempt(
    root: &Path,
    request: UnboundRestartRequest,
    process: ProcessAllocation,
) -> Result<RestartAttemptRecord, PublicationError> {
    publish(root, request, process, RestartAttemptId::mint(), &mut FsOps)
}
fn publish(
    root: &Path,
    request: UnboundRestartRequest,
    process: ProcessAllocation,
    attempt_id: RestartAttemptId,
    ops: &mut impl PublicationOps,
) -> Result<RestartAttemptRecord, PublicationError> {
    let record = RestartAttemptRecord {
        schema_version: RESTART_PROTOCOL_VERSION,
        request: request.0,
        binding: Binding {
            attempt_id,
            process,
        },
        phase: Phase::Bound,
    };
    let bytes = encode(&record).map_err(PublicationError::Invalid)?;
    let attempt = attempt_path(root, &record).map_err(PublicationError::Invalid)?;
    let binding = binding_path(root, &record).map_err(PublicationError::Invalid)?;
    for directory in parent_chain(root, &[&attempt, &binding])? {
        if ops.create_dir(&directory).map_err(io_error)? {
            ops.sync_dir(directory.parent().ok_or_else(|| {
                PublicationError::Invalid("created directory lacks parent".into())
            })?)
            .map_err(io_error)?;
        }
    }
    let temp = attempt
        .parent()
        .unwrap()
        .join(format!(".{}.tmp", Uuid::new_v4().simple()));
    ops.write_temp(&temp, &bytes).map_err(io_error)?;
    ops.sync_temp(&temp).map_err(io_error)?;
    ops.publish_no_replace(&temp, &attempt).map_err(io_error)?;
    if let Err(error) = ops.publish_no_replace(&temp, &binding) {
        let _ = ops.remove_temp(&temp);
        return if error.kind() == std::io::ErrorKind::AlreadyExists {
            Err(PublicationError::Conflict)
        } else {
            Err(io_error(error))
        };
    }
    ops.remove_temp(&temp).map_err(io_error)?;
    ops.sync_dir(attempt.parent().unwrap()).map_err(io_error)?;
    ops.sync_dir(binding.parent().unwrap()).map_err(io_error)?;
    Ok(record)
}
fn parent_chain(root: &Path, paths: &[&Path]) -> Result<Vec<PathBuf>, PublicationError> {
    let mut result = Vec::new();
    for path in paths {
        let mut current = root.to_path_buf();
        for component in path
            .parent()
            .unwrap()
            .strip_prefix(root)
            .map_err(|_| PublicationError::Invalid("path escaped root".into()))?
            .components()
        {
            current.push(component);
            if !result.contains(&current) {
                result.push(current.clone());
            }
        }
    }
    Ok(result)
}
fn io_error(error: std::io::Error) -> PublicationError {
    PublicationError::Io(error.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    fn request(provider: &str) -> UnboundRestartRequest {
        UnboundRestartRequest::new(
            RestartRequestId::new("11111111-1111-4111-8111-111111111111").unwrap(),
            RestartAttemptTarget::new(provider, 123).unwrap(),
            "nonce",
            41,
            FrozenReportPrincipal::new(provider, "hash", 456).unwrap(),
        )
        .unwrap()
    }
    fn process() -> ProcessAllocation {
        ProcessAllocation::new(42, AllocationRoute::AdvancedWithSyncedRename)
    }
    fn mint(value: &str) -> RestartAttemptId {
        validate_uuid(value).unwrap();
        RestartAttemptId(value.into())
    }
    fn first(root: &Path) -> RestartAttemptRecord {
        publish(
            root,
            request("claude"),
            process(),
            mint("22222222-2222-4222-8222-222222222222"),
            &mut FsOps,
        )
        .unwrap()
    }
    #[test]
    fn append_once_request_binding_rejects_second_attempt_and_preserves_first() {
        let root = tempfile::tempdir().unwrap();
        let record = first(root.path());
        let path = attempt_path(root.path(), &record).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            publish(
                root.path(),
                request("claude"),
                process(),
                mint("33333333-3333-4333-8333-333333333333"),
                &mut FsOps
            ),
            Err(PublicationError::Conflict)
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    #[test]
    fn canonical_load_rejects_path_payload_mismatch_and_preserves_bytes() {
        let root = tempfile::tempdir().unwrap();
        let record = first(root.path());
        let bytes = encode(&record).unwrap();
        let wrong = namespace(root.path(), record.target())
            .unwrap()
            .join("attempts/33333333-3333-4333-8333-333333333333.json");
        fs::write(&wrong, &bytes).unwrap();
        assert!(
            matches!(load_restart_attempt_from_canonical_path(root.path(), &wrong), LoadResult::Closed { bytes: kept, .. } if kept == bytes)
        );
    }
    #[test]
    fn same_version_unknown_authority_field_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(first(root.path())).unwrap();
        value["binding"]["caller_attempt_authority"] = serde_json::json!(true);
        assert!(matches!(
            decode_restart_attempt(&serde_json::to_vec(&value).unwrap()),
            DecodeResult::Closed { .. }
        ));
    }
    #[test]
    fn future_version_fails_closed_and_preserves_bytes() {
        let bytes = br#"{"schema_version":999,"future":true}"#;
        assert!(
            matches!(decode_restart_attempt(bytes), DecodeResult::Closed { bytes: kept, .. } if kept == bytes)
        );
    }
    #[test]
    fn provider_encoding_is_case_injective_and_avoids_windows_hazards() {
        assert_ne!(
            provider_component("con").unwrap().to_ascii_lowercase(),
            provider_component("CON").unwrap().to_ascii_lowercase()
        );
        for provider in ["con", "name. ", "Claude/β"] {
            let encoded = provider_component(provider).unwrap();
            assert_eq!(decode_provider(&encoded).unwrap(), provider);
            assert!(!encoded.ends_with(['.', ' ']));
        }
    }
    #[test]
    fn relative_refs_reject_cross_platform_traversal_hazards() {
        for bad in [
            "", "/a", "C:/a", ".", "..", "a/./b", "a/../b", "a//b", "a\\b",
        ] {
            assert!(SafeRelativeRef::new(bad).is_err(), "accepted {bad:?}");
        }
    }
    #[test]
    fn reducer_is_only_phase_authority_and_terminal_is_immutable() {
        let root = tempfile::tempdir().unwrap();
        let bound = first(root.path());
        let identity = bound.bound_identity();
        assert_eq!(identity.epoch(), RestartProtocolEpoch::V2);
        let running = bound.start_running().unwrap();
        assert_eq!(running.bound_identity(), identity);
        let terminal = running
            .finish(
                TerminalOutcome::Completed,
                SafeRelativeRef::new("proof/final.json").unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(terminal.bound_identity(), identity);
        assert_eq!(terminal.start_running(), Err(TransitionError::IllegalPhase));
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Event {
        Create(PathBuf),
        Sync(PathBuf),
        Write,
        SyncFile,
        Attempt,
        Binding,
        Remove,
    }
    #[derive(Default)]
    struct RecordingOps {
        events: Vec<Event>,
        dirs: HashSet<PathBuf>,
        files: HashSet<PathBuf>,
    }
    impl PublicationOps for RecordingOps {
        fn create_dir(&mut self, path: &Path) -> std::io::Result<bool> {
            self.events.push(Event::Create(path.into()));
            Ok(self.dirs.insert(path.into()))
        }
        fn sync_dir(&mut self, path: &Path) -> std::io::Result<()> {
            self.events.push(Event::Sync(path.into()));
            Ok(())
        }
        fn write_temp(&mut self, _: &Path, _: &[u8]) -> std::io::Result<()> {
            self.events.push(Event::Write);
            Ok(())
        }
        fn sync_temp(&mut self, _: &Path) -> std::io::Result<()> {
            self.events.push(Event::SyncFile);
            Ok(())
        }
        fn publish_no_replace(&mut self, _: &Path, path: &Path) -> std::io::Result<()> {
            self.events
                .push(if path.to_string_lossy().contains("/attempts/") {
                    Event::Attempt
                } else {
                    Event::Binding
                });
            if self.files.insert(path.into()) {
                Ok(())
            } else {
                Err(std::io::ErrorKind::AlreadyExists.into())
            }
        }
        fn remove_temp(&mut self, _: &Path) -> std::io::Result<()> {
            self.events.push(Event::Remove);
            Ok(())
        }
    }
    #[test]
    fn first_publication_syncs_every_new_parent_before_authority() {
        let root = Path::new("store");
        let mut ops = RecordingOps::default();
        ops.dirs.insert(root.into());
        publish(
            root,
            request("claude"),
            process(),
            mint("22222222-2222-4222-8222-222222222222"),
            &mut ops,
        )
        .unwrap();
        for (index, event) in ops.events.iter().enumerate() {
            if let Event::Create(path) = event {
                assert_eq!(
                    ops.events[index + 1],
                    Event::Sync(path.parent().unwrap().into())
                );
            }
        }
        assert!(matches!(
            &ops.events[ops.events.len() - 7..],
            [
                Event::Write,
                Event::SyncFile,
                Event::Attempt,
                Event::Binding,
                Event::Remove,
                Event::Sync(_),
                Event::Sync(_)
            ]
        ));
    }
}
