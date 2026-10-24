#![cfg(unix)]

use super::*;
use crate::services::tui_o::store::LEDGER_FILE;

/// A child forked while recovery holds the ledger keeps the descriptor until its exec; the lock
/// must still end with the recovery, or the next recovery is refused as if another writer held it.
#[test]
fn a_recovery_lock_ends_with_the_recovery_while_a_forked_child_still_holds_the_file() {
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join(LEDGER_FILE);
    std::fs::write(&path, b"{").unwrap();
    let mut release = [0; 2];
    assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
    for fd in release {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let mut child = -1;
    recover_with_tail(&path, 100, || {
        child = unsafe { libc::fork() };
        if child == 0 {
            // Only async-signal-safe calls: wait for the parent to close the pipe, then exit.
            unsafe {
                libc::close(release[1]);
                let mut byte = 0u8;
                libc::read(release[0], (&raw mut byte).cast(), 1);
                libc::_exit(0);
            }
        }
    })
    .unwrap();
    let next = recover(&path, 100);
    unsafe {
        libc::close(release[0]);
        libc::close(release[1]);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    assert!(child > 0, "fork failed");
    assert!(next.is_ok(), "{:?}", next.err());
}
