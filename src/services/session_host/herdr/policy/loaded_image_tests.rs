use super::super::super::super::contract::{ServerHello, ServerWitness};
use super::super::super::super::provenance::{self, PeerToken, ProcessStart, StartIdentity};
use super::super::super::PeerSource;
use super::super::SocketPeerSource;
use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, SystemTime};

const ROLE: &str = "HERDR_LOADED_IMAGE_ROLE";
const SOCKET: &str = "HERDR_LOADED_IMAGE_SOCKET";
const DECOY: &str = "HERDR_LOADED_IMAGE_DECOY";
const NEXT: &str = "HERDR_LOADED_IMAGE_NEXT";
const READY: &str = "HERDR_LOADED_IMAGE_READY";
/// The listener's copy without close-on-exec, so an exec'd image keeps serving it.
const LISTENER_FD: i32 = 100;
const CPU_TYPE_X86_64: i32 = 0x0100_0007;

/// The server role a re-executed copy of this binary plays; a plain run does nothing.
#[test]
fn herdr_loaded_image_macos_child_role() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let listener = if role == "serve-fd" {
        // SAFETY: the previous image left its listener open at LISTENER_FD across exec.
        unsafe { UnixListener::from_raw_fd(LISTENER_FD) }
    } else {
        let listener = UnixListener::bind(std::env::var(SOCKET).unwrap()).unwrap();
        // SAFETY: dup2 onto a fixed unused descriptor; dup2 leaves the copy without FD_CLOEXEC.
        assert_eq!(
            unsafe { libc::dup2(listener.as_raw_fd(), LISTENER_FD) },
            LISTENER_FD
        );
        listener
    };
    if role == "listen-decoy" {
        let decoy = std::fs::File::open(std::env::var(DECOY).unwrap()).unwrap();
        // SAFETY: a private read-only mapping of an open file that this process never unmaps.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                16 * 1024,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                decoy.as_raw_fd(),
                0,
            )
        };
        assert_ne!(mapped, libc::MAP_FAILED);
    }
    if role == "listen-exec" {
        std::thread::spawn(exec_on_signal);
    }
    println!("{READY}");
    std::io::stdout().flush().unwrap();
    serve(listener);
}

/// Keeps each connection open until the client closes it, as a server awaiting a request does;
/// the peer-token and peer-pid reads need the connection still attached.
fn serve(listener: UnixListener) -> ! {
    loop {
        if let Ok((mut stream, _)) = listener.accept() {
            std::thread::spawn(move || std::io::copy(&mut stream, &mut std::io::sink()));
        }
    }
}

