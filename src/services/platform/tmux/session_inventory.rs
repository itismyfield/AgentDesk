//! A complete, read-only inventory of one unchanged tmux namespace.
use super::{tmux_command, wait_for_tmux_output};
use std::ffi::OsString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EmptyReason {
    MissingSocket(PathBuf),
    NoServer(PathBuf),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inventory {
    Sessions(Vec<String>),
    Empty(EmptyReason),
    ProbeFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Namespace {
    program: OsString,
    executable: Option<(u64, u64, i64, i64, i64, i64)>,
    path: Option<OsString>,
    selectors: [Option<OsString>; 2],
    ambient: [Option<OsString>; 3],
    uid: u32,
    euid: u32,
    cwd: Option<PathBuf>,
    runtime_path: Option<OsString>,
    explicit: Option<PathBuf>,
}
impl Namespace {
    fn capture(command: &Command, explicit: Option<PathBuf>) -> Self {
        let effective = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == name)
                .map_or_else(
                    || std::env::var_os(name),
                    |(_, value)| value.map(OsString::from),
                )
        };
        Self {
            program: command.get_program().into(),
            executable: executable_identity(Path::new(command.get_program())),
            path: effective("PATH"),
            selectors: [effective("TMUX"), effective("TMUX_TMPDIR")],
            ambient: [
                std::env::var_os("TMUX"),
                std::env::var_os("TMUX_TMPDIR"),
                std::env::var_os("PATH"),
            ],
            // SAFETY: these calls only read this process's credentials.
            uid: unsafe { libc::getuid() },
            euid: unsafe { libc::geteuid() },
            explicit,
            cwd: std::env::current_dir().ok(),
            runtime_path: crate::services::platform::binary_resolver::prepared_runtime_path()
                .map(OsString::from),
        }
    }
    fn current(&self) -> bool {
        self.cwd.is_some()
            && self.executable.is_some()
            && self.executable == executable_identity(Path::new(&self.program))
            && self.ambient
                == [
                    std::env::var_os("TMUX"),
                    std::env::var_os("TMUX_TMPDIR"),
                    std::env::var_os("PATH"),
                ]
            && self.uid == unsafe { libc::getuid() }
            && self.euid == unsafe { libc::geteuid() }
            && self.cwd == std::env::current_dir().ok()
            && self.runtime_path
                == crate::services::platform::binary_resolver::prepared_runtime_path()
                    .map(OsString::from)
    }
    fn matches(&self, path: &Path) -> bool {
        if !path.is_absolute() {
            return false;
        }
        if let Some(explicit) = &self.explicit {
            return path == explicit;
        }
        if let Some(tmux) = &self.selectors[0] {
            let Some(text) = tmux.to_str() else {
                return false;
            };
            let mut parts = text.rsplitn(3, ',');
            let (Some(session), Some(pid), Some(socket)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return false;
            };
            return session.parse::<u64>().is_ok()
                && pid.parse::<u32>().is_ok()
                && path == Path::new(socket);
        }
        if let Some(root) = &self.selectors[1] {
            let Ok(root) = std::path::absolute(root) else {
                return false;
            };
            if !path.starts_with(root) {
                return false;
            }
        }
        // Use tmux's reported path; the default socket-name algorithm is not reproduced.
        path.parent()
            .and_then(|p| std::fs::symlink_metadata(p).ok())
            .is_some_and(|m| m.is_dir() && m.uid() == self.uid)
    }
}

