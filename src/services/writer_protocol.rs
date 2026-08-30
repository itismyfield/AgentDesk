//! Dormant writer classification and process-local registration protocol.
//!
//! No production consumer exists. This module performs no I/O and does not
//! open, lock, mutate, rotate, or clean artifacts. Its registry coordinates
//! only this process; it makes no host, node, or fleet claim.

use crate::services::provider::ProviderKind;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
};

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
    if matches!(origin, ArtifactOrigin::ProviderNative) {
        return WriterDisposition::Observed;
    }
    if matches!(provider, ProviderDomain::Unsupported)
        || matches!(origin, ArtifactOrigin::Unsupported)
        || matches!(artifact, ArtifactKind::Unknown)
    {
        return WriterDisposition::Unsupported;
    }
    match (provider, origin, artifact) {
        (
            ProviderDomain::Gemini | ProviderDomain::OpenCode,
            ArtifactOrigin::AgentDeskManaged,
            ArtifactKind::NoManagedLocalTranscript,
        ) => WriterDisposition::Observed,
        (
            ProviderDomain::Claude | ProviderDomain::Codex | ProviderDomain::Qwen,
            ArtifactOrigin::AgentDeskManaged,
            ArtifactKind::RelayJsonl,
        )
        | (
            ProviderDomain::Claude | ProviderDomain::Codex | ProviderDomain::Qwen,
            ArtifactOrigin::SessionAuxiliary,
            ArtifactKind::Prompt
            | ArtifactKind::InputFifo
            | ArtifactKind::OwnerMarker
            | ArtifactKind::WrapperScript
            | ArtifactKind::RuntimeMarker,
        ) => WriterDisposition::DormantManaged,
        _ => WriterDisposition::Unsupported,
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

    fn safe_session_component(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"agentdesk.writer-protocol.session-component.v1\0");
        hasher.update(&(self.session.len() as u64).to_be_bytes());
        hasher.update(self.session.as_bytes());
        format!("s-{}", hasher.finalize().to_hex())
    }
}

/// One record owner's trusted association, not global filesystem/path authority.
/// No path canonicalization, symlink inspection, catalog lookup, or I/O occurs.
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

