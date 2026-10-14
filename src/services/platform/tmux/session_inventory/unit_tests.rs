use super::*;
use std::cell::RefCell;
use std::os::unix::process::ExitStatusExt;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

thread_local! { static PROBE_TRACE: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) }; }
thread_local! { static COMMAND: RefCell<Option<(Command, Option<PathBuf>)>> = const { RefCell::new(None) }; }
pub(super) fn take_command() -> Option<(Command, Option<PathBuf>)> {
    COMMAND.with(|slot| slot.borrow_mut().take())
}
pub(crate) fn probe_in_namespace(path: &Path) -> Inventory {
    let mut command = tmux_command().unwrap();
    command
        .arg("-S")
        .arg(path)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    COMMAND.with(|slot| {
        assert!(
            slot.replace(Some((command, Some(path.to_owned()))))
                .is_none()
        )
    });
    probe_session_inventory()
}
pub(crate) struct ProbeTrace(Arc<AtomicUsize>);
impl ProbeTrace {
    pub(crate) fn new() -> Self {
        let count = Arc::new(AtomicUsize::new(0));
        PROBE_TRACE.with(|slot| assert!(slot.replace(Some(count.clone())).is_none()));
        Self(count)
    }
    pub(crate) fn calls(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}
impl Drop for ProbeTrace {
    fn drop(&mut self) {
        PROBE_TRACE.with(|slot| {
            slot.replace(None);
        });
    }
}
pub(super) fn record_probe() {
    PROBE_TRACE.with(|slot| {
        if let Some(count) = &*slot.borrow() {
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
}

fn context(path: &Path) -> Namespace {
    context_for_command(path, &tmux_command().unwrap())
}
fn context_for_command(path: &Path, command: &Command) -> Namespace {
    Namespace::capture(command, Some(path.to_owned()))
}
fn output(code: i32, stdout: &[u8], stderr: &str) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}
fn failed(result: Inventory) {
    assert!(matches!(result, Inventory::ProbeFailed(_)), "{result:?}");
}

#[test]
fn u_inventory_command_context() {
    let bin = tempfile::tempdir().unwrap();
    let tool = bin.path().join("tmux");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The first prepared cache and each command's PATH are independent snapshots.
    assert!(crate::services::platform::binary_resolver::merged_runtime_path().is_some());
    let prepared = crate::services::platform::binary_resolver::prepared_runtime_path().unwrap();
    let mut runtime = crate::services::platform::binary_resolver::command_with_path(
        std::ffi::OsStr::new("tmux"),
        Some(bin.path().as_os_str()),
    )
    .unwrap();
    runtime.arg("-u");
    let effective_path = runtime
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .and_then(|(_, value)| value)
        .unwrap();
    let snapshot = Namespace::capture(&runtime, None);
    assert!(Path::new(&snapshot.program).is_absolute());
    assert_eq!(runtime.get_program(), tool.as_os_str());
    assert_eq!(effective_path, bin.path().as_os_str());
    assert_eq!(snapshot.path.as_deref(), Some(effective_path));
    assert_eq!(snapshot.runtime_path.as_deref(), Some(prepared));
    runtime
        .env("TMUX", "/tmp/test-socket,10,0")
        .env("LISTEN_PID", "10")
        .env("LISTEN_FDS", "3")
        .env("LISTEN_CUSTOM", "bad");
    let command = prepare(runtime);
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["-u", "-N", "list-sessions", "-F", "#{session_name}"]
    );
    let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(
        env[std::ffi::OsStr::new("PATH")],
        Some(bin.path().as_os_str())
    );
    assert_eq!(
        env[std::ffi::OsStr::new("TMUX")],
        Some(std::ffi::OsStr::new("/tmp/test-socket,10,0"))
    );
    assert_eq!(
        env[std::ffi::OsStr::new("LC_ALL")],
        Some(std::ffi::OsStr::new("C"))
    );
    for key in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_CUSTOM"] {
        assert_eq!(env[std::ffi::OsStr::new(key)], None);
    }
}

#[test]
fn u_inventory_complete_stdout() {
    let ctx = context(Path::new("/tmp/s0u-none"));
    let bin = tempfile::tempdir().unwrap();
    let tool = bin.path().join("tmux");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = crate::services::platform::binary_resolver::command_with_path(
        std::ffi::OsStr::new("tmux"),
        Some(bin.path().as_os_str()),
    )
    .unwrap();
    let path = bin.path().join("socket");
    let portable = context_for_command(&path, &runtime);
    assert_eq!(
        portable.program,
        runtime.get_program(),
        "resolved executable is preserved"
    );
    for (out, expected) in [
        (
            output(0, "alpha\n한글-β\n".as_bytes(), ""),
            Inventory::Sessions(vec!["alpha".into(), "한글-β".into()]),
        ),
        (output(0, b"", ""), Inventory::Sessions(vec![])),
        (
            output(
                1,
                b"",
                &format!(
                    "error connecting to {} (No such file or directory)\n",
                    path.display()
                ),
            ),
            Inventory::Empty(EmptyReason::MissingSocket(path.clone())),
        ),
        (
            output(
                1,
                b"",
                &format!("no server running on {}\n", path.display()),
            ),
            Inventory::Empty(EmptyReason::NoServer(path.clone())),
        ),
    ] {
        assert_eq!(
            classify_output(&out, &portable, |_| SocketState::Missing),
            expected
        );
    }
    let parse = |bytes: &[u8]| {
        classify_output(&output(0, bytes, ""), &ctx, |_| {
            panic!("success must not invent a socket path")
        })
    };
    assert_eq!(
        parse("alpha\n한글-β\n".as_bytes()),
        Inventory::Sessions(vec!["alpha".into(), "한글-β".into()])
    );
    assert_eq!(parse(b""), Inventory::Sessions(vec![]));
    for bytes in [
        b"alpha\npartial".as_slice(),
        b"alpha\n\n",
        b"alpha\nbad:name\n",
        b"alpha\nbad.name\n",
        b"alpha\n\xff\n",
        b"alpha\nx\r\n",
        b"alpha\n\t\n",
    ] {
        failed(parse(bytes));
    }
    failed(classify_output(
        &output(0, b"alpha\n", "warning\n"),
        &ctx,
        |_| SocketState::Missing,
    ));
}

#[test]
fn u_inventory_absence_recheck() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("socket");
    let ctx = context(&path);
    let out = output(
        1,
        b"",
        &format!(
            "error connecting to {} (No such file or directory)\n",
            path.display()
        ),
    );
    assert_eq!(
        classify_output(&out, &ctx, filesystem),
        Inventory::Empty(EmptyReason::MissingSocket(path.clone()))
    );
    std::os::unix::fs::symlink(root.path().join("gone"), &path).unwrap();
    failed(classify_output(&out, &ctx, filesystem));
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "appeared").unwrap();
    failed(classify_output(&out, &ctx, filesystem));
    failed(classify_output(&out, &ctx, |_| SocketState::Other));
    let denied = root.path().join("denied");
    std::fs::create_dir(&denied).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
    let denied_path = denied.join("socket");
    let state = filesystem(&denied_path);
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(state, SocketState::Other),
        "metadata permission failure is not ENOENT"
    );
    let mut changed = ctx.clone();
    changed.ambient[0] = Some("changed".into());
    failed(classify_output(&out, &changed, |_| SocketState::Missing));
    changed = ctx.clone();
    changed.runtime_path = Some("changed".into());
    failed(classify_output(&out, &changed, |_| SocketState::Missing));
    let executable = root.path().join("executable");
    std::fs::write(&executable, "before").unwrap();
    let fixed = Namespace::capture(&Command::new(&executable), Some(path.clone()));
    std::fs::remove_file(&executable).unwrap();
    std::fs::write(&executable, "replacement").unwrap();
    failed(classify_output(&out, &fixed, |_| SocketState::Missing));
    let fixed = Namespace::capture(&Command::new(&executable), Some(path.clone()));
    failed(classify_output(&out, &fixed, |_| {
        std::fs::remove_file(&executable).unwrap();
        SocketState::Missing
    }));
    for reported in ["relative", "/different/socket"] {
        failed(classify_output(
            &output(
                1,
                b"",
                &format!("error connecting to {reported} (No such file or directory)"),
            ),
            &ctx,
            |_| SocketState::Missing,
        ));
    }
    let mut selected = Command::new("tmux");
    selected.env("TMUX", format!("{},12,0", path.display()));
    assert!(Namespace::capture(&selected, None).matches(&path));
    assert!(!Namespace::capture(&selected, None).matches(Path::new("/foreign/socket")));
}

