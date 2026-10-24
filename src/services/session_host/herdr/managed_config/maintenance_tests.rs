use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::path::PersonalPaths;
use super::super::publication::STATE_FILE;
use super::*;
use crate::config::session_hosts::{
    BootSessionHosts, EndpointConfig, EndpointMode, SessionHostsConfig,
};
use crate::services::session_host::herdr::observe::CANONICAL_CONFIG;
use crate::services::session_host::herdr_socket_rig_tests::{
    NativeHerdrRig, native_registry_on_this_thread, native_server_child,
};

const UNSAFE: &[u8] = b"[session]\nresume_agents_on_restore = true\n";

struct Fixture {
    root: tempfile::TempDir,
    personal: PersonalPaths,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        fs::create_dir(&user).unwrap();
        Self {
            root,
            personal: PersonalPaths {
                home: user,
                xdg: None,
            },
        }
    }

    fn home(&self, name: &str, config: Option<&[u8]>) -> PathBuf {
        let home = self.root.path().join(name);
        fs::create_dir(&home).unwrap();
        if let Some(bytes) = config {
            fs::write(home.join("config.toml"), bytes).unwrap();
        }
        home
    }
}

fn endpoint(mode: EndpointMode, node: &str, home: &Path) -> EndpointConfig {
    EndpointConfig {
        mode,
        execution_node: node.into(),
        socket_path: home.join("herdr.sock"),
        herdr_home: home.to_path_buf(),
        herdr_session: "agentdesk".into(),
    }
}

fn boot(node: &str, endpoints: &[(&str, &Path)]) -> BootSessionHosts {
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some(node.into());
    for (key, home) in endpoints {
        config.session_hosts.herdr.endpoints.insert(
            (*key).into(),
            endpoint(EndpointMode::LegacyDedicated, node, home),
        );
    }
    BootSessionHosts::from_config(&config).unwrap()
}

type Snapshot = BTreeMap<PathBuf, (Vec<u8>, u64, i64, i64, u32)>;

fn snapshot(dir: &Path) -> Snapshot {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            (
                path,
                (
                    bytes,
                    meta.ino(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                    meta.mode(),
                ),
            )
        })
        .collect()
}

fn backups(home: &Path) -> Vec<Vec<u8>> {
    let mut found: Vec<_> = fs::read_dir(home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("config.toml.bak.")
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|path| fs::read(path).unwrap())
        .collect()
}

fn record(key: &str) -> Record {
    records().remove(key).unwrap()
}

#[test]
fn upkeep_repairs_missing_and_drifted_homes_and_leaves_canonical_bytes_untouched() {
    let f = Fixture::new();
    let missing = f.home("missing", None);
    let drifted = f.home("drifted", Some(UNSAFE));
    let canonical = f.home("canonical", Some(CANONICAL_CONFIG));
    let before = snapshot(&canonical);
    let boot = boot(
        "upkeep-node",
        &[
            ("upkeep-missing", &missing),
            ("upkeep-drifted", &drifted),
            ("upkeep-canonical", &canonical),
        ],
    );

    pass(&boot, &f.personal, false);

    for home in [&missing, &drifted] {
        assert_eq!(
            fs::read(home.join("config.toml")).unwrap(),
            CANONICAL_CONFIG
        );
    }
    assert!(backups(&missing).is_empty());
    assert_eq!(backups(&drifted), vec![UNSAFE.to_vec()]);
    assert_eq!(
        snapshot(&canonical),
        before,
        "canonical config is not rewritten"
    );
    let repaired = record("upkeep-drifted");
    assert_eq!((repaired.config, repaired.cause), ("prepared", None));
    assert!(repaired.backup.is_some() && repaired.earliest_start_after.is_some());
    let kept = record("upkeep-canonical");
    assert_eq!(
        (kept.config, kept.cause, kept.last_e7.as_str()),
        ("unchanged", None, "never")
    );
    let at = kept.observed_at;
    assert!(!kept.stale_at(at + chrono::Duration::seconds(89)));
    assert!(
        kept.stale_at(at + chrono::Duration::seconds(91)),
        "an upkeep that stopped reporting is not read as healthy"
    );
}

