use super::super::has_session;
use super::*;
use crate::config::TestEnvVarGuard as Guard;
use std::os::unix::fs::PermissionsExt;

/// Kills the private test server however the test ends.
struct Server;

impl Drop for Server {
    fn drop(&mut self) {
        let _ = tmux_output(["kill-server"]);
    }
}

// Cleanup takes the server pid and session id together: the same id under another server pid
// is left alone and reported unconfirmed, and only the created identity is removed.
#[test]
fn sized_cleanup_removes_only_the_created_identity() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let path = std::env::var("PATH").unwrap();
    let real = std::env::split_paths(&path)
        .map(|dir| dir.join("tmux"))
        .find(|path| path.is_file())
        .expect("a real tmux binary");
    let shim = dir.path().join("tmux");
    let socket = format!("cga-cleanup-{}", std::process::id());
    let body = format!(
        "#!/bin/sh\nexec '{}' -L {socket} -f /dev/null \"$@\"\n",
        real.display()
    );
    std::fs::write(&shim, body).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let tmux_env = Guard::capture_after_shared_test_env_lock("TMUX");
    unsafe { std::env::remove_var("TMUX") };
    let shimmed = format!("{}:{path}", dir.path().display());
    let _guards = [
        Guard::set_value_after_shared_test_env_lock("PATH", shimmed.as_ref()),
        Guard::set_path_after_shared_test_env_lock("TMUX_TMPDIR", dir.path()),
        tmux_env,
    ];
    let _server = Server;
    let name = "AgentDesk-cga-cleanup";
    let created = tmux_output([
        "new-session",
        "-d",
        "-P",
        "-F",
        CREATED_IDENTITY,
        "-s",
        name,
        "sleep 600",
    ])
    .unwrap();
    assert!(created.status.success(), "{created:?}");
    let identity = String::from_utf8_lossy(&created.stdout).trim().to_string();
    let (_, ids) = identity.split_once(' ').unwrap();
    let session = ids.split(' ').next().unwrap();

    let (cleanup, other_server) = remove_created_session(name, &format!("1 {ids}"));
    assert_eq!(cleanup, SizedCleanup::Unconfirmed, "{other_server}");
    assert!(
        other_server.starts_with("cleanup unconfirmed"),
        "{other_server}"
    );
    assert!(
        has_session(name),
        "another server's identity must not be killed"
    );
    let (cleanup, unknown) = remove_created_session(name, "");
    assert_eq!(cleanup, SizedCleanup::Unconfirmed);
    assert!(unknown.contains("identity of"), "{unknown}");
    assert!(has_session(name));

    let removed = remove_created_session(name, &identity);
    let expected = format!("created session {session} removed");
    assert_eq!(removed, (SizedCleanup::Removed, expected));
    assert!(!has_session(name));
}
