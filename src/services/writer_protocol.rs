//! Dormant writer classification and process-local registration protocol.
//!
//! This module does not open, lock, mutate, rotate, or clean any artifact. Its
//! registry coordinates only callers in this process; it is not host, node, or
//! fleet authority. `control_lock_path` is pure path derivation, not a lock.

use crate::services::provider::ProviderKind;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ProviderDomain {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Qwen,
    Unsupported,
}

impl From<&ProviderKind> for ProviderDomain {
    fn from(value: &ProviderKind) -> Self {
        match value {
            ProviderKind::Claude => Self::Claude,
            ProviderKind::Codex => Self::Codex,
            ProviderKind::Gemini => Self::Gemini,
            ProviderKind::OpenCode => Self::OpenCode,
            ProviderKind::Qwen => Self::Qwen,
            ProviderKind::Unsupported(_) => Self::Unsupported,
        }
    }
}

impl ProviderDomain {
    const fn slug(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ArtifactOrigin {
    AgentDeskManaged,
    ProviderNative,
    SessionAuxiliary,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ArtifactKind {
    RelayJsonl,
    NativeTranscript,
    NativeRollout,
    Prompt,
    InputFifo,
    OwnerMarker,
    WrapperScript,
    RuntimeMarker,
    HookRelayQueueRecord,
    HookRelayQueueLock,
    NoManagedLocalTranscript,
    Unknown,
}

impl ArtifactKind {
    const fn slug(self) -> &'static str {
        match self {
            Self::RelayJsonl => "relay-jsonl",
            Self::NativeTranscript => "native-transcript",
            Self::NativeRollout => "native-rollout",
            Self::Prompt => "prompt",
            Self::InputFifo => "input-fifo",
            Self::OwnerMarker => "owner-marker",
            Self::WrapperScript => "wrapper-script",
            Self::RuntimeMarker => "runtime-marker",
            Self::HookRelayQueueRecord => "hook-relay-queue-record",
            Self::HookRelayQueueLock => "hook-relay-queue-lock",
            Self::NoManagedLocalTranscript => "no-managed-local-transcript",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriterDisposition {
    DormantManaged,
    Observed,
    Unsupported,
}

pub(crate) const fn classify_writer(
    provider: ProviderDomain,
    origin: ArtifactOrigin,
    artifact: ArtifactKind,
) -> WriterDisposition {
    match origin {
        ArtifactOrigin::ProviderNative => WriterDisposition::Observed,
        ArtifactOrigin::Unsupported => WriterDisposition::Unsupported,
        ArtifactOrigin::AgentDeskManaged | ArtifactOrigin::SessionAuxiliary => match artifact {
            ArtifactKind::Unknown => WriterDisposition::Unsupported,
            ArtifactKind::NoManagedLocalTranscript => match provider {
                ProviderDomain::Gemini | ProviderDomain::OpenCode => WriterDisposition::Observed,
                ProviderDomain::Claude
                | ProviderDomain::Codex
                | ProviderDomain::Qwen
                | ProviderDomain::Unsupported => WriterDisposition::Unsupported,
            },
            ArtifactKind::RelayJsonl
            | ArtifactKind::NativeTranscript
            | ArtifactKind::NativeRollout
            | ArtifactKind::Prompt
            | ArtifactKind::InputFifo
            | ArtifactKind::OwnerMarker
            | ArtifactKind::WrapperScript
            | ArtifactKind::RuntimeMarker
            | ArtifactKind::HookRelayQueueRecord
            | ArtifactKind::HookRelayQueueLock => match provider {
                ProviderDomain::Claude | ProviderDomain::Codex | ProviderDomain::Qwen => {
                    WriterDisposition::DormantManaged
                }
                ProviderDomain::Gemini | ProviderDomain::OpenCode | ProviderDomain::Unsupported => {
                    WriterDisposition::Unsupported
                }
            },
        },
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct LogicalArtifactIdentity {
    session: String,
    artifact: ArtifactKind,
}

impl LogicalArtifactIdentity {
    pub(crate) fn new(session: impl Into<String>, artifact: ArtifactKind) -> Self {
        Self {
            session: session.into(),
            artifact,
        }
    }
}

pub(crate) struct RecordPathAliases {
    identity: LogicalArtifactIdentity,
    canonical: PathBuf,
    legacy: PathBuf,
}

impl RecordPathAliases {
    pub(crate) fn new(
        identity: LogicalArtifactIdentity,
        canonical: impl Into<PathBuf>,
        legacy: impl Into<PathBuf>,
    ) -> Self {
        Self {
            identity,
            canonical: canonical.into(),
            legacy: legacy.into(),
        }
    }

    pub(crate) fn logical_key(&self, path: &Path) -> Result<LogicalArtifactIdentity, UnknownPath> {
        if path == self.canonical || path == self.legacy {
            Ok(self.identity.clone())
        } else {
            Err(UnknownPath)
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct UnknownPath;

pub(crate) fn control_lock_path(
    runtime_control_root: &Path,
    provider: ProviderDomain,
    identity: &LogicalArtifactIdentity,
) -> PathBuf {
    runtime_control_root
        .join("writer-protocol")
        .join(provider.slug())
        .join(&identity.session)
        .join(format!("{}.lock", identity.artifact.slug()))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WriterRegistrationKey {
    provider: ProviderDomain,
    identity: LogicalArtifactIdentity,
}

impl WriterRegistrationKey {
    pub(crate) fn new(provider: ProviderDomain, identity: LogicalArtifactIdentity) -> Self {
        Self { provider, identity }
    }
}

pub(crate) struct WriterRegistry {
    active: Mutex<HashSet<WriterRegistrationKey>>,
}

impl WriterRegistry {
    fn new() -> Self {
        Self {
            active: Mutex::new(HashSet::new()),
        }
    }

    fn active(&self) -> MutexGuard<'_, HashSet<WriterRegistrationKey>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn register(
        &self,
        key: WriterRegistrationKey,
    ) -> Result<WriterRegistration<'_>, DuplicateRegistration> {
        if !self.active().insert(key.clone()) {
            return Err(DuplicateRegistration);
        }
        Ok(WriterRegistration {
            registry: self,
            key,
            released: false,
        })
    }

    fn release(&self, key: &WriterRegistrationKey) {
        self.active().remove(key);
    }
}

pub(crate) struct WriterRegistration<'a> {
    registry: &'a WriterRegistry,
    key: WriterRegistrationKey,
    released: bool,
}

impl WriterRegistration<'_> {
    pub(crate) fn release(mut self) {
        self.registry.release(&self.key);
        self.released = true;
    }
}

impl Drop for WriterRegistration<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.registry.release(&self.key);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct DuplicateRegistration;

pub(crate) fn writer_registry() -> &'static WriterRegistry {
    static REGISTRY: OnceLock<WriterRegistry> = OnceLock::new();
    REGISTRY.get_or_init(WriterRegistry::new)
}

#[cfg(test)]
#[path = "writer_protocol/tests.rs"]
mod tests;
