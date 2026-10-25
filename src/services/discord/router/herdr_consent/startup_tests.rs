use super::*;
use crate::services::session_host::ServerWitness;
use crate::services::session_host::policy::consent::{
    ConsentKey, IntentRef, Preview, Question, QuestionResult,
};

fn accepted(
    store: &ConsentStore,
    path: &std::path::Path,
    provider: &str,
    socket: &str,
) -> crate::services::session_host::policy::consent::Consent {
    std::fs::write(path, "[session]\nresume_agents_on_restore = true\n").unwrap();
    let mut witness = ServerWitness::for_test(7);
    witness.socket = socket.into();
    // SAFETY: geteuid has no preconditions.
    let user = unsafe { libc::geteuid() };
    let question = Question {
        key: ConsentKey {
            node: "node".into(),
            user,
            socket: socket.into(),
            named_session: "desk".into(),
            config: path.into(),
            target: path.into(),
        },
        preview: Preview {
            revision: 1,
            witness,
            target_hash: [1; 32],
            chain_hash: [2; 32],
            checker_hash: [3; 32],
        },
        owner: 1,
        bot: 2,
        provider: provider.into(),
        channel: 3,
        message: 4,
        intent: IntentRef {
            store: "mailbox".into(),
            id: provider.into(),
            provider: provider.into(),
            channel: 3,
            source_message: 9,
            fifo: 1,
        },
    };
    let held = store
        .hold(question, 100)
        .map(QuestionResult::into_consent)
        .unwrap();
    store.decide(&held, 1, true, 101).unwrap()
}

#[test]
fn startup_recovers_all_local_provider_accepts_before_apply_admission() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let one = accepted(
        &store,
        &temp.path().join("one.toml"),
        "claude",
        "/fixture/one.sock",
    );
    let two = accepted(
        &store,
        &temp.path().join("two.toml"),
        "codex",
        "/fixture/two.sock",
    );
    let unselected = accepted(
        &store,
        &temp.path().join("other.toml"),
        "claude",
        "/fixture/other.sock",
    );
    let before = std::fs::read(&one.key.config).unwrap();
    let local = Local {
        node: "node".into(),
        endpoints: vec![
            (one.key.socket.clone(), "desk".into()),
            (two.key.socket.clone(), "desk".into()),
        ],
    };
    let boot = BootState::new();
    let result = interrupt(&store, &local, &boot);
    assert!(
        result.is_ok(),
        "startup_metadata_recovery_completes_before_apply_contract: {result:?}"
    );
    assert!(
        boot.is_ready(),
        "startup_completed_recovery_admits_apply_contract"
    );
    for key in [&one.key, &two.key] {
        let recovered = store.load(key).unwrap().unwrap();
        assert_eq!(
            recovered.state,
            State::ApplyFailed,
            "startup_all_local_providers_interrupt_only_at_boot_contract"
        );
        let receipt = recovered.apply.unwrap();
        assert_eq!(receipt.before, receipt.after);
        assert!(receipt.backup.is_none());
    }
    assert_eq!(
        store.load(&unselected.key).unwrap().unwrap().state,
        State::Accepted,
        "startup_unselected_endpoint_has_no_file_effect_contract"
    );
    assert_eq!(std::fs::read(&one.key.config).unwrap(), before);
    assert!(interrupt(&store, &local, &boot).is_ok());
}

