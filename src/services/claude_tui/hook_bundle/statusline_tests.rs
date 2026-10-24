use super::*;
use std::io::Write;
use std::process::{Command, Output, Stdio};

struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    records: PathBuf,
}

impl Fixture {
    fn new(original: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home with 'quotes'");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let value = original.map_or(
            json!({}),
            |command| json!({"statusLine":{"type":"command","command":command}}),
        );
        std::fs::write(home.join(".claude/settings.json"), value.to_string()).unwrap();
        let records = root.path().join("records with 'quotes'");
        Self {
            root,
            home,
            records,
        }
    }

    fn run(&self, raw: &[u8]) -> Output {
        run(
            &self.home,
            settings(&self.records)["command"].as_str().unwrap(),
            raw,
        )
    }
}

fn run(home: &Path, command: &str, raw: &[u8]) -> Output {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(raw).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn passthrough_preserves_original_stdin_and_stdout_byte_for_byte() {
    let original = r"printf '\033[32m사용량\033[0m\n'; cat; printf '\n끝  '";
    let fixture = Fixture::new(Some(original));
    let raw = b" {\"session_id\":\"session-a\", \"rate_limits\":{\"five_hour\":{\"used_percentage\":24,\"resets_at\":1792722600}},\"secret\":\"not cached\"} \r\n\n";
    let expected = run(&fixture.home, original, raw);
    let actual = fixture.run(raw);
    assert!(actual.status.success(), "{:?}", actual.stderr);
    assert_eq!(
        actual.stdout, expected.stdout,
        "passthrough must preserve exact display bytes"
    );
    assert_eq!(actual.stderr, expected.stderr);
    let recorded: Value =
        serde_json::from_slice(&std::fs::read(record_path(&fixture.records, "session-a")).unwrap())
            .unwrap();
    assert_eq!(recorded.as_object().unwrap().len(), 2);
    assert_eq!(recorded["session_id"], "session-a");
    assert_eq!(
        recorded["rate_limits"],
        serde_json::from_slice::<Value>(raw).unwrap()["rate_limits"]
    );
}

#[test]
fn recorder_errors_and_malformed_stdin_do_not_block_the_original_display() {
    let fixture = Fixture::new(Some("cat; printf '\\nvisible'"));
    for raw in [b"{malformed input\r\n".as_slice(), b"[]", b"null"] {
        let output = fixture.run(raw);
        assert!(output.status.success());
        assert_eq!(output.stdout, [raw, b"\nvisible"].concat());
    }
    std::fs::write(&fixture.records, b"not a directory").unwrap();
    let raw = br#"{"session_id":"session-a","rate_limits":{}}"#;
    let output = fixture.run(raw);
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        [raw.as_slice(), b"\nvisible"].concat(),
        "recording I/O failure must not hide the user's display"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn absent_statusline_is_empty_and_config_dir_override_selects_the_user_command() {
    let fixture = Fixture::new(None);
    let raw = br#"{"session_id":"session-a","rate_limits":null}"#;
    let output = fixture.run(raw);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(record_path(&fixture.records, "session-a").exists());
    let alternate = fixture.root.path().join("alternate config");
    std::fs::create_dir_all(&alternate).unwrap();
    std::fs::write(
        alternate.join("settings.json"),
        json!({"statusLine":{
        "type":"command", "command":"printf alternate; exit 7"}})
        .to_string(),
    )
    .unwrap();
    let command = settings(&fixture.records)["command"]
        .as_str()
        .unwrap()
        .to_string();
    let mut child = Command::new("sh")
        .args(["-c", &command])
        .env("HOME", &fixture.home)
        .env("CLAUDE_CONFIG_DIR", alternate)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(raw).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.stdout, b"alternate");
    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn concurrent_sessions_publish_complete_isolated_records_by_atomic_rename() {
    let fixture = Fixture::new(None);
    let workers = (0..8)
        .map(|index| {
            let command = settings(&fixture.records)["command"]
                .as_str()
                .unwrap()
                .to_string();
            let home = fixture.home.clone();
            std::thread::spawn(move || {
                for _ in 0..3 {
                    let raw = json!({"session_id":format!("session-{index}"),
                    "rate_limits":{"five_hour":{"used_percentage":index,"resets_at":1792722600}}});
                    let output = run(&home, &command, raw.to_string().as_bytes());
                    assert!(output.status.success());
                }
            })
        })
        .collect::<Vec<_>>();
    while workers.iter().any(|worker| !worker.is_finished()) {
        if let Ok(entries) = std::fs::read_dir(&fixture.records) {
            for entry in entries {
                let path = entry.unwrap().path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    let data: Value =
                        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                    assert!(data["rate_limits"]["five_hour"]["used_percentage"].is_number());
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    for index in 0..8 {
        let session = format!("session-{index}");
        let data: Value = serde_json::from_slice(
            &std::fs::read(record_path(&fixture.records, &session)).unwrap(),
        )
        .unwrap();
        assert_eq!(data["session_id"], session);
        assert_eq!(data["rate_limits"]["five_hour"]["used_percentage"], index);
    }
    assert_eq!(std::fs::read_dir(&fixture.records).unwrap().count(), 8);
}

#[test]
fn generated_session_settings_install_the_executable_wrapper() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let fixture = Fixture::new(Some("printf 'user status'"));
    let config = super::super::HookBundleConfig {
        endpoint: "http://127.0.0.1:49152".into(),
        provider: "claude".into(),
        session_id: uuid::Uuid::new_v4().to_string(),
        agentdesk_exe: "/fixture/agentdesk".into(),
    };
    let path = fixture.root.path().join("session-settings.json");
    super::super::write_claude_hook_settings(&path, &config).unwrap();
    let settings: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(settings["statusLine"]["type"], "command");
    let output = run(
        &fixture.home,
        settings["statusLine"]["command"].as_str().unwrap(),
        json!({"session_id":config.session_id,"rate_limits":{
            "five_hour":{"used_percentage":24,"resets_at":chrono::Utc::now().timestamp()+18000}
        }})
        .to_string()
        .as_bytes(),
    );
    let record = record_path(&record_dir(), &config.session_id);
    let exists = record.exists();
    let quota = crate::services::turn_summary_line::quota::snapshot(
        "claude",
        7_319_003,
        None,
        Some(config.session_id.as_str()),
    );
    let line = crate::services::turn_summary_line::render(
        crate::services::turn_summary_line::Outcome::Completed,
        Some(1000),
        None,
        quota,
    );
    let _ = std::fs::remove_file(record);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"user status");
    assert!(line.starts_with("-# ⏱️ 1초 · 5h 24% ("), "{line}");
    assert!(
        exists,
        "the session --settings command must record its stdin"
    );
}
