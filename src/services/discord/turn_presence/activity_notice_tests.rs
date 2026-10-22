use super::notice_identity::ExecutionIdentity;
use super::tests::{NoticeReaderProbe, resumed_notice_observations};
use super::*;

#[test]
fn retry_metadata_uses_actual_rebuild_install_and_std_deadline() {
    let probe = NoticeReaderProbe::new(6_190_001);
    assert!(probe.read().notice_observation().progress);
    assert_eq!(probe.run_jobs(), 1);
    let failure = probe.read().notice_observation();
    assert_eq!(failure.reason, "binding_unreadable");
    assert!(!failure.progress);
    assert_eq!(probe.counts(), (1, 1));
    for _ in 0..3 {
        assert!(!probe.read().notice_observation().progress);
        assert_eq!(probe.counts(), (1, 1));
    }
    probe.force_retry_due();
    let retry = probe.read();
    assert_eq!(retry.observed.reason, "catching_up");
    assert!(retry.notice_observation().progress);
    assert_eq!(probe.counts(), (1, 2));
    assert_eq!(probe.run_jobs(), 1);
    assert_eq!(probe.read().notice_observation(), failure);
    assert_eq!(probe.counts(), (2, 2));
    probe.pending();
    assert!(probe.read().notice_observation().progress);
    assert_eq!(probe.run_jobs(), 1);
    let pending = probe.read().notice_observation();
    assert_eq!(pending.reason, "binding_pending");
    assert!(!pending.progress);
}

#[test]
fn resumed_worker_progress_differs_from_settled_boundary_wait() {
    let root = tempfile::tempdir().unwrap();
    let (resumed, jobs, boundary) = resumed_notice_observations(root.path());
    assert_eq!(resumed.reason, "facts_resumed");
    assert!(resumed.progress);
    assert_eq!(jobs, 1);
    assert_eq!(boundary.reason, "facts_resumed");
    assert!(!boundary.progress);
}

#[test]
fn binding_seq_failure_retains_the_judged_target_label() {
    let probe = NoticeReaderProbe::new(6_190_002);
    probe.unreadable_seq();
    let notice = probe.read().notice_observation();
    assert_eq!(notice.reason, "binding_unreadable");
    assert_eq!(
        notice.target_label.as_deref(),
        Some("notice-reader-6190002")
    );
    assert_eq!(
        notice.identity,
        ExecutionIdentity::LegacySession("notice-reader-6190002".into())
    );
    assert!(!notice.progress);
    assert_eq!(probe.counts(), (0, 0));
}

#[tokio::test(flavor = "current_thread")]
async fn a_changed_spawn_nonce_revokes_the_old_notice_publication() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let probe = NoticeReaderProbe::new(6_190_003);
    let label = "notice-reader-6190003";
    let legacy = std::path::PathBuf::from(crate::services::tmux_common::legacy_tmp_session_path(
        label,
        "spawn_nonce",
    ));
    let path = crate::services::tmux_common::persistent_sessions_dir()
        .unwrap()
        .join(legacy.file_name().unwrap());
    assert!(!path.parent().unwrap().exists());
    let cold = probe.read();
    assert!(
        !path.parent().unwrap().exists(),
        "notice reads created a runtime directory"
    );
    assert!(cold.publish_if_current(|| ()).await.is_some());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "nonce-a").unwrap();
    assert!(cold.publish_if_current(|| ()).await.is_none());
    let current = probe.read();
    assert_eq!(
        current.notice_observation().identity,
        ExecutionIdentity::Nonce("nonce-a".into())
    );
    assert!(current.publish_if_current(|| ()).await.is_some());
    std::fs::write(&path, "nonce-b").unwrap();
    assert!(current.publish_if_current(|| ()).await.is_none());
}

#[tokio::test(start_paused = true)]
async fn tokio_time_alone_does_not_retry_and_fast_install_preserves_captured_progress() {
    let probe = NoticeReaderProbe::new(6_190_004);
    let cold = probe.read();
    assert!(cold.notice_observation().progress);
    assert_eq!(probe.run_jobs(), 1);
    assert!(
        cold.notice_observation().progress,
        "worker installation cannot rewrite captured metadata"
    );
    assert!(cold.publish_if_current(|| ()).await.is_some());
    assert!(!probe.read().notice_observation().progress);
    assert_eq!(probe.counts(), (1, 1));
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(!probe.read().notice_observation().progress);
    assert_eq!(
        probe.counts(),
        (1, 1),
        "the reader retry deadline uses std Instant"
    );
    probe.force_retry_due();
    let retry = probe.read();
    assert!(retry.notice_observation().progress);
    assert_eq!(probe.counts(), (1, 2));
    assert_eq!(probe.run_jobs(), 1);
    assert!(retry.notice_observation().progress);
    assert!(retry.publish_if_current(|| ()).await.is_some());
    let actual = probe.read().notice_observation();
    assert_eq!(actual.reason, "binding_unreadable");
    assert!(!actual.progress);
    assert_eq!(probe.counts(), (2, 2));
}