#[test]
fn startup_store_failure_refuses_apply_without_aborting_the_process() {
    let temp = tempfile::tempdir().unwrap();
    let broken = temp.path().join("not-a-directory");
    std::fs::write(&broken, "not a store").unwrap();
    let store = ConsentStore::new(Some(broken));
    let boot = BootState::new();
    let local = Local {
        node: "node".into(),
        endpoints: vec![],
    };
    let result = interrupt(&store, &local, &boot);
    assert!(
        result.is_err(),
        "startup_store_failure_is_not_an_empty_success_contract"
    );
    assert!(
        !boot.is_ready(),
        "startup_store_failure_keeps_apply_fail_closed_contract"
    );
    assert!(boot.try_claim().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn startup_legacy_returns_before_settings_or_policy_store_io() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _boot = session_hosts::force_for_test(Some("node"), &[(331, "node")]);
    use std::os::unix::fs::PermissionsExt;
    let directory = crate::config::runtime_root()
        .unwrap()
        .join("runtime/herdr_policy/consents");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let poison = directory.join(format!("{}.json", "a".repeat(64)));
    let original = b"{malformed preexisting consent snapshot}";
    std::fs::write(&poison, original).unwrap();
    std::fs::set_permissions(&poison, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = super::super::STORE_READS.load(std::sync::atomic::Ordering::SeqCst);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _settings = shared.settings.write().await;
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        restore(&http, &shared, &ProviderKind::Claude),
    )
    .await;
    let held = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        restore_only(&http, &shared, &ProviderKind::Claude, 331),
    )
    .await;
    assert!(
        matches!(held, Ok(Ok(()))),
        "startup_legacy_held_replay_precedes_lock_and_io_contract"
    );
    assert_eq!(
        super::super::STORE_READS.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "startup_legacy_policy_store_reads_zero_contract"
    );
    assert!(
        matches!(result, Ok(Ok(()))),
        "startup_legacy_precedes_lock_and_io_contract"
    );
    assert_eq!(
        std::fs::read(&poison).unwrap(),
        original,
        "startup_legacy_preexisting_snapshot_unchanged_contract"
    );
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        1,
        "startup_legacy_additional_policy_files_zero_contract"
    );
}

#[test]
fn startup_one_interrupted_config_failure_does_not_skip_later_accepts() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    accepted(
        &store,
        &temp.path().join("one.toml"),
        "claude",
        "/fixture/one.sock",
    );
    accepted(
        &store,
        &temp.path().join("two.toml"),
        "codex",
        "/fixture/two.sock",
    );
    let records = store.load_all().unwrap();
    let first = &records[0];
    std::fs::remove_file(&first.key.config).unwrap();
    std::fs::create_dir(&first.key.config).unwrap();
    let local = Local {
        node: "node".into(),
        endpoints: records
            .iter()
            .map(|record| (record.key.socket.clone(), record.key.named_session.clone()))
            .collect(),
    };
    let boot = BootState::new();
    assert!(
        interrupt(&store, &local, &boot).is_err(),
        "startup_partial_interruption_reports_failure_contract"
    );
    assert!(
        !boot.is_ready(),
        "startup_partial_interruption_keeps_apply_closed_contract"
    );
    assert_eq!(
        store.load(&first.key).unwrap().unwrap().state,
        State::Accepted
    );
    assert_eq!(
        store.load(&records[1].key).unwrap().unwrap().state,
        State::ApplyFailed,
        "startup_failed_consent_does_not_skip_remaining_contract"
    );
}

#[test]
fn startup_one_corrupt_snapshot_does_not_skip_valid_accepted_neighbors() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let broken = accepted(
        &store,
        &temp.path().join("broken.toml"),
        "claude",
        "/fixture/broken.sock",
    );
    let valid = accepted(
        &store,
        &temp.path().join("valid.toml"),
        "codex",
        "/fixture/valid.sock",
    );
    let filename = crate::services::session_host::policy::durable::key(&broken.key).unwrap();
    std::fs::write(
        directory.join(format!("{filename}.json")),
        b"{broken snapshot",
    )
    .unwrap();
    let local = Local {
        node: "node".into(),
        endpoints: vec![
            (broken.key.socket, "desk".into()),
            (valid.key.socket.clone(), "desk".into()),
        ],
    };
    let boot = BootState::new();
    assert!(
        interrupt(&store, &local, &boot).is_err(),
        "startup_corrupt_neighbor_failure_is_reported_contract"
    );
    assert!(
        !boot.is_ready(),
        "startup_corrupt_neighbor_keeps_apply_closed_contract"
    );
    assert_eq!(
        store.load(&valid.key).unwrap().unwrap().state,
        State::ApplyFailed,
        "startup_corrupt_neighbor_never_skips_valid_consent_contract"
    );
    assert!(
        store.load_all().is_err(),
        "startup_recovery_does_not_mask_corruption_contract"
    );
}
