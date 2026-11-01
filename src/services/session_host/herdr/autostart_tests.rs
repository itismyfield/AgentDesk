//! The refusals an absent default socket meets before any binary runs, through `assess` and the
//! `ensure` entry, in a temporary HOME; no herdr is involved.

use std::os::unix::fs::PermissionsExt;

use super::spawn::{Context, Ensured, ensure, ensure_at, scoped_context};
use super::*;

struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Home {
    fn new(config: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".config/herdr/sessions/work")).unwrap();
        if let Some(config) = config {
            std::fs::write(home.join(".config/herdr/config.toml"), config).unwrap();
        }
        Self { _dir: dir, home }
    }

    fn socket(&self) -> PathBuf {
        self.home.join(".config/herdr/herdr.sock")
    }

    /// No binary can be found: every install dir this host has is excluded.
    fn env(&self) -> Environment {
        Environment {
            home: Some(self.home.clone()),
            path: None,
            excluded: vec![
                self.home.clone(),
                "/opt/homebrew".into(),
                "/usr/local".into(),
            ],
        }
    }

    fn tree(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = walk(&self.home);
        paths.sort();
        paths
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            let mut below = if path.is_dir() {
                walk(&path)
            } else {
                Vec::new()
            };
            below.push(path);
            below
        })
        .collect()
}

const FALSE: &str = "[session]\nresume_agents_on_restore = false\n";

async fn refusal(home: &Home, socket: &Path, env: Environment) -> Value {
    let before = home.tree();
    let report = assess(socket, env).await;
    assert_eq!(home.tree(), before, "a dry run writes nothing: {report}");
    assert_eq!(report["eligible"], false, "{report}");
    assert!(
        report["action"].is_string(),
        "every refusal says what to do: {report}"
    );
    report
}

