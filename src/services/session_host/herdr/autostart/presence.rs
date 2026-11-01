//! Whether a server answers on a socket, judged apart from the next-restart policy: `Peer` and
//! `Changed` also come from live servers, so they never mean "no server". Only a socket that is
//! missing or refuses connections three times in a row, with no herdr server or launcher of this
//! user selecting it, is absent; anything unclear is unknown.

use std::path::Path;

use super::selector::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbsentKind {
    NoFile,
    /// A stale socket file: connections are refused.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unknown {
    SocketDirMissing,
    NotSocket,
    ForeignOwner,
    /// A connect error other than "no such file" or "refused", as its errno.
    Connect(i32),
    NotLocalFilesystem,
    ProcessTable(String),
    /// A herdr server or launcher of this user selects this socket, or may.
    HerdrProcess {
        pid: u32,
        role: Role,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Presence {
    Present,
    Absent(AbsentKind),
    Unknown(Unknown),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Socket,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub kind: EntryKind,
    pub uid: u32,
}

/// A process of this user whose executable is named `herdr`; `None` fields could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrProcess {
    pub pid: u32,
    pub argv: Option<Vec<String>>,
    pub environ: Option<Vec<String>>,
}

/// The OS reads behind one judgment, so tests can script them.
pub(crate) trait PresenceOs {
    fn euid(&self) -> u32;
    /// The path itself, without following a final symlink.
    fn entry(&self, path: &Path) -> std::io::Result<Entry>;
    fn is_dir(&self, path: &Path) -> bool;
    fn connect(&self, path: &Path) -> std::io::Result<()>;
    /// The wait between confirmations.
    fn pause(&self);
    /// This user's `herdr` processes other than this one.
    fn herdr_processes(&self) -> Result<Vec<HerdrProcess>, String>;
    fn local_filesystem(&self, dir: &Path) -> bool;
}

/// Readings taken this many times in a row, a pause apart, before a socket counts as absent: a
/// listening socket whose backlog is full refuses too on BSD.
const CONFIRMATIONS: usize = 3;

pub(crate) fn classify(socket: &Path, os: &dyn PresenceOs) -> Presence {
    let mut kind = AbsentKind::NoFile;
    for reading in 0..CONFIRMATIONS {
        if reading > 0 {
            os.pause();
        }
        match once(socket, os) {
            Ok(absent) => kind = absent,
            Err(other) => return other,
        }
    }
    let Some(dir) = socket.parent() else {
        return Presence::Unknown(Unknown::SocketDirMissing);
    };
    if !os.local_filesystem(dir) {
        return Presence::Unknown(Unknown::NotLocalFilesystem);
    }
    let processes = match os.herdr_processes() {
        Ok(processes) => processes,
        Err(why) => return Presence::Unknown(Unknown::ProcessTable(why)),
    };
    for process in processes {
        let selection = match (&process.argv, &process.environ) {
            (Some(argv), Some(environ)) => super::selector::select(argv, environ),
            _ => super::selector::Selection {
                role: Role::Unknown,
                socket: None,
            },
        };
        if selection.role == Role::Other {
            continue;
        }
        if selection
            .socket
            .as_deref()
            .is_none_or(|path| same_socket(path, socket))
        {
            return Presence::Unknown(Unknown::HerdrProcess {
                pid: process.pid,
                role: selection.role,
            });
        }
    }
    Presence::Absent(kind)
}

/// One reading: `Ok` is absent of that kind, `Err` the final present or unknown answer.
fn once(socket: &Path, os: &dyn PresenceOs) -> Result<AbsentKind, Presence> {
    let unknown = |why| Err(Presence::Unknown(why));
    match os.entry(socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match socket.parent().is_some_and(|dir| os.is_dir(dir)) {
                true => Ok(AbsentKind::NoFile),
                false => unknown(Unknown::SocketDirMissing),
            };
        }
        Err(error) => return unknown(Unknown::Connect(error.raw_os_error().unwrap_or(0))),
        Ok(entry) if entry.kind != EntryKind::Socket => return unknown(Unknown::NotSocket),
        Ok(entry) if entry.uid != os.euid() => return unknown(Unknown::ForeignOwner),
        Ok(_) => {}
    }
    match os.connect(socket) {
        Ok(()) => Err(Presence::Present),
        Err(error) => match error.raw_os_error() {
            Some(libc::ECONNREFUSED) => Ok(AbsentKind::Refused),
            Some(libc::ENOENT) => Ok(AbsentKind::NoFile),
            errno => unknown(Unknown::Connect(errno.unwrap_or(0))),
        },
    }
}