/// Pure derivation; creating or locking the returned path is a future concern.
pub(crate) fn control_lock_path(
    root: &Path,
    provider: ProviderDomain,
    identity: &LogicalArtifactIdentity,
) -> PathBuf {
    root.join("writer-protocol")
        .join(provider.slug())
        .join(identity.safe_session_component())
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
mod tests {
    use super::*;
    use std::{path::Component, sync::Arc};

    #[rustfmt::skip]
    const PROVIDERS: [ProviderDomain; 6] = [ProviderDomain::Claude, ProviderDomain::Codex, ProviderDomain::Gemini, ProviderDomain::OpenCode, ProviderDomain::Qwen, ProviderDomain::Unsupported];
    #[rustfmt::skip]
    const ORIGINS: [ArtifactOrigin; 4] = [ArtifactOrigin::AgentDeskManaged, ArtifactOrigin::ProviderNative, ArtifactOrigin::SessionAuxiliary, ArtifactOrigin::Unsupported];
    #[rustfmt::skip]
    const ARTIFACTS: [ArtifactKind; 10] = [ArtifactKind::RelayJsonl, ArtifactKind::NativeTranscript, ArtifactKind::NativeRollout, ArtifactKind::Prompt, ArtifactKind::InputFifo, ArtifactKind::OwnerMarker, ArtifactKind::WrapperScript, ArtifactKind::RuntimeMarker, ArtifactKind::NoManagedLocalTranscript, ArtifactKind::Unknown];
    type Tuple = (ProviderDomain, ArtifactOrigin, ArtifactKind);
    #[rustfmt::skip]
    const MANAGED: [Tuple; 3] = [(ProviderDomain::Claude, ArtifactOrigin::AgentDeskManaged, ArtifactKind::RelayJsonl), (ProviderDomain::Codex, ArtifactOrigin::AgentDeskManaged, ArtifactKind::RelayJsonl), (ProviderDomain::Qwen, ArtifactOrigin::AgentDeskManaged, ArtifactKind::RelayJsonl)];
    #[rustfmt::skip]
    const OBSERVED: [Tuple; 2] = [(ProviderDomain::Gemini, ArtifactOrigin::AgentDeskManaged, ArtifactKind::NoManagedLocalTranscript), (ProviderDomain::OpenCode, ArtifactOrigin::AgentDeskManaged, ArtifactKind::NoManagedLocalTranscript)];
    #[rustfmt::skip]
    const AUXILIARY: [ArtifactKind; 5] = [ArtifactKind::Prompt, ArtifactKind::InputFifo, ArtifactKind::OwnerMarker, ArtifactKind::WrapperScript, ArtifactKind::RuntimeMarker];

    #[rustfmt::skip]
    #[test]
    fn provider_kind_mapping_is_total() {
        let cases = [(ProviderKind::Claude, ProviderDomain::Claude), (ProviderKind::Codex, ProviderDomain::Codex), (ProviderKind::Gemini, ProviderDomain::Gemini), (ProviderKind::OpenCode, ProviderDomain::OpenCode), (ProviderKind::Qwen, ProviderDomain::Qwen), (ProviderKind::Unsupported("future".into()), ProviderDomain::Unsupported)];
        for (kind, expected) in cases { assert_eq!(ProviderDomain::from(&kind), expected); }
    }

    #[test]
    fn exact_tuple_allowlist_is_exhaustive_and_independent() {
        for provider in PROVIDERS {
            for origin in ORIGINS {
                for artifact in ARTIFACTS {
                    let tuple = (provider, origin, artifact);
                    let expected = if origin == ArtifactOrigin::ProviderNative {
                        WriterDisposition::Observed
                    } else if OBSERVED.contains(&tuple) {
                        WriterDisposition::Observed
                    } else if MANAGED.contains(&tuple)
                        || (matches!(
                            provider,
                            ProviderDomain::Claude | ProviderDomain::Codex | ProviderDomain::Qwen
                        ) && origin == ArtifactOrigin::SessionAuxiliary
                            && AUXILIARY.contains(&artifact))
                    {
                        WriterDisposition::DormantManaged
                    } else {
                        WriterDisposition::Unsupported
                    };
                    assert_eq!(classify_writer(provider, origin, artifact), expected);
                }
            }
        }
    }

    #[test]
    fn provider_native_precedes_unknown_and_unsupported_provider() {
        for provider in PROVIDERS {
            for artifact in ARTIFACTS {
                assert_eq!(
                    classify_writer(provider, ArtifactOrigin::ProviderNative, artifact),
                    WriterDisposition::Observed
                );
            }
        }
    }

    #[rustfmt::skip]
    #[test]
    fn adjudicated_anti_cases_fail_closed() {
        let cases = [(ProviderDomain::Qwen, ArtifactOrigin::AgentDeskManaged, ArtifactKind::NativeTranscript), (ProviderDomain::Qwen, ArtifactOrigin::SessionAuxiliary, ArtifactKind::RelayJsonl), (ProviderDomain::Claude, ArtifactOrigin::AgentDeskManaged, ArtifactKind::Prompt), (ProviderDomain::Gemini, ArtifactOrigin::SessionAuxiliary, ArtifactKind::Prompt), (ProviderDomain::Unsupported, ArtifactOrigin::AgentDeskManaged, ArtifactKind::RelayJsonl), (ProviderDomain::Claude, ArtifactOrigin::SessionAuxiliary, ArtifactKind::Unknown)];
        for (provider, origin, artifact) in cases { assert_eq!(classify_writer(provider, origin, artifact), WriterDisposition::Unsupported); }
    }

    #[test]
    fn lock_path_is_contained_safe_injective_for_examples_and_stable() {
        let dangerous = [
            "/tmp/x",
            "../x",
            "a/b",
            "a\\b",
            ".",
            "..",
            "",
            "Cláude/β",
            "a_b",
            "x",
        ];
        let root = Path::new("/control");
        let mut components = HashSet::new();
        for session in dangerous {
            let identity = LogicalArtifactIdentity::new(session, ArtifactKind::RelayJsonl);
            let path = control_lock_path(root, ProviderDomain::Claude, &identity);
            let relative: Vec<_> = path.strip_prefix(root).unwrap().components().collect();
            assert_eq!(relative.len(), 4);
            assert!(relative.iter().all(|p| matches!(p, Component::Normal(_))));
            let component = relative[2].as_os_str().to_str().unwrap().to_owned();
            assert_eq!(component.len(), 66);
            assert!(components.insert(component));
            assert_eq!(
                path,
                control_lock_path(root, ProviderDomain::Claude, &identity)
            );
        }
    }

    #[rustfmt::skip]
    #[test]
    fn lock_path_distinguishes_provider_and_artifact_and_record_path() {
        let identity = LogicalArtifactIdentity::new("same", ArtifactKind::RelayJsonl);
        let claude = control_lock_path(Path::new("/control"), ProviderDomain::Claude, &identity);
        assert_ne!(claude, control_lock_path(Path::new("/control"), ProviderDomain::Codex, &identity));
        assert_ne!(claude, control_lock_path(Path::new("/control"), ProviderDomain::Claude, &LogicalArtifactIdentity::new("same", ArtifactKind::Prompt)));
        assert_ne!(claude, Path::new("/records/same.jsonl"));
    }

    #[rustfmt::skip]
    #[test]
    fn aliases_are_per_value_and_unknown_paths_fail() {
        let identity = LogicalArtifactIdentity::new("session", ArtifactKind::RelayJsonl);
        let aliases = RecordPathAliases::new(identity.clone(), "/records/current", "/records/legacy");
        assert_eq!(aliases.logical_key(Path::new("/records/current")), Ok(identity.clone()));
        assert_eq!(aliases.logical_key(Path::new("/records/legacy")), Ok(identity));
        assert_eq!(aliases.logical_key(Path::new("/records/other")), Err(UnknownPath));
    }

    fn key(
        provider: ProviderDomain,
        session: &str,
        artifact: ArtifactKind,
    ) -> WriterRegistrationKey {
        WriterRegistrationKey::new(provider, LogicalArtifactIdentity::new(session, artifact))
    }

    #[rustfmt::skip]
    #[test]
    fn singleton_registry_enforces_exact_key_and_exact_release() {
        assert!(std::ptr::eq(writer_registry(), writer_registry()));
        let registry = WriterRegistry::new(); let exact = key(ProviderDomain::Claude, "same", ArtifactKind::RelayJsonl);
        let first = registry.register(exact.clone()).unwrap(); assert_eq!(registry.register(exact.clone()).err(), Some(DuplicateRegistration));
        let provider = registry.register(key(ProviderDomain::Codex, "same", ArtifactKind::RelayJsonl)).unwrap();
        let session = registry.register(key(ProviderDomain::Claude, "other", ArtifactKind::RelayJsonl)).unwrap();
        let artifact = registry.register(key(ProviderDomain::Claude, "same", ArtifactKind::Prompt)).unwrap();
        drop(provider); assert_eq!(registry.register(exact.clone()).err(), Some(DuplicateRegistration));
        drop(session); artifact.release(); first.release(); assert!(registry.register(exact).is_ok());
    }

    #[rustfmt::skip]
    #[test]
    fn poisoned_registry_recovers() {
        let registry = Arc::new(WriterRegistry::new()); let target = Arc::clone(&registry);
        let _ = std::thread::spawn(move || { let _guard = target.active.lock().unwrap(); panic!("poison registry"); }).join();
        assert!(registry.register(key(ProviderDomain::Qwen, "recovered", ArtifactKind::RelayJsonl)).is_ok());
    }
}
