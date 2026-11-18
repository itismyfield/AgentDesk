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

fn probe_with_descendant(root: &Path) -> std::path::PathBuf {
    let probe = root.join("probe.sh");
    std::fs::write(
        &probe,
        "/bin/sleep 20 &\nprintf '%s %s\\n' \"$$\" \"$!\" > \"$ADK_REPO/pids\"\nwait\n",
    )
    .unwrap();
    probe
}

fn no_probe_descendant_remains(root: &Path) {
    use crate::services::process::{ProcessIdentity, kill_pid_tree_if_identity_matches};

    let live = std::fs::read_to_string(root.join("pids"))
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse::<u32>().unwrap())
        .filter(|pid| {
            let output = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            output.status.success()
                && !String::from_utf8_lossy(&output.stdout)
                    .trim_start()
                    .starts_with('Z')
        })
        .collect::<Vec<_>>();
    for &pid in &live {
        kill_pid_tree_if_identity_matches(pid, ProcessIdentity::capture(pid));
    }
    assert!(
        live.is_empty(),
        "headroom returned with live probe processes: {live:?}"
    );
}

#[tokio::test]
async fn m4_regression_headroom_timeout_terminates_the_probe_descendant() {
    let tmp = tempfile::tempdir().unwrap();
    let probe = probe_with_descendant(tmp.path());
    assert_eq!(
        wait_for_headroom(tmp.path(), &probe, Duration::from_secs(1)).await,
        ResourceAdmission::WaitingForHeadroom
    );
    no_probe_descendant_remains(tmp.path());
}