/// The same socket path, comparing resolved parent directories since the file may not exist.
fn same_socket(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| {
        let parent = path
            .parent()
            .map(|dir| std::fs::canonicalize(dir).unwrap_or(dir.into()));
        (parent, path.file_name().map(ToOwned::to_owned))
    };
    a == b || resolve(a) == resolve(b)
}

/// The JSON word health and the probe show for a presence.
pub(crate) fn describe(presence: &Presence) -> String {
    match presence {
        Presence::Present => "present".into(),
        Presence::Absent(AbsentKind::NoFile) => "absent:no_file".into(),
        Presence::Absent(AbsentKind::Refused) => "absent:refused".into(),
        Presence::Unknown(why) => format!(
            "unknown:{}",
            match why {
                Unknown::SocketDirMissing => "socket_dir_missing".into(),
                Unknown::NotSocket => "not_a_socket".into(),
                Unknown::ForeignOwner => "socket_owned_by_another_user".into(),
                Unknown::Connect(errno) => format!("connect_errno_{errno}"),
                Unknown::NotLocalFilesystem => "not_a_local_filesystem".into(),
                Unknown::ProcessTable(_) => "process_table_unreadable".into(),
                Unknown::HerdrProcess { pid, role } => {
                    format!("herdr_process_{pid}_{}", format!("{role:?}").to_lowercase())
                }
            }
        ),
    }
}

/// This host's reads. The process table comes from `ps`, then each `herdr`'s own argv and
/// environment from the kernel.
pub(crate) struct HostOs;

impl PresenceOs for HostOs {
    fn euid(&self) -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }
    fn entry(&self, path: &Path) -> std::io::Result<Entry> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let meta = std::fs::symlink_metadata(path)?;
        let kind = match meta.file_type().is_socket() {
            true => EntryKind::Socket,
            false => EntryKind::Other,
        };
        Ok(Entry {
            kind,
            uid: meta.uid(),
        })
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn connect(&self, path: &Path) -> std::io::Result<()> {
        std::os::unix::net::UnixStream::connect(path).map(drop)
    }
    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    fn herdr_processes(&self) -> Result<Vec<HerdrProcess>, String> {
        let output = std::process::Command::new("/bin/ps")
            .args(["-axo", "pid=,uid=,comm="])
            .env_clear()
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!("ps exited {}", output.status));
        }
        let (me, euid) = (std::process::id(), self.euid());
        use super::super::provenance::{process_argv, process_environ};
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid: u32 = fields.next()?.parse().ok()?;
                let uid: u32 = fields.next()?.parse().ok()?;
                let command = fields.collect::<Vec<_>>().join(" ");
                let named = Path::new(&command).file_name() == Some("herdr".as_ref());
                (named && uid == euid && pid != me).then(|| HerdrProcess {
                    pid,
                    argv: process_argv(pid).ok(),
                    environ: process_environ(pid).ok(),
                })
            })
            .collect())
    }
    fn local_filesystem(&self, dir: &Path) -> bool {
        super::candidate::filesystem(dir).is_some_and(|kind| kind == "apfs")
    }
}

#[cfg(test)]
#[path = "presence_tests.rs"]
mod tests;
