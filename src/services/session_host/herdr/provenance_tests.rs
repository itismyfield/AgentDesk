//! E7: the restore-off proof over scripted readings, and the OS reads on this platform.
use std::collections::BTreeMap;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::services::session_host::herdr::contract::{
    HerdrOutcome, HerdrTransport, ServerHello, ServerWitness, Witnessed,
};
use crate::services::session_host::herdr::model::{HerdrCall, HerdrEndpoint};
use crate::services::session_host::herdr::observe::{
    CANONICAL_CONFIG, RestoreResume, read_restore_resume_with,
};

const HOME: &str = "/srv/herdr-home";
const SOCKET: &str = "/tmp/h.sock";

/// A transport that only says hello and names servers, as E7 needs.
struct E7Only {
    hello: Result<ServerHello, RestoreUnverified>,
    last: Witnessed,
}

impl HerdrTransport for E7Only {
    fn call(&self, _call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        unreachable!("E7 sends no request")
    }

    fn call_with_witness(&self, _call: &HerdrCall, _expected: &ServerWitness) -> HerdrOutcome {
        unreachable!("E7 sends no request")
    }

    fn hello(&self) -> Result<ServerHello, RestoreUnverified> {
        self.hello.clone()
    }

    fn server_witness(&self) -> Witnessed {
        self.last.clone()
    }
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn started(seconds: u64) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin { seconds, micros: 0 },
        wall_clock: at(seconds),
    }
}

fn server(pid: u32, seconds: u64) -> ServerWitness {
    ServerWitness {
        socket: SOCKET.into(),
        pid,
        start: started(seconds).identity,
    }
}

fn hello(version: &str, connected_at: SystemTime) -> ServerHello {
    ServerHello {
        witness: server(77, 1_000),
        started: at(1_000),
        version: version.to_string(),
        connected_at,
    }
}

/// OS answers for one server; `starts` pops a value per read, the last one repeating.
#[derive(Clone)]
struct Scripted {
    starts: Vec<Result<ProcessStart, RestoreUnverified>>,
    env: BTreeMap<&'static str, Vec<String>>,
    config: Result<ConfigRead, RestoreUnverified>,
}

struct FakeOs(Mutex<Scripted>);

impl ServerProvenance for FakeOs {
    fn process_start(&self, _pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let mut os = self.0.lock().unwrap();
        if os.starts.len() > 1 {
            os.starts.remove(0)
        } else {
            os.starts[0]
        }
    }

    fn process_env(&self, _pid: u32, key: &str) -> Result<Vec<String>, RestoreUnverified> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .env
            .get(key)
            .cloned()
            .unwrap_or_default())
    }

    fn read_config(&self, _path: &Path) -> Result<ConfigRead, RestoreUnverified> {
        self.0.lock().unwrap().config.clone()
    }
}

fn bootstrapped() -> Scripted {
    Scripted {
        starts: vec![Ok(started(1_000))],
        env: BTreeMap::from([
            ("HERDR_CONFIG_PATH", vec![format!("{HOME}/config.toml")]),
            ("XDG_CONFIG_HOME", vec![format!("{HOME}/xdg")]),
        ]),
        config: Ok(ConfigRead {
            bytes: CANONICAL_CONFIG.to_vec(),
            modified: at(900),
        }),
    }
}

fn endpoint(home: Option<&str>) -> HerdrEndpoint {
    let endpoint = HerdrEndpoint::new("mac-mini", "pilot", Path::new(SOCKET), "adk").unwrap();
    match home {
        Some(home) => endpoint.with_herdr_home(Path::new(home)).unwrap(),
        None => endpoint,
    }
}

/// One E7 reading: the hello, then the last request-free connection's server.
type Seen = (Result<ServerHello, RestoreUnverified>, Witnessed);

fn reading(seen: Seen, os: Scripted, home: Option<&str>) -> RestoreResume {
    let transport = E7Only {
        hello: seen.0,
        last: seen.1,
    };
    read_restore_resume_with(&transport, &endpoint(home), &FakeOs(Mutex::new(os)))
}