#[test]
fn drift_after_a_repair_is_repaired_again_on_the_next_tick() {
    let f = Fixture::new();
    let home = f.home("recurring", Some(UNSAFE));
    let boot = boot("recurring-node", &[("recurring", &home)]);
    pass(&boot, &f.personal, false);
    fs::write(home.join("config.toml"), b"edited by hand\n").unwrap();

    pass(&boot, &f.personal, false);

    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        CANONICAL_CONFIG
    );
    assert_eq!(
        backups(&home),
        vec![UNSAFE.to_vec(), b"edited by hand\n".to_vec()]
    );
    assert_eq!(record("recurring").config, "prepared");
}

#[test]
fn only_local_dedicated_endpoints_reach_the_writer_alongside_user_default() {
    let f = Fixture::new();
    let legacy = f.home("legacy", Some(UNSAFE));
    let private = f.personal.home.join(".config/herdr");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("config.toml"), b"personal\n").unwrap();
    let before = snapshot(&private);
    let mut hosts = SessionHostsConfig::default();
    let endpoints = &mut hosts.herdr.endpoints;
    endpoints.insert(
        "dedicated".into(),
        endpoint(EndpointMode::LegacyDedicated, "mixed-node", &legacy),
    );
    endpoints.insert(
        "user".into(),
        endpoint(EndpointMode::UserDefault, "mixed-node", &private),
    );

    let targets = ManagedHome::of(Some("mixed-node"), &hosts, &f.personal);

    assert_eq!(targets.len(), 1, "user-default endpoint was resolved");
    assert_eq!(targets[0].keys, vec!["dedicated".to_string()]);
    targets[0].home.as_ref().unwrap().ensure().unwrap();
    assert_eq!(
        fs::read(legacy.join("config.toml")).unwrap(),
        CANONICAL_CONFIG
    );
    assert_eq!(snapshot(&private), before);
}

#[test]
fn a_refused_home_never_blocks_another_dedicated_home() {
    let f = Fixture::new();
    let good = f.home("good", Some(UNSAFE));
    let private = f.personal.home.join(".config/herdr");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("config.toml"), b"personal\n").unwrap();
    let before = snapshot(&private);
    let boot = boot(
        "isolate-node",
        &[("isolate-good", &good), ("isolate-private", &private)],
    );

    pass(&boot, &f.personal, false);

    assert_eq!(
        fs::read(good.join("config.toml")).unwrap(),
        CANONICAL_CONFIG
    );
    assert_eq!(record("isolate-good").cause, None);
    let refused = record("isolate-private");
    assert_eq!(
        (refused.config, refused.cause),
        ("refused", Some("herdr_managed_home_unsafe"))
    );
    assert_eq!(snapshot(&private), before);
}

#[test]
fn a_personal_xdg_inside_a_managed_home_refuses_every_dedicated_home() {
    let f = Fixture::new();
    let first = f.home("first", Some(UNSAFE));
    let second = f.home("second", Some(UNSAFE));
    let personal = PersonalPaths {
        home: f.personal.home.clone(),
        xdg: Some(first.join("xdg")),
    };
    let before = snapshot(&second);
    let boot = boot(
        "xdg-node",
        &[("xdg-first", &first), ("xdg-second", &second)],
    );

    pass(&boot, &personal, false);

    assert_eq!(
        snapshot(&second),
        before,
        "personal boundary was not verifiable"
    );
    for key in ["xdg-first", "xdg-second"] {
        assert_eq!(record(key).cause, Some("herdr_managed_home_unsafe"));
    }
}

