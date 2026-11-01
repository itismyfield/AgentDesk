//! `classify` over scripted OS reads, and the `ps` rows the host scan keeps.

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
        process(57, &["--remote=mac-mini"], &[]),
        process(58, &["--machine=m1", "pane", "list"], &[]),
    ]);
    assert_eq!(others.run(), Presence::Absent(AbsentKind::Refused));
}

#[test]
fn the_process_table_keeps_unreadable_live_herdr_rows_and_drops_zombies() {
    let ps = "  10   501 S    herdr\n  11   501 Ss   herdr\n  12   501 Z    herdr\n  13   502 S    herdr\n  14   501 S    bash\n  15   501 S    herdr\n  16   501 R+   herdrx\n";
    // pid 15 is this process; pid 11 cannot be read, as when ps itself would print `(herdr)`.
    let rows = table(ps, 501, 15, |pid| match pid {
        11 => (None, None),
        _ => (
            Some(vec!["herdr".into(), "server".into()]),
            Some(vec!["HOME=/h".into()]),
        ),
    });
    assert_eq!(
        rows.iter().map(|row| row.pid).collect::<Vec<_>>(),
        vec![10, 11]
    );
    assert_eq!((&rows[1].argv, &rows[1].environ), (&None, &None));
    let blocked = stale().with_processes(rows);
    assert_eq!(
        blocked.run(),
        Presence::Unknown(Unknown::HerdrProcess {
            pid: 10,
            role: Role::Server
        }),
        "the first live row already selects the default socket"
    );
    let unread = stale().with_processes(table(ps, 501, 15, |_| (None, None)));
    assert_eq!(
        unread.run(),
        Presence::Unknown(Unknown::HerdrProcess {
            pid: 10,
            role: Role::Unknown
        })
    );
    let zombie_only =
        stale().with_processes(table("  12   501 Z    herdr\n", 501, 15, |_| (None, None)));
    assert_eq!(zombie_only.run(), Presence::Absent(AbsentKind::Refused));
}

/// A missing socket that starts answering after `answers_after` pauses, with one process table
/// per scan (the last one repeats).
struct Timeline {
    tables: Vec<Vec<HerdrProcess>>,
    answers_after: Option<usize>,
    pauses: Cell<usize>,
    scans: Cell<usize>,
}

impl Timeline {
    fn new(tables: Vec<Vec<HerdrProcess>>, answers_after: Option<usize>) -> Self {
        Self {
            tables,
            answers_after,
            pauses: Cell::new(0),
            scans: Cell::new(0),
        }
    }

    fn answering(&self) -> bool {
        self.answers_after
            .is_some_and(|after| self.pauses.get() >= after)
    }
}

impl PresenceOs for Timeline {
    fn euid(&self) -> u32 {
        EUID
    }
    fn entry(&self, _: &Path) -> std::io::Result<Entry> {
        if self.answering() {
            socket()
        } else {
            missing()
        }
    }
    fn is_dir(&self, _: &Path) -> bool {
        true
    }
    fn connect(&self, _: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn pause(&self) {
        self.pauses.set(self.pauses.get() + 1);
    }
    fn herdr_processes(&self) -> Result<Vec<HerdrProcess>, String> {
        let scan = self.scans.get();
        self.scans.set(scan + 1);
        Ok(self.tables[scan.min(self.tables.len() - 1)].clone())
    }
    fn local_filesystem(&self, _: &Path) -> bool {
        true
    }
}

#[test]
fn a_launcher_still_idle_after_the_grace_is_set_aside() {
    let idle = Timeline::new(vec![vec![process(42, &[], &[])]], None);
    let (presence, excused) = settle(Path::new(SOCKET), &idle, 5);
    assert_eq!(
        (presence, excused),
        (Presence::Absent(AbsentKind::NoFile), vec![42])
    );
    assert_eq!(
        idle.scans.get(),
        7,
        "first reading, the set-aside reading and five rounds"
    );
}

#[test]
fn a_launcher_that_starts_its_server_in_the_grace_leaves_nothing_to_start() {
    let starting = Timeline::new(vec![vec![process(42, &[], &[])]], Some(8));
    let (presence, _) = settle(Path::new(SOCKET), &starting, 30);
    assert_eq!(presence, Presence::Present);
}

#[test]
fn a_launcher_appearing_in_the_grace_refuses_this_attempt() {
    let first = vec![process(42, &[], &[])];
    let both = vec![
        process(42, &[], &[]),
        process(43, &["session", "attach", "default"], &[]),
    ];
    // 43 shows in one scan only: even a launcher that vanished again refuses this attempt.
    let crowded = Timeline::new(
        vec![first.clone(), first.clone(), first.clone(), both, first],
        None,
    );
    let (presence, excused) = settle(Path::new(SOCKET), &crowded, 30);
    assert_eq!(
        presence,
        Presence::Unknown(Unknown::HerdrProcess {
            pid: 43,
            role: Role::Launcher
        })
    );
    assert_eq!(excused, vec![42]);
}

#[test]
fn a_server_selecting_the_socket_gets_no_grace() {
    let binding = Timeline::new(vec![vec![process(41, &["server"], &[])]], None);
    let (presence, excused) = settle(Path::new(SOCKET), &binding, 30);
    assert_eq!(
        presence,
        Presence::Unknown(Unknown::HerdrProcess {
            pid: 41,
            role: Role::Server
        })
    );
    assert!(excused.is_empty());
    assert_eq!(binding.scans.get(), 1);
}
