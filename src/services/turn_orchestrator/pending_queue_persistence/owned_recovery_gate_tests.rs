use super::*;
use sha2::{Digest, Sha256};

struct RecoveryPass(String);

#[derive(Debug, PartialEq, Eq)]
enum RecoveryGate {
    Complete,
    Held,
}

impl RecoveryPass {
    fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    fn gate(&self, discovery: &OwnedRecoveryDiscovery) -> RecoveryGate {
        if discovery.failed.is_empty() && discovery.namespace_errors.is_empty() {
            RecoveryGate::Complete
        } else {
            RecoveryGate::Held
        }
    }

    fn notice_key(&self, failure: &OwnedRecoveryFailure) -> String {
        let identity = (
            &failure.provider,
            &failure.namespace,
            failure.channel.get(),
            failure.expected_path.as_os_str().as_encoded_bytes(),
            &self.0,
            failure.stage,
        );
        let bytes = serde_json::to_vec(&identity).unwrap();
        format!("boot-queue-io:v1:{}", hex::encode(Sha256::digest(bytes)))
    }
}

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
    let key = pass.notice_key(&failure);
    for error in [
        "invalid UTF-8",
        "invalid JSON",
        "unsupported snapshot version",
    ] {
        failure.error = error.into();
        assert_eq!(pass.notice_key(&failure), key);
    }
    assert_eq!(failure.stage, "catalog_unreadable");
    assert_ne!(RecoveryPass::new().notice_key(&failure), key);
    failure.channel = ChannelId::new(8_057_102);
    assert_ne!(pass.notice_key(&failure), key);
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