#[test]
fn u_inventory_execution_failures() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("socket");
    let ctx = context(&path);
    for stderr in [
        "No such file or directory",
        "tmux: unknown option -- N",
        "permission denied",
        "no server running on /foreign",
        "error connecting to relative (No such file or directory)",
    ] {
        failed(classify_output(&output(1, b"", stderr), &ctx, |_| {
            SocketState::Missing
        }));
    }
    let missing = format!(
        "error connecting to {} (No such file or directory)",
        path.display()
    );
    for out in [
        output(2, b"", &missing),
        output(1, b"extra", &missing),
        output(1, b"", &format!("{missing}\nwarning\n")),
        Output {
            status: std::process::ExitStatus::from_raw(libc::SIGTERM),
            ..output(1, b"", &missing)
        },
    ] {
        failed(classify_output(&out, &ctx, |_| SocketState::Missing));
    }
    failed(probe_command(
        Command::new(root.path().join("missing-exe")),
        Some(path.clone()),
        Duration::from_secs(1),
    ));
    let executable = root.path().join("non-executable");
    std::fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
    failed(probe_command(
        Command::new(&executable),
        Some(path.clone()),
        Duration::from_secs(1),
    ));
    for script in ["sleep 5", "kill -TERM $$"] {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        failed(probe_command(
            command,
            Some(path.clone()),
            Duration::from_millis(40),
        ));
    }
    let resolver = crate::services::platform::binary_resolver::command_with_path;
    assert_eq!(
        resolver(
            std::ffi::OsStr::new("missing-exe"),
            Some(root.path().as_os_str())
        )
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        resolver(
            std::ffi::OsStr::new("non-executable"),
            Some(root.path().as_os_str())
        )
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[test]
fn u_inventory_no_server_control() {
    let path = Path::new("/tmp/s0u-stale");
    let ctx = context(path);
    let out = output(1, b"", "no server running on /tmp/s0u-stale\n");
    for state in [SocketState::Missing, SocketState::Stale] {
        assert_eq!(
            classify_output(&out, &ctx, |_| state),
            Inventory::Empty(EmptyReason::NoServer(path.to_owned()))
        );
    }
    failed(classify_output(&out, &ctx, |_| SocketState::Other));
}
