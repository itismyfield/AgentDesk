use super::*;
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

fn child(test: &str) -> bool {
    if std::env::var_os(MARKER).is_some() {
        return true;
    }
    let (_temporary, root) = super::tests::fixture();
    let output = super::tests::command(&root)
        .args([
            "--exact",
            test.strip_prefix("agentdesk::").unwrap(),
            "--nocapture",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    false
}

fn pin_listener(root: &Path) -> TcpListener {
    let listener = (18791..18807)
        .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("a private instance API port must be available");
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}");
    for name in ["manifest.json", "receipt.json", "config/agentdesk.yaml"] {
        let path = root.join(name);
        let mut value = read_json(&path).unwrap();
        if name == "config/agentdesk.yaml" {
            value["server"]["port"] = json!(port);
            value["cluster"]["api_base_url"] = json!(endpoint);
        } else {
            value["api"] = json!({"host":"127.0.0.1", "port":port, "self_url":endpoint});
        }
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    // Only this isolated child observes the declaration and its environment.
    unsafe {
        std::env::set_var("AGENTDESK_API_URL", &endpoint);
        std::env::set_var("ADK_API_URL", endpoint);
    }
    listener
}

fn receive(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before its headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let body_length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + body_length {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }
}

#[test]
fn instance_api_and_monitoring_stay_on_receipt_endpoint() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    if !child(concat!(
        module_path!(),
        "::instance_api_and_monitoring_stay_on_receipt_endpoint"
    )) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let listener = pin_listener(&root);
    listener.set_nonblocking(true).unwrap();
    let endpoint = std::env::var("AGENTDESK_API_URL").unwrap();
    let foreign = TcpListener::bind("127.0.0.1:0").unwrap();
    foreign.set_nonblocking(true).unwrap();
    let foreign_url = format!("http://{}", foreign.local_addr().unwrap());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let escaped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let foreign_worker = {
        let stop = stop.clone();
        let escaped = escaped.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !stop.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
                match foreign.accept() {
                    Ok((mut stream, _)) => {
                        escaped.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = receive(&mut stream);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("foreign listener: {error}"),
                }
            }
        })
    };
    initialize().unwrap();
    let mut proxy_guards = Vec::new();
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        proxy_guards.push(
            crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(key, &foreign_url),
        );
    }
    let _no_proxy =
        crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock("NO_PROXY", "");
    let _lower_no_proxy =
        crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock("no_proxy", "");
    let redirect = foreign_url.clone();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut requests = Vec::new();
        while requests.len() < 5 && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let request = receive(&mut stream);
                    let redirecting = request.lines().next().unwrap().contains("/redirect")
                        || request.contains("\"key\":\"redirect\"");
                    let response = if redirecting {
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {redirect}/escaped\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                        )
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()
                    };
                    stream.write_all(response.as_bytes()).unwrap();
                    requests.push(request);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("private listener: {error}"),
            }
        }
        requests
    });
    assert_eq!(
        crate::cli::client::get_json_at(&endpoint, "/api/positive").unwrap(),
        json!({})
    );
    crate::cli::monitoring::start(900004, "instance-proof", "private registration").unwrap();
    crate::cli::monitoring::stop(900004, "instance-proof").unwrap();
    assert!(crate::cli::client::get_json_at(&endpoint, "/api/redirect").is_err());
    // The monitoring API uses its own transport and must reject redirects too.
    assert!(crate::cli::monitoring::start(900004, "redirect", "redirect").is_err());
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[0].starts_with("GET /api/positive HTTP/1.1"));
    assert!(requests[1].starts_with("POST /api/channels/900004/monitoring HTTP/1.1"));
    let registration: Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        registration,
        json!({"key":"instance-proof", "description":"private registration"})
    );
    assert!(
        requests[2].starts_with("DELETE /api/channels/900004/monitoring/instance-proof HTTP/1.1")
    );
    assert_eq!(escaped.load(std::sync::atomic::Ordering::SeqCst), 0);
    for target in [&foreign_url, "http://127.0.0.1:8791"] {
        assert!(crate::cli::client::get_json_at(target, "/api/negative").is_err());
        assert!(
            crate::cli::client::post_json_local_at(target, "/api/negative", &json!({})).is_err()
        );
    }
    let _override = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_API_URL",
        &foreign_url,
    );
    assert!(crate::cli::monitoring::start(900004, "foreign", "foreign").is_err());
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    foreign_worker.join().unwrap();
    assert_eq!(escaped.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn instance_service_management_refuses_before_fake_programs() {
    use crate::cli::{dcserver, doctor};
    use std::os::unix::fs::PermissionsExt;
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    if !child(concat!(
        module_path!(),
        "::instance_service_management_refuses_before_fake_programs"
    )) {
        return;
    }
    initialize().unwrap();
    let instance = current().unwrap().unwrap();
    let programs = instance.root.join("fixture-worktree/target/debug");
    std::fs::create_dir_all(&programs).unwrap();
    let trace = instance.root.join("service-program-trace");
    for program in ["id", "launchctl", "tmux", "systemctl", "sc", "nssm"] {
        let path = programs.join(program);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' called >> '{}'\nexit 0\n",
                trace.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(dcserver::current_launchd_domain().is_none());
    assert!(!dcserver::is_launchd_job_loaded("com.agentdesk.release"));
    assert!(!dcserver::kickstart_launchd_job("com.agentdesk.release"));
    assert!(!dcserver::is_tmux_fallback_session_loaded(
        "AgentDesk-dcserver-release-manual"
    ));
    assert!(!dcserver::is_systemd_service_active());
    assert!(!dcserver::is_systemd_service_enabled());
    assert!(!dcserver::restart_systemd_service());
    assert!(!dcserver::is_windows_service_installed());
    assert!(!dcserver::is_windows_service_running());
    assert!(!dcserver::restart_windows_service());
    let timeout = Duration::ZERO;
    assert!(
        dcserver::restart_launchd_dcserver_and_verify("com.agentdesk.release", timeout).is_err()
    );
    assert!(dcserver::restart_systemd_dcserver_and_verify(timeout).is_err());
    assert!(dcserver::restart_windows_dcserver_and_verify(timeout).is_err());
    assert!(dcserver::rollback_to_previous_release("com.agentdesk.release", timeout).is_err());
    assert!(dcserver::handle_restart_dcserver(None).is_err());
    assert!(
        doctor::run_doctor_report(doctor::DoctorOptions {
            fix: true,
            json: true,
            allow_restart: true,
            repair_sqlite_cache: true,
            allow_remote: true,
            profile: None,
            run_context: doctor::contract::RunContext::ManualCli,
            artifact_path: None,
        })
        .is_err()
    );
    assert!(
        !trace.exists(),
        "service helper invoked a fake operating program"
    );
    assert!(!instance.root.join("restart_pending").exists());
}