fn executable_identity(path: &Path) -> Option<(u64, u64, i64, i64, i64, i64)> {
    std::fs::metadata(path).ok().map(|m| {
        (
            m.dev(),
            m.ino(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    })
}

fn prepare(mut command: Command) -> Command {
    command
        .args(["-N", "list-sessions", "-F", "#{session_name}"])
        .env("LC_ALL", "C");
    let keys: Vec<_> = command
        .get_envs()
        .map(|(key, _)| key.to_os_string())
        .chain(std::env::vars_os().map(|(key, _)| key))
        .collect();
    for key in keys {
        if key.as_bytes().starts_with(b"LISTEN_") {
            command.env_remove(key);
        }
    }
    for key in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        command.env_remove(key);
    }
    command
}

/// One inventory and the command context in which it was obtained.
#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub inventory: Inventory,
    context: Option<Namespace>,
    ambient: [Option<OsString>; 3],
    production: bool,
    process: (u32, u32, Option<PathBuf>),
}
impl Observation {
    pub(crate) fn current(&self) -> bool {
        if self.ambient != ambient() || self.process != process_context() {
            return false;
        }
        match &self.context {
            Some(context) if context.current() => {
                !self.production
                    || tmux_command()
                        .is_ok_and(|command| Namespace::capture(&command, None) == *context)
            }
            None => tmux_command().is_err(),
            _ => false,
        }
    }
}
fn process_context() -> (u32, u32, Option<PathBuf>) {
    // SAFETY: these calls only read the process credentials.
    (
        unsafe { libc::getuid() },
        unsafe { libc::geteuid() },
        std::env::current_dir().ok(),
    )
}
fn ambient() -> [Option<OsString>; 3] {
    [
        std::env::var_os("TMUX"),
        std::env::var_os("TMUX_TMPDIR"),
        std::env::var_os("PATH"),
    ]
}
pub(crate) fn observe_inventory(channel: u64) -> Observation {
    #[cfg(test)]
    {
        tests::record_probe();
        tests::record_channel_probe(channel);
        if let Some((command, socket)) = tests::take_command() {
            return observe_command(command, socket, Duration::from_secs(3), false);
        }
    }
    let _ = channel;
    let observed_ambient = ambient();
    let process = process_context();
    match tmux_command() {
        Ok(command) => observe_command(command, None, Duration::from_secs(3), true),
        Err(error) => Observation {
            inventory: Inventory::ProbeFailed(error.to_string()),
            context: None,
            ambient: observed_ambient,
            production: true,
            process,
        },
    }
}
#[cfg(test)]
pub(crate) fn probe_session_inventory() -> Inventory {
    observe_inventory(0).inventory
}
#[cfg(test)]
fn probe_command(command: Command, socket: Option<PathBuf>, timeout: Duration) -> Inventory {
    observe_command(command, socket, timeout, false).inventory
}
fn observe_command(
    command: Command,
    socket: Option<PathBuf>,
    timeout: Duration,
    production: bool,
) -> Observation {
    let context = Namespace::capture(&command, socket);
    let process = process_context();
    let command = prepare(command);
    let inventory = match wait_for_tmux_output(command, timeout, "tmux session inventory") {
        Ok(output) if context.current() => classify_output(&output, &context, filesystem),
        Ok(_) => Inventory::ProbeFailed("namespace changed".into()),
        Err(error) => Inventory::ProbeFailed(error),
    };
    Observation {
        inventory,
        context: Some(context),
        ambient: ambient(),
        production,
        process,
    }
}

#[derive(Debug)]
enum SocketState {
    Missing,
    Stale,
    Other,
}
fn filesystem(path: &Path) -> SocketState {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => SocketState::Missing,
        Ok(meta) if meta.file_type().is_socket() && connection_refused(path) => SocketState::Stale,
        _ => SocketState::Other,
    }
}
fn connection_refused(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: the address is zero-filled, bounded, and the owned fd closes on every path.
    unsafe {
        let mut address: libc::sockaddr_un = std::mem::zeroed();
        if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
            return false;
        }
        address.sun_family = libc::AF_UNIX as _;
        for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
            *target = *byte as _;
        }
        let raw = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if raw < 0 {
            return false;
        }
        let fd = OwnedFd::from_raw_fd(raw);
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return false;
        }
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) < 0 {
            return false;
        }
        let result = libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as _,
        );
        result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECONNREFUSED)
    }
}
fn classify_output(
    output: &Output,
    context: &Namespace,
    check: impl FnOnce(&Path) -> SocketState,
) -> Inventory {
    let fail = || Inventory::ProbeFailed("incomplete or contradictory inventory".into());
    if !context.current() || !output.stderr.is_empty() && output.status.success() {
        return fail();
    }
    if output.status.success() {
        let Ok(text) = std::str::from_utf8(&output.stdout) else {
            return fail();
        };
        if !text.is_empty() && !text.ends_with('\n') {
            return fail();
        }
        let names: Vec<_> = text.split_terminator('\n').collect();
        if names.iter().any(|n| {
            n.trim().is_empty() || n.chars().any(char::is_control) || n.contains([':', '.'])
        }) {
            return fail();
        }
        if !context.current() {
            return fail();
        }
        return Inventory::Sessions(names.into_iter().map(str::to_owned).collect());
    }
    if output.status.code() != Some(1) || !output.stdout.is_empty() {
        return fail();
    }
    let Ok(stderr) = std::str::from_utf8(&output.stderr) else {
        return fail();
    };
    let line = stderr.strip_suffix('\n').unwrap_or(stderr);
    if line.contains(['\n', '\r']) {
        return fail();
    }
    let missing = line
        .strip_prefix("error connecting to ")
        .and_then(|s| s.strip_suffix(" (No such file or directory)"));
    let no_server = line.strip_prefix("no server running on ");
    let Some(reported) = missing.or(no_server) else {
        return fail();
    };
    let path = PathBuf::from(reported);
    if !context.matches(&path) {
        return fail();
    }
    let state = check(&path);
    if !context.current() {
        return fail();
    }
    match (missing.is_some(), state) {
        (true, SocketState::Missing) => Inventory::Empty(EmptyReason::MissingSocket(path)),
        (false, SocketState::Missing | SocketState::Stale) => {
            Inventory::Empty(EmptyReason::NoServer(path))
        }
        _ => fail(),
    }
}

#[cfg(test)]
mod real_tests;
#[cfg(test)]
#[path = "session_inventory/unit_tests.rs"]
pub(crate) mod tests;
