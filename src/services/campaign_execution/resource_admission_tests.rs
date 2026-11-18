use super::*;

#[cfg(unix)]
#[tokio::test]
async fn only_the_sustained_headroom_marker_and_success_admit() {
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("probe.sh");
    let budget = Duration::from_secs(3);
    for (script, expected) in [
        (
            "test \"$WH_RECLAIM\" = /dev/null && printf 'HEADROOM_SUSTAINED ok=1 at 12:34:56\\n'",
            ResourceAdmission::Ready,
        ),
        (
            "printf 'HEADROOM_SUSTAINED ok=1 at 12:34:56\\n'; exit 1",
            ResourceAdmission::Unavailable,
        ),
        ("printf 'HEADROOM: OK\\n'", ResourceAdmission::Unavailable),
        (
            "printf 'HEADROOM_SUSTAINED ok=1 at garbage\\n'",
            ResourceAdmission::Unavailable,
        ),
        (
            "printf 'HEADROOM_SUSTAINED ok=1 at 12:34:56\\n'; printf 'probe error' >&2",
            ResourceAdmission::Unavailable,
        ),
        ("exit 0", ResourceAdmission::Unavailable),
    ] {
        std::fs::write(&probe, script).unwrap();
        assert_eq!(
            wait_for_headroom(tmp.path(), &probe, budget).await,
            expected
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_headroom_deadline_preserves_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("probe.sh");
    std::fs::write(&probe, "exec /bin/sleep 10").unwrap();
    assert_eq!(
        wait_for_headroom(tmp.path(), &probe, Duration::from_millis(50)).await,
        ResourceAdmission::WaitingForHeadroom
    );
}