// E7 reads Off only from one server's full provenance; every other case names why.
#[test]
fn e7_reads_off_only_from_a_bootstrapped_server_and_names_every_refusal() {
    use RestoreUnverified as Why;
    let good = || (Ok(hello("0.9.3", at(2_000))), Ok(server(77, 1_000)));
    let with = |change: fn(&mut Scripted)| {
        let mut os = bootstrapped();
        change(&mut os);
        os
    };
    let env = |key: &'static str, values: &[&str]| {
        let mut os = bootstrapped();
        os.env
            .insert(key, values.iter().map(|v| v.to_string()).collect());
        os
    };
    let config = |bytes: &[u8], modified| {
        let mut os = bootstrapped();
        os.config = Ok(ConfigRead {
            bytes: bytes.to_vec(),
            modified,
        });
        os
    };
    assert_eq!(
        reading(good(), bootstrapped(), Some(HOME)),
        RestoreResume::Off {
            witness: server(77, 1_000)
        }
    );
    let home_config = format!("{HOME}/config.toml");
    let last = |last: Witnessed| (good().0, last);
    let elsewhere = ServerHello {
        witness: ServerWitness {
            socket: "/tmp/other.sock".into(),
            ..server(77, 1_000)
        },
        ..hello("0.9.3", at(2_000))
    };
    type Case<'a> = (Seen, Scripted, Option<&'a str>, Why);
    let cases: Vec<Case> = vec![
        (
            (Err(Why::NoPeer), Ok(server(77, 1_000))),
            bootstrapped(),
            Some(HOME),
            Why::NoPeer,
        ),
        (
            (Ok(hello("0.9.0", at(2_000))), Ok(server(77, 1_000))),
            bootstrapped(),
            Some(HOME),
            Why::VersionNotVerified,
        ),
        (good(), bootstrapped(), None, Why::NotBootstrapped),
        (
            (Ok(elsewhere), Ok(server(77, 1_000))),
            bootstrapped(),
            Some(HOME),
            Why::NotBootstrapped,
        ),
        (
            good(),
            env("HERDR_CONFIG_PATH", &["/Users/u/.config/herdr/config.toml"]),
            Some(HOME),
            Why::NotBootstrapped,
        ),
        (
            good(),
            env("XDG_CONFIG_HOME", &[]),
            Some(HOME),
            Why::NotBootstrapped,
        ),
        (
            good(),
            env(
                "HERDR_CONFIG_PATH",
                &[&home_config, "/elsewhere/config.toml"],
            ),
            Some(HOME),
            Why::NotBootstrapped,
        ),
        (
            good(),
            with(|os| os.starts = vec![Ok(started(1_500))]),
            Some(HOME),
            Why::ProcessChanged,
        ),
        (
            (Ok(hello("0.9.3", at(999))), Ok(server(77, 1_000))),
            bootstrapped(),
            Some(HOME),
            Why::ProcessChanged,
        ),
        (
            good(),
            with(|os| os.starts = vec![Err(Why::ProcessUnreadable)]),
            Some(HOME),
            Why::ProcessUnreadable,
        ),
        (
            good(),
            with(|os| os.config = Err(Why::ConfigMissing)),
            Some(HOME),
            Why::ConfigMissing,
        ),
        (
            good(),
            config(
                b"[session]\nresume_agents_on_restore = false\n[[[broken\n",
                at(900),
            ),
            Some(HOME),
            Why::ConfigNotCanonical,
        ),
        (
            good(),
            config(
                b"[session]\nresume_agents_on_restore = \"false\"\n",
                at(900),
            ),
            Some(HOME),
            Why::ConfigNotCanonical,
        ),
        (
            good(),
            config(CANONICAL_CONFIG, at(999)),
            Some(HOME),
            Why::ConfigChangedSinceStart,
        ),
        (
            good(),
            config(CANONICAL_CONFIG, at(1_200)),
            Some(HOME),
            Why::ConfigChangedSinceStart,
        ),
        (
            last(Ok(server(78, 1_000))),
            bootstrapped(),
            Some(HOME),
            Why::ServerChanged,
        ),
        (
            last(Ok(server(77, 1_001))),
            bootstrapped(),
            Some(HOME),
            Why::ServerChanged,
        ),
        (
            last(Err(Why::ProcessUnreadable)),
            bootstrapped(),
            Some(HOME),
            Why::ProcessUnreadable,
        ),
    ];
    for (seen, os, home, why) in cases {
        let got = reading(seen, os, home);
        assert_eq!(got, RestoreResume::Unverified(why), "{why:?}");
        assert_eq!(got.admitted_witness(), None);
    }
}

