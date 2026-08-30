use super::*;
use std::path::Path;
use std::sync::Arc;

const PROVIDERS: [ProviderDomain; 6] = [
    ProviderDomain::Claude,
    ProviderDomain::Codex,
    ProviderDomain::Gemini,
    ProviderDomain::OpenCode,
    ProviderDomain::Qwen,
    ProviderDomain::Unsupported,
];
const ARTIFACTS: [ArtifactKind; 12] = [
    ArtifactKind::RelayJsonl,
    ArtifactKind::NativeTranscript,
    ArtifactKind::NativeRollout,
    ArtifactKind::Prompt,
    ArtifactKind::InputFifo,
    ArtifactKind::OwnerMarker,
    ArtifactKind::WrapperScript,
    ArtifactKind::RuntimeMarker,
    ArtifactKind::HookRelayQueueRecord,
    ArtifactKind::HookRelayQueueLock,
    ArtifactKind::NoManagedLocalTranscript,
    ArtifactKind::Unknown,
];

#[test]
fn provider_kind_mapping_is_total() {
    let cases = [
        (ProviderKind::Claude, ProviderDomain::Claude),
        (ProviderKind::Codex, ProviderDomain::Codex),
        (ProviderKind::Gemini, ProviderDomain::Gemini),
        (ProviderKind::OpenCode, ProviderDomain::OpenCode),
        (ProviderKind::Qwen, ProviderDomain::Qwen),
        (
            ProviderKind::Unsupported("future".into()),
            ProviderDomain::Unsupported,
        ),
    ];
    for (kind, expected) in cases {
        assert_eq!(ProviderDomain::from(&kind), expected);
    }
}

#[test]
fn total_matrix_has_explicit_terminal_dispositions() {
    for provider in PROVIDERS {
        for origin in [
            ArtifactOrigin::AgentDeskManaged,
            ArtifactOrigin::ProviderNative,
            ArtifactOrigin::SessionAuxiliary,
            ArtifactOrigin::Unsupported,
        ] {
            for artifact in ARTIFACTS {
                let actual = classify_writer(provider, origin, artifact);
                let expected = match origin {
                    ArtifactOrigin::ProviderNative => WriterDisposition::Observed,
                    ArtifactOrigin::Unsupported => WriterDisposition::Unsupported,
                    ArtifactOrigin::AgentDeskManaged | ArtifactOrigin::SessionAuxiliary => {
                        match artifact {
                            ArtifactKind::NoManagedLocalTranscript
                                if matches!(
                                    provider,
                                    ProviderDomain::Gemini | ProviderDomain::OpenCode
                                ) =>
                            {
                                WriterDisposition::Observed
                            }
                            ArtifactKind::RelayJsonl
                            | ArtifactKind::NativeTranscript
                            | ArtifactKind::NativeRollout
                            | ArtifactKind::Prompt
                            | ArtifactKind::InputFifo
                            | ArtifactKind::OwnerMarker
                            | ArtifactKind::WrapperScript
                            | ArtifactKind::RuntimeMarker
                            | ArtifactKind::HookRelayQueueRecord
                            | ArtifactKind::HookRelayQueueLock
                                if matches!(
                                    provider,
                                    ProviderDomain::Claude
                                        | ProviderDomain::Codex
                                        | ProviderDomain::Qwen
                                ) =>
                            {
                                WriterDisposition::DormantManaged
                            }
                            ArtifactKind::NoManagedLocalTranscript
                            | ArtifactKind::Unknown
                            | ArtifactKind::RelayJsonl
                            | ArtifactKind::NativeTranscript
                            | ArtifactKind::NativeRollout
                            | ArtifactKind::Prompt
                            | ArtifactKind::InputFifo
                            | ArtifactKind::OwnerMarker
                            | ArtifactKind::WrapperScript
                            | ArtifactKind::RuntimeMarker
                            | ArtifactKind::HookRelayQueueRecord
                            | ArtifactKind::HookRelayQueueLock => WriterDisposition::Unsupported,
                        }
                    }
                };
                assert_eq!(actual, expected, "{provider:?}/{origin:?}/{artifact:?}");
            }
        }
    }
}

#[test]
fn provider_native_is_permanently_observation_only() {
    for provider in PROVIDERS {
        for artifact in ARTIFACTS {
            assert_eq!(
                classify_writer(provider, ArtifactOrigin::ProviderNative, artifact),
                WriterDisposition::Observed
            );
        }
    }
}

#[test]
fn control_path_is_stable_separate_and_aliases_share_identity() {
    let identity = LogicalArtifactIdentity::new("AgentDesk-demo", ArtifactKind::RelayJsonl);
    let aliases = RecordPathAliases::new(
        identity.clone(),
        "/runtime/sessions/demo.jsonl",
        "/tmp/AgentDesk-demo.jsonl",
    );
    assert_eq!(
        aliases.logical_key(Path::new("/runtime/sessions/demo.jsonl")),
        Ok(identity.clone())
    );
    assert_eq!(
        aliases.logical_key(Path::new("/tmp/AgentDesk-demo.jsonl")),
        Ok(identity.clone())
    );
    assert_eq!(
        aliases.logical_key(Path::new("/other/demo.jsonl")),
        Err(UnknownPath)
    );
    let before = control_lock_path(Path::new("/control"), ProviderDomain::Claude, &identity);
    let after = control_lock_path(Path::new("/control"), ProviderDomain::Claude, &identity);
    assert_eq!(before, after);
    assert_ne!(before, Path::new("/runtime/sessions/demo.jsonl"));
}

fn key(provider: ProviderDomain, session: &str) -> WriterRegistrationKey {
    WriterRegistrationKey::new(
        provider,
        LogicalArtifactIdentity::new(session, ArtifactKind::RelayJsonl),
    )
}

#[test]
fn singleton_registry_rejects_only_exact_duplicate_and_releases_exactly() {
    assert!(std::ptr::eq(writer_registry(), writer_registry()));
    let registry = WriterRegistry::new();
    let claude = key(ProviderDomain::Claude, "same");
    let codex = key(ProviderDomain::Codex, "same");
    let other = key(ProviderDomain::Claude, "other");
    let first = registry.register(claude.clone()).unwrap();
    assert_eq!(
        registry.register(claude.clone()).err(),
        Some(DuplicateRegistration)
    );
    let codex_registration = registry.register(codex);
    assert!(codex_registration.is_ok());
    let codex_registration = codex_registration.unwrap();
    let other_registration = registry.register(other).unwrap();
    codex_registration.release();
    assert_eq!(
        registry.register(claude.clone()).err(),
        Some(DuplicateRegistration)
    );
    drop(other_registration);
    first.release();
    assert!(registry.register(claude).is_ok());
}

#[test]
fn poisoned_registry_recovers() {
    let registry = Arc::new(WriterRegistry::new());
    let poison_target = Arc::clone(&registry);
    let _ = std::thread::spawn(move || {
        let _guard = poison_target.active.lock().unwrap();
        panic!("poison writer registry for recovery test");
    })
    .join();
    assert!(
        registry
            .register(key(ProviderDomain::Qwen, "recovered"))
            .is_ok()
    );
}
