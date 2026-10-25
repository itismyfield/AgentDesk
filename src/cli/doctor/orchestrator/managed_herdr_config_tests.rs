use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::*;
use crate::config::session_hosts::{EndpointConfig, EndpointMode, SessionHostsConfig};
use crate::services::session_host::managed_config::path::PersonalPaths;

const UNSAFE: &[u8] = b"[session]\nresume_agents_on_restore = true\n";
const CANONICAL: &[u8] = b"[session]\nresume_agents_on_restore = false\n";

struct Fixture {
    root: tempfile::TempDir,
    personal: PersonalPaths,
    hosts: SessionHostsConfig,
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
            hosts: SessionHostsConfig::default(),
        }
    }

    fn home(&mut self, key: &str, mode: EndpointMode, config: Option<&[u8]>) -> PathBuf {
        let home = self.root.path().join(key);
        fs::create_dir(&home).unwrap();
        if let Some(bytes) = config {
            fs::write(home.join("config.toml"), bytes).unwrap();
        }
        self.hosts.herdr.endpoints.insert(
            key.into(),
            EndpointConfig {
                mode,
                execution_node: "doctor-node".into(),
                socket_path: home.join("herdr.sock"),
                herdr_home: home.clone(),
                herdr_session: "agentdesk".into(),
            },
        );
        home
    }

    fn job(&self, body: &str) -> PathBuf {
        let path = self.root.path().join("com.agentdesk.herdr.plist");
        fs::write(&path, body).unwrap();
        path
    }

    fn checks(&self, job: &Path) -> Vec<Check> {
        checks_for(Some("doctor-node"), &self.hosts, &self.personal, job)
    }
}

type Snapshot = BTreeMap<PathBuf, (Vec<u8>, u64, i64, i64)>;

fn snapshot(dir: &Path) -> Snapshot {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            let bytes = fs::read(&path).unwrap_or_default();
            (path, (bytes, meta.ino(), meta.mtime(), meta.mtime_nsec()))
        })
        .collect()
}

fn find<'a>(checks: &'a [Check], id: &str) -> &'a Check {
    checks.iter().find(|check| check.id == id).unwrap()
}

#[test]
fn plain_doctor_names_each_home_problem_and_writes_nothing() {
    let mut f = Fixture::new();
    let drifted = f.home("drifted", EndpointMode::LegacyDedicated, Some(UNSAFE));
    let record = f.home("record", EndpointMode::LegacyDedicated, Some(CANONICAL));
    fs::write(record.join(".agentdesk-config-state"), "v2 committed 1 0\n").unwrap();
    let open = f.home("open", EndpointMode::LegacyDedicated, Some(UNSAFE));
    fs::set_permissions(&open, fs::Permissions::from_mode(0o775)).unwrap();
    let before: Vec<_> = [&drifted, &record, &open].map(|home| snapshot(home)).into();

    let checks = f.checks(&f.root.path().join("absent.plist"));

    let check = find(&checks, "herdr_managed_config");
    assert_eq!(check.status, CheckStatus::Warn);
    for part in [
        "config differs",
        "publication record unreadable",
        "home refused",
    ] {
        assert!(check.detail.contains(part), "{}", check.detail);
    }
    let guidance = check.guidance.as_deref().unwrap();
    assert!(guidance.contains("doctor --fix") && guidance.contains("never removed automatically"));
    let after: Vec<_> = [&drifted, &record, &open].map(|home| snapshot(home)).into();
    assert_eq!(after, before, "a plain doctor run wrote to a managed home");
}

