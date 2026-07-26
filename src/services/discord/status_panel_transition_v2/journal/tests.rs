use std::fs;

use super::*;

fn plan(journal: &ChannelJournal, turn_id: u64, prior: Option<u64>) -> PanelPlan {
    PanelPlan {
        identity: journal.identity().clone(),
        turn_id,
        expected_prior_message_id: prior,
    }
}

fn applied<T: std::fmt::Debug>(mutation: Mutation<T>) -> T {
    match mutation {
        Mutation::Applied(value) => value,
        other => panic!("expected applied mutation, got {other:?}"),
    }
}

fn journal(root: &Path, channel_id: u64) -> ChannelJournal {
    ChannelJournal::open(root, "claude", "discord_0123456789abcdef", channel_id).unwrap()
}

#[test]
fn canonical_identity_rejects_noncanonical_path_components() {
    let root = tempfile::tempdir().unwrap();
    for provider in ["", "unsupported", "../claude", "Claude"] {
        assert!(
            ChannelJournal::open(root.path(), provider, "discord_0123456789abcdef", 1).is_err()
        );
    }
    for token_hash in [
        "",
        "discord_0123456789abcde",
        "discord_0123456789ABCDE",
        "../discord_0123456789abcdef",
    ] {
        assert!(ChannelJournal::open(root.path(), "claude", token_hash, 1).is_err());
    }
    assert!(ChannelJournal::open(root.path(), "claude", "discord_0123456789abcdef", 0).is_err());
}

#[test]
fn crash_reload_recovers_only_bind_authorized_state() {
    let root = tempfile::tempdir().unwrap();
    let channel_journal = journal(root.path(), 31);
    let prepared = applied(channel_journal.prepare(plan(&channel_journal, 10, Some(90)), 0));
    assert!(matches!(
        channel_journal.recover_bind_authorization(),
        ReadOutcome::Missing
    ));
    let authorization = applied(channel_journal.record_sent(&prepared, 91));

    let reopened = journal(root.path(), 31);
    assert_eq!(
        reopened.recover_bind_authorization(),
        ReadOutcome::Present(authorization.clone())
    );
    let bound = applied(reopened.commit_bind(&authorization));
    assert!(matches!(
        reopened.recover_bind_authorization(),
        ReadOutcome::Missing
    ));
    let retire = applied(reopened.authorize_retire(&bound));
    assert_eq!(retire.delete_message_id(), 90);
}

#[test]
fn channel_generation_is_monotonic_across_equal_turn_observations() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path(), 32);
    let first = applied(journal.prepare(plan(&journal, 1, None), 0));
    assert_eq!(first.generation, 1);
    let first_auth = applied(journal.record_sent(&first, 100));
    let _ = applied(journal.commit_bind(&first_auth));

    let snapshot = match journal.load() {
        ReadOutcome::Present(snapshot) => snapshot,
        other => panic!("expected snapshot, got {other:?}"),
    };
    let second = applied(journal.prepare(plan(&journal, 2, Some(100)), snapshot.revision));
    assert_eq!(second.generation, 2);
}

#[test]
fn stale_cas_and_mutated_authorization_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path(), 33);
    let prepared = applied(journal.prepare(plan(&journal, 1, Some(7)), 0));
    assert!(matches!(
        journal.prepare(plan(&journal, 2, Some(7)), 0),
        Mutation::Stale
    ));
    let authorization = applied(journal.record_sent(&prepared, 8));
    let mut mutated = authorization.clone();
    mutated.candidate.message_id = 9;
    assert!(matches!(
        journal.commit_bind(&mutated),
        Mutation::DurabilityFailure(StoreError::InvariantViolation)
    ));
    assert!(matches!(
        journal.commit_bind(&authorization),
        Mutation::Applied(_)
    ));
}