#[tokio::test]
async fn each_gate_refuses_with_its_own_reason() {
    let home = Home::new(Some(FALSE));
    let no_home = Environment {
        home: None,
        ..home.env()
    };
    let report = refusal(&home, &home.socket(), no_home).await;
    assert_eq!(report["reason"], "home_unavailable", "{report}");

    let named = home.home.join(".config/herdr/sessions/work/herdr.sock");
    let report = refusal(&home, &named, home.env()).await;
    assert_eq!(report["reason"], "socket_not_default", "{report}");

    let report = refusal(&home, &home.socket(), home.env()).await;
    assert_eq!(report["reason"], "binary_not_found", "{report}");
    assert!(report["rejected"].is_array(), "{report}");

    let dir = home.home.join(".config/herdr");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let report = refusal(&home, &home.socket(), home.env()).await;
    assert_eq!(report["reason"], "socket_dir_unsafe", "{report}");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn a_config_that_is_missing_or_not_a_file_is_refused() {
    let missing = Home::new(None);
    let report = refusal(&missing, &missing.socket(), missing.env()).await;
    assert_eq!(report["reason"], "config_resume_not_off", "{report}");
    assert!(
        report["action"]
            .as_str()
            .is_some_and(|a| a.contains("resume_agents_on_restore = false") && a.contains("retry")),
        "{report}"
    );
    let directory = Home::new(None);
    std::fs::create_dir(directory.home.join(".config/herdr/config.toml")).unwrap();
    let report = refusal(&directory, &directory.socket(), directory.env()).await;
    assert_eq!(report["reason"], "config_unreadable", "{report}");
}

fn context(home: &Home, records: Option<PathBuf>) -> Context {
    Context {
        env: home.env(),
        local_node: Some("node".into()),
        records,
        grace_rounds: 0,
        lock_wait: std::time::Duration::from_millis(200),
        ready_wait: std::time::Duration::from_millis(200),
    }
}

fn launch(socket: &Path, node: &str) -> crate::services::herdr_launch::HerdrLaunchEndpoint {
    crate::services::herdr_launch::HerdrLaunchEndpoint {
        execution_node: node.into(),
        config_key: "user".into(),
        socket_addr: socket.display().to_string(),
        herdr_session: "default".into(),
    }
}

#[tokio::test]
async fn ensure_refuses_another_node_and_a_non_default_socket_before_any_reading() {
    let home = Home::new(Some(FALSE));
    let records = home.home.join("records");
    let _scoped = scoped_context(&home.socket(), context(&home, Some(records.clone())));
    let before = home.tree();
    assert_eq!(
        ensure(&launch(&home.socket(), "elsewhere")).await,
        Ensured::Refused("remote")
    );
    let named = home.home.join(".config/herdr/sessions/work/herdr.sock");
    assert_eq!(
        ensure_at(&named, &context(&home, Some(records.clone()))).await,
        Ensured::Refused("socket_not_default")
    );
    assert_eq!(home.tree(), before, "neither wrote anything");
    assert_eq!(last(&home.socket()).unwrap()["reason"], "remote");
}

#[tokio::test]
async fn ensure_refuses_before_spawning_when_no_record_or_binary_is_available() {
    let home = Home::new(Some(FALSE));
    let socket = home.socket();
    assert_eq!(
        ensure_at(&socket, &context(&home, None)).await,
        Ensured::Refused("record_unavailable")
    );
    let records = home.home.join("records");
    assert_eq!(
        ensure_at(&socket, &context(&home, Some(records.clone()))).await,
        Ensured::Refused("binary_not_found")
    );
    assert!(
        !records.exists(),
        "nothing is recorded for a start that never happened"
    );
    assert!(!socket.exists());
    let shown = last(&socket).unwrap();
    assert_eq!(shown["reason"], "binary_not_found", "{shown}");
    assert_eq!(shown["presence"], "absent:no_file", "{shown}");
    let health = super::super::policy::diagnostics::snapshot(&socket);
    assert_eq!(
        health["autostart"], shown,
        "health shows the last start decision"
    );
}

#[tokio::test]
async fn ensure_backs_off_after_three_spawns_in_the_window() {
    let home = Home::new(Some(FALSE));
    let socket = home.socket();
    let records = home.home.join("records");
    let mut full = record::Record {
        socket: socket.clone(),
        spawns: Vec::new(),
    };
    for _ in 0..record::LIMIT {
        full.begin(record::now());
        full.finish(Some(1), "not_ready");
    }
    record::save(&records, &full).unwrap();
    assert_eq!(
        ensure_at(&socket, &context(&home, Some(records.clone()))).await,
        Ensured::Refused("backoff")
    );
    assert_eq!(
        record::load(&records, &socket).unwrap(),
        full,
        "a refusal records nothing"
    );
}

#[test]
fn a_full_window_of_spawns_backs_off() {
    let mut record = record::Record::default();
    for at in [1_000, 1_100, 1_200] {
        assert!(!record.exhausted(at));
        record.begin(at);
        record.finish(Some(7), "started");
    }
    assert!(record.exhausted(1_300));
    assert!(
        !record.exhausted(1_000 + record::WINDOW + 1),
        "the first spawn left the window"
    );
    assert!(record.exhausted(900), "a clock moved back still counts");
}

#[tokio::test]
async fn a_fresh_launch_permit_asks_for_its_server_before_the_policy() {
    use crate::services::herdr_launch::HerdrLaunchHost;
    let home = Home::new(Some("[session]\nresume_agents_on_restore = true\n"));
    let socket = home.socket();
    let _scoped = scoped_context(&socket, context(&home, Some(home.home.join("records"))));
    let endpoint = super::super::model::HerdrEndpoint::new("node", "user", &socket, "default")
        .unwrap()
        .with_mode(crate::config::session_hosts::EndpointMode::UserDefault);
    let host = super::super::launch_host::SocketHerdrLaunchHost::new(
        vec![endpoint],
        super::super::transport::HerdrSocketConfig::default(),
    );
    let before = spawn::ensured_on_this_thread();
    let permit = host.create_permit(&launch(&socket, "node")).await;
    assert!(
        permit.is_err(),
        "no server answers, so the policy refuses the permit"
    );
    assert_eq!(spawn::ensured_on_this_thread(), before + 1);
    let shown = last(&socket).unwrap();
    assert_eq!(shown["reason"], "config_resume_not_off", "{shown}");
}