/// Waits for one byte on stdin, then execs the next copy as the same libtest role, keeping fds.
fn exec_on_signal() {
    let mut byte = [0u8; 1];
    std::io::stdin().read_exact(&mut byte).unwrap();
    let cstring = |text: String| std::ffi::CString::new(text).unwrap();
    let program = cstring(std::env::var(NEXT).unwrap());
    let args: Vec<_> = std::env::args().map(cstring).collect();
    let env: Vec<_> = std::env::vars()
        .filter(|(key, _)| key != ROLE)
        .map(|(key, value)| format!("{key}={value}"))
        .chain([format!("{ROLE}=serve-fd")])
        .map(cstring)
        .collect();
    let pointers = |list: &[std::ffi::CString]| {
        let mut pointers: Vec<_> = list.iter().map(|item| item.as_ptr()).collect();
        pointers.push(std::ptr::null());
        pointers
    };
    let (argv, envp) = (pointers(&args), pointers(&env));
    // SAFETY: both arrays are NUL-terminated and outlive the call; execve returns only on error.
    unsafe { libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    panic!("execve failed: {}", std::io::Error::last_os_error());
}

fn child_role_name() -> String {
    let module = module_path!().split_once("::").unwrap().1;
    format!("{module}::herdr_loaded_image_macos_child_role")
}

/// A copy of this binary serving one socket from its own temporary directory.
struct Server {
    child: Child,
    out: Option<BufReader<ChildStdout>>,
    root: tempfile::TempDir,
    binary: PathBuf,
    socket: PathBuf,
}

impl Server {
    /// Copies this binary to `herdr`, lets `prepare` add files, waits `delay`, then spawns it.
    fn start(role: &str, prepare: impl FnOnce(&Path), delay: Option<Duration>) -> Self {
        Self::launch(role, prepare, delay, false)
    }

    /// Like `start`, but execs `herdr` through the symlink `herdr-link`.
    fn start_linked(role: &str) -> Self {
        Self::launch(role, |_| {}, None, true)
    }

    fn launch(
        role: &str,
        prepare: impl FnOnce(&Path),
        delay: Option<Duration>,
        linked: bool,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("herdr");
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        prepare(root.path());
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        let socket = root.path().join("s.sock");
        let program = if linked {
            let link = root.path().join("herdr-link");
            std::os::unix::fs::symlink(&binary, &link).unwrap();
            link
        } else {
            binary.clone()
        };
        let mut child = Command::new(&program)
            .args(["--exact", &child_role_name(), "--nocapture"])
            .args(["--test-threads=1"])
            .env(ROLE, role)
            .env(SOCKET, &socket)
            .env(DECOY, root.path().join("decoy"))
            .env(NEXT, root.path().join("herdr-next"))
            .env("HOME", root.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HERDR_CONFIG_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = Some(BufReader::new(child.stdout.take().unwrap()));
        let mut server = Self {
            child,
            out,
            root,
            binary,
            socket,
        };
        server.wait_ready();
        server
    }

    fn wait_ready(&mut self) {
        wait_ready(self.out.as_mut().unwrap());
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn source(&self) -> SocketPeerSource {
        let (hello, witness, generation) = (
            self.socket.clone(),
            self.socket.clone(),
            self.socket.clone(),
        );
        SocketPeerSource::new(
            self.socket.clone(),
            move || read_hello(&hello),
            move || read_witness(&witness),
        )
        .with_generation(move || read_generation(&generation))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_ready(out: &mut BufReader<ChildStdout>) {
    let mut line = String::new();
    loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "child exited");
        if line.contains(READY) {
            return;
        }
    }
}

fn read_hello(socket: &Path) -> Result<ServerHello, Unverified> {
    let stream = UnixStream::connect(socket).map_err(|_| Unverified::Peer)?;
    let connected_at = SystemTime::now();
    let (pid, start) = provenance::socket_peer(&stream).map_err(|_| Unverified::Peer)?;
    Ok(ServerHello {
        witness: ServerWitness {
            socket: socket.into(),
            pid,
            start: start.identity,
        },
        started: start.wall_clock,
        version: "0.9.3".into(),
        connected_at,
    })
}

fn read_witness(socket: &Path) -> Result<ServerWitness, Unverified> {
    Ok(read_hello(socket)?.witness)
}

/// A refused connection reads as Peer and a missing token as Changed, so failures stay apart.
fn read_generation(socket: &Path) -> Result<PeerToken, Unverified> {
    let stream = UnixStream::connect(socket).map_err(|_| Unverified::Peer)?;
    provenance::socket_peer_token(&stream).ok_or(Unverified::Changed)
}

fn dev_ino(path: &Path) -> (u64, u64) {
    let metadata = std::fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

/// Every vnode-backed mapping's inode, in address order.
fn mapped_inodes(pid: u32) -> Vec<u64> {
    let (mut address, mut inodes) = (0u64, Vec::new());
    while let Ok(region) = pid_info::<RegionWithPath>(pid, PROC_PIDREGIONPATHINFO, address) {
        if region.vip.stat.ino != 0 {
            inodes.push(region.vip.stat.ino);
        }
        match region.info.address.checked_add(region.info.size) {
            Some(next) if next > address => address = next,
            _ => break,
        }
    }
    inodes
}

fn start_at(seconds: u64, micros: u64) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin { seconds, micros },
        wall_clock: SystemTime::UNIX_EPOCH,
    }
}

/// A native arm64 thin APFS mapping changed `changed` after the epoch.
fn mapped(changed: (i64, i64)) -> Mapped {
    Mapped {
        cputype: CPU_TYPE_ARM64,
        offset: 0,
        protection: 5,
        path: b"/opt/herdr/bin/herdr".to_vec(),
        text_path: b"/opt/herdr/bin/herdr".to_vec(),
        dev: 7,
        ino: 11,
        changed,
        filesystem: b"apfs".to_vec(),
    }
}

#[test]
fn herdr_loaded_image_macos_self_matches_main_header_region() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("self.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let pid = std::process::id();
    let token = read_generation(&socket).unwrap();
    assert_eq!(token.pid, pid);
    let start = provenance::process_start(pid).unwrap();
    let image = for_process(pid, &start, Some(&token)).unwrap();
    let exe = dev_ino(&std::env::current_exe().unwrap());
    assert_eq!(image, fixture(exe), "self_loaded_image_is_current_exe");
    unsafe extern "C" {
        safe static _mh_execute_header: u8;
    }
    let header_address = std::ptr::addr_of!(_mh_execute_header) as u64;
    let header: RegionWithPath = pid_info(pid, PROC_PIDREGIONPATHINFO, header_address).unwrap();
    assert_eq!(
        (header.vip.stat.dev as i32 as u64, header.vip.stat.ino),
        exe
    );
    assert_eq!(
        main_region(pid).unwrap().info.address,
        header.info.address,
        "main_region_is_the_mach_header_region"
    );
}

#[test]
fn herdr_loaded_image_macos_decoy_mapping_never_selected() {
    let server = Server::start(
        "listen-decoy",
        |root| {
            std::fs::copy(std::env::current_exe().unwrap(), root.join("decoy")).unwrap();
        },
        None,
    );
    let decoy = dev_ino(&server.root.path().join("decoy"));
    assert!(
        mapped_inodes(server.pid()).contains(&decoy.1),
        "decoy mapping is present"
    );
    let peer = server.source().snapshot().unwrap();
    assert_eq!(peer.image.dev_ino(), dev_ino(&server.binary));
    assert_ne!(peer.image.dev_ino(), decoy);
}

#[test]
fn herdr_loaded_image_macos_install_then_immediate_start_is_verified() {
    let server = Server::start("listen", |_| {}, None);
    let peer = server.source().snapshot().unwrap();
    assert_eq!(peer.image.dev_ino(), dev_ino(&server.binary));
}

#[test]
fn herdr_loaded_image_macos_install_then_delayed_start_is_verified() {
    let server = Server::start("listen", |_| {}, Some(Duration::from_millis(10)));
    let peer = server.source().snapshot().unwrap();
    assert_eq!(peer.image.dev_ino(), dev_ino(&server.binary));
}

#[test]
fn herdr_loaded_image_macos_same_inode_overwrite_after_start_is_refused() {
    let mut server = Server::start("listen", |_| {}, None);
    let before = dev_ino(&server.binary);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&server.binary)
        .unwrap();
    let last = file.metadata().unwrap().len() - 1;
    let mut byte = [0u8; 1];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut byte, last).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, &byte, last).unwrap();
    drop(file);
    assert_eq!(dev_ino(&server.binary), before, "same inode");
    assert!(server.alive(), "the overwritten server keeps running");
    assert_eq!(
        server.source().snapshot().map(|_| ()),
        Err(Unverified::ImageChangedSinceStart)
    );
}