#[test]
fn deletion_model_keeps_or_consumes_exact_authorization() {
    for observation in [
        DeleteObservation::Forbidden403,
        DeleteObservation::Transient,
    ] {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path(), 34);
        let prepared = applied(journal.prepare(plan(&journal, 1, Some(20)), 0));
        let bind = applied(journal.record_sent(&prepared, 21));
        let bound = applied(journal.commit_bind(&bind));
        let retire = applied(journal.authorize_retire(&bound));
        assert_eq!(
            journal.record_delete_observation(&retire, observation),
            Mutation::Replayed(DeleteDisposition::RetainAuthorization)
        );
    }

    for observation in [
        DeleteObservation::Deleted,
        DeleteObservation::NotFound404,
        DeleteObservation::UnknownMessage10008,
    ] {
        let root = tempfile::tempdir().unwrap();
        let journal = journal(root.path(), 35);
        let prepared = applied(journal.prepare(plan(&journal, 1, Some(20)), 0));
        let bind = applied(journal.record_sent(&prepared, 21));
        let bound = applied(journal.commit_bind(&bind));
        let retire = applied(journal.authorize_retire(&bound));
        assert_eq!(
            journal.record_delete_observation(&retire, observation),
            Mutation::Applied(DeleteDisposition::Retired)
        );
    }
}

#[test]
fn every_write_stage_failpoint_is_typed_and_reloadable() {
    for target in [WriteTarget::Operation, WriteTarget::Channel] {
        for stage in [
            WriteStage::CreateTemp,
            WriteStage::WriteAll,
            WriteStage::SyncFile,
            WriteStage::Rename,
            WriteStage::SyncParent,
        ] {
            let root = tempfile::tempdir().unwrap();
            let channel_journal = journal(root.path(), 36).with_failpoint(target, stage);
            assert!(matches!(
                channel_journal.prepare(plan(&channel_journal, 1, None), 0),
                Mutation::DurabilityFailure(StoreError::WriteFailed(actual)) if actual == stage
            ));
            let reopened = journal(root.path(), 36);
            assert!(matches!(
                reopened.load(),
                ReadOutcome::Missing | ReadOutcome::Present(_)
            ));
        }
    }
}

#[test]
fn exact_replay_tuple_accepts_only_identical_operation() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path(), 39);
    let prepared = applied(journal.prepare(plan(&journal, 1, Some(50)), 0));
    let authorization = applied(journal.record_sent(&prepared, 51));
    assert_eq!(
        journal.record_sent(&prepared, 51),
        Mutation::Replayed(authorization.clone())
    );
    assert_eq!(journal.record_sent(&prepared, 52), Mutation::Stale);
    assert!(matches!(
        journal.commit_bind(&authorization),
        Mutation::Applied(_)
    ));
    assert!(matches!(
        journal.commit_bind(&authorization),
        Mutation::Replayed(_)
    ));
}

#[cfg(unix)]
#[test]
fn channel_lock_excludes_a_second_process() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path(), 40);
    let _held = journal.lock().unwrap();
    let executable = std::env::current_exe().unwrap();
    let helper =
        "services::discord::status_panel_transition_v2::journal::tests::channel_lock_child_process";
    let output = std::process::Command::new(executable)
        .args(["--ignored", "--exact", helper])
        .env("AGENTDESK_PANEL_JOURNAL_TEST_ROOT", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
#[ignore = "helper subprocess for the channel-lock concurrency test"]
fn channel_lock_child_process() {
    let root = std::env::var_os("AGENTDESK_PANEL_JOURNAL_TEST_ROOT").unwrap();
    let journal = journal(Path::new(&root), 40);
    assert!(matches!(
        storage::try_lock_for_test(&journal.channel_dir),
        Err(StoreError::LockFailed)
    ));
}

#[test]
fn operation_and_quarantine_gc_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path(), 37);
    let operations = journal.channel_dir.join("operations");
    let quarantine = journal.channel_dir.join("quarantine");
    for index in 0..(MAX_OPERATION_RECORDS + 5) {
        fs::write(operations.join(format!("{index:020}.json")), b"{}").unwrap();
    }
    for index in 0..(MAX_QUARANTINE_RECORDS + 5) {
        fs::write(quarantine.join(format!("{index:020}.json")), b"{}").unwrap();
    }
    journal.gc_locked().unwrap();
    assert_eq!(
        fs::read_dir(operations).unwrap().count(),
        MAX_OPERATION_RECORDS
    );
    assert_eq!(
        fs::read_dir(quarantine).unwrap().count(),
        MAX_QUARANTINE_RECORDS
    );
}

#[cfg(unix)]
#[test]
fn symlinked_channel_file_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path(), 38);
    let target = root.path().join("outside.json");
    fs::write(&target, b"{}").unwrap();
    symlink(target, journal.channel_dir.join(CHANNEL_FILE)).unwrap();
    assert_eq!(
        journal.load(),
        ReadOutcome::DurabilityFailure(StoreError::SymlinkRejected)
    );
}