#[test]
fn fix_repairs_through_the_upkeep_writer_and_repeats_without_a_write() {
    let mut f = Fixture::new();
    let drifted = f.home("drifted", EndpointMode::LegacyDedicated, Some(UNSAFE));
    let record = f.home("record", EndpointMode::LegacyDedicated, Some(CANONICAL));
    fs::write(record.join(".agentdesk-config-state"), "garbage\n").unwrap();
    let record_before = snapshot(&record);

    let first = fixes_for(Some("doctor-node"), &f.hosts, &f.personal);

    assert_eq!(fs::read(drifted.join("config.toml")).unwrap(), CANONICAL);
    let repaired = first
        .iter()
        .find(|a| a.detail.starts_with("drifted"))
        .unwrap();
    assert!(repaired.ok && !repaired.skipped);
    assert!(repaired.detail.contains("original kept at") && repaired.detail.contains("2.1s"));
    let refused = first
        .iter()
        .find(|a| a.detail.starts_with("record"))
        .unwrap();
    assert!(!refused.ok && refused.detail.contains("never removed automatically"));
    assert_eq!(snapshot(&record), record_before);
    let repaired = snapshot(&drifted);
    let second = fixes_for(Some("doctor-node"), &f.hosts, &f.personal);
    let again = second
        .iter()
        .find(|a| a.detail.starts_with("drifted"))
        .unwrap();
    assert!(
        again.detail.contains("already canonical"),
        "{}",
        again.detail
    );
    assert_eq!(snapshot(&drifted), repaired);
}

#[test]
fn user_default_endpoints_are_neither_inspected_nor_job_checked() {
    let mut f = Fixture::new();
    let private = f.personal.home.join(".config/herdr");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("config.toml"), b"personal\n").unwrap();
    f.hosts.herdr.endpoints.insert(
        "user".into(),
        EndpointConfig {
            mode: EndpointMode::UserDefault,
            execution_node: "doctor-node".into(),
            socket_path: private.join("herdr.sock"),
            herdr_home: private.clone(),
            herdr_session: "default".into(),
        },
    );
    let before = snapshot(&private);
    let job = f.job("not a plist");

    assert!(f.checks(&job).is_empty());
    assert!(fixes_for(Some("doctor-node"), &f.hosts, &f.personal).is_empty());
    assert_eq!(snapshot(&private), before);
}

#[cfg(target_os = "macos")]
fn plist(label: &str, env: &[(&str, &Path)]) -> String {
    let env: String = env
        .iter()
        .map(|(key, value)| format!("<key>{key}</key><string>{}</string>", value.display()))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>{label}</string>
<key>EnvironmentVariables</key><dict>{env}</dict></dict></plist>"#
    )
}

#[cfg(target_os = "macos")]
#[test]
fn launchd_job_drift_is_reported_from_its_definition_without_touching_it() {
    let mut f = Fixture::new();
    let home = f.home("managed", EndpointMode::LegacyDedicated, Some(CANONICAL));
    let wrong = f.root.path().join("elsewhere/config.toml");
    let job = f.job(&plist(
        "com.agentdesk.herdr",
        &[("HERDR_CONFIG_PATH", &wrong)],
    ));
    let job_before = fs::read(&job).unwrap();

    let checks = f.checks(&job);

    let check = find(&checks, "herdr_launchd_job");
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(check.detail.contains("HERDR_CONFIG_PATH") && check.detail.contains("XDG_CONFIG_HOME"));
    assert!(
        check
            .guidance
            .as_deref()
            .unwrap()
            .contains("never installs")
    );
    assert_eq!(fs::read(&job).unwrap(), job_before);

    let config = home.join("config.toml");
    let xdg = home.join("xdg");
    let job = f.job(&plist(
        "com.agentdesk.herdr",
        &[("HERDR_CONFIG_PATH", &config), ("XDG_CONFIG_HOME", &xdg)],
    ));
    assert_eq!(
        find(&f.checks(&job), "herdr_launchd_job").status,
        CheckStatus::Pass
    );
    let missing = find(
        &f.checks(&f.root.path().join("absent.plist")),
        "herdr_launchd_job",
    )
    .detail
    .clone();
    assert!(missing.contains("unreadable"), "{missing}");

    f.home("second", EndpointMode::LegacyDedicated, Some(CANONICAL));
    let ambiguous = f.checks(&job);
    let ambiguous = find(&ambiguous, "herdr_launchd_job");
    assert_eq!(ambiguous.status, CheckStatus::Warn);
    assert!(
        ambiguous.detail.contains("2 dedicated homes"),
        "{}",
        ambiguous.detail
    );
}
