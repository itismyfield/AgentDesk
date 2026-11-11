use super::*;

#[test]
fn one_pass_keeps_catalog_notice_identity_across_error_changes() {
    let mut failure = OwnedRecoveryFailure::new(
        &ProviderKind::Codex,
        "gate-fixture",
        ChannelId::new(8_057_101),
        PathBuf::from("/fixture/discord_pending_queue/codex/gate-fixture/8057101.json"),
        "read permission denied".into(),
    );
    let pass = RecoveryPass::new();
    let key = pass.notice_key(&failure).unwrap();
    for error in [
        "invalid UTF-8",
        "invalid JSON",
        "unsupported snapshot version",
    ] {
        failure.error = error.into();
        assert_eq!(pass.notice_key(&failure).unwrap(), key);
    }
    assert_eq!(failure.stage, "catalog_unreadable");
    assert_ne!(RecoveryPass::new().notice_key(&failure).unwrap(), key);
    failure.channel = ChannelId::new(8_057_102);
    assert_ne!(pass.notice_key(&failure).unwrap(), key);
}

#[test]
fn discovery_errors_hold_the_prepared_gate_until_the_next_read_recovers() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let token = "adm-gate-observation";
    let root = super::super::super::pending_queue_root().unwrap();
    let dir = root.join(provider.as_str()).join(token);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("8057111.json");
    std::fs::write(&path, b"invalid JSON").unwrap();
    let pass = RecoveryPass::new();
    let unreadable = super::super::v2_channels(&provider, token);
    assert_eq!(pass.gate(&unreadable), RecoveryGate::Held);
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid JSON");
    std::fs::write(&path, b"[]").unwrap();
    let recovered = super::super::v2_channels(&provider, token);
    assert_eq!(pass.gate(&recovered), RecoveryGate::Complete);
    assert!(recovered.ready.is_empty());
    let namespace_error = OwnedRecoveryDiscovery {
        ready: vec![ChannelId::new(8_057_111)],
        failed: vec![],
        namespace_errors: vec!["read_dir denied".into()],
    };
    assert_eq!(pass.gate(&namespace_error), RecoveryGate::Held);
    let ready = OwnedRecoveryDiscovery {
        ready: namespace_error.ready,
        failed: vec![],
        namespace_errors: vec![],
    };
    assert_eq!(pass.gate(&ready), RecoveryGate::Complete);
}

#[test]
fn cloned_pass_keeps_normalized_catalog_key_and_held_gate_is_absorbing() {
    let failure = OwnedRecoveryFailure::new(
        &ProviderKind::Codex,
        "gate-fixture",
        ChannelId::new(8_057_104),
        PathBuf::from("/fixture/discord_pending_queue/codex/gate-fixture/./8057104.json"),
        "unreadable".into(),
    );
    let pass = RecoveryPass::new();
    let key = pass.notice_key(&failure).unwrap();
    let mut normalized = failure.clone();
    normalized.expected_path =
        PathBuf::from("/fixture/discord_pending_queue/codex/gate-fixture/8057104.json");
    assert_eq!(pass.clone().notice_key(&normalized).unwrap(), key);
    assert_eq!(pass.id(), pass.clone().id());
    assert_eq!(
        RecoveryGate::Complete.combine(RecoveryGate::Held),
        RecoveryGate::Held
    );
    assert_eq!(
        RecoveryGate::Held.combine(RecoveryGate::Complete),
        RecoveryGate::Held
    );
}
