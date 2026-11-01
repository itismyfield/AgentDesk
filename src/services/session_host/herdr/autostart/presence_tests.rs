//! `classify` over scripted OS reads, and once over this host's real process table.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::{Error, ErrorKind};

use super::*;

const SOCKET: &str = "/h/.config/herdr/herdr.sock";
const EUID: u32 = 501;

struct Script {
    entry: RefCell<VecDeque<std::io::Result<Entry>>>,
    connect: RefCell<VecDeque<std::io::Result<()>>>,
    dir: bool,
    local: bool,
    processes: Result<Vec<HerdrProcess>, String>,
    pauses: Cell<usize>,
    scans: Cell<usize>,
}

fn socket() -> std::io::Result<Entry> {
    Ok(Entry {
        kind: EntryKind::Socket,
        uid: EUID,
    })
}

fn missing() -> std::io::Result<Entry> {
    Err(Error::from(ErrorKind::NotFound))
}

fn refused() -> std::io::Result<()> {
    Err(Error::from_raw_os_error(libc::ECONNREFUSED))
}

impl Script {
    fn new(entry: Vec<std::io::Result<Entry>>, connect: Vec<std::io::Result<()>>) -> Self {
        Self {
            entry: RefCell::new(entry.into()),
            connect: RefCell::new(connect.into()),
            dir: true,
            local: true,
            processes: Ok(Vec::new()),
            pauses: Cell::new(0),
            scans: Cell::new(0),
        }
    }

    fn with_processes(mut self, processes: Vec<HerdrProcess>) -> Self {
        self.processes = Ok(processes);
        self
    }

    fn run(&self) -> Presence {
        classify(Path::new(SOCKET), self)
    }
}

impl PresenceOs for Script {
    fn euid(&self) -> u32 {
        EUID
    }
    fn entry(&self, _: &Path) -> std::io::Result<Entry> {
        self.entry
            .borrow_mut()
            .pop_front()
            .expect("unscripted entry read")
    }
    fn is_dir(&self, _: &Path) -> bool {
        self.dir
    }
    fn connect(&self, _: &Path) -> std::io::Result<()> {
        self.connect
            .borrow_mut()
            .pop_front()
            .expect("unscripted connect")
    }
    fn pause(&self) {
        self.pauses.set(self.pauses.get() + 1);
    }
    fn herdr_processes(&self) -> Result<Vec<HerdrProcess>, String> {
        self.scans.set(self.scans.get() + 1);
        self.processes.clone()
    }
    fn local_filesystem(&self, _: &Path) -> bool {
        self.local
    }
}

fn process(pid: u32, argv: &[&str], env: &[&str]) -> HerdrProcess {
    HerdrProcess {
        pid,
        argv: Some(
            std::iter::once("herdr")
                .chain(argv.iter().copied())
                .map(String::from)
                .collect(),
        ),
        environ: Some(
            std::iter::once("HOME=/h")
                .chain(env.iter().copied())
                .map(String::from)
                .collect(),
        ),
    }
}

fn stale() -> Script {
    Script::new(
        vec![socket(), socket(), socket()],
        vec![refused(), refused(), refused()],
    )
}

#[test]
fn a_missing_or_refusing_socket_is_absent_only_after_three_readings() {
    let gone = Script::new(vec![missing(), missing(), missing()], vec![]);
    assert_eq!(gone.run(), Presence::Absent(AbsentKind::NoFile));
    assert_eq!(gone.pauses.get(), 2);
    let refusing = stale();
    assert_eq!(refusing.run(), Presence::Absent(AbsentKind::Refused));
    assert_eq!((refusing.pauses.get(), refusing.scans.get()), (2, 1));
}

#[test]
fn one_refusal_followed_by_an_answer_is_present() {
    let busy = Script::new(vec![socket(), socket()], vec![refused(), Ok(())]);
    assert_eq!(busy.run(), Presence::Present);
    let later = Script::new(
        vec![socket(), socket(), socket()],
        vec![refused(), refused(), Ok(())],
    );
    assert_eq!(later.run(), Presence::Present);
    assert_eq!(
        later.scans.get(),
        0,
        "a present socket needs no process scan"
    );
}