#[test]
fn a_busy_home_is_retried_without_a_new_health_reason() {
    let f = Fixture::new();
    let home = f.home("busy", Some(CANONICAL_CONFIG));
    let boot = boot("busy-node", &[("busy", &home)]);
    pass(&boot, &f.personal, false);
    let settled = record("busy");
    fs::write(home.join("config.toml"), UNSAFE).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(".agentdesk-config.lock"))
        .unwrap();
    fs::set_permissions(
        home.join(".agentdesk-config.lock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    lock.try_lock().unwrap();

    pass(&boot, &f.personal, false);

    let busy = record("busy");
    assert_eq!(busy.config, "busy");
    assert_eq!(busy.cause, None, "busy is a retry, not a cause");
    assert_eq!(busy.observed_at, settled.observed_at);
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), UNSAFE);
    lock.unlock().unwrap();
    pass(&boot, &f.personal, false);
    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        CANONICAL_CONFIG
    );
    assert_eq!(record("busy").config, "prepared");
}

#[test]
fn unreadable_records_and_unsafe_homes_get_their_own_stable_reasons() {
    let f = Fixture::new();
    let record_home = f.home("record", Some(CANONICAL_CONFIG));
    fs::write(record_home.join(STATE_FILE), "v9 unknown\n").unwrap();
    let open_home = f.home("open", Some(UNSAFE));
    fs::set_permissions(&open_home, fs::Permissions::from_mode(0o775)).unwrap();
    let before = (snapshot(&record_home), snapshot(&open_home));
    let boot = boot(
        "reason-node",
        &[("reason-record", &record_home), ("reason-open", &open_home)],
    );

    for _ in 0..2 {
        pass(&boot, &f.personal, false);
        assert_eq!(
            record("reason-record").cause,
            Some("herdr_config_record_unreadable")
        );
        assert_eq!(
            record("reason-open").cause,
            Some("herdr_managed_home_unsafe")
        );
    }
    assert_eq!((snapshot(&record_home), snapshot(&open_home)), before);
}

/// The running server read the old config: the upkeep repairs the file, E7 keeps refusing that
/// server on every later tick, and the server only ever receives read-only pings.
#[test]
fn a_running_server_on_the_old_config_stays_refused_and_is_never_restarted() {
    const TEST: &str = "services::session_host::herdr::managed_config::maintenance::tests::a_running_server_on_the_old_config_stays_refused_and_is_never_restarted";
    if native_server_child() {
        return;
    }
    let f = Fixture::new();
    let rig = NativeHerdrRig::start(TEST);
    let config = rig.home().join("config.toml");
    fs::write(&config, UNSAFE).unwrap();
    let mut hosts = crate::config::Config::default();
    hosts.cluster.instance_id = Some("test-node".into());
    hosts.session_hosts.herdr.endpoints.insert(
        "native".into(),
        EndpointConfig {
            mode: EndpointMode::LegacyDedicated,
            execution_node: "test-node".into(),
            socket_path: rig.socket(),
            herdr_home: rig.home(),
            herdr_session: "agentdesk".into(),
        },
    );
    let boot = BootSessionHosts::from_config(&hosts).unwrap();
    let _registry = native_registry_on_this_thread(&boot);

    let _ = pass(&boot, &f.personal, true);

    let repaired = record("native");
    assert_eq!(fs::read(&config).unwrap(), CANONICAL_CONFIG);
    assert_eq!(repaired.config, "prepared");
    assert_eq!(backups(&rig.home()), vec![UNSAFE.to_vec()]);
    let written = fs::metadata(&config).unwrap();
    let only_pings = || {
        let methods: Vec<_> = rig
            .requests()
            .iter()
            .map(|request| request["method"].as_str().unwrap_or("?").to_string())
            .collect();
        methods.iter().all(|method| method == "ping")
    };
    let mut first = Some(repaired.last_e7);
    let reread = || {
        first.take().unwrap_or_else(|| {
            let _ = pass(&boot, &f.personal, true);
            let again = record("native");
            assert_eq!(again.config, "unchanged", "a later pass rewrote the config");
            let meta = fs::metadata(&config).unwrap();
            assert_eq!(
                (meta.ino(), meta.modified().unwrap()),
                (written.ino(), written.modified().unwrap())
            );
            assert_eq!(backups(&rig.home()).len(), 1);
            assert!(only_pings(), "the server received more than pings");
            again.last_e7
        })
    };
    assert_eq!(
        past_no_peer(PEER_SETTLE, reread).unwrap(),
        "unverified:ConfigChangedSinceStart"
    );
    let _ = pass(&boot, &f.personal, true);
    let settled = record("native");
    assert_eq!(
        (settled.config, settled.cause),
        ("unchanged", Some("herdr_config_restart_required")),
        "the confirmed cause holds whatever this pass read"
    );
    assert_eq!(fs::metadata(&config).unwrap().ino(), written.ino());
    assert_eq!(backups(&rig.home()).len(), 1);
    assert!(!rig.requests().is_empty() && only_pings());
}

