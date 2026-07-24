
use poise::serenity_prelude::{ChannelId, MessageId};
use std::path::Path;

struct RuntimeRootEnvGuard {
    previous: Option<std::ffi::OsString>,
}

impl RuntimeRootEnvGuard {
    fn new(path: &Path) -> Self {
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        Self { previous }
    }
}

impl Drop for RuntimeRootEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

#[test]
fn required_reference_failure_never_retries_as_plain_message() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let channel = ChannelId::new(4_055_771);
        let card = MessageId::new(4_055_772);
        let attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = attempts.clone();
        let _hook = super::rollback_transport_test_hook::install(
            Box::new(
                move |seen_channel, _content, reference, _nonce, _enforce_nonce| {
                    if seen_channel != channel {
                        return None;
                    }
                    seen.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(reference);
                    Some(Err("referenced send rejected".to_string()))
                },
            ),
            Box::new(|_, _| Some(Ok(()))),
        );
        let http = poise::serenity_prelude::Http::new("test-token");
        let shared = crate::services::discord::make_shared_data_for_tests();
        super::long_send_rollback::send_long_message_raw_with_required_reference_rollback(
            &http,
            channel,
            card,
            "task answer",
            &shared,
            (channel, card),
            &"a".repeat(64),
        )
        .await
        .expect_err("a rejected required reference must fail delivery");
        assert_eq!(
            *attempts.lock().unwrap_or_else(|error| error.into_inner()),
            vec![Some((channel, card))],
            "the sender must not make a second unreferenced POST"
        );
    });
}

// #3805 P1: a multi-chunk answer must re-anchor the completion footer onto the
// TAIL continuation chunk (highest snowflake, #3717 latest-wins) carrying the
// tail chunk's OWN text — NOT chunk 0, and NOT the full body (which would
// clobber the tail chunk once the footer edit rewrites it, §4 regression).
#[test]
fn completion_footer_anchor_reanchors_to_last_chunk_with_tail_text() {
    let chunk0 = MessageId::new(1000);
    let full_body = "chunk-0 body ... continuation tail body";
    let tail = super::ReplaceLastChunkAnchor {
        msg_id: 2000,
        text: "continuation tail body".to_string(),
    };

    let (target_id, target_text) =
        super::watcher_completion_footer_anchor(Some(&tail), chunk0, full_body);

    // Re-anchored to the tail chunk id (2000 > 1000: never re-anchors DOWN).
    assert_eq!(target_id, MessageId::new(2000));
    assert!(target_id > chunk0, "must re-anchor to the higher snowflake");
    // Registered text is the tail chunk's OWN text, never the full body.
    assert_eq!(target_text, "continuation tail body");
    assert_ne!(target_text, full_body);
}

// #3805 P1: single-chunk answers have no continuation anchor → keep chunk 0 +
// the full relay text (identical there); the fix is a strict no-op for them.
#[test]
fn completion_footer_anchor_single_chunk_keeps_chunk0_and_full_text() {
    let chunk0 = MessageId::new(1000);
    let full_body = "short single-chunk answer";

    let (target_id, target_text) = super::watcher_completion_footer_anchor(None, chunk0, full_body);

    assert_eq!(target_id, chunk0);
    assert_eq!(target_text, full_body);
}

#[test]
fn partial_continuation_failure_reports_cleanup_scope() {
    let outcome = super::ReplaceLongMessageOutcome::PartialContinuationFailure {
        sent_chunks: 2,
        total_chunks: 3,
        failed_chunk_index: 2,
        sent_continuation_message_ids: vec![9001],
        cleanup_errors: Vec::new(),
        error: "timeout".to_string(),
    };

    if let super::ReplaceLongMessageOutcome::PartialContinuationFailure {
        sent_continuation_message_ids,
        cleanup_errors,
        ..
    } = &outcome
    {
        assert_eq!(sent_continuation_message_ids, &[9001]);
        assert!(cleanup_errors.is_empty());
    } else {
        panic!("expected partial continuation failure");
    }
    assert!(super::replace_long_message_outcome_to_result(outcome).is_err());
}

#[test]
fn continuation_rollback_carries_failed_cleanup_ids() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(7), MessageId::new(11));

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );

    super::record_replace_continuation_rollback(&key, vec![101, 202]).expect("record rollback");
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::Owner(vec![101, 202])
    );
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::InProgress(vec![101, 202])
    );

    super::record_replace_continuation_rollback(&key, Vec::new()).expect("clear by record");
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );
}

#[test]
fn continuation_rollback_progress_can_be_persisted_before_cleanup() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(13), MessageId::new(29));

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    super::record_replace_continuation_rollback(&key, vec![401]).expect("record rollback");
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::Owner(vec![401])
    );

    super::record_replace_continuation_rollback(&key, vec![401, 402])
        .expect("record rollback progress");
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::Owner(vec![401, 402])
    );

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
}

#[test]
fn continuation_rollback_memory_only_quarantines_failed_cleanup_ids() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(31), MessageId::new(37));

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    super::record_replace_continuation_rollback_memory_only(&key, vec![701, 702]);
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::Owner(vec![701, 702])
    );

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
}