// A config rewritten or replaced while it is read is never taken as read.
#[cfg(unix)]
#[test]
fn e7_config_read_refuses_a_file_changed_or_replaced_mid_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CANONICAL_CONFIG).unwrap();
    let bytes = read_config_between(&path, || {}).map(|read| read.bytes);
    assert_eq!(bytes, Ok(CANONICAL_CONFIG.to_vec()));
    let rewrite = || std::fs::write(&path, CANONICAL_CONFIG).unwrap();
    assert_eq!(
        read_config_between(&path, rewrite),
        Err(RestoreUnverified::ConfigChanged),
        "same bytes rewritten in place"
    );
    let replace = || {
        let fresh = dir.path().join("fresh.toml");
        std::fs::write(&fresh, CANONICAL_CONFIG).unwrap();
        std::fs::rename(&fresh, &path).unwrap();
    };
    assert_eq!(
        read_config_between(&path, replace),
        Err(RestoreUnverified::ConfigChanged),
        "another file renamed over the path"
    );
}

#[test]
fn e7_start_times_convert_from_each_platform_unit() {
    assert_eq!(
        macos_start(1_700_000_000, 250_000),
        at(1_700_000_000) + Duration::from_millis(250)
    );
    assert_eq!(
        linux_start(12_345, 100, 1_700_000_000),
        Some(at(1_700_000_123) + Duration::from_millis(450))
    );
    assert_eq!(linux_start(1, 0, 1), None);
}

// An argument shaped like an environment entry is never read as one.
#[test]
fn procargs2_environment_starts_after_the_exec_path_and_every_argument() {
    let mut raw = 2i32.to_ne_bytes().to_vec();
    raw.extend_from_slice(b"/bin/herdr\0\0\0\0herdr\0HERDR_CONFIG_PATH=/evil\0");
    raw.extend_from_slice(
        b"HERDR_CONFIG_PATH=/good/config.toml\0XDG_CONFIG_HOME=/good/xdg\0\0junk=1\0",
    );
    assert_eq!(
        procargs2_environ(&raw),
        Some(vec![
            "HERDR_CONFIG_PATH=/good/config.toml".to_string(),
            "XDG_CONFIG_HOME=/good/xdg".to_string(),
        ])
    );
    assert_eq!(procargs2_environ(&raw[..3]), None);
}