#[test]
fn unclear_socket_readings_are_unknown() {
    let denied = Script::new(
        vec![socket()],
        vec![Err(Error::from_raw_os_error(libc::EACCES))],
    );
    assert_eq!(
        denied.run(),
        Presence::Unknown(Unknown::Connect(libc::EACCES))
    );
    let file = Script::new(
        vec![Ok(Entry {
            kind: EntryKind::Other,
            uid: EUID,
        })],
        vec![],
    );
    assert_eq!(file.run(), Presence::Unknown(Unknown::NotSocket));
    let foreign = Script::new(
        vec![Ok(Entry {
            kind: EntryKind::Socket,
            uid: 0,
        })],
        vec![],
    );
    assert_eq!(foreign.run(), Presence::Unknown(Unknown::ForeignOwner));
    let mut no_dir = Script::new(vec![missing()], vec![]);
    no_dir.dir = false;
    assert_eq!(no_dir.run(), Presence::Unknown(Unknown::SocketDirMissing));
    let mut remote = stale();
    remote.local = false;
    assert_eq!(remote.run(), Presence::Unknown(Unknown::NotLocalFilesystem));
    let mut table = stale();
    table.processes = Err("ps failed".into());
    assert!(matches!(
        table.run(),
        Presence::Unknown(Unknown::ProcessTable(_))
    ));
}

#[test]
fn a_herdr_selecting_this_socket_keeps_it_unknown() {
    let starting = stale().with_processes(vec![process(41, &["server"], &[])]);
    assert_eq!(
        starting.run(),
        Presence::Unknown(Unknown::HerdrProcess {
            pid: 41,
            role: Role::Server
        })
    );
    let client = stale().with_processes(vec![process(42, &[], &[])]);
    assert_eq!(
        client.run(),
        Presence::Unknown(Unknown::HerdrProcess {
            pid: 42,
            role: Role::Launcher
        })
    );
    let unread = stale().with_processes(vec![HerdrProcess {
        pid: 43,
        argv: None,
        environ: None,
    }]);
    assert_eq!(
        unread.run(),
        Presence::Unknown(Unknown::HerdrProcess {
            pid: 43,
            role: Role::Unknown
        })
    );
}

#[test]
fn herdr_processes_of_other_sockets_do_not_block_the_default() {
    let others = stale().with_processes(vec![
        process(51, &["server"], &["HERDR_SESSION=work"]),
        process(
            52,
            &["--session", "work", "server"],
            &[&format!("HERDR_SOCKET_PATH={SOCKET}")],
        ),
        process(53, &["server"], &["HERDR_SOCKET_PATH=/a/.adk/herdr/h.sock"]),
        process(54, &["server"], &["XDG_CONFIG_HOME=/a/xdg"]),
        process(55, &["server", "stop"], &[]),
        process(56, &["session", "attach", "work"], &[]),
    ]);
    assert_eq!(others.run(), Presence::Absent(AbsentKind::Refused));
}

/// `/bin/sh` behind a `herdr` symlink (a copy is killed by code signing) run as `herdr server` and
/// blocked on stdin: the host scan must see a server selecting the default socket before it binds.
#[cfg(target_os = "macos")]
#[test]
fn the_host_process_table_finds_a_herdr_server_before_it_binds() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let config = home.join(".config/herdr");
    std::fs::create_dir_all(&config).unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("herdr");
    std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
    std::fs::write(dir.path().join("server"), "read line\n").unwrap();
    let mut child = std::process::Command::new(&fake)
        .arg("server")
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let socket = config.join("herdr.sock");
    let found = classify(&socket, &HostOs);
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let _ = child.wait();
    assert_eq!(
        found,
        Presence::Unknown(Unknown::HerdrProcess {
            pid: child.id(),
            role: Role::Server
        }),
        "the production scan read the stand-in's argv and HOME"
    );
    assert_eq!(
        classify(&socket, &HostOs),
        Presence::Absent(AbsentKind::NoFile),
        "once it exits the socket is absent"
    );
}