#[test]
fn continuation_rollback_memory_clear_suppresses_persisted_reload_4154() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(33), MessageId::new(39));

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    super::record_replace_continuation_rollback(&key, vec![711, 712]).expect("record rollback");
    let rollback_path = super::replace_continuation_rollback_path(&key).expect("rollback path");
    assert!(rollback_path.exists(), "rollback sidecar must be persisted");

    super::clear_replace_continuation_rollback_memory_only(&key);
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );
    assert!(
        rollback_path.exists(),
        "memory tombstone should not require disk cleanup to succeed"
    );

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
}

#[test]
fn continuation_rollback_clear_remove_failure_writes_cleared_marker_4154() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(35), MessageId::new(41));

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    super::record_replace_continuation_rollback(&key, vec![721, 722]).expect("record rollback");
    let rollback_path = super::replace_continuation_rollback_path(&key).expect("rollback path");
    assert!(rollback_path.exists(), "rollback sidecar must be persisted");

    super::force_next_replace_continuation_rollback_remove_failure(&key);
    super::clear_replace_continuation_rollback(&key)
        .expect("clear should write a cleared marker after remove failure");
    let marker = std::fs::read_to_string(&rollback_path).expect("cleared marker");
    assert!(
        marker.contains("\"message_ids\": []"),
        "clear marker must erase delivered rollback ids"
    );

    super::REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&key);
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );
    assert!(
        !rollback_path.exists(),
        "claiming a cleared marker should best-effort remove it"
    );
}

#[test]
fn continuation_rollback_successful_clear_removes_memory_entry_4154() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(37), MessageId::new(41));

    super::record_replace_continuation_rollback(&key, vec![731, 732]).expect("record rollback");
    super::clear_replace_continuation_rollback(&key).expect("clear rollback");

    assert!(
        !super::REPLACE_CONTINUATION_ROLLBACKS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&key),
        "successful clear should leave absence, not a permanent tombstone"
    );
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );
}

#[test]
fn continuation_rollback_corrupt_sidecar_warns_open_and_removes_4154() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(39), MessageId::new(45));
    let rollback_path = super::replace_continuation_rollback_path(&key).expect("rollback path");
    std::fs::create_dir_all(rollback_path.parent().expect("rollback parent"))
        .expect("create rollback parent");
    std::fs::write(&rollback_path, "{not-json").expect("write corrupt sidecar");

    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );
    assert!(
        !rollback_path.exists(),
        "corrupt rollback sidecar should be removed after fail-open"
    );
}

#[test]
fn continuation_rollback_non_utf8_sidecar_warns_open_and_removes_4154() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(40), MessageId::new(46));
    let rollback_path = super::replace_continuation_rollback_path(&key).expect("rollback path");
    std::fs::create_dir_all(rollback_path.parent().expect("rollback parent"))
        .expect("create rollback parent");
    std::fs::write(&rollback_path, [0xff, 0xfe, 0xfd]).expect("write non-UTF8 sidecar");

    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );
    assert!(
        !rollback_path.exists(),
        "non-UTF8 rollback sidecar should be removed after fail-open"
    );
}

#[test]
fn continuation_rollback_unclaim_allows_retry_after_clear_failure() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(41), MessageId::new(43));

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    super::record_replace_continuation_rollback(&key, vec![801]).expect("record rollback");
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::Owner(vec![801])
    );
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::InProgress(vec![801])
    );
    super::unclaim_replace_continuation_rollback(&key);
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::Owner(vec![801])
    );

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
}

#[test]
fn continuation_rollback_survives_memory_loss_until_cleared() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let key = super::replace_continuation_rollback_key(ChannelId::new(17), MessageId::new(23));

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    super::record_replace_continuation_rollback(&key, vec![301, 302]).expect("record rollback");
    let rollback_path = super::replace_continuation_rollback_path(&key).expect("rollback path");
    assert!(rollback_path.exists(), "rollback sidecar must be persisted");

    super::REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&key);
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::Owner(vec![301, 302])
    );
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::InProgress(vec![301, 302])
    );

    super::clear_replace_continuation_rollback(&key).expect("clear rollback");
    super::REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&key);
    assert_eq!(
        super::claim_replace_continuation_rollback(&key),
        super::ReplaceContinuationRollbackClaim::None
    );
    assert!(
        !rollback_path.exists(),
        "clearing rollback must remove persisted sidecar"
    );
}

#[test]
fn task_response_rollback_journal_is_turn_scoped_across_restart() {
    let _guard = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _env = RuntimeRootEnvGuard::new(tempdir.path());
    let channel = ChannelId::new(47);
    let card = MessageId::new(53);
    let turn_one = "1".repeat(64);
    let turn_two = "2".repeat(64);
    let key_one = super::task_response_continuation_rollback_key(channel, card, &turn_one);
    let key_two = super::task_response_continuation_rollback_key(channel, card, &turn_two);

    super::record_replace_continuation_rollback(&key_one, vec![901, 902])
        .expect("persist turn-one rollback debt");
    super::REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();

    assert_eq!(
        super::claim_replace_continuation_rollback(&key_two),
        super::ReplaceContinuationRollbackClaim::None,
        "a new response turn on the same card must not claim stale turn-one debt"
    );
    assert_eq!(
        super::claim_replace_continuation_rollback(&key_one),
        super::ReplaceContinuationRollbackClaim::Owner(vec![901, 902]),
        "the original turn must retain its own restart-recoverable rollback debt"
    );
    super::clear_replace_continuation_rollback(&key_one).expect("clear turn-one debt");
}
