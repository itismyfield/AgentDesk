//! A child group recorded by an earlier dcserver, ended only while it is provably that child's.

use std::time::{Duration, Instant};

use super::{
    ProcessGroupProbe as Group, ProcessIdentity, ProcessIdentityProbe as Leader, kill_pid_tree,
    kill_pid_tree_if_identity_matches, process_group_probe, unix_process_presence,
};

#[cfg(test)]
#[path = "orphan_group_tests.rs"]
mod tests;

/// Ends the recorded child's group, or confirms it is gone; a PID now used by another process gets
/// no signal. `Err` when neither can be proven, or when the group outlives the deadline.
pub(crate) fn end_recorded_group(
    pid: u32,
    pgid: u32,
    identity: ProcessIdentity,
) -> Result<(), String> {
    match identity.probe(pid) {
        Leader::Same => {
            kill_pid_tree_if_identity_matches(pid, identity);
        }
        Leader::GoneOrReused => match unix_process_presence(pid) {
            // A live group keeps its number from being reissued, so it is still the old child's.
            Group::Gone if process_group_probe(pgid) == Group::Live => kill_pid_tree(pgid),
            Group::Gone => {}
            // A reused PID means the old group had already vanished.
            Group::Live => return Ok(()),
            Group::ProbeError => return Err("could not probe the recorded process".into()),
        },
        Leader::ProbeError => return Err("could not verify the recorded process".into()),
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_group_probe(pgid) != Group::Gone {
        if Instant::now() >= deadline {
            return Err("the recorded process group is still alive".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