// Linux reads `/proc/<pid>/environ` empty for a zombie or a process still inside exec.
#[test]
fn linux_environ_never_reads_an_empty_environment_as_one_without_the_key() {
    assert_eq!(
        linux_environ(b"HERDR_CONFIG_PATH=/good/config.toml\0XDG_CONFIG_HOME=/good/xdg\0"),
        Ok(vec![
            "HERDR_CONFIG_PATH=/good/config.toml".to_string(),
            "XDG_CONFIG_HOME=/good/xdg".to_string(),
        ])
    );
    for empty in [&b""[..], b"\0", b"\0\0"] {
        assert_eq!(
            linux_environ(empty),
            Err(RestoreUnverified::ProcessUnreadable),
            "{empty:?}"
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
/// A child running this test binary, killed when dropped. macOS hides the environment of
/// platform binaries such as `/bin/sleep`, so the child is a binary like the Herdr server.
struct Child {
    process: std::process::Child,
    stderr: PathBuf,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Child {
    fn failure(&self, what: &str) -> String {
        let stderr = std::fs::read_to_string(&self.stderr).unwrap_or_default();
        format!("child {} {what}; stderr:\n{stderr}", self.process.id())
    }

    /// Fails with the child's own stderr unless it is still running.
    fn assert_alive(&mut self) {
        if let Some(status) = self.process.try_wait().unwrap() {
            panic!("{}", self.failure(&format!("exited {status}")));
        }
    }

    /// Waits until the child's test body wrote `ready`: its exec is over, so its
    /// environment is in place. Linux reads the environment empty before that.
    fn wait_ready(&mut self, ready: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !ready.exists() {
            self.assert_alive();
            assert!(
                std::time::Instant::now() < deadline,
                "{}",
                self.failure("never ready")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        self.assert_alive();
    }

    /// Kills the child and waits until it has exited without reaping it, so its pid
    /// names a zombie.
    #[allow(unsafe_code)]
    fn kill_unreaped(&mut self) {
        self.process.kill().unwrap();
        let pid = self.process.id() as libc::id_t;
        // SAFETY: `info` is a zeroed siginfo_t that waitid fills; WNOWAIT leaves the child.
        let waited = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        assert_eq!(waited, 0, "{}", self.failure("could not be waited for"));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Set in the child, naming the file its test body writes once it runs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const CHILD_FLAG: &str = "ADK_E7_PROVENANCE_CHILD";

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn set_modified(path: &Path, modified: SystemTime) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(modified).unwrap();
}

// On this platform the OS reads give E7 a real child's env, wall-clock start and config file.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn e7_os_reads_prove_off_for_a_real_bootstrapped_process_only() {
    if let Some(ready) = std::env::var_os(CHILD_FLAG) {
        std::fs::write(ready, b"").unwrap();
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home: PathBuf = std::fs::canonicalize(dir.path()).unwrap();
    let config = home.join("config.toml");
    std::fs::write(&config, CANONICAL_CONFIG).unwrap();
    set_modified(&config, SystemTime::now() - Duration::from_secs(60));
    let test = module_path!().split_once("::").unwrap().1;
    let test = format!("{test}::e7_os_reads_prove_off_for_a_real_bootstrapped_process_only");
    let spawn = |name: &str, env: &[(&str, PathBuf)]| {
        let ready = home.join(format!("{name}.ready"));
        let stderr = home.join(format!("{name}.stderr"));
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", &test, "--test-threads=1", "--nocapture"]);
        command.env_clear().env(CHILD_FLAG, &ready);
        command.envs(env.iter().map(|(key, value)| (key, value)));
        command.stdout(std::process::Stdio::null());
        command.stderr(std::fs::File::create(&stderr).unwrap());
        let mut child = Child {
            process: command.spawn().unwrap(),
            stderr,
        };
        child.wait_ready(&ready);
        child
    };
    let before = SystemTime::now();
    let mut server = spawn(
        "server",
        &[
            ("HERDR_CONFIG_PATH", config.clone()),
            ("XDG_CONFIG_HOME", home.join("xdg")),
        ],
    );
    let pid = server.process.id();
    let os = OsProvenance;
    let started = os.process_start(pid).unwrap().wall_clock;
    assert!(
        started + Duration::from_secs(2) > before
            && started < SystemTime::now() + Duration::from_secs(1),
        "start {started:?} is not near the spawn at {before:?}"
    );
    assert_eq!(
        os.process_env(pid, "HERDR_CONFIG_PATH"),
        Ok(vec![config.display().to_string()]),
        "{}",
        server.failure("env")
    );
    std::thread::sleep(Duration::from_millis(1_100));
    let home_str = home.to_str().unwrap();
    let named = |pid| {
        let start = os.process_start(pid)?;
        let witness = ServerWitness {
            socket: SOCKET.into(),
            pid,
            start: start.identity,
        };
        let hello = ServerHello {
            witness: witness.clone(),
            started: start.wall_clock,
            version: "0.9.3".into(),
            connected_at: SystemTime::now(),
        };
        Ok((hello, witness))
    };
    let read = |pid| match named(pid) {
        Ok((hello, witness)) => {
            let transport = E7Only {
                hello: Ok(hello),
                last: Ok(witness),
            };
            read_restore_resume_with(&transport, &endpoint(Some(home_str)), &os)
        }
        Err(why) => RestoreResume::Unverified(why),
    };
    assert!(matches!(read(pid), RestoreResume::Off { witness } if witness.pid == pid));

    std::fs::write(&config, CANONICAL_CONFIG).unwrap();
    assert_eq!(
        read(pid),
        RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart),
        "a config written after the server started proves nothing"
    );
    set_modified(&config, SystemTime::now() - Duration::from_secs(60));
    std::fs::write(home.join("other.toml"), b"x").unwrap();
    let elsewhere = spawn(
        "elsewhere",
        &[
            ("HERDR_CONFIG_PATH", home.join("other.toml")),
            ("XDG_CONFIG_HOME", home.join("xdg")),
        ],
    );
    assert_eq!(
        read(elsewhere.process.id()),
        RestoreResume::Unverified(RestoreUnverified::NotBootstrapped)
    );
    std::fs::remove_file(&config).unwrap();
    server.assert_alive();
    assert_eq!(
        read(pid),
        RestoreResume::Unverified(RestoreUnverified::ConfigMissing)
    );
    server.kill_unreaped();
    assert_eq!(
        os.process_env(pid, "HERDR_CONFIG_PATH"),
        Err(RestoreUnverified::ProcessUnreadable),
        "a dead server's environment is unreadable, never one without the key"
    );
    drop(server);
    assert_eq!(
        read(pid),
        RestoreResume::Unverified(RestoreUnverified::ProcessUnreadable)
    );
}