#[test]
fn herdr_loaded_image_macos_change_after_start_stays_refused_later() {
    let start = start_at(1_000, 200_000);
    let started = 1_000_200_000_000i64;
    let after = mapped((started / 1_000_000_000, started % 1_000_000_000 + 120_000));
    // The judgment uses only the fixed start, never the current time.
    for _ in 0..2 {
        assert_eq!(
            judge(&after, &start),
            Err(Unverified::ImageChangedSinceStart)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = mapped((1_000, 200_000_000 - 3_000));
    assert_eq!(judge(&before, &start), Ok(fixture((7, 11))));
}

#[test]
fn herdr_loaded_image_macos_hfs_second_resolution_is_never_verified() {
    // HFS+ keeps whole seconds: start 10.2s, a change at 10.3s reads back as 10.0s.
    let start = start_at(10, 200_000);
    for filesystem in [&b"hfs"[..], b"msdos", b"", b"apfsx"] {
        let mut observed = mapped((10, 0));
        observed.filesystem = filesystem.to_vec();
        assert_eq!(
            judge(&observed, &start),
            Err(Unverified::ImageFilesystemUnsupported),
            "{filesystem:?}"
        );
    }
}

#[test]
fn herdr_loaded_image_macos_same_microsecond_change_is_ambiguous() {
    let start = start_at(10, 200_000);
    let bound = 200_000_000;
    let at = |nanos: i64| judge(&mapped((10, bound + nanos)), &start);
    assert_eq!(at(0), Ok(fixture((7, 11))));
    assert_eq!(at(1), Err(Unverified::ImageTimeAmbiguous));
    assert_eq!(at(999), Err(Unverified::ImageTimeAmbiguous));
    assert_eq!(at(1_000), Err(Unverified::ImageChangedSinceStart));
}

#[test]
fn herdr_loaded_image_macos_rename_replacement_is_refused() {
    let server = Server::start("listen", |_| {}, None);
    let replacement = server.root.path().join("herdr.new");
    std::fs::copy(std::env::current_exe().unwrap(), &replacement).unwrap();
    std::fs::rename(&replacement, &server.binary).unwrap();
    // Unlinking the replaced inode changes its link count, so its change time follows the start.
    assert_eq!(
        server.source().snapshot().map(|_| ()),
        Err(Unverified::ImageChangedSinceStart)
    );
}

#[test]
fn herdr_loaded_image_macos_retargeted_launch_path_is_refused() {
    let server = Server::start_linked("listen");
    let link = server.root.path().join("herdr-link");
    assert_eq!(
        provenance::process_exec_path(server.pid()).map(PathBuf::from),
        Some(link.clone()),
        "the checker path is the path the server was exec'd through"
    );
    let changed = |path: &Path| {
        let metadata = std::fs::metadata(path).unwrap();
        (metadata.ctime(), metadata.ctime_nsec())
    };
    let running = changed(&server.binary);
    let other = server.root.path().join("herdr-other");
    std::fs::copy(std::env::current_exe().unwrap(), &other).unwrap();
    let staged = server.root.path().join("herdr-link.next");
    std::os::unix::fs::symlink(&other, &staged).unwrap();
    std::fs::rename(&staged, &link).unwrap();
    assert_eq!(
        changed(&server.binary),
        running,
        "the mapped inode is untouched"
    );
    assert_eq!(
        server.source().snapshot().map(|_| ()),
        Err(Unverified::Identity)
    );
}

#[test]
fn herdr_loaded_image_macos_exec_between_image_and_closing_generation_is_changed() {
    let mut server = Server::start(
        "listen-exec",
        |root| {
            std::fs::copy(std::env::current_exe().unwrap(), root.join("herdr-next")).unwrap();
        },
        None,
    );
    let source = server.source();
    let mut stdin = server.child.stdin.take().unwrap();
    let mut out = server.out.take().unwrap();
    let pid = server.pid();
    let fired = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = fired.clone();
    // Exec after the token-fenced image reads and before the closing generation read.
    let step: Box<dyn FnMut()> = Box::new(move || {
        if flag.replace(true) {
            return;
        }
        stdin.write_all(b"x").unwrap();
        stdin.flush().unwrap();
        wait_ready(&mut out);
    });
    AFTER_IMAGE.with(|hook| *hook.borrow_mut() = Some(step));
    let result = source.snapshot().map(|_| ());
    AFTER_IMAGE.with(|hook| *hook.borrow_mut() = None);
    assert!(fired.get(), "exec barrier ran");
    assert_eq!(result, Err(Unverified::Changed));
    let after = read_generation(&server.socket).unwrap();
    assert_eq!(after.pid, pid, "same pid serves after exec");
    assert!(server.alive());
}

#[test]
fn herdr_loaded_image_macos_non_arm64_or_offset_layouts_are_unsupported() {
    // Rosetta is not assumed on the runner, so foreign layouts are injected observations.
    let start = start_at(1_000, 0);
    assert_eq!(judge(&mapped((900, 0)), &start), Ok(fixture((7, 11))));
    type Edit = (fn(&mut Mapped), Unverified);
    let edits: [Edit; 6] = [
        (
            |m| m.cputype = CPU_TYPE_X86_64,
            Unverified::ImageLayoutUnsupported,
        ),
        (|m| m.offset = 32_768, Unverified::ImageLayoutUnsupported),
        (|m| m.protection = 1, Unverified::ImageLayoutUnsupported),
        (|m| m.path.clear(), Unverified::ImageLayoutUnsupported),
        (|m| m.ino = 0, Unverified::ImageLayoutUnsupported),
        (|m| m.text_path = b"/other".to_vec(), Unverified::Identity),
    ];
    for (edit, expected) in edits {
        let mut observed = mapped((900, 0));
        edit(&mut observed);
        assert_eq!(judge(&observed, &start), Err(expected), "{observed:?}");
    }
    let pid = std::process::id();
    let arch: ArchInfo = pid_info(pid, PROC_PIDARCHINFO, 0).unwrap();
    assert_eq!(arch.cputype, CPU_TYPE_ARM64, "native arm64 runner");
    assert_eq!(
        pid_info::<[u32; 3]>(pid, PROC_PIDARCHINFO, 0).map(|_| ()),
        Err(Unverified::Identity),
        "short arch read"
    );
}

#[test]
fn herdr_loaded_image_macos_dead_pid_and_missing_generation_fail_closed() {
    let mut server = Server::start("listen", |_| {}, None);
    let pid = server.pid();
    let token = read_generation(&server.socket).unwrap();
    let start = provenance::process_start(pid).unwrap();
    assert!(for_process(pid, &start, Some(&token)).is_ok());
    assert_eq!(
        for_process(pid, &start, None).map(|_| ()),
        Err(Unverified::Changed)
    );
    let other = PeerToken {
        pid: pid + 1,
        ..token
    };
    assert_eq!(
        for_process(pid, &start, Some(&other)).map(|_| ()),
        Err(Unverified::Changed)
    );
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    assert!(for_process(pid, &start, Some(&token)).is_err());
    assert_eq!(text_path(&token), Err(Unverified::Changed), "stale token");
    assert!(pid_info::<ArchInfo>(pid, PROC_PIDARCHINFO, 0).is_err());
}

#[test]
fn herdr_loaded_image_macos_ffi_layout_matches_sdk() {
    use std::mem::{align_of, offset_of, size_of};
    assert_eq!((size_of::<RegionInfo>(), align_of::<RegionInfo>()), (96, 8));
    assert_eq!(
        (
            offset_of!(RegionInfo, protection),
            offset_of!(RegionInfo, offset),
            offset_of!(RegionInfo, address),
            offset_of!(RegionInfo, size),
        ),
        (0, 16, 80, 88)
    );
    assert_eq!(size_of::<VnodeStat>(), 136);
    assert_eq!(
        (
            offset_of!(VnodeStat, dev),
            offset_of!(VnodeStat, ino),
            offset_of!(VnodeStat, ctime),
            offset_of!(VnodeStat, ctimensec),
        ),
        (0, 8, 56, 64)
    );
    assert_eq!(size_of::<VnodeInfoPath>(), 1176);
    assert_eq!(
        (
            offset_of!(VnodeInfoPath, fsid),
            offset_of!(VnodeInfoPath, path)
        ),
        (144, 152)
    );
    assert_eq!(size_of::<RegionWithPath>(), 1272);
    assert_eq!(offset_of!(RegionWithPath, vip), 96);
    assert_eq!(size_of::<ArchInfo>(), 8);
    assert_eq!(size_of::<AuditToken>(), 32);
}
