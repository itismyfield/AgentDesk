//! A recorded group is signalled only while it is provably the recorded child's.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::*;
use crate::services::process::{
    ProcessGroupProbe, ProcessIdentity, ProcessIdentityProbe, configure_child_process_group,
    process_group_probe, unix_process_presence,
};
use crate::test_support::wait::{WAIT_CAP, poll_within};

fn grouped(script: &str) -> Child {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    configure_child_process_group(&mut command);
    command.spawn().unwrap()
}

// A live process whose start time differs from the record is someone else's: no signal at all.
#[test]
fn a_reused_pid_gets_no_signal() {
    let mut child = grouped("exec sleep 30");
    let pid = child.id();
    let saved = ProcessIdentity::capture(pid);
    let later = |value: Option<u128>| value.map(|value| value + 1);
    let reused = ProcessIdentity::from_persisted(
        later(saved.persisted_starttime()),
        later(saved.persisted_macos_lstart_hash()),
    );
    assert_eq!(reused.probe(pid), ProcessIdentityProbe::GoneOrReused);
    assert_eq!(end_recorded_group(pid, pid, reused), Ok(()));
    assert_eq!(unix_process_presence(pid), ProcessGroupProbe::Live);
    assert_eq!(process_group_probe(pid), ProcessGroupProbe::Live);
    child.kill().unwrap();
    child.wait().unwrap();
}

// The recorded leader is gone but its group lives on: that group is still the child's, so it ends.
#[test]
fn a_dead_leaders_live_group_is_swept() {
    let mut leader = grouped("sleep 30 & echo $!");
    let pid = leader.id();
    let identity = ProcessIdentity::capture(pid);
    let mut line = String::new();
    BufReader::new(leader.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let descendant: u32 = line.trim().parse().unwrap();
    leader.wait().unwrap();
    assert_eq!(unix_process_presence(descendant), ProcessGroupProbe::Live);
    assert_eq!(end_recorded_group(pid, pid, identity), Ok(()));
    assert_eq!(process_group_probe(pid), ProcessGroupProbe::Gone);
}

// C3: the identity a started record keeps still names the same process a second later.
#[tokio::test]
async fn a_recorded_identity_still_matches_after_a_second() {
    let mut child = grouped("exec sleep 30");
    let pid = child.id();
    let saved = ProcessIdentity::capture(pid);
    let recorded = ProcessIdentity::from_persisted(
        saved.persisted_starttime(),
        saved.persisted_macos_lstart_hash(),
    );
    let since = Instant::now();
    let probe = poll_within(
        async || match since.elapsed() < Duration::from_millis(1100) {
            true => Err("a second has not passed".to_owned()),
            false => Ok(recorded.probe(pid)),
        },
        WAIT_CAP,
        "recorded identity a second later",
    )
    .await
    .unwrap();
    assert_eq!(probe, ProcessIdentityProbe::Same);
    child.kill().unwrap();
    child.wait().unwrap();
}
