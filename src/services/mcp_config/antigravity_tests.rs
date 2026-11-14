use super::*;
use crate::services::mcp_config::retirement_tests::{isolated, write};
use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Server {
    url: String,
    tools: Arc<Mutex<Vec<String>>>,
    failed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn new(sse: bool) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let tools = Arc::new(Mutex::new(vec![
            "remember".into(),
            "recall".into(),
            "reflect".into(),
            "apply_update".into(),
            "check_update".into(),
            "memory_consolidate".into(),
            "session_rotate".into(),
        ]));
        let failed = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (tool_list, fail, stop) = (tools.clone(), failed.clone(), stopped.clone());
        let protocol = if sse { "2025-11-25" } else { "2025-06-18" };
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("mock accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                let (headers, body) = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8(bytes[..end].to_vec())
                            .unwrap()
                            .to_ascii_lowercase();
                        let len: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + len {
                            break (
                                headers,
                                serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + len])
                                    .unwrap(),
                            );
                        }
                    }
                };
                assert!(headers.contains("accept: application/json, text/event-stream"));
                let id = &body["id"];
                let (status, content_type, response) = if fail.load(Ordering::SeqCst) {
                    (
                        "500 Internal Server Error",
                        "application/json",
                        "fake-bearer-secret".to_string(),
                    )
                } else if body["method"] == "initialize" {
                    ("200 OK", "application/json", json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":protocol,"capabilities":{"tools":{}}}}).to_string())
                } else {
                    assert!(headers.contains("mcp-session-id: fixture-session"));
                    assert!(headers.contains(&format!("mcp-protocol-version: {protocol}")));
                    if body["method"] == "notifications/initialized" {
                        ("202 Accepted", "application/json", String::new())
                    } else {
                        assert_eq!(body["method"], "tools/list");
                        let tools = tool_list.lock().unwrap();
                        let page = if body["params"]["cursor"] == "next" {
                            json!({"tools":tools.iter().skip(1).map(|name| json!({"name":name})).collect::<Vec<_>>()})
                        } else {
                            json!({"tools":[{"name":tools[0]}],"nextCursor":"next"})
                        };
                        let rpc = json!({"jsonrpc":"2.0","id":id,"result":page}).to_string();
                        if sse {
                            (
                                "200 OK",
                                "text/event-stream",
                                format!(
                                    "data: {{\"jsonrpc\":\"2.0\",\"method\":\"notification\"}}\r\n\r\nevent: message\r\ndata: {rpc}\r\n\r\n"
                                ),
                            )
                        } else {
                            ("200 OK", "application/json", rpc)
                        }
                    }
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nMCP-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        Self {
            url,
            tools,
            failed,
            stopped,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        crate::test_support::wait::join_within(
            self.worker.take().unwrap(),
            std::time::Duration::from_secs(5),
        );
    }
}

fn source(home: &Path, server: &Server, legacy: bool) {
    write(&home.join(if legacy { ".claude/.mcp.json" } else { ".claude.json" }), &json!({
        "mcpServers":{"memento":{"type":"http","url":server.url,"headers":{"Authorization":"Bearer fake-bearer-secret"}}},
        "projects":{"/private/project":{"mcpServers":{"project-only":{"url":"http://project.test"}}}},
        "other":"fake-private-value"
    }).to_string());
}

fn settings_path(home: &Path) -> PathBuf {
    home.join(".gemini/antigravity-cli/settings.json")
}
fn config_path(home: &Path) -> PathBuf {
    home.join(".gemini/config/mcp_config.json")
}
fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
fn state_path() -> PathBuf {
    crate::runtime_layout::config_dir(&crate::config::runtime_root().unwrap())
        .join("antigravity-mcp-sync-state.json")
}

#[test]
fn antigravity_syncs_http_servers_and_specific_permissions_from_user_scope() {
    isolated(|home| {
        let server = Server::new(true);
        source(home, &server, false);
        write(
            &settings_path(home),
            r#"{"permissions":{"allow":["shell(ls)","mcp(manual/tool)","mcp(memento/remember)","shell(pwd)"],"deny":["manual"]},"theme":"user"}"#,
        );
        write(
            &config_path(home),
            r#"{"mcpServers":{"manual":{"command":"user"}},"theme":"user"}"#,
        );
        let mut runtime = Config::default();
        runtime.mcp_servers.insert(
            "memento".into(),
            McpServerConfig {
                url: "http://runtime-default.test/mcp".into(),
                auth: None,
            },
        );
        let state_path = state_path();
        let fault = crate::domain::atomic_write::rename_fault::Armed::new(&state_path);
        assert!(sync(&runtime).is_err());
        let (attempts, errors) = fault.evidence();
        assert_eq!(attempts, 1);
        assert_eq!(errors.len(), 1);
        drop(fault);
        sync(&runtime).unwrap();
        let target = read(&config_path(home));
        assert_eq!(
            target["mcpServers"]["memento"],
            json!({"disabled":false,"serverUrl":server.url,"headers":{"Authorization":"Bearer fake-bearer-secret"}})
        );
        assert_eq!(target["mcpServers"]["manual"]["command"], "user");
        assert_eq!(target["theme"], "user");
        assert!(target["mcpServers"].get("project-only").is_none());
        assert!(target.get("other").is_none());
        let settings = read(&settings_path(home));
        assert_eq!(
            settings["permissions"]["allow"],
            json!([
                "shell(ls)",
                "mcp(manual/tool)",
                "mcp(memento/remember)",
                "shell(pwd)",
                "mcp(memento/recall)"
            ])
        );
        assert_eq!(settings["permissions"]["deny"], json!(["manual"]));
        assert_eq!(settings["theme"], "user");
        let state = std::fs::read_to_string(&state_path).unwrap();
        assert!(!state.contains("fake-bearer-secret"));
        assert!(!state.contains("fake-private-value"));
        assert!(!state.contains(&server.url));
        assert!(!state.contains("mcp(memento/remember)"));
        let bytes = std::fs::read(settings_path(home)).unwrap();
        sync(&runtime).unwrap();
        assert_eq!(std::fs::read(settings_path(home)).unwrap(), bytes);
        *server.tools.lock().unwrap() = vec!["remember".into(), "new_tool".into()];
        let fault = crate::domain::atomic_write::rename_fault::Armed::new(&settings_path(home));
        assert!(sync(&runtime).is_err());
        let (attempts, errors) = fault.evidence();
        assert_eq!(attempts, 1);
        assert_eq!(errors.len(), 1);
        assert_eq!(std::fs::read(settings_path(home)).unwrap(), bytes);
        drop(fault);
        sync(&runtime).unwrap();
        assert_eq!(
            read(&settings_path(home))["permissions"]["allow"],
            json!([
                "shell(ls)",
                "mcp(manual/tool)",
                "mcp(memento/remember)",
                "shell(pwd)",
                "mcp(memento/new_tool)"
            ])
        );
        runtime.mcp_servers.clear();
        write(&home.join(".claude.json"), r#"{"mcpServers":{}}"#);
        sync(&runtime).unwrap();
        assert_eq!(
            read(&settings_path(home))["permissions"]["allow"],
            json!([
                "shell(ls)",
                "mcp(manual/tool)",
                "mcp(memento/remember)",
                "shell(pwd)"
            ])
        );
        assert!(
            read(&config_path(home))["mcpServers"]
                .get("memento")
                .is_none()
        );
    });
}

#[test]
fn antigravity_discovery_failure_keeps_existing_allow_and_never_reports_secrets() {
    isolated(|home| {
        let server = Server::new(false);
        source(home, &server, true);
        write(
            &settings_path(home),
            r#"{"permissions":{"allow":["manual(first)","manual(second)"]}}"#,
        );
        sync(&Config::default()).unwrap();
        let settings = std::fs::read(settings_path(home)).unwrap();
        let state = std::fs::read(state_path()).unwrap();
        server.failed.store(true, Ordering::SeqCst);
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || Capture(output.clone()))
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let error =
            tracing::subscriber::with_default(subscriber, || sync(&Config::default())).unwrap_err();
        let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(!logs.contains("fake-bearer-secret"));
        assert!(!error.contains("fake-bearer-secret"));
        assert!(!error.contains(&server.url));
        assert_eq!(std::fs::read(settings_path(home)).unwrap(), settings);
        assert_eq!(std::fs::read(state_path()).unwrap(), state);
        server.failed.store(false, Ordering::SeqCst);
        *server.tools.lock().unwrap() = vec!["new_tool".into(), "reflect".into()];
        sync(&Config::default()).unwrap();
        assert_eq!(
            read(&settings_path(home))["permissions"]["allow"],
            json!(["manual(first)", "manual(second)", "mcp(memento/new_tool)"])
        );
        source(home, &server, false);
        write(&home.join(".claude/.mcp.json"), r#"{"mcpServers":{}}"#);
        sync(&Config::default()).unwrap();
        assert_eq!(
            read(&settings_path(home))["permissions"]["allow"],
            json!(["manual(first)", "manual(second)"])
        );
        assert!(
            read(&config_path(home))["mcpServers"]
                .get("memento")
                .is_none()
        );
    });
}

#[test]
fn antigravity_rejects_wildcard_tools_and_invalid_settings_without_loss() {
    isolated(|home| {
        let server = Server::new(false);
        source(home, &server, false);
        sync(&Config::default()).unwrap();
        let settings = std::fs::read(settings_path(home)).unwrap();
        *server.tools.lock().unwrap() = vec!["*".into(), "recall".into()];
        assert!(sync(&Config::default()).is_err());
        assert_eq!(std::fs::read(settings_path(home)).unwrap(), settings);
        let config = std::fs::read(config_path(home)).unwrap();
        write(&settings_path(home), r#"{"permissions":{"allow":{}}}"#);
        assert!(sync(&Config::default()).is_err());
        assert_eq!(std::fs::read(config_path(home)).unwrap(), config);
        assert_eq!(
            std::fs::read_to_string(settings_path(home)).unwrap(),
            r#"{"permissions":{"allow":{}}}"#
        );
    });
}

struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