/// How long a rig test re-reads E7 while the peer read reports `NoPeer`.
pub(crate) const PEER_SETTLE: Duration = Duration::from_secs(10);

/// Re-reads while E7 says `NoPeer`, an intermittent peer read; any other reading ends the wait at
/// once, and a `NoPeer` that outlasts `within` is a failure, never a pass.
pub(crate) fn past_no_peer(
    within: Duration,
    mut read: impl FnMut() -> String,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + within;
    loop {
        let reading = read();
        if reading != "unverified:NoPeer" {
            return Ok(reading);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("E7 still read NoPeer after {within:?}"));
        }
        std::thread::yield_now();
    }
}

#[test]
fn a_no_peer_read_is_re_read_and_a_lasting_one_fails() {
    let mut readings = [
        "unverified:NoPeer",
        "unverified:NoPeer",
        "unverified:ConfigChangedSinceStart",
    ]
    .into_iter()
    .map(String::from);
    assert_eq!(
        past_no_peer(PEER_SETTLE, || readings.next().unwrap()),
        Ok("unverified:ConfigChangedSinceStart".into())
    );
    let mut reads = 0;
    let off = past_no_peer(PEER_SETTLE, || {
        reads += 1;
        "off".into()
    });
    assert_eq!(
        (off, reads),
        (Ok("off".into()), 1),
        "any other reading ends the wait at once"
    );
    let lasting = past_no_peer(Duration::from_millis(50), || "unverified:NoPeer".into());
    assert!(lasting.is_err(), "a lasting NoPeer passed: {lasting:?}");
}

fn notified_boot(node: &str, key: &str, home: &Path, channels: &[u64]) -> BootSessionHosts {
    let mut config = crate::config::Config::default();
    config.agents = serde_yaml::from_str(
        "- id: notice-fixture\n  name: Notice fixture\n  channels:\n    codex: {id: '7189042', runtime: tui}\n    claude: {id: '7189041', runtime: tui}\n",
    )
    .unwrap();
    config.cluster.instance_id = Some(node.into());
    let herdr = &mut config.session_hosts.herdr;
    herdr.endpoints.insert(
        key.into(),
        endpoint(EndpointMode::LegacyDedicated, node, home),
    );
    for channel in channels {
        herdr.channels.insert(channel.to_string(), key.into());
    }
    BootSessionHosts::from_config(&config).unwrap()
}

fn paused() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap()
}

