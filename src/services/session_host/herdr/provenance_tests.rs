//! E7: the restore-off proof over scripted readings, and the OS reads on this platform.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::services::session_host::herdr::contract::{HerdrOutcome, HerdrTransport, ServerPeer};
use crate::services::session_host::herdr::model::{HerdrCall, HerdrEndpoint};
use crate::services::session_host::herdr::observe::{
    CANONICAL_CONFIG, RestoreResume, read_restore_resume_with,
};

const HOME: &str = "/srv/herdr-home";

/// A transport that only names its peer: each `server_peer` pops the next reading.
struct PeerOnly(Mutex<Vec<Result<ServerPeer, RestoreUnverified>>>);

impl HerdrTransport for PeerOnly {
    fn call(&self, _call: &HerdrCall) -> (HerdrOutcome, u64) {
        unreachable!("E7 sends no request")
    }

    fn call_on(&self, _call: &HerdrCall, _generation: u64) -> (HerdrOutcome, u64) {
        unreachable!("E7 sends no request")
    }

    fn server_peer(&self) -> Result<ServerPeer, RestoreUnverified> {
        let mut peers = self.0.lock().unwrap();
        if peers.len() > 1 {
            peers.remove(0)
        } else {
            peers[0].clone()
        }
    }
}

fn peer(pid: u32, version: &str, connected_at: SystemTime) -> ServerPeer {
    ServerPeer {
        generation: 4,
        pid,
        version: version.to_string(),
        connected_at,
    }
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// OS answers for one server; `starts` pops a value per read, the last one repeating.
#[derive(Clone)]
struct Scripted {
    starts: Vec<Result<SystemTime, RestoreUnverified>>,
    env: BTreeMap<&'static str, Vec<String>>,
    config: Result<ConfigRead, RestoreUnverified>,
}

struct FakeOs(Mutex<Scripted>);

impl ServerProvenance for FakeOs {
    fn process_start(&self, _pid: u32) -> Result<SystemTime, RestoreUnverified> {
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
        starts: vec![Ok(at(1_000))],
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
    let endpoint =
        HerdrEndpoint::new("mac-mini", "pilot", Path::new("/tmp/h.sock"), "adk").unwrap();
    match home {
        Some(home) => endpoint.with_herdr_home(Path::new(home)).unwrap(),
        None => endpoint,
    }
}

fn reading(
    peers: Vec<Result<ServerPeer, RestoreUnverified>>,
    os: Scripted,
    home: Option<&str>,
) -> RestoreResume {
    let transport = PeerOnly(Mutex::new(peers));
    read_restore_resume_with(&transport, &endpoint(home), &FakeOs(Mutex::new(os)))
}

// E7 reads Off only from the connected server's full provenance; every other case names why.
#[test]
fn e7_reads_off_only_from_a_bootstrapped_server_and_names_every_refusal() {
    use RestoreUnverified as Why;
    let good = || Ok(peer(77, "0.9.3", at(2_000)));
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
        reading(vec![good()], bootstrapped(), Some(HOME)),
        RestoreResume::Off { generation: 4 }
    );
    let home_config = format!("{HOME}/config.toml");
    let cases: Vec<(Vec<Result<ServerPeer, Why>>, Scripted, Option<&str>, Why)> = vec![
        (
            vec![Err(Why::NoPeer)],
            bootstrapped(),
            Some(HOME),
            Why::NoPeer,
        ),
        (
            vec![Ok(peer(77, "0.9.0", at(2_000)))],
            bootstrapped(),
            Some(HOME),
            Why::VersionNotVerified,
        ),
        (vec![good()], bootstrapped(), None, Why::NotBootstrapped),
        (
            vec![good()],
            env("HERDR_CONFIG_PATH", &["/Users/u/.config/herdr/config.toml"]),
            Some(HOME),
            Why::NotBootstrapped,
        ),
        (
            vec![good()],
            env("XDG_CONFIG_HOME", &[]),
            Some(HOME),
            Why::NotBootstrapped,
        ),
        (
            vec![good()],
            env(
                "HERDR_CONFIG_PATH",
                &[&home_config, "/elsewhere/config.toml"],
            ),
            Some(HOME),
            Why::NotBootstrapped,
        ),
        (
            vec![good()],
            with(|os| os.starts = vec![Ok(at(1_000)), Ok(at(1_500))]),
            Some(HOME),
            Why::ProcessChanged,
        ),
        (
            vec![Ok(peer(77, "0.9.3", at(999)))],
            bootstrapped(),
            Some(HOME),
            Why::ProcessChanged,
        ),
        (
            vec![good()],
            with(|os| os.starts = vec![Err(Why::ProcessUnreadable)]),
            Some(HOME),
            Why::ProcessUnreadable,
        ),
        (
            vec![good()],
            with(|os| os.config = Err(Why::ConfigMissing)),
            Some(HOME),
            Why::ConfigMissing,
        ),
        (
            vec![good()],
            config(
                b"[session]\nresume_agents_on_restore = false\n[[[broken\n",
                at(900),
            ),
            Some(HOME),
            Why::ConfigNotCanonical,
        ),
        (
            vec![good()],
            config(
                b"[session]\nresume_agents_on_restore = \"false\"\n",
                at(900),
            ),
            Some(HOME),
            Why::ConfigNotCanonical,
        ),
        (
            vec![good()],
            config(CANONICAL_CONFIG, at(999)),
            Some(HOME),
            Why::ConfigChangedSinceStart,
        ),
        (
            vec![good()],
            config(CANONICAL_CONFIG, at(1_200)),
            Some(HOME),
            Why::ConfigChangedSinceStart,
        ),
        (
            vec![
                good(),
                Ok(ServerPeer {
                    generation: 5,
                    ..peer(77, "0.9.3", at(2_000))
                }),
            ],
            bootstrapped(),
            Some(HOME),
            Why::Reconnected,
        ),
        (
            vec![good(), Err(Why::NoPeer)],
            bootstrapped(),
            Some(HOME),
            Why::Reconnected,
        ),
    ];
    for (peers, os, home, why) in cases {
        let got = reading(peers, os, home);
        assert_eq!(got, RestoreResume::Unverified(why), "{why:?}");
        assert_eq!(got.admitted_generation(), None);
    }
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

/// A child running this test binary, killed when dropped. macOS hides the environment of
/// platform binaries such as `/bin/sleep`, so the child is a binary like the Herdr server.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const CHILD_FLAG: &str = "ADK_E7_PROVENANCE_CHILD";

fn set_modified(path: &Path, modified: SystemTime) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(modified).unwrap();
}

// On this platform the OS reads give E7 a real child's env, wall-clock start and config file.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn e7_os_reads_prove_off_for_a_real_bootstrapped_process_only() {
    if std::env::var_os(CHILD_FLAG).is_some() {
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
    let spawn = |env: &[(&str, PathBuf)]| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", &test, "--test-threads=1"]);
        command.env_clear().env(CHILD_FLAG, "1");
        command.envs(env.iter().map(|(key, value)| (key, value)));
        command.stdout(std::process::Stdio::null());
        Child(command.spawn().unwrap())
    };
    let before = SystemTime::now();
    let server = spawn(&[
        ("HERDR_CONFIG_PATH", config.clone()),
        ("XDG_CONFIG_HOME", home.join("xdg")),
    ]);
    let pid = server.0.id();
    let os = OsProvenance;
    let started = os.process_start(pid).unwrap();
    assert!(
        started + Duration::from_secs(2) > before
            && started < SystemTime::now() + Duration::from_secs(1),
        "start {started:?} is not near the spawn at {before:?}"
    );
    assert_eq!(
        os.process_env(pid, "HERDR_CONFIG_PATH"),
        Ok(vec![config.display().to_string()])
    );
    std::thread::sleep(Duration::from_millis(1_100));
    let home_str = home.to_str().unwrap();
    let transport = |pid| PeerOnly(Mutex::new(vec![Ok(peer(pid, "0.9.3", SystemTime::now()))]));
    let read = |pid| read_restore_resume_with(&transport(pid), &endpoint(Some(home_str)), &os);
    assert_eq!(read(pid), RestoreResume::Off { generation: 4 });

    std::fs::write(&config, CANONICAL_CONFIG).unwrap();
    assert_eq!(
        read(pid),
        RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart),
        "a config written after the server started proves nothing"
    );
    set_modified(&config, SystemTime::now() - Duration::from_secs(60));
    std::fs::write(home.join("other.toml"), b"x").unwrap();
    let elsewhere = spawn(&[
        ("HERDR_CONFIG_PATH", home.join("other.toml")),
        ("XDG_CONFIG_HOME", home.join("xdg")),
    ]);
    assert_eq!(
        read(elsewhere.0.id()),
        RestoreResume::Unverified(RestoreUnverified::NotBootstrapped)
    );
    std::fs::remove_file(&config).unwrap();
    assert_eq!(
        read(pid),
        RestoreResume::Unverified(RestoreUnverified::ConfigMissing)
    );
    drop(server);
    assert_eq!(
        read(pid),
        RestoreResume::Unverified(RestoreUnverified::ProcessUnreadable)
    );
}
