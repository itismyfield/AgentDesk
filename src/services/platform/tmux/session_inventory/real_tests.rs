use super::*;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

struct Rig {
    root: tempfile::TempDir,
    binary: PathBuf,
    socket: PathBuf,
    trace: PathBuf,
    _env: crate::config::test_env_lock::SharedTestEnvLockGuard,
}
impl Rig {
    fn new() -> Self {
        // Namespace validation compares ambient PATH/TMUX even for an explicit socket.
        let env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let binary = PathBuf::from(tmux_command().unwrap().get_program());
        assert!(binary.is_absolute());
        let version = Command::new(&binary).arg("-V").output().unwrap();
        assert!(version.status.success());
        eprintln!(
            "S0U_REAL binary={} version={} uid={} skip=0",
            binary.display(),
            String::from_utf8(version.stdout).unwrap().trim(),
            unsafe { libc::geteuid() }
        );
        let socket = root.path().join("isolated");
        let trace = root.path().join("executed");
        Self {
            root,
            binary,
            socket,
            trace,
            _env: env,
        }
    }
    fn command(&self, default: bool) -> Command {
        let wrapper = self.root.path().join("probe");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$S0U_TRACE\"\nexec \"$S0U_TMUX\" \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(wrapper);
        command
            .arg("-u")
            .env("S0U_TRACE", &self.trace)
            .env("S0U_TMUX", &self.binary)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        if default {
            command.env("TMUX_TMPDIR", self.root.path().canonicalize().unwrap());
        } else {
            command.arg("-S").arg(&self.socket);
        }
        command
    }
    fn probe(&self, default: bool) -> Inventory {
        let command = self.command(default);
        let ctx = Namespace::capture(&command, (!default).then(|| self.socket.clone()));
        let output = wait_for_tmux_output(
            prepare(command),
            Duration::from_secs(3),
            "isolated inventory",
        )
        .unwrap();
        let result = classify_output(&output, &ctx, filesystem);
        eprintln!(
            "S0U_REAL namespace={:?} explicit={:?} status={} stderr={} classification={result:?} executed=1 skip=0",
            ctx.selectors,
            ctx.explicit,
            output.status,
            String::from_utf8(output.stderr).unwrap().trim()
        );
        result
    }
    fn calls(&self) -> usize {
        std::fs::read_to_string(&self.trace)
            .unwrap_or_default()
            .lines()
            .filter(|s| *s == "list-sessions")
            .count()
    }
    fn setup(&self, args: &[&str]) {
        let out = Command::new(&self.binary)
            .arg("-u")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        // Only this fixture's explicit namespace can be cleaned up.
        if std::fs::symlink_metadata(&self.socket).is_ok() {
            let _ = Command::new(&self.binary)
                .arg("-S")
                .arg(&self.socket)
                .arg("kill-server")
                .env_remove("TMUX")
                .output();
        }
    }
}

#[test]
fn u_inventory_missing_socket_real() {
    for default in [false, true] {
        let rig = Rig::new();
        assert!(!rig.socket.exists());
        let result = rig.probe(default);
        let Inventory::Empty(EmptyReason::MissingSocket(path)) = result else {
            panic!("{result:?}");
        };
        assert!(!path.exists());
        assert_eq!(rig.calls(), 1);
        assert_eq!(
            std::fs::read_dir(rig.root.path()).unwrap().count(),
            if default { 3 } else { 2 }
        );
    }
}

#[test]
fn u_inventory_stale_socket_real() {
    let rig = Rig::new();
    drop(UnixListener::bind(&rig.socket).unwrap());
    assert_eq!(
        rig.probe(false),
        Inventory::Empty(EmptyReason::NoServer(rig.socket.clone()))
    );
    assert_eq!(rig.calls(), 1);
}

#[test]
fn u_inventory_successful_list() {
    let rig = Rig::new();
    rig.setup(&["new-session", "-d", "-s", "adk-s0u-alpha", "sleep 60"]);
    rig.setup(&["new-session", "-d", "-s", "adk-s0u-한글", "sleep 60"]);
    assert_eq!(
        rig.probe(false),
        Inventory::Sessions(vec!["adk-s0u-alpha".into(), "adk-s0u-한글".into()])
    );
    assert_eq!(rig.calls(), 1);
}

#[test]
fn u_probe_execution_is_real() {
    let rig = Rig::new();
    assert!(matches!(
        probe_command(
            rig.command(false),
            Some(rig.socket.clone()),
            Duration::from_secs(3)
        ),
        Inventory::Empty(EmptyReason::MissingSocket(_))
    ));
    assert_eq!(rig.calls(), 1, "an empty answer must still execute tmux");
    let args = std::fs::read_to_string(&rig.trace).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "-u",
            "-S",
            rig.socket.to_str().unwrap(),
            "-N",
            "list-sessions",
            "-F",
            "#{session_name}"
        ]
    );
}

#[test]
fn u_inventory_permission_real() {
    let rig = Rig::new();
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "root cannot demonstrate this permission fixture"
    );
    let dir = rig.root.path().join("denied");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("socket");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let error = std::fs::symlink_metadata(&path).unwrap_err();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut command = Command::new(&rig.binary);
    command.arg("-u").arg("-S").arg(&path).env_remove("TMUX");
    let result = probe_command(command, Some(path), Duration::from_secs(3));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(result, Inventory::ProbeFailed(_)), "{result:?}");
}