/// `restart_required → NoPeer → stale → restart_required → Off`: the cause and its one notice hold
/// through every unverifiable reading and only a fresh `Off` ends the episode.
#[test]
fn an_unverifiable_server_keeps_the_restart_cause_and_its_one_notice_until_a_fresh_off() {
    let f = Fixture::new();
    let home = f.home("episode", Some(CANONICAL_CONFIG));
    let boot = notified_boot("episode-node", "episode", &home, &[7_189_042, 7_189_041]);
    let sent: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
    let _send = OperatorNotice::send_on_this_thread({
        let sent = sent.clone();
        move |channel, _| {
            sent.lock().unwrap().push(channel);
            Ok(())
        }
    });
    let witness = crate::services::session_host::herdr::contract::ServerWitness {
        socket: home.join("herdr.sock"),
        pid: 1,
        start: crate::services::session_host::herdr::provenance::StartIdentity::LinuxTicks(1),
    };
    let restart = RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart);
    let unverified = RestoreResume::Unverified(RestoreUnverified::NoPeer);
    let off = RestoreResume::Off { witness };
    let sends = || sent.lock().unwrap().clone();
    // Receipts under this test's own root: the process env root is shared with other tests.
    let receipts = f.root.path().join("receipts");
    paused().block_on(async {
        let tick = |e7: &RestoreResume| {
            let mut faults = pass_with(&boot, &f.personal, &|_| Some(e7.clone()));
            for notice in faults.iter_mut().filter_map(|fault| fault.notice.as_mut()) {
                notice.root = Some(receipts.clone());
            }
            notify(faults)
        };
        tick(&restart).await;
        assert_eq!(record("episode").cause, Some(RESTART));
        tokio::time::advance(Duration::from_secs(61)).await;
        tick(&restart).await;
        assert_eq!(
            sends(),
            vec![7_189_041],
            "one notice to the lowest mapped channel: {:?}",
            record("episode")
        );
        assert_eq!(record("episode").notice, "sent");

        tokio::time::advance(Duration::from_secs(30)).await;
        tick(&unverified).await;
        let unsure = record("episode");
        assert_eq!(
            (unsure.cause, unsure.last_e7.as_str()),
            (Some(RESTART), "unverified:NoPeer")
        );
        assert!(unsure.stale_at(unsure.observed_at + chrono::Duration::seconds(91)));
        tokio::time::advance(Duration::from_secs(120)).await;
        tick(&restart).await;
        assert_eq!(record("episode").cause, Some(RESTART));
        assert_eq!(
            sends().len(),
            1,
            "uncertainty and staleness are not a new episode"
        );

        tick(&off).await;
        let recovered = record("episode");
        assert_eq!((recovered.cause, recovered.notice), (None, "none"));
    });
    assert_eq!(sends().len(), 1);
}

/// A cause with no BOOT channel mapped to its home stays pending: nothing is posted or counted.
#[test]
fn a_home_with_no_mapped_channel_keeps_its_notice_pending() {
    let f = Fixture::new();
    let home = f.home("unmapped", Some(CANONICAL_CONFIG));
    let boot = boot("unmapped-node", &[("unmapped", &home)]);
    let posted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _send = OperatorNotice::send_on_this_thread({
        let posted = posted.clone();
        move |_, _| {
            posted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    });
    let restart = RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart);
    paused().block_on(async {
        for _ in 0..3 {
            notify(pass_with(&boot, &f.personal, &|_| Some(restart.clone()))).await;
            tokio::time::advance(Duration::from_secs(61)).await;
        }
    });
    let unmapped = record("unmapped");
    assert_eq!(
        (unmapped.cause, unmapped.notice),
        (Some(RESTART), "pending")
    );
    assert_eq!(posted.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// One process starts one upkeep task, and that task runs a pass on its first tick.
#[test]
fn the_process_starts_one_upkeep_task() {
    const TEST: &str = "services::session_host::herdr::managed_config::maintenance::tests::the_process_starts_one_upkeep_task";
    let Some(user) = std::env::var_os("ADK_UPKEEP_TASK_USER") else {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("ADK_UPKEEP_TASK_USER", root.path())
            .env("HOME", root.path())
            .env("AGENTDESK_ROOT_DIR", root.path().join("runtime"))
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let home = PathBuf::from(user).join("managed");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("config.toml"), UNSAFE).unwrap();
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some("task-node".into());
    config.session_hosts.herdr.endpoints.insert(
        "task".into(),
        endpoint(EndpointMode::LegacyDedicated, "task-node", &home),
    );
    let candidate = BootSessionHosts::from_config(&config).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let selection = crate::bootstrap::prepare_session_hosts_boot(candidate, None).await;
        crate::config::session_hosts::install_selected(selection);
        assert!(spawn(), "the first start runs the task");
        assert!(!spawn(), "a second start adds no task");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !records().contains_key("task") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the task never ran a pass"
            );
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        CANONICAL_CONFIG
    );
}
